//! Streaming sample-rate converter for interleaved (mic, system) Float32.
//! Brings every capture session (typically 48 kHz, or e.g. 16 kHz for
//! Bluetooth headsets) to the pipeline rate.

use anyhow::{Context, Result};

use crate::audio::CHANNELS;
use crate::convert::Converter;
use crate::coreaudio::{AudioStreamBasicDescription, fcc};

const OUT_CHUNK_FRAMES: usize = 4096;
const PROP_SRC_QUALITY: u32 = fcc(b"srcq");
const QUALITY_HIGH: u32 = 0x60;

pub struct Resampler {
    conv: Converter,
    scratch: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: f64, out_rate: f64) -> Result<Self> {
        let format = |rate| AudioStreamBasicDescription::float32(rate, CHANNELS as u32);
        let conv = Converter::new(
            &format(in_rate),
            &format(out_rate),
            &format!("Creating {in_rate} Hz -> {out_rate} Hz resampler"),
        )?;
        let _ = conv.set(PROP_SRC_QUALITY, QUALITY_HIGH, "Setting resampler quality");
        Ok(Self {
            conv,
            scratch: vec![0.0; OUT_CHUNK_FRAMES * CHANNELS],
        })
    }

    /// Resample `input`; the result replaces the contents of `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        out.clear();
        self.conv
            .pcm(Some(input), &mut self.scratch, out)
            .context("Resampling audio")
    }

    /// Drain the converter's internal delay line at the end of a session.
    pub fn flush(&mut self, out: &mut Vec<f32>) -> Result<()> {
        out.clear();
        self.conv
            .pcm(None, &mut self.scratch, out)
            .context("Resampling audio")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(in_rate: f64, out_rate: f64) -> usize {
        let mut r = Resampler::new(in_rate, out_rate).unwrap();
        let frames = in_rate as usize; // 1 second
        let input: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / in_rate as f32).sin() * 0.5;
                [s, -s]
            })
            .collect();
        let (mut out, mut total) = (Vec::new(), 0);
        for chunk in input.chunks(512 * CHANNELS) {
            r.process(chunk, &mut out).unwrap();
            total += out.len() / CHANNELS;
        }
        r.flush(&mut out).unwrap();
        total + out.len() / CHANNELS
    }

    #[test]
    fn one_second_in_is_one_second_out() {
        for (i, o) in [
            (48_000.0, 24_000.0),
            (44_100.0, 24_000.0),
            (16_000.0, 24_000.0),
            (96_000.0, 24_000.0),
        ] {
            let n = run(i, o) as f64;
            assert!((n - o).abs() < o * 0.002, "{i}->{o}: got {n} frames");
        }
    }
}
