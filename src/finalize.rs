//! Turning a recording journal into the final files:
//!   1. `remux`: ADTS journal → `<start>_<end>.m4a` (lossless, no re-encoding),
//!   2. `compact`: if it contains long silences, `<start>_<end>_c.m4a` without them.
//!
//! Also used at startup to rescue journals left behind by a crash.

use anyhow::{Context, Result, bail};
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::audio::SAMPLE_RATE;
use crate::encoder::{AacEncoder, StreamInfo};
use crate::{journal, m4a, output, silence};

/// A finished recording, with its packets still in memory for `compact`.
pub struct Remuxed {
    pub path: PathBuf,
    /// Audio length in frames.
    pub frames: u64,
    info: StreamInfo,
    data: Vec<u8>,
    packets: Vec<Range<usize>>,
}

pub struct Compacted {
    pub path: PathBuf,
    pub frames: u64,
    pub cuts: usize,
}

/// Remux `journal` into `dst` and delete the journal. `frames` is the exact
/// audio length if known (otherwise derived from the packet count).
pub fn remux(journal_path: &Path, dst: &Path, frames: Option<u64>) -> Result<Remuxed> {
    // A fresh encoder with identical settings describes the stream (cookie, priming).
    let info = AacEncoder::new()?.info().clone();
    let data = std::fs::read(journal_path)
        .with_context(|| format!("Reading {}", journal_path.display()))?;
    let packets = journal::parse(&data);
    if packets.is_empty() {
        bail!("{} contains no audio", journal_path.display());
    }
    let available = (packets.len() as u64 * info.format.frames_per_packet as u64)
        .saturating_sub(info.priming as u64);
    let frames = frames.map_or(available, |f| f.min(available));
    m4a::write(dst, &info, &data, &packets, frames)?;
    std::fs::remove_file(journal_path)
        .with_context(|| format!("Removing {}", journal_path.display()))?;
    Ok(Remuxed {
        path: dst.to_owned(),
        frames,
        info,
        data,
        packets,
    })
}

/// Write a copy of `rec` without its long silences, if it has any.
pub fn compact(rec: &Remuxed) -> Result<Option<Compacted>> {
    let window = (silence::WINDOW_SECS * SAMPLE_RATE) as usize;
    let levels = m4a::channel_levels(&rec.path, window)?;
    let cuts = silence::find_cuts(&levels, window as u64, SAMPLE_RATE, rec.frames);
    if cuts.is_empty() {
        return Ok(None);
    }
    let fpp = rec.info.format.frames_per_packet as u64;
    let (kept, removed) =
        silence::keep_packets(rec.packets.len(), fpp, rec.info.priming as u64, &cuts);
    if removed == 0 {
        return Ok(None);
    }
    let packets: Vec<Range<usize>> = kept.iter().map(|&i| rec.packets[i].clone()).collect();
    let frames = rec.frames - removed;
    let path = output::compact_path(&rec.path);
    m4a::write(&path, &rec.info, &rec.data, &packets, frames)?;
    Ok(Some(Compacted {
        path,
        frames,
        cuts: cuts.len(),
    }))
}

/// A journal left behind by a recording that never finished.
pub struct Orphan {
    pub journal: PathBuf,
    pub start: String,
    pub end: String,
    /// Holds the journal's lock while we convert it.
    _lock: File,
}

/// Journals in `dir` whose recorder is gone (live recordings hold a lock).
pub fn orphans(dir: &Path) -> Vec<Orphan> {
    output::find_journals(dir)
        .into_iter()
        .filter_map(|journal| {
            let start = output::journal_start(&journal)?;
            let file = File::open(&journal).ok()?;
            file.try_lock().ok()?;
            // The last write is the best estimate of when the recording ended.
            let modified = file.metadata().and_then(|m| m.modified()).ok()?;
            Some(Orphan {
                journal,
                start,
                end: output::timestamp(modified),
                _lock: file,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CHANNELS;
    use crate::journal::Journal;

    /// sound(3 s) | silence(8 s) | sound(3 s) on both channels, through the real
    /// encoder → journal → remux → compact path.
    #[test]
    fn end_to_end_remux_and_compact() {
        let dir = std::env::temp_dir().join(format!("arec-finalize-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let journal_path = dir.join("j.aac");

        let rate = SAMPLE_RATE as usize;
        let mut pcm = Vec::new();
        for (secs, amp) in [(3, 0.2f32), (8, 0.0), (3, 0.2)] {
            for i in 0..secs * rate {
                let s = amp * (i as f32 * 300.0 * std::f32::consts::TAU / rate as f32).sin();
                pcm.extend_from_slice(&[s, s * 0.5]);
            }
        }
        let frames = (pcm.len() / CHANNELS) as u64;

        let mut enc = AacEncoder::new().unwrap();
        let mut journal = Journal::create(&journal_path).unwrap();
        let mut sink = |p: &[u8]| journal.append(p);
        for chunk in pcm.chunks(4096) {
            enc.encode(chunk, &mut sink).unwrap();
        }
        enc.finish(&mut sink).unwrap();
        drop(journal.close().unwrap());

        let rec = remux(&journal_path, &dir.join("full.m4a"), Some(frames)).unwrap();
        assert!(
            !journal_path.exists(),
            "journal should be removed after remux"
        );
        assert_eq!(rec.frames, frames);
        let levels = m4a::channel_levels(&rec.path, rate).unwrap();
        assert_eq!(levels.len(), 14, "decoded length should be exactly 14 s");

        let c = compact(&rec).unwrap().expect("a silence to cut");
        assert_eq!(c.cuts, 1);
        // 8 s silence → 1 s kept (± one AAC packet on each side).
        assert_eq!(c.path, dir.join("full_c.m4a"));
        let secs = c.frames as f64 / SAMPLE_RATE;
        assert!((secs - 7.0).abs() < 0.1, "compact copy is {secs} s");
        let decoded: usize = m4a::channel_levels(&c.path, 1).unwrap().len();
        assert_eq!(decoded as u64, c.frames);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
