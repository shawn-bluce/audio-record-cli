//! Crash-safe recording journal: an append-only ADTS (`.aac`) stream.
//!
//! An M4A file only becomes playable once its index (`moov`) is written at the
//! very end, so a crash or `kill -9` would lose the whole recording. Instead we
//! append every AAC packet to an ADTS stream as soon as it is encoded: each
//! packet carries its own 7-byte header, so the file is valid (and playable)
//! after every write. On stop the packets are remuxed losslessly into M4A.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::audio::{CHANNELS, SAMPLE_RATE};

/// Force data to stable storage this often (guards against power loss / kernel panic;
/// a crash of the process itself loses nothing that was already written).
const SYNC_INTERVAL: Duration = Duration::from_secs(5);
const HEADER_LEN: usize = 7;
const AAC_LC_PROFILE: u8 = 1; // MPEG-4 audio object type 2 (AAC LC) minus 1
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn sample_rate_index(rate: u32) -> Result<u8> {
    match SAMPLE_RATES.iter().position(|&r| r == rate) {
        Some(i) => Ok(i as u8),
        None => bail!("ADTS does not support a {rate} Hz sample rate"),
    }
}

fn header(payload_len: usize, sr_index: u8) -> [u8; HEADER_LEN] {
    let channels = CHANNELS as u8;
    let len = payload_len + HEADER_LEN;
    [
        0xFF,
        0xF1, // sync, MPEG-4, layer 0, no CRC
        (AAC_LC_PROFILE << 6) | (sr_index << 2) | (channels >> 2),
        ((channels & 3) << 6) | ((len >> 11) & 0x3) as u8,
        ((len >> 3) & 0xFF) as u8,
        (((len & 7) << 5) as u8) | 0x1F,
        0xFC, // buffer fullness 0x7FF (VBR), one raw data block
    ]
}

pub struct Journal {
    file: File,
    path: PathBuf,
    buf: Vec<u8>,
    sr_index: u8,
    last_sync: Instant,
}

impl Journal {
    /// Create the journal and take an exclusive lock on it, so another `arec`
    /// never mistakes a live recording for an interrupted one.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("Creating {}", path.display()))?;
        file.try_lock()
            .with_context(|| format!("Locking {}", path.display()))?;
        Ok(Self {
            file,
            path: path.to_owned(),
            buf: Vec::with_capacity(64 * 1024),
            sr_index: sample_rate_index(SAMPLE_RATE as u32)?,
            last_sync: Instant::now(),
        })
    }

    /// Queue one AAC packet; it reaches the file on the next `flush`.
    pub fn append(&mut self, packet: &[u8]) {
        self.buf
            .extend_from_slice(&header(packet.len(), self.sr_index));
        self.buf.extend_from_slice(packet);
    }

    /// Write queued packets to the file (and periodically to stable storage).
    pub fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.file
                .write_all(&self.buf)
                .with_context(|| format!("Writing {}", self.path.display()))?;
            self.buf.clear();
        }
        if self.last_sync.elapsed() >= SYNC_INTERVAL {
            self.last_sync = Instant::now();
            self.file
                .sync_data()
                .with_context(|| format!("Syncing {}", self.path.display()))?;
        }
        Ok(())
    }

    /// Flush everything to disk. The returned file keeps the lock held until dropped.
    pub fn close(mut self) -> Result<File> {
        self.flush()?;
        self.file
            .sync_all()
            .with_context(|| format!("Syncing {}", self.path.display()))?;
        Ok(self.file)
    }
}

/// Payload ranges of every complete ADTS frame in `data`. Parsing stops at the
/// first damaged or truncated frame (the tail of an interrupted recording).
pub fn parse(data: &[u8]) -> Vec<Range<usize>> {
    let mut frames = Vec::with_capacity(data.len() / 200);
    let mut pos = 0;
    while pos + HEADER_LEN <= data.len() {
        let h = &data[pos..pos + HEADER_LEN];
        if h[0] != 0xFF || h[1] & 0xF0 != 0xF0 {
            break;
        }
        let header_len = if h[1] & 1 == 1 { 7 } else { 9 }; // 9 = with CRC
        let len = (((h[3] & 3) as usize) << 11) | ((h[4] as usize) << 3) | (h[5] as usize >> 5);
        if len <= header_len || pos + len > data.len() {
            break;
        }
        frames.push(pos + header_len..pos + len);
        pos += len;
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_truncation() {
        let dir = std::env::temp_dir().join(format!("arec-journal-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let mut j = Journal::create(&dir).unwrap();
        let packets: Vec<Vec<u8>> = (0..50).map(|i| vec![i as u8; 10 + i * 37]).collect();
        for p in &packets {
            j.append(p);
        }
        drop(j.close().unwrap());

        let mut data = std::fs::read(&dir).unwrap();
        let _ = std::fs::remove_file(&dir);
        let frames = parse(&data);
        assert_eq!(frames.len(), packets.len());
        for (r, p) in frames.iter().zip(&packets) {
            assert_eq!(&data[r.clone()], &p[..]);
        }
        // Header fields: AAC LC, 24 kHz (index 6), 2 channels.
        assert_eq!(data[2] >> 6, 1);
        assert_eq!((data[2] >> 2) & 0xF, 6);
        assert_eq!(((data[2] & 1) << 2) | (data[3] >> 6), 2);

        // A crash mid-write leaves a partial last frame: it is ignored.
        data.truncate(data.len() - 5);
        assert_eq!(parse(&data).len(), packets.len() - 1);
    }
}
