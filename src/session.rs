//! Keeping a capture running on the system default devices.
//!
//! A `Session` is one capture bound to specific devices. The `Supervisor`
//! watches for device changes and stalls, tears the session down, and builds a
//! new one; every session's ring buffer is handed to the writer in order.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ringbuf::HeapRb;
use ringbuf::traits::Split;

use crate::coreaudio::{self, Capture, Device, DeviceWatch};
use crate::meter::StatusLine;
use crate::ui::{self, paint};
use crate::writer::Feed;

/// Ring buffer capacity per session in (mic, sys) frames: ~10 s at 48 kHz.
const RING_FRAMES: usize = 48_000 * 10;
/// Device notifications come in bursts (e.g. Bluetooth connect); act once they settle.
const DEVICE_DEBOUNCE: Duration = Duration::from_millis(500);
/// No callbacks for this long → treat the session as dead and rebuild it.
const STALL_TIMEOUT: Duration = Duration::from_millis(1500);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// One capture bound to specific devices. Dropping it tears the IO down and
/// marks its feed closed so the writer can finish draining it.
pub struct Session {
    pub mic: Device,
    pub output: Device,
    mic_rate: Option<f64>,
    output_rate: Option<f64>,
    /// Always `Some` until drop, which needs to destroy it before closing the feed.
    capture: Option<Capture>,
    closed: Arc<AtomicBool>,
    feed: Option<Feed>,
    _watch: DeviceWatch,
    last_callbacks: u64,
    last_progress: Instant,
}

impl Session {
    /// Build a capture for the current system default devices.
    fn open(dropped: &Arc<AtomicU64>, events: &Arc<AtomicU64>) -> Result<Self> {
        let mic = Device::default_input()?;
        let output = Device::default_output()?;
        let (producer, consumer) = HeapRb::<f32>::new(RING_FRAMES * 2).split();
        let capture = Capture::new(&mic, &output, producer, dropped.clone())?;

        let mut watch = DeviceWatch::new(events.clone());
        watch.watch_device(&mic);
        if output.uid != mic.uid {
            watch.watch_device(&output);
        }
        let closed = Arc::new(AtomicBool::new(false));
        let feed = Feed {
            cons: consumer,
            rate: capture.sample_rate,
            diag: capture.diag.clone(),
            closed: closed.clone(),
        };
        Ok(Self {
            mic_rate: mic.nominal_rate(),
            output_rate: output.nominal_rate(),
            mic,
            output,
            capture: Some(capture),
            closed,
            feed: Some(feed),
            _watch: watch,
            last_callbacks: 0,
            last_progress: Instant::now(),
        })
    }

    fn capture(&self) -> &Capture {
        self.capture.as_ref().expect("capture present until drop")
    }

    /// Hand the session's ring buffer to the writer, then start IO.
    fn start(&mut self, tx: &Sender<Feed>) -> Result<()> {
        if let Some(feed) = self.feed.take() {
            tx.send(feed)
                .map_err(|_| anyhow!("The writer thread has stopped"))?;
        }
        self.last_progress = Instant::now();
        self.capture.as_mut().expect("capture").start()
    }

    pub fn rate(&self) -> f64 {
        self.capture().sample_rate
    }

    fn callbacks(&self) -> u64 {
        self.capture().diag.callbacks.load(Ordering::Relaxed)
    }

    /// True if the IOProc has not run for `STALL_TIMEOUT` (device gone, HAL hiccup…).
    fn stalled(&mut self) -> bool {
        let cb = self.callbacks();
        if cb != self.last_callbacks {
            self.last_callbacks = cb;
            self.last_progress = Instant::now();
            false
        } else {
            self.last_progress.elapsed() > STALL_TIMEOUT
        }
    }

