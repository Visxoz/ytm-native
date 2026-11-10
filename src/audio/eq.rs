//! The equaliser: six peaking filters, run in the
//! device callback so a change is heard at once rather than after the
//! audio already decoded ahead.

use std::f64::consts::PI;

/// Each band's centre in Hz.
pub const BANDS: [f64; 6] = [60.0, 150.0, 400.0, 1_000.0, 2_400.0, 15_000.0];
/// The furthest a band goes either way, in dB.
pub const MAX_DB: f32 = 12.0;
/// The decoder only gives mono or stereo; channels past this would pass
/// through unfiltered.
const MAX_CHANNELS: usize = 2;
/// How wide the peaks are: about an octave and a half.
const Q: f64 = 0.9;
/// Filter memory below this is flushed to zero, as denormal floats would
/// slow every sample down once the music goes quiet.
const DENORMAL: f32 = 1e-20;

pub type Gains = [f32; BANDS.len()];

/// Starting points: each band's gain in dB, from low to high.
pub const PRESETS: [(&str, Gains); 7] = [
    ("Flat", [0.0; 6]),
    ("Bass boost", [6.0, 4.0, 1.0, 0.0, 0.0, 0.0]),
    ("Treble boost", [0.0, 0.0, 0.0, 1.0, 4.0, 6.0]),
    ("Vocal", [-2.0, -1.0, 2.0, 4.0, 3.0, 0.0]),
    ("Rock", [4.0, 2.0, -2.0, -1.0, 2.0, 4.0]),
    ("Electronic", [5.0, 3.0, 0.0, -1.0, 2.0, 4.0]),
    ("Acoustic", [3.0, 2.0, 1.0, 1.0, 2.0, 2.0]),
];

