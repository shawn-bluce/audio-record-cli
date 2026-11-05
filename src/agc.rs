//! Per-channel automatic gain control + peak limiter.
//!
//! Brings each channel's speech towards a common level (quiet microphones are
//! raised, loud meeting audio is tamed) and prevents clipping. The gain only
//! adapts while a channel is clearly above its own noise floor, so steady room
//! tone or fan noise is never pumped up to speech level, and the gain holds
//! during pauses.

use crate::audio::{CHANNELS, power_db};

/// Target speech level (average dBFS of 10 ms blocks containing speech).
pub const TARGET_DB: f32 = -24.0;
/// Enough for a quiet talker; more would mostly pump up background chatter.
const MAX_GAIN_DB: f32 = 12.0;
const MIN_GAIN_DB: f32 = -12.0;
/// Below this a block is never treated as speech.
const GATE_DB: f32 = -50.0;
/// A block must be this far above the tracked noise floor to count as speech.
const FLOOR_MARGIN_DB: f32 = 10.0;
/// Noise floor tracker: follows dips instantly, rises slowly.
const FLOOR_RISE_DB_PER_S: f32 = 3.0;
const FLOOR_MIN_DB: f32 = -80.0;
/// Speech level estimate time constant.
const LEVEL_TAU_S: f32 = 1.5;
/// Gain slew limits.
const GAIN_DOWN_DB_PER_S: f32 = 6.0;
const GAIN_UP_DB_PER_S: f32 = 3.0;
/// Per-sample gain smoothing (avoids zipper noise between blocks).
const GAIN_SMOOTH_S: f32 = 0.02;
/// Limiter ceiling (-1 dBFS) and release.
const CEILING: f32 = 0.891;
const LIMITER_RELEASE_S: f32 = 0.15;
const BLOCK_S: f32 = 0.01;

struct Channel {
    acc: f32,
    n: usize,
    floor_db: Option<f32>,
    level_db: Option<f32>,
    gain_db: f32,
    /// Linear target of `gain_db`; `gain` glides towards it sample by sample.
    target: f32,
    gain: f32,
    limiter: f32,
}

impl Channel {
    fn new() -> Self {
        Self {
            acc: 0.0,
            n: 0,
            floor_db: None,
            level_db: None,
            gain_db: 0.0,
            target: 1.0,
            gain: 1.0,
            limiter: 1.0,
        }
    }

    /// Update the target gain from one finished 10 ms block.
    fn block(&mut self, mean_square: f32) {
        let db = power_db(mean_square).max(FLOOR_MIN_DB);
        let floor = match self.floor_db {
            Some(f) if db >= f => f + FLOOR_RISE_DB_PER_S * BLOCK_S,
            _ => db,
        };
        self.floor_db = Some(floor);
        if db <= GATE_DB.max(floor + FLOOR_MARGIN_DB) {
            return; // silence or noise: hold the gain
        }
        let a = BLOCK_S / LEVEL_TAU_S;
        let level = self.level_db.map_or(db, |l| l + (db - l) * a);
        self.level_db = Some(level);
        let desired = (TARGET_DB - level).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        self.gain_db = if desired < self.gain_db {
            (self.gain_db - GAIN_DOWN_DB_PER_S * BLOCK_S).max(desired)
        } else {
            (self.gain_db + GAIN_UP_DB_PER_S * BLOCK_S).min(desired)
        };
        self.target = 10f32.powf(self.gain_db / 20.0);
    }
}

pub struct Agc {
    ch: [Channel; CHANNELS],
    block_len: usize,
    smooth: f32,
    release: f32,
}

impl Agc {
    pub fn new(rate: f64) -> Self {
        let rate = rate as f32;
        Self {
            ch: std::array::from_fn(|_| Channel::new()),
            block_len: (BLOCK_S * rate) as usize,
            smooth: 1.0 - (-1.0 / (GAIN_SMOOTH_S * rate)).exp(),
            release: 1.0 - (-1.0 / (LIMITER_RELEASE_S * rate)).exp(),
        }
    }

    /// Process interleaved (mic, sys) frames in place.
    pub fn process(&mut self, frames: &mut [f32]) {
        for frame in frames.chunks_exact_mut(CHANNELS) {
            for (x, c) in frame.iter_mut().zip(self.ch.iter_mut()) {
                c.acc += *x * *x;
                c.n += 1;
                if c.n == self.block_len {
                    let ms = c.acc / c.n as f32;
                    (c.acc, c.n) = (0.0, 0);
                    c.block(ms);
                }
                c.gain += (c.target - c.gain) * self.smooth;
                let y = *x * c.gain;
                if (y * c.limiter).abs() > CEILING {
                    c.limiter = CEILING / y.abs();
                }
                *x = y * c.limiter;
                c.limiter += (1.0 - c.limiter) * self.release;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = crate::audio::SAMPLE_RATE;

    fn tone(secs: f32, amp: f32, ch: usize) -> Vec<f32> {
        let n = (secs * RATE as f32) as usize;
        (0..n)
            .flat_map(|i| {
                // Speech-like: 200 ms "syllables" of a 200 Hz tone, 100 ms pauses.
                let t = i as f32 / RATE as f32;
                let on = (t % 0.3) < 0.2;
                let s = if on {
                    amp * (t * 200.0 * std::f32::consts::TAU).sin()
                } else {
                    0.0
                };
                let mut f = [0.0; 2];
                f[ch] = s;
                f
            })
            .collect()
    }

    fn rms_db(frames: &[f32], ch: usize) -> f32 {
        let (sum, n) = frames.chunks_exact(2).fold((0.0f64, 0usize), |(s, n), f| {
            (s + (f[ch] as f64).powi(2), n + 1)
        });
        10.0 * (sum / n as f64).log10() as f32
    }

    #[test]
    fn quiet_speech_is_raised_and_loud_is_lowered() {
        let mut agc = Agc::new(RATE);
        let mut quiet = tone(20.0, 0.02, 0); // ≈ -37 dBFS while "speaking"
        agc.process(&mut quiet);
        let tail = &quiet[quiet.len() - 2 * RATE as usize..];
        let db = rms_db(tail, 0);
        assert!(db > -30.0 && db < -20.0, "quiet mic ended at {db} dB");

        let mut agc = Agc::new(RATE);
        let mut loud = tone(10.0, 0.9, 1);
        agc.process(&mut loud);
        assert!(
            loud.iter().all(|s| s.abs() <= CEILING + 1e-4),
            "limiter let a peak through"
        );
        let tail = &loud[loud.len() - 2 * RATE as usize..];
        let db = rms_db(tail, 1);
        assert!(db < -10.0, "loud system audio still at {db} dB");
    }

    #[test]
    fn steady_noise_is_not_amplified() {
        let mut agc = Agc::new(RATE);
        // Constant hiss at about -40 dBFS for 30 s.
        let mut state = 1u32;
        let mut noise: Vec<f32> = (0..30 * RATE as usize)
            .flat_map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let s = ((state >> 8) as f32 / (1 << 24) as f32 - 0.5) * 0.035;
                [s, 0.0]
            })
            .collect();
        let before = rms_db(&noise, 0);
        agc.process(&mut noise);
        let after = rms_db(&noise[noise.len() - 2 * RATE as usize..], 0);
        assert!(
            (after - before).abs() < 1.5,
            "noise went {before} → {after} dB"
        );
    }
}
