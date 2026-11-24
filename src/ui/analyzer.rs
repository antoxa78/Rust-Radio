//! Spectrum analyzer: turns GStreamer `spectrum` magnitudes into 64 bars and
//! draws them in one of four styles.

use crate::model::AnalyzerType;
use crate::player::{SPECTRUM_BANDS, SPECTRUM_THRESHOLD_DB};
use gtk::prelude::*;
use std::sync::OnceLock;

pub const BARS: usize = 64;
/// Bar heights live in [FLOOR, CEIL] (units of the drawing code, 50 = full).
pub const FLOOR: f64 = 4.0;
const CEIL: f64 = 48.0;
const MIN_HZ: f64 = 40.0;
const MAX_HZ: f64 = 16_000.0;
/// dB range mapped onto the bar height.
const DB_LOW: f64 = -70.0;
const DB_HIGH: f64 = -10.0;

pub struct Analyzer {
    pub heights: Vec<f64>,
    pub peaks: Vec<f64>,
    hold: Vec<u8>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Analyzer {
            heights: vec![FLOOR; BARS],
            peaks: vec![FLOOR; BARS],
            hold: vec![0; BARS],
        }
    }
}

struct BandRanges {
    lo: [f64; BARS],
    hi: [f64; BARS],
}

/// Log-spaced bar edges are fixed, so compute them once instead of doing
/// 64 `powf` calls for every spectrum frame (20 per second).
fn band_ranges() -> &'static BandRanges {
    static RANGES: OnceLock<BandRanges> = OnceLock::new();
    RANGES.get_or_init(|| {
        let ratio = (MAX_HZ / MIN_HZ).powf(1.0 / BARS as f64);
        let mut ranges = BandRanges {
            lo: [0.0; BARS],
            hi: [0.0; BARS],
        };
        for bar in 0..BARS {
            let lo = MIN_HZ * ratio.powi(bar as i32);
            ranges.lo[bar] = lo;
            ranges.hi[bar] = lo * ratio;
        }
        ranges
    })
}

/// Group linear FFT bands into `BARS` log-spaced bars (max dB per bar).
pub fn bars_from_magnitudes(magnitudes: &[f32], sample_rate: f64) -> [f64; BARS] {
    let bands = magnitudes.len().max(1);
    let band_hz = (sample_rate / 2.0) / bands as f64;
    let ranges = band_ranges();
    std::array::from_fn(|bar| {
        let lo = ((ranges.lo[bar] / band_hz) as usize).min(bands - 1);
        let hi = ((ranges.hi[bar] / band_hz).ceil() as usize).clamp(lo + 1, bands);
        let db = magnitudes[lo..hi]
            .iter()
            .copied()
            .fold(SPECTRUM_THRESHOLD_DB as f32, f32::max) as f64;
        let level = ((db - DB_LOW) / (DB_HIGH - DB_LOW)).clamp(0.0, 1.0);
        FLOOR + level * (CEIL - FLOOR)
    })
}

impl Analyzer {
    /// Feed a new spectrum frame: fast attack, smooth release, peak hold.
    pub fn push(&mut self, magnitudes: &[f32], sample_rate: Option<i32>) {
        if magnitudes.len() != SPECTRUM_BANDS as usize {
            return;
        }
        let target = bars_from_magnitudes(magnitudes, sample_rate.unwrap_or(44_100) as f64);
        for (i, value) in target.into_iter().enumerate() {
            let h = &mut self.heights[i];
            *h = if value > *h {
                value
            } else {
                *h - (*h - value) * 0.35
            };
            if *h >= self.peaks[i] {
                self.peaks[i] = *h;
                self.hold[i] = 12;
            } else if self.hold[i] > 0 {
                self.hold[i] -= 1;
            } else {
                self.peaks[i] = (self.peaks[i] - 1.8).max(FLOOR);
            }
        }
    }

    /// Clear the bars, e.g. after the visualizer was hidden for a while.
    pub fn reset(&mut self) {
        self.heights.fill(FLOOR);
        self.peaks.fill(FLOOR);
        self.hold.fill(0);
    }

    /// One step of the fall-off animation used when nothing is playing.
    /// Returns `true` while something is still moving.
    pub fn decay(&mut self) -> bool {
        let mut moving = false;
        for i in 0..BARS {
            if self.heights[i] > FLOOR {
                self.heights[i] = (self.heights[i] * 0.7).max(FLOOR);
                moving = true;
            }
            if self.peaks[i] > FLOOR {
                self.peaks[i] = (self.peaks[i] - 1.5).max(FLOOR);
                moving = true;
            }
        }
        moving
    }