/// Coefficients of one band's peaking filter, normalised by a0 (RBJ's
/// Audio EQ Cookbook). A peaking filter's b1 equals its a1, so only a1 is kept.
#[derive(Debug, Default, Clone, Copy)]
struct Biquad {
    b0: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    fn new(band: usize, db: f32, rate: u32) -> Self {
        // A centre at or past Nyquist makes the filter unstable, which would
        // turn the music into full-scale noise; such a band passes through.
        if BANDS[band] >= f64::from(rate) * 0.45 {
            return Self {
                b0: 1.0,
                ..Self::default()
            };
        }
        let a = 10f64.powf(f64::from(db) / 40.0);
        let w0 = 2.0 * PI * BANDS[band] / f64::from(rate);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * Q);
        let [b0, b2, a0, a1, a2] = [
            1.0 + alpha * a,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ];
        Self {
            b0: (b0 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Equaliser {
    gains: Option<Gains>,
    /// The format the filters were made for, and whether they need making.
    rate: u32,
    channels: usize,
    stale: bool,
    /// Every band's filter, so the memory keeps its layout as gains change.
    filters: [Biquad; BANDS.len()],
    /// Each channel's memory for each filter, channel by channel. Kept
    /// through a change of gains, as zeroing it mid-song would click. Sized
    /// for the most channels the decoder gives, so the device callback
    /// never allocates.
    memory: [[f32; 2]; BANDS.len() * MAX_CHANNELS],
    /// Lowers the whole signal by the peak of the combined boost, so the
    /// bands cannot push the music past full scale.
    preamp: f32,
    /// Every band at 0 dB, so nothing needs running.
    flat: bool,
}

impl Equaliser {
    /// `None` turns the equaliser off.
    pub(super) fn set(&mut self, gains: Option<Gains>) {
        let gains = gains.map(|g| g.map(|db| db.clamp(-MAX_DB, MAX_DB)));
        // A slider sends every pointer move, most of them changing nothing.
        if gains != self.gains {
            self.gains = gains;
            self.stale = true;
        }
    }

    fn rebuild(&mut self, rate: u32, channels: usize) {
        let gains = self.gains.unwrap_or_default();
        self.filters = std::array::from_fn(|band| Biquad::new(band, gains[band], rate));
        if channels != self.channels {
            self.memory.fill([0.0; 2]);
        }
        // Neighbouring bands overlap and their boosts add, so the peak is
        // found on a sixth-octave grid from 20 Hz to below Nyquist.
        let top = f64::from(rate) * 0.45;
        let boost = (0..64)
            .map(|i| (20.0 * 1_000f64.powf(f64::from(i) / 63.0)).min(top))
            .map(|hz| {
                self.filters
                    .iter()
                    .map(|&f| response_db(f, hz, rate))
                    .sum::<f64>()
            })
            .fold(0.0, f64::max);
        self.preamp = 10f64.powf(-boost / 20.0) as f32;
        self.flat = gains.iter().all(|&db| db == 0.0);
        (self.rate, self.channels, self.stale) = (rate, channels, false);
    }

    /// Filters interleaved `samples` in place.
    pub(super) fn process(&mut self, samples: &mut [f32], rate: u32, channels: usize) {
        // Filters left idle are kept at rest, so they don't later resume with
        // stale music in their memory, which thumps. An idle 0 dB filter
        // would have settled there anyway.
        if self.gains.is_none() || rate == 0 || channels == 0 {
            self.memory.fill([0.0; 2]);
            return;
        }
        if self.stale || rate != self.rate || channels != self.channels {
            self.rebuild(rate, channels);
        }
        if self.flat {
            self.memory.fill([0.0; 2]);
            return;
        }
        for frame in samples.chunks_exact_mut(channels) {
            let (memories, _) = self.memory.as_chunks_mut::<{ BANDS.len() }>();
            for (sample, memory) in frame.iter_mut().zip(memories) {
                let mut x = *sample;
                // Transposed direct form II, with b1 = a1.
                for (f, [z1, z2]) in self.filters.iter().zip(memory) {
                    let y = f.b0 * x + *z1;
                    *z1 = f.a1 * (x - y) + *z2;
                    *z2 = f.b2 * x - f.a2 * y;
                    x = y;
                }
                *sample = x * self.preamp;
            }
        }
        // A filter that blew up anyway is put back at rest rather than left
        // sending infinities to the device.
        for z in self.memory.iter_mut().flatten() {
            if z.abs() < DENORMAL || !z.is_finite() {
                *z = 0.0;
            }
        }
    }
}

/// The filter's gain in dB at `hz`, from its transfer function.
fn response_db(f: Biquad, hz: f64, rate: u32) -> f64 {
    let w = 2.0 * PI * hz / f64::from(rate);
    // H(e^jw) = (b0 + b1 e^-jw + b2 e^-2jw) / (1 + a1 e^-jw + a2 e^-2jw)
    let part = |c0: f32, c1: f32, c2: f32| {
        let (c0, c1, c2) = (f64::from(c0), f64::from(c1), f64::from(c2));
        let re = c0 + c1 * w.cos() + c2 * (2.0 * w).cos();
        let im = -c1 * w.sin() - c2 * (2.0 * w).sin();
        (re * re + im * im).sqrt()
    };
    20.0 * (part(f.b0, f.a1, f.b2) / part(1.0, f.a1, f.a2)).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_reach_their_gain() {
        for rate in [44_100, 48_000] {
            for db in [-12.0, 6.0] {
                for (band, &hz) in BANDS.iter().enumerate() {
                    let f = Biquad::new(band, db, rate);
                    assert!((response_db(f, hz, rate) - f64::from(db)).abs() < 0.01);
                }
                let mid = Biquad::new(3, db, rate);
                assert!(
                    response_db(mid, 20.0, rate).abs() < 0.1,
                    "far bands untouched"
                );
                assert!(response_db(mid, 20_000.0, rate).abs() < 0.1);
            }
        }
    }

    #[test]
    fn off_and_flat_leave_samples_alone() {
        let mut eq = Equaliser::default();
        let mut samples = [0.5, -0.25, 0.125, 1.0];
        eq.process(&mut samples, 48_000, 2);
        assert_eq!(samples, [0.5, -0.25, 0.125, 1.0], "off");
        eq.set(Some([0.0; 6]));
        eq.process(&mut samples, 48_000, 2);
        assert_eq!(samples, [0.5, -0.25, 0.125, 1.0], "flat runs no filters");
    }

    #[test]
    fn boosts_leave_headroom() {
        let mut eq = Equaliser::default();
        eq.set(Some([20.0, 0.0, 0.0, 0.0, -3.0, 0.0]));
        // A long 60 Hz tone at full scale stays within it once settled.
        let mut samples: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * PI * 60.0 * f64::from(i) / 48_000.0).sin() as f32)
            .collect();
        eq.process(&mut samples, 48_000, 1);
        let peak = samples[24_000..].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.05, "boost clamped to 12 dB and cancelled: {peak}");
        assert!(peak > 0.7, "the boosted band stays near full scale: {peak}");

        // Overlapping boosts add up, and the preamp covers their sum.
        for gains in [[12.0; 6], PRESETS[1].1] {
            eq.set(Some(gains));
            let mut tones: Vec<f32> = (0..96_000)
                .map(|i| {
                    let t = f64::from(i) / 48_000.0;
                    ((2.0 * PI * 63.0 * t).sin() * 0.5 + (2.0 * PI * 1_000.0 * t).sin() * 0.5)
                        as f32
                })
                .collect();
            eq.process(&mut tones, 48_000, 1);
            let peak = tones[48_000..].iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!(peak <= 1.02, "{gains:?} peaks at {peak}");
        }
    }

