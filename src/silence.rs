//! Finding long silences and choosing which AAC packets to drop for the
//! compact (`_c`) copy. Pure logic, no I/O.

use std::ops::Range;

use crate::audio::{CHANNELS, power_db};

/// Silences at least this long are shortened.
pub const MIN_SILENCE_SECS: f64 = 5.0;
/// Silence kept on each side of a cut, so speech isn't clipped and sentence
/// boundaries stay audible (a long pause becomes 2 × this).
pub const KEEP_SECS: f64 = 0.5;
/// Analysis window.
pub const WINDOW_SECS: f64 = 0.05;
/// The noise floor of a channel is this quantile of its window levels.
const FLOOR_QUANTILE: f64 = 0.1;
/// A window is silent when it is less than this far above the noise floor…
const FLOOR_MARGIN_DB: f32 = 12.0;
/// …clamped to these absolute bounds (a very noisy channel never counts as silent).
const THRESHOLD_MIN_DB: f32 = -60.0;
const THRESHOLD_MAX_DB: f32 = -35.0;

/// Per-channel silence threshold in dBFS, adapted to each channel's noise floor.
fn thresholds(levels: &[[f32; CHANNELS]]) -> [f32; CHANNELS] {
    let mut out = [THRESHOLD_MIN_DB; CHANNELS];
    for (ch, t) in out.iter_mut().enumerate() {
        let mut db: Vec<f32> = levels.iter().map(|l| power_db(l[ch])).collect();
        if db.is_empty() {
            continue;
        }
        db.sort_by(f32::total_cmp);
        let floor = db[((db.len() - 1) as f64 * FLOOR_QUANTILE) as usize];
        *t = (floor + FLOOR_MARGIN_DB).clamp(THRESHOLD_MIN_DB, THRESHOLD_MAX_DB);
    }
    out
}

/// Frame ranges to remove: the inside of every stretch where both channels stay
/// silent for at least `MIN_SILENCE_SECS`, minus `KEEP_SECS` on each side.
/// `levels` holds the mean square per channel per `window` frames.
pub fn find_cuts(
    levels: &[[f32; CHANNELS]],
    window: u64,
    rate: f64,
    total_frames: u64,
) -> Vec<Range<u64>> {
    let thr = thresholds(levels);
    let min_frames = (MIN_SILENCE_SECS * rate) as u64;
    let keep = (KEEP_SECS * rate) as u64;
    let mut cuts = Vec::new();
    let mut run_start: Option<usize> = None;
    for i in 0..=levels.len() {
        let silent = levels
            .get(i)
            .is_some_and(|l| l.iter().zip(thr).all(|(&ms, t)| power_db(ms) < t));
        match (silent, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(s)) => {
                run_start = None;
                let start = s as u64 * window;
                let end = (i as u64 * window).min(total_frames);
                if end - start >= min_frames {
                    cuts.push(start + keep..end - keep);
                }
            }
            _ => {}
        }
    }
    cuts
}

/// Indices of the packets to keep, and how many frames the dropped ones held.
/// A packet is dropped only if it lies entirely inside a cut. Cut positions are
/// in decoded frames; packet `p` holds stream frames `p*fpp .. (p+1)*fpp`,
/// where stream frame = decoded frame + `priming`.
pub fn keep_packets(
    packets: usize,
    frames_per_packet: u64,
    priming: u64,
    cuts: &[Range<u64>],
) -> (Vec<usize>, u64) {
    let mut kept = Vec::with_capacity(packets);
    let mut removed = 0;
    let mut cut = cuts
        .iter()
        .map(|c| c.start + priming..c.end + priming)
        .peekable();
    for p in 0..packets {
        let (a, b) = (
            p as u64 * frames_per_packet,
            (p as u64 + 1) * frames_per_packet,
        );
        while cut.peek().is_some_and(|c| c.end < b) {
            cut.next();
        }
        if cut.peek().is_some_and(|c| c.start <= a && b <= c.end) {
            removed += frames_per_packet;
        } else {
            kept.push(p);
        }
    }
    (kept, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 1000.0; // 1 window = 50 frames
    const W: u64 = 50;

    /// Build window levels from (seconds, dBFS mic, dBFS sys) segments.
    fn levels(segments: &[(f64, f32, f32)]) -> Vec<[f32; CHANNELS]> {
        segments
            .iter()
            .flat_map(|&(secs, m, s)| {
                let n = (secs / WINDOW_SECS).round() as usize;
                std::iter::repeat_n([10f32.powf(m / 10.0), 10f32.powf(s / 10.0)], n)
            })
            .collect()
    }

    #[test]
    fn cuts_only_long_silences_and_keeps_margins() {
        // speech 10s | silence 3s | speech 5s | silence 8s | speech 4s
        let l = levels(&[
            (10.0, -20.0, -90.0),
            (3.0, -70.0, -90.0),
            (5.0, -70.0, -25.0),
            (8.0, -70.0, -100.0),
            (4.0, -22.0, -100.0),
        ]);
        let total = l.len() as u64 * W;
        let cuts = find_cuts(&l, W, RATE, total);
        assert_eq!(cuts, vec![18_500..25_500]);
    }

    #[test]
    fn noisy_room_tone_counts_as_silence() {
        // Mic hiss at -45 dBFS between speech at -20: the floor adapts, so it's silence.
        let l = levels(&[
            (4.0, -20.0, -100.0),
            (20.0, -45.0, -100.0),
            (4.0, -20.0, -100.0),
        ]);
        let total = l.len() as u64 * W;
        assert_eq!(find_cuts(&l, W, RATE, total), vec![4_500..23_500]);
    }

    #[test]
    fn trailing_silence_is_shortened() {
        let l = levels(&[(3.0, -20.0, -20.0), (10.0, -80.0, -80.0)]);
        let total = l.len() as u64 * W - 20; // last window partial
        assert_eq!(find_cuts(&l, W, RATE, total), vec![3_500..12_480]);
    }

    #[test]
    fn packets_fully_inside_cuts_are_dropped() {
        // fpp 10, priming 5: packet p = stream [10p, 10p+10) = decoded [10p-5, 10p+5)
        let (kept, removed) = keep_packets(10, 10, 5, &[20..60, 1000..1100]);
        // stream cut 25..65 → packets 3,4,5 (30..60) fully inside.
        assert_eq!(kept, vec![0, 1, 2, 6, 7, 8, 9]);
        assert_eq!(removed, 30);
        let (kept, removed) = keep_packets(10, 10, 0, &[0..20, 40..50, 70..100]);
        assert_eq!(kept, vec![2, 3, 5, 6]);
        assert_eq!(removed, 60);
    }
}
