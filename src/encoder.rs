//! Streaming AAC-LC encoder: interleaved (mic, system) Float32 in, raw AAC
//! packets out. Packets go to the crash-safe ADTS journal while recording and
//! are remuxed losslessly into M4A at the end.

use anyhow::{Context, Result};

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::convert::{AudioStreamPacketDescription, Converter};
use crate::coreaudio::{AudioStreamBasicDescription, fcc};

pub const AAC_BITRATE: u32 = 64_000;
const FRAMES_PER_PACKET: u32 = 1024;

const FORMAT_MPEG4_AAC: u32 = fcc(b"aac ");
const CONV_ENCODE_BITRATE: u32 = fcc(b"brat");
const CONV_CODEC_QUALITY: u32 = fcc(b"cdqu");
const CONV_MAX_OUTPUT_PACKET_SIZE: u32 = fcc(b"xops");
const CONV_MAGIC_COOKIE: u32 = fcc(b"cmgc");
const CONV_PRIME_INFO: u32 = fcc(b"prim");
const CONV_OUTPUT_FORMAT: u32 = fcc(b"acod");
const CODEC_BITRATE_CONTROL_MODE: u32 = fcc(b"acbf");
const BITRATE_MODE_CONSTANT: u32 = 0;
const QUALITY_MAX: u32 = 0x7F;
/// Packets requested per conversion call.
const OUT_PACKETS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct PrimeInfo {
    leading_frames: u32,
    trailing_frames: u32,
}

/// What a container needs to describe the encoded stream.
#[derive(Clone)]
pub struct StreamInfo {
    pub format: AudioStreamBasicDescription,
    pub cookie: Vec<u8>,
    /// Encoder delay at the start of the stream (frames to trim on decode).
    pub priming: u32,
}

pub struct AacEncoder {
    conv: Converter,
    buf: Vec<u8>,
    descs: Vec<AudioStreamPacketDescription>,
    info: StreamInfo,
}

impl AacEncoder {
    pub fn new() -> Result<Self> {
        let src = AudioStreamBasicDescription::float32(SAMPLE_RATE, CHANNELS as u32);
        let dst = AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE,
            format_id: FORMAT_MPEG4_AAC,
            channels_per_frame: CHANNELS as u32,
            frames_per_packet: FRAMES_PER_PACKET,
            ..Default::default()
        };
        let conv = Converter::new(&src, &dst, "Creating the AAC encoder")?;
        // Best effort: not every encoder exposes these.
        let _ = conv.set(
            CODEC_BITRATE_CONTROL_MODE,
            BITRATE_MODE_CONSTANT,
            "Setting AAC bit rate mode",
        );
        let _ = conv.set(CONV_CODEC_QUALITY, QUALITY_MAX, "Setting AAC quality");
        conv.set(CONV_ENCODE_BITRATE, AAC_BITRATE, "Setting AAC bit rate")?;

        let max_packet: u32 = conv.get(CONV_MAX_OUTPUT_PACKET_SIZE, "Querying AAC packet size")?;
        let info = StreamInfo {
            format: conv.get(CONV_OUTPUT_FORMAT, "Querying AAC output format")?,
            cookie: conv.get_bytes(CONV_MAGIC_COOKIE, "Reading AAC magic cookie")?,
            priming: conv
                .get::<PrimeInfo>(CONV_PRIME_INFO, "Querying AAC encoder delay")
                .map_or(0, |p| p.leading_frames),
        };
        Ok(Self {
            conv,
            buf: vec![0; max_packet.max(1) as usize * OUT_PACKETS],
            descs: vec![AudioStreamPacketDescription::default(); OUT_PACKETS],
            info,
        })
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Encode interleaved frames; every finished AAC packet is passed to `sink`.
    pub fn encode(&mut self, input: &[f32], sink: impl FnMut(&[u8])) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        self.conv
            .packets(Some(input), &mut self.buf, &mut self.descs, sink)
            .context("Encoding AAC")
    }

    /// Flush the encoder's tail (end of stream). The encoder can't be used afterwards.
    pub fn finish(&mut self, sink: impl FnMut(&[u8])) -> Result<()> {
        self.conv
            .packets(None, &mut self.buf, &mut self.descs, sink)
            .context("Encoding AAC")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_whole_packets() {
        let mut enc = AacEncoder::new().unwrap();
        assert!(!enc.info().cookie.is_empty());
        let frames = SAMPLE_RATE as usize; // 1 second
        let input: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin();
                [s * 0.3, s * 0.1]
            })
            .collect();
        let (mut packets, mut bytes) = (0usize, 0usize);
        let mut sink = |p: &[u8]| {
            packets += 1;
            bytes += p.len();
        };
        for chunk in input.chunks(333 * CHANNELS) {
            enc.encode(chunk, &mut sink).unwrap();
        }
        enc.finish(&mut sink).unwrap();
        let priming = enc.info().priming as usize;
        let expected = (frames + priming).div_ceil(FRAMES_PER_PACKET as usize);
        assert!(packets >= expected, "{packets} packets < {expected}");
        assert!(
            packets <= expected + 2,
            "{packets} packets > {expected} + 2"
        );
        // ~64 kbps → ~8 KB per second.
        assert!((5_000..12_000).contains(&bytes), "{bytes} bytes");
    }
}
