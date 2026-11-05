//! The stream format shared by every stage after capture: interleaved
//! (microphone, system) Float32 frames at one fixed rate, which is also the
//! rate of the AAC output.

pub const SAMPLE_RATE: f64 = 24_000.0;
/// Left = microphone, right = system audio.
pub const CHANNELS: usize = 2;

/// Level in dBFS of a mean square value (floored at -100 dB).
pub fn power_db(mean_square: f32) -> f32 {
    10.0 * mean_square.max(1e-10).log10()
}