    #[test]
    fn bands_past_nyquist_pass_through() {
        let mut eq = Equaliser::default();
        eq.set(Some(PRESETS[2].1));
        let mut tone: Vec<f32> = (0..22_050)
            .map(|i| (2.0 * PI * 1_000.0 * f64::from(i) / 22_050.0).sin() as f32)
            .collect();
        eq.process(&mut tone, 22_050, 1);
        let peak = tone.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(
            peak <= 1.02,
            "the 15 kHz band stays stable at 22 kHz: {peak}"
        );
    }

    #[test]
    fn changing_gains_keeps_the_filters_memory() {
        let mut eq = Equaliser::default();
        eq.set(Some([6.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        let mut samples = vec![0.5f32; 4_800];
        eq.process(&mut samples, 48_000, 1);
        let memory = eq.memory;
        eq.set(Some([6.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        assert!(!eq.stale, "the same gains change nothing");
        eq.set(Some([7.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        eq.process(&mut [], 48_000, 1);
        assert_eq!(eq.memory, memory, "a new gain carries the memory over");

        // Flat only between two callbacks: the filters never stopped.
        eq.set(Some([0.0; 6]));
        eq.set(Some([-1.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        eq.process(&mut [], 48_000, 1);
        assert_eq!(eq.memory, memory, "a passing 0 dB keeps the memory");

        // Off or flat for a callback, they resume from rest.
        for gains in [None, Some([0.0; 6])] {
            eq.set(Some([7.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
            eq.process(&mut samples, 48_000, 1);
            eq.set(gains);
            eq.process(&mut [], 48_000, 1);
            assert!(
                eq.memory.iter().flatten().all(|&z| z == 0.0),
                "{gains:?} leaves the filters at rest"
            );
        }
    }

    #[test]
    fn filters_each_channel_alike() {
        let tone: Vec<f32> = (0..4_800)
            .map(|i| (2.0 * PI * 100.0 * f64::from(i) / 48_000.0).sin() as f32)
            .collect();
        let mut mono = tone.clone();
        let mut stereo: Vec<f32> = tone.iter().flat_map(|&s| [s, s]).collect();
        let mut eq = Equaliser::default();
        eq.set(Some(PRESETS[4].1));
        eq.process(&mut mono, 48_000, 1);
        let mut eq = Equaliser::default();
        eq.set(Some(PRESETS[4].1));
        eq.process(&mut stereo, 48_000, 2);
        for (frame, &m) in stereo.as_chunks::<2>().0.iter().zip(&mono) {
            assert_eq!(*frame, [m, m]);
        }
    }
}
