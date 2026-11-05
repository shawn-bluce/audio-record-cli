//! Writer thread: drains capture sessions in order and encodes them continuously.
//!
//! Each capture session has its own ring buffer of interleaved (mic, system)
//! frames at the session's sample rate. The writer:
//!   * resamples to the pipeline rate when a session runs at another rate,
//!   * inserts silence for the wall-clock gap between two sessions (device switch),
//!     so the file duration matches real time and both channels stay aligned,
//!   * feeds the level meter, applies per-channel gain control, and encodes
//!     (left = microphone, right = system) into the crash-safe journal.

use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use ringbuf::HeapCons;
use ringbuf::traits::{Consumer, Observer};

use crate::agc::Agc;
use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::coreaudio::Diag;
use crate::encoder::AacEncoder;
use crate::journal::Journal;
use crate::meter::Levels;
use crate::resample::Resampler;

const CHUNK_FRAMES: usize = 8192;
/// Never insert more silence than this for a single gap (guards against clock glitches).
const MAX_GAP_SECS: f64 = 6.0 * 3600.0;
/// Ignore timeline differences below this (callback timestamp jitter).
const RESYNC_THRESHOLD_SECS: f64 = 0.01;
const POLL: Duration = Duration::from_millis(10);
static ZEROS: [f32; CHUNK_FRAMES * CHANNELS] = [0.0; CHUNK_FRAMES * CHANNELS];

/// Round a sample count down to whole frames.
fn whole_frames(samples: usize) -> usize {
    samples - samples % CHANNELS
}

/// One capture session's output, handed from the recorder to the writer.
pub struct Feed {
    pub cons: HeapCons<f32>,
    pub rate: f64,
    pub diag: Arc<Diag>,
    /// Set by the recorder once the session's IO has been torn down (no more data will come).
    pub closed: Arc<AtomicBool>,
}

struct Active {
    feed: Feed,
    started: bool,
    resampler: Option<Resampler>,
    /// `sink.frames` when this session's audio began (after any leading gap).
    first_frame: u64,
}

/// Where the pipeline-rate stream ends up: meter → gain control → encoder → journal.
struct Sink<'a> {
    encoder: AacEncoder,
    journal: Journal,
    agc: Agc,
    levels: &'a Levels,
    /// Frames written so far.
    frames: u64,
}

impl Sink<'_> {
    /// Captured audio. The meter shows what the devices deliver, before gain control.
    fn audio(&mut self, data: &mut [f32]) -> Result<()> {
        self.levels.record(data);
        self.agc.process(data);
        self.write(data)
    }

    /// Digital silence (gaps between sessions). Bypasses the gain control on purpose.
    fn silence(&mut self, mut frames: u64) -> Result<()> {
        while frames > 0 {
            let n = frames.min(CHUNK_FRAMES as u64);
            self.write(&ZEROS[..n as usize * CHANNELS])?;
            frames -= n;
        }
        Ok(())
    }

    fn write(&mut self, data: &[f32]) -> Result<()> {
        let journal = &mut self.journal;
        self.encoder.encode(data, |p| journal.append(p))?;
        self.frames += (data.len() / CHANNELS) as u64;
        self.journal.flush()
    }

    /// Flush the encoder tail and sync the journal. Returns the frame count and
    /// the still-locked journal file.
    fn finish(mut self) -> Result<(u64, File)> {
        let journal = &mut self.journal;
        self.encoder.finish(|p| journal.append(p))?;
        Ok((self.frames, self.journal.close()?))
    }
}

/// Runs until `done` is set and every session has been drained.
/// Returns the number of frames written and the locked journal.
pub fn run(
    rx: Receiver<Feed>,
    encoder: AacEncoder,
    journal: Journal,
    levels: &Levels,
    done: &AtomicBool,
) -> Result<(u64, File)> {
    let mut sink = Sink {
        encoder,
        journal,
        agc: Agc::new(SAMPLE_RATE),
        levels,
        frames: 0,
    };
    let result = drain(&rx, &mut sink, done);
    // Always flush the encoder tail — even after an error, keep what we have.
    let finished = sink.finish();
    result?;
    finished
}