    pub fn draw(
        &self,
        kind: AnalyzerType,
        widget: &gtk::DrawingArea,
        cr: &gtk::cairo::Context,
        width: i32,
        height: i32,
    ) {
        let accent = widget.color();
        let (ar, ag, ab) = (
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
        );
        let (width, height) = (width as f64, height as f64);
        cr.set_source_rgba(ar, ag, ab, 0.95);
        let spacing = 2.0;
        let total = self.heights.len();
        let bar_width = (width - spacing * (total - 1) as f64) / total as f64;
        let dot_h = 2.0;
        let dot_gap = 1.0;
        let max_dots = (height / (dot_h + dot_gap)) as usize;

        match kind {
            AnalyzerType::Bars => {
                for (i, &val) in self.heights.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let bar_h = (val / 50.0) * height;
                    let y = (height - bar_h) / 2.0;
                    cr.new_sub_path();
                    cr.arc(
                        x + bar_width / 2.0,
                        y,
                        bar_width / 2.0,
                        std::f64::consts::PI,
                        0.0,
                    );
                    cr.arc(
                        x + bar_width / 2.0,
                        y + bar_h,
                        bar_width / 2.0,
                        0.0,
                        std::f64::consts::PI,
                    );
                    cr.close_path();
                    let _ = cr.fill();
                }
            }
            AnalyzerType::Wave => {
                cr.set_line_width(2.0);
                cr.move_to(0.0, height / 2.0);
                for (i, &val) in self.heights.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing) + bar_width / 2.0;
                    // Alternate up/down so the line reads as a waveform.
                    let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                    let offset = sign * (val - FLOOR) / (50.0 - FLOOR) * (height / 2.0 - 2.0);
                    cr.line_to(x, height / 2.0 + offset);
                }
                cr.line_to(width, height / 2.0);
                let _ = cr.stroke();
            }
            AnalyzerType::BlocksWithHold | AnalyzerType::DigitalVuBlocks => {
                if max_dots == 0 {
                    return;
                }
                for (i, &val) in self.heights.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let active =
                        ((((val / 50.0) * height) / (dot_h + dot_gap)) as usize).clamp(1, max_dots);
                    for j in 0..active {
                        let y = height - (j as f64 * (dot_h + dot_gap)) - dot_h;
                        cr.rectangle(x, y, bar_width, dot_h);
                    }
                    let _ = cr.fill();
                    if kind == AnalyzerType::BlocksWithHold {
                        let peak_idx = ((((self.peaks[i] / 50.0) * height) / (dot_h + dot_gap))
                            as usize)
                            .min(max_dots - 1);
                        let y = height - (peak_idx as f64 * (dot_h + dot_gap)) - dot_h;
                        cr.set_source_rgba(ar, ag, ab, 0.40);
                        cr.rectangle(x, y, bar_width, dot_h);
                        let _ = cr.fill();
                        cr.set_source_rgba(ar, ag, ab, 0.95);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_flat_and_tone_lights_one_region() {
        let silence = vec![-80.0f32; SPECTRUM_BANDS as usize];
        assert!(bars_from_magnitudes(&silence, 44_100.0)
            .iter()
            .all(|&h| h == FLOOR));

        // A loud 1 kHz tone.
        let mut tone = silence.clone();
        let band_hz = 22_050.0 / SPECTRUM_BANDS as f64;
        tone[(1000.0 / band_hz) as usize] = -12.0;
        let bars = bars_from_magnitudes(&tone, 44_100.0);
        let lit: Vec<usize> = (0..BARS).filter(|&i| bars[i] > FLOOR + 1.0).collect();
        assert!(!lit.is_empty() && lit.len() <= 4, "lit bars: {:?}", lit);
        let expected = ((1000.0f64 / MIN_HZ).ln() / (MAX_HZ / MIN_HZ).ln() * BARS as f64) as usize;
        assert!(
            lit.iter().all(|&i| i.abs_diff(expected) <= 3),
            "{:?} vs {}",
            lit,
            expected
        );
    }

    #[test]
    fn decay_reaches_floor() {
        let mut a = Analyzer {
            heights: vec![40.0; BARS],
            peaks: vec![45.0; BARS],
            ..Default::default()
        };
        let mut steps = 0;
        while a.decay() {
            steps += 1;
            assert!(steps < 100);
        }
        assert!(a.heights.iter().all(|&h| h == FLOOR));
    }
}
