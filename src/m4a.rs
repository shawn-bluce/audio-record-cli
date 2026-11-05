//! M4A container I/O via AudioToolbox:
//!   * `write` — mux already-encoded AAC packets (no re-encoding),
//!   * `channel_levels` — decode a file and measure per-window energy.

use std::ffi::c_void;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use anyhow::{Result, anyhow};
use core_foundation_sys::base::{CFRelease, kCFAllocatorDefault};
use core_foundation_sys::url::{CFURLCreateFromFileSystemRepresentation, CFURLRef};

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::convert::AudioStreamPacketDescription;
use crate::coreaudio::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, OSStatus, check, fcc,
};
use crate::encoder::StreamInfo;

type AudioFileID = *mut c_void;
type ExtAudioFileRef = *mut c_void;

const FILE_TYPE_M4A: u32 = fcc(b"m4af");
const FILE_FLAGS_ERASE: u32 = 1;
const FILE_PROP_MAGIC_COOKIE: u32 = fcc(b"mgic");
const FILE_PROP_PACKET_TABLE: u32 = fcc(b"pnfo");
const EXT_PROP_CLIENT_FORMAT: u32 = fcc(b"cfmt");
const WRITE_BATCH: usize = 256;

#[repr(C)]
struct PacketTableInfo {
    valid_frames: i64,
    priming_frames: i32,
    remainder_frames: i32,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioFileCreateWithURL(
        url: CFURLRef,
        file_type: u32,
        format: *const AudioStreamBasicDescription,
        flags: u32,
        out: *mut AudioFileID,
    ) -> OSStatus;
    fn AudioFileSetProperty(f: AudioFileID, id: u32, size: u32, data: *const c_void) -> OSStatus;
    fn AudioFileWritePackets(
        f: AudioFileID,
        use_cache: u8,
        num_bytes: u32,
        descs: *const AudioStreamPacketDescription,
        start_packet: i64,
        io_num_packets: *mut u32,
        buffer: *const c_void,
    ) -> OSStatus;
    fn AudioFileClose(f: AudioFileID) -> OSStatus;

    fn ExtAudioFileOpenURL(url: CFURLRef, out: *mut ExtAudioFileRef) -> OSStatus;
    fn ExtAudioFileSetProperty(
        f: ExtAudioFileRef,
        id: u32,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn ExtAudioFileRead(
        f: ExtAudioFileRef,
        frames: *mut u32,
        data: *mut AudioBufferList,
    ) -> OSStatus;
    fn ExtAudioFileDispose(f: ExtAudioFileRef) -> OSStatus;
}

struct Url(CFURLRef);

impl Url {
    fn new(path: &Path) -> Result<Self> {
        let bytes = path.as_os_str().as_bytes();
        let url = unsafe {
            CFURLCreateFromFileSystemRepresentation(
                kCFAllocatorDefault,
                bytes.as_ptr(),
                bytes.len() as _,
                0,
            )
        };
        if url.is_null() {
            return Err(anyhow!("Cannot build file URL for {}", path.display()));
        }
        Ok(Self(url))
    }
}

impl Drop for Url {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as *const c_void) };
    }
}