fn drain(rx: &Receiver<Feed>, sink: &mut Sink, done: &AtomicBool) -> Result<()> {
    let mut buf = vec![0.0f32; CHUNK_FRAMES * CHANNELS];
    let mut resampled = Vec::new();
    let mut current: Option<Active> = None;
    // Wall-clock end of the previous session, to size the silence gap before the next.
    let mut prev_end_ns: Option<u64> = None;
    let debug = std::env::var_os("AREC_DEBUG").is_some();

    loop {
        let Some(a) = current.as_mut() else {
            // Read `done` before polling so a feed sent just before `done` is never missed.
            let finished = done.load(Ordering::SeqCst);
            match rx.try_recv() {
                Ok(feed) => {
                    current = Some(Active {
                        feed,
                        started: false,
                        resampler: None,
                        first_frame: 0,
                    })
                }
                Err(TryRecvError::Empty) if !finished => thread::sleep(POLL),
                Err(_) => return Ok(()),
            }
            continue;
        };

        // Read `closed` before checking for data so nothing pushed before close is lost.
        let closed = a.feed.closed.load(Ordering::SeqCst);
        let avail = whole_frames(a.feed.cons.occupied_len());
        if avail == 0 {
            if closed {
                if a.started {
                    if let Some(r) = a.resampler.as_mut() {
                        r.flush(&mut resampled)?;
                        sink.audio(&mut resampled)?;
                    }
                    // If the HAL paused IO inside this session (e.g. a sample-rate change),
                    // the session produced less audio than the wall clock says. Pad the
                    // shortfall so everything after it stays on the real timeline.
                    let d = &a.feed.diag;
                    if let (Some(first), Some(end)) = (d.first_ns(), d.end_ns(a.feed.rate)) {
                        let wall = end.saturating_sub(first) as f64 / 1e9;
                        let audio = (sink.frames - a.first_frame) as f64 / SAMPLE_RATE;
                        let short = (wall - audio).min(MAX_GAP_SECS);
                        if short > RESYNC_THRESHOLD_SECS {
                            sink.silence((short * SAMPLE_RATE).round() as u64)?;
                        }
                        if debug {
                            eprintln!(
                                "\r\x1b[2K[debug] session done: rate={} audio={audio:.3}s \
                                 wall={wall:.3}s padded={:.3}s",
                                a.feed.rate,
                                short.max(0.0)
                            );
                        }
                        prev_end_ns = Some(end);
                    }
                }
                current = None;
            } else {
                thread::sleep(POLL);
            }
            continue;
        }

        if !a.started {
            a.started = true;
            if let (Some(prev), Some(first)) = (prev_end_ns, a.feed.diag.first_ns()) {
                let gap_secs = (first.saturating_sub(prev) as f64 / 1e9).min(MAX_GAP_SECS);
                sink.silence((gap_secs * SAMPLE_RATE).round() as u64)?;
                if debug {
                    eprintln!("\r\x1b[2K[debug] gap filled: {gap_secs:.3}s");
                }
            }
            a.first_frame = sink.frames;
            if (a.feed.rate - SAMPLE_RATE).abs() > 0.5 {
                a.resampler = Some(Resampler::new(a.feed.rate, SAMPLE_RATE)?);
            }
        }

        let want = avail.min(buf.len());
        let n = whole_frames(a.feed.cons.pop_slice(&mut buf[..want]));
        match a.resampler.as_mut() {
            Some(r) => {
                r.process(&buf[..n], &mut resampled)?;
                sink.audio(&mut resampled)?;
            }
            None => sink.audio(&mut buf[..n])?,
        }
    }
}