    /// Why this session no longer matches the system's audio setup, if it doesn't.
    fn outdated(&self) -> Option<&'static str> {
        if !self.mic.is_alive() {
            return Some("microphone disconnected");
        }
        if !self.output.is_alive() {
            return Some("output device disconnected");
        }
        if Device::default_input().is_ok_and(|d| d.uid != self.mic.uid) {
            return Some("default microphone changed");
        }
        if Device::default_output().is_ok_and(|d| d.uid != self.output.uid) {
            return Some("default output device changed");
        }
        if self.mic.nominal_rate() != self.mic_rate
            || self.output.nominal_rate() != self.output_rate
        {
            return Some("device sample rate changed");
        }
        None
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Stop IO and destroy the IOProc first; only then is it safe to declare the feed closed.
        drop(self.capture.take());
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Keeps one session running on the current default devices, rebuilding it
/// when they change or IO stalls. Dropping it stops the capture.
pub struct Supervisor {
    session: Option<Session>,
    tx: Sender<Feed>,
    /// Bumped by device listeners; debounced in `poll`.
    events: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    _defaults_watch: DeviceWatch,
    seen_events: u64,
    debounce_until: Option<Instant>,
    next_retry: Instant,
    retry_warned: bool,
    /// While set, a replacement session only counts as recovered once its first buffer arrives.
    reconnecting: bool,
    /// Devices of the last session that actually delivered audio.
    mic_name: String,
    out_name: String,
}

impl Supervisor {
    /// Open a session on the current default devices; `start` begins capturing.
    pub fn new(tx: Sender<Feed>) -> Result<Self> {
        let events = Arc::new(AtomicU64::new(0));
        let mut defaults_watch = DeviceWatch::new(events.clone());
        if let Err(e) =
            coreaudio::use_hal_notification_thread().and_then(|_| defaults_watch.watch_defaults())
        {
            ui::warning(format_args!(
                "device change notifications unavailable ({e:#}); \
                 only disconnects will be detected"
            ));
        }
        let dropped = Arc::new(AtomicU64::new(0));
        let session = Session::open(&dropped, &events)?;
        Ok(Self {
            mic_name: session.mic.name.clone(),
            out_name: session.output.name.clone(),
            session: Some(session),
            tx,
            seen_events: events.load(Ordering::SeqCst),
            events,
            dropped,
            _defaults_watch: defaults_watch,
            debounce_until: None,
            next_retry: Instant::now(),
            retry_warned: false,
            reconnecting: false,
        })
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub fn start(&mut self) -> Result<()> {
        match self.session.as_mut() {
            Some(s) => s.start(&self.tx),
            None => Ok(()),
        }
    }

    /// The current session has delivered audio.
    pub fn has_audio(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.callbacks() > 0)
    }

    /// Frames lost because the writer fell behind (all sessions).
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn debug_line(&self) -> String {
        match &self.session {
            Some(s) => format!("[debug] rate={} {}", s.rate(), s.capture().diag.summary()),
            None => "[debug] no active session".to_string(),
        }
    }

    /// Check the devices and rebuild the session if needed. Call every UI tick.
    pub fn poll(&mut self, status: &mut StatusLine) {
        let now = Instant::now();
        if self.reconnecting
            && let Some(s) = self.session.as_ref().filter(|s| s.callbacks() > 0)
        {
            status.message(&paint("32", &self.switch_message(s)));
            (self.mic_name, self.out_name) = (s.mic.name.clone(), s.output.name.clone());
            self.reconnecting = false;
            status.set_reconnecting(false);
        }

        let generation = self.events.load(Ordering::SeqCst);
        if generation != self.seen_events {
            self.seen_events = generation;
            self.debounce_until = Some(now + DEVICE_DEBOUNCE);
        }

        match self.session.as_mut() {
            Some(s) => {
                let reason = if s.stalled() {
                    Some("audio input stopped")
                } else if self.debounce_until.is_some_and(|d| now >= d) {
                    self.debounce_until = None;
                    s.outdated()
                } else {
                    None
                };
                if let Some(reason) = reason {
                    // A replacement that never produced audio is retried quietly.
                    if !self.reconnecting {
                        status.message(&paint("33", &format!("⟳ {reason}, reconnecting…")));
                        self.retry_warned = false;
                    }
                    self.session = None; // tears down IO, closes the feed
                    self.reconnecting = true;
                    status.set_reconnecting(true);
                    self.next_retry = now;
                }
            }
            None if now >= self.next_retry => {
                // Opening uses the current defaults, so pending notifications are covered.
                self.debounce_until = None;
                let opened = Session::open(&self.dropped, &self.events)
                    .and_then(|mut s| s.start(&self.tx).map(|_| s));
                match opened {
                    Ok(s) => self.session = Some(s),
                    Err(e) => {
                        if !self.retry_warned {
                            self.retry_warned = true;
                            status.warn(&format!(
                                "Waiting for audio devices ({e:#}); retrying every second…"
                            ));
                        }
                        self.next_retry = now + RETRY_INTERVAL;
                    }
                }
            }
            None => {}
        }
    }

    fn switch_message(&self, s: &Session) -> String {
        let mut parts = Vec::new();
        if s.mic.name != self.mic_name {
            parts.push(format!("microphone: {} → {}", self.mic_name, s.mic.name));
        }
        if s.output.name != self.out_name {
            parts.push(format!("output: {} → {}", self.out_name, s.output.name));
        }
        if parts.is_empty() {
            format!(
                "✔ Capture restarted (microphone: {}, output: {})",
                s.mic.name, s.output.name
            )
        } else {
            format!("✔ Switched {}", parts.join(", "))
        }
    }
}
