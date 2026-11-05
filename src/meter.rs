//! Live status line: elapsed time, a level meter per channel, and hints.
//! The writer thread publishes energy (`Levels`); the UI thread renders it.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::audio::{CHANNELS, power_db};
use crate::ui::{COLOR, TTY, fmt_duration, paint};

/// Energy accumulated since the last UI read (writer thread → UI thread; not
/// real-time, so a mutex is fine).
#[derive(Default)]
pub struct Levels {
    /// Sum of squares per channel, and the number of frames.
    acc: Mutex<([f64; CHANNELS], u64)>,
    /// Any non-zero sample ever seen. Pure digital silence usually means "no permission".
    seen: [AtomicBool; CHANNELS],
}

impl Levels {
    /// Accumulate interleaved frames.
    pub fn record(&self, frames: &[f32]) {
        let mut sums = [0f64; CHANNELS];
        for frame in frames.chunks_exact(CHANNELS) {
            for (s, &x) in sums.iter_mut().zip(frame) {
                *s += (x as f64) * (x as f64);
            }
        }
        for (seen, &s) in self.seen.iter().zip(&sums) {
            if s > 0.0 {
                seen.store(true, Ordering::Relaxed);
            }
        }
        let mut acc = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        for (a, s) in acc.0.iter_mut().zip(sums) {
            *a += s;
        }
        acc.1 += (frames.len() / CHANNELS) as u64;
    }

    /// Mean square of each channel since the previous call; `None` if no new audio arrived.
    fn take(&self) -> Option<[f32; CHANNELS]> {
        let (sums, n) = std::mem::take(&mut *self.acc.lock().unwrap_or_else(|e| e.into_inner()));
        (n > 0).then(|| sums.map(|s| (s / n as f64) as f32))
    }
}

const LABELS: [&str; CHANNELS] = ["MIC", "SYS"];
/// Shown once if a channel has delivered nothing but digital silence for this long.
const SILENCE_HINTS: [(Duration, &str); CHANNELS] = [
    (
        Duration::from_secs(3),
        "The microphone is completely silent. Your terminal app may lack microphone \
         access: System Settings › Privacy & Security › Microphone",
    ),
    (
        Duration::from_secs(10),
        "System audio is completely silent. If something is playing, check: System \
         Settings › Privacy & Security › Screen & System Audio Recording › System Audio \
         Recording Only",
    ),
];
const FLOOR_DB: f32 = -60.0;
/// Above this counts as "active" (-50 dBFS ≈ quiet room tone; speech/music is well above).
const ACTIVE_DB: f32 = -50.0;
/// Bar release per UI tick (100 ms) → 40 dB/s.
const RELEASE_DB: f32 = 4.0;
const HOLD: Duration = Duration::from_millis(400);
const BAR_WIDTH: usize = 16;
/// Without a terminal (pipes, logs), print a plain status line this often.
const PLAIN_INTERVAL: Duration = Duration::from_secs(5);

struct Meter {
    label: &'static str,
    shown_db: f32,
    last_active: Option<Instant>,
    hinted: bool,
}

impl Meter {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            shown_db: f32::NEG_INFINITY,
            last_active: None,
            hinted: false,
        }
    }

    fn update(&mut self, mean_square: f32, now: Instant) {
        let db = power_db(mean_square);
        // Instant attack, smooth release so the bar is readable.
        self.shown_db = if db >= self.shown_db {
            db
        } else {
            (self.shown_db - RELEASE_DB).max(db)
        };
        if db > ACTIVE_DB {
            self.last_active = Some(now);
        }
    }

    fn reset(&mut self) {
        self.shown_db = f32::NEG_INFINITY;
        self.last_active = None;
    }

    fn render(&self, now: Instant) -> String {
        let frac = ((self.shown_db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
        let filled = (frac * BAR_WIDTH as f32).round() as usize;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled));
        let bar = if *COLOR {
            format!("▕{bar}▏")
        } else {
            format!("[{bar}]")
        };
        let db = if self.shown_db > FLOOR_DB {
            format!("{:>4.0} dB", self.shown_db)
        } else {
            "  -∞ dB".to_string()
        };
        let active = self
            .last_active
            .is_some_and(|t| now.duration_since(t) < HOLD);
        let (code, state) = if active {
            ("32", "● active")
        } else {
            ("2", "○ silent")
        };
        format!(
            "{} {} {db} {}",
            self.label,
            paint(code, &bar),
            paint(code, state)
        )
    }
}

pub struct StatusLine {
    meters: [Meter; CHANNELS],
    /// Set when the first audio buffer arrives (device spin-up can take a moment).
    started: Option<Instant>,
    /// Set while the capture is being rebuilt after a device change.
    reconnecting: bool,
    last_plain: Option<Instant>,
}

impl StatusLine {
    pub fn new() -> Self {
        Self {
            meters: LABELS.map(Meter::new),
            started: None,
            reconnecting: false,
            last_plain: None,
        }
    }

    pub fn mark_started(&mut self) {
        self.started.get_or_insert_with(Instant::now);
    }

    pub fn set_reconnecting(&mut self, on: bool) {
        if on && !self.reconnecting {
            self.meters.iter_mut().for_each(Meter::reset);
        }
        self.reconnecting = on;
    }

    pub fn tick(&mut self, levels: &Levels, dropped_frames: u64) {
        let now = Instant::now();
        if let Some(ms) = levels.take() {
            for (m, ms) in self.meters.iter_mut().zip(ms) {
                m.update(ms, now);
            }
        }
        let Some(started) = self.started else {
            if *TTY {
                self.draw("◌ Starting, waiting for audio devices…");
            }
            return;
        };

        let elapsed = now.duration_since(started);
        for (i, (after, hint)) in SILENCE_HINTS.iter().enumerate() {
            if !self.meters[i].hinted && elapsed > *after && !levels.seen[i].load(Ordering::Relaxed)
            {
                self.meters[i].hinted = true;
                self.warn(hint);
            }
        }

        let badge = if self.reconnecting {
            paint("33", "⟳ WAIT")
        } else {
            format!("{} REC ", paint("31", "●"))
        };
        let mut parts = vec![format!("{badge} {}", fmt_duration(elapsed))];
        parts.extend(self.meters.iter().map(|m| m.render(now)));
        if self.reconnecting {
            parts.push("reconnecting…".into());
        }
        if dropped_frames > 0 {
            parts.push(format!("dropped {dropped_frames}"));
        }
        let line = parts.join(" │ ");

        if *TTY {
            self.draw(&line);
        } else if self
            .last_plain
            .is_none_or(|t| now.duration_since(t) >= PLAIN_INTERVAL)
        {
            self.last_plain = Some(now);
            eprintln!("{line}");
        }
    }

    fn draw(&self, line: &str) {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{line}");
        let _ = err.flush();
    }

    /// Print a message above the live status line.
    pub fn message(&self, msg: &str) {
        self.clear();
        eprintln!("{msg}");
    }

    pub fn warn(&self, msg: &str) {
        self.message(&paint("33", &format!("⚠ {msg}")));
    }

    pub fn clear(&self) {
        if *TTY {
            self.draw("");
        }
    }
}