/// Write `packets` (payload ranges into `data`) as an M4A file.
/// `valid_frames` is the real audio length after removing encoder priming/padding.
pub fn write(
    path: &Path,
    info: &StreamInfo,
    data: &[u8],
    packets: &[Range<usize>],
    valid_frames: u64,
) -> Result<()> {
    let url = Url::new(path)?;
    let mut file: AudioFileID = ptr::null_mut();
    check(
        unsafe {
            AudioFileCreateWithURL(
                url.0,
                FILE_TYPE_M4A,
                &info.format,
                FILE_FLAGS_ERASE,
                &mut file,
            )
        },
        &format!("Creating {}", path.display()),
    )?;
    let result = write_into(file, info, data, packets, valid_frames);
    let closed = check(
        unsafe { AudioFileClose(file) },
        &format!("Finalizing {}", path.display()),
    );
    let result = result.and(closed);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn write_into(
    file: AudioFileID,
    info: &StreamInfo,
    data: &[u8],
    packets: &[Range<usize>],
    valid_frames: u64,
) -> Result<()> {
    check(
        unsafe {
            AudioFileSetProperty(
                file,
                FILE_PROP_MAGIC_COOKIE,
                info.cookie.len() as u32,
                info.cookie.as_ptr() as *const c_void,
            )
        },
        "Setting AAC magic cookie",
    )?;

    let mut buf = Vec::new();
    let mut descs = Vec::with_capacity(WRITE_BATCH);
    let mut next_packet = 0i64;
    for batch in packets.chunks(WRITE_BATCH) {
        buf.clear();
        descs.clear();
        for r in batch {
            descs.push(AudioStreamPacketDescription {
                start_offset: buf.len() as i64,
                variable_frames_in_packet: 0,
                data_byte_size: r.len() as u32,
            });
            buf.extend_from_slice(&data[r.clone()]);
        }
        let mut n = batch.len() as u32;
        check(
            unsafe {
                AudioFileWritePackets(
                    file,
                    0,
                    buf.len() as u32,
                    descs.as_ptr(),
                    next_packet,
                    &mut n,
                    buf.as_ptr() as *const c_void,
                )
            },
            "Writing AAC packets",
        )?;
        if n as usize != batch.len() {
            return Err(anyhow!(
                "Writing AAC packets: only {n} of {} written",
                batch.len()
            ));
        }
        next_packet += n as i64;
    }

    // Tell players to skip the encoder delay and the padding of the last packet.
    let total = packets.len() as u64 * info.format.frames_per_packet as u64;
    let priming = (info.priming as u64).min(total);
    let valid = valid_frames.min(total - priming);
    let table = PacketTableInfo {
        valid_frames: valid as i64,
        priming_frames: priming as i32,
        remainder_frames: (total - priming - valid) as i32,
    };
    check(
        unsafe {
            AudioFileSetProperty(
                file,
                FILE_PROP_PACKET_TABLE,
                std::mem::size_of::<PacketTableInfo>() as u32,
                &table as *const PacketTableInfo as *const c_void,
            )
        },
        "Setting AAC priming info",
    )
}

/// Decode a stereo file and return the mean square of each channel per
/// `window` frames (at the file's own rate, priming already trimmed).
pub fn channel_levels(path: &Path, window: usize) -> Result<Vec<[f32; CHANNELS]>> {
    let url = Url::new(path)?;
    let mut ext: ExtAudioFileRef = ptr::null_mut();
    check(
        unsafe { ExtAudioFileOpenURL(url.0, &mut ext) },
        &format!("Opening {}", path.display()),
    )?;
    let result = read_levels(ext, window);
    unsafe { ExtAudioFileDispose(ext) };
    result.map_err(|e| e.context(format!("Decoding {}", path.display())))
}

fn read_levels(ext: ExtAudioFileRef, window: usize) -> Result<Vec<[f32; CHANNELS]>> {
    // Client format: interleaved Float32 at the file's own rate (no resampling).
    let client = AudioStreamBasicDescription::float32(SAMPLE_RATE, CHANNELS as u32);
    check(
        unsafe {
            ExtAudioFileSetProperty(
                ext,
                EXT_PROP_CLIENT_FORMAT,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                (&raw const client).cast(),
            )
        },
        "Setting decode format",
    )?;

    const CHUNK: usize = 16 * 1024;
    let mut buf = vec![0f32; CHUNK * CHANNELS];
    let mut out = Vec::new();
    let (mut acc, mut n) = ([0f64; CHANNELS], 0usize);
    let mean = |acc: [f64; CHANNELS], n: usize| acc.map(|a| (a / n as f64) as f32);
    loop {
        let mut frames = CHUNK as u32;
        let mut list = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: CHANNELS as u32,
                byte_size: (buf.len() * 4) as u32,
                data: buf.as_mut_ptr().cast(),
            }],
        };
        check(
            unsafe { ExtAudioFileRead(ext, &mut frames, &mut list) },
            "Reading audio",
        )?;
        if frames == 0 {
            break;
        }
        for frame in buf[..frames as usize * CHANNELS].chunks_exact(CHANNELS) {
            for (a, &x) in acc.iter_mut().zip(frame) {
                *a += (x as f64) * (x as f64);
            }
            n += 1;
            if n == window {
                out.push(mean(acc, n));
                (acc, n) = ([0.0; CHANNELS], 0);
            }
        }
    }
    if n > 0 {
        out.push(mean(acc, n));
    }
    Ok(out)
}
