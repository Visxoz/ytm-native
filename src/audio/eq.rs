//! Ten peaking filters, run in the device callback so a change is heard at
//! once rather than after the audio already decoded ahead.

use std::f64::consts::{PI, SQRT_2};

/// Each band's centre in Hz, an octave apart.
pub const BANDS: [f64; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
];
/// The furthest a band goes either way, in dB.
pub const MAX_DB: f32 = 12.0;
/// The decoder only gives mono or stereo; channels past this would pass
/// through unfiltered.
const MAX_CHANNELS: usize = 2;
/// How wide the peaks are: an octave.
const Q: f64 = SQRT_2;
/// How fast a gain glides to a new setting, in dB a second: the whole
/// 12 dB in 75 ms, so a slider's steps never click.
const GLIDE: f32 = 160.0;
/// Frames between new coefficients while gliding.
const BLOCK: usize = 32;
/// Filter memory below this is flushed to zero, as denormal floats would
/// slow every sample down once the music goes quiet.
const DENORMAL: f32 = 1e-20;

pub type Gains = [f32; BANDS.len()];

/// Starting points: each band's gain in dB, from low to high.
pub const PRESETS: [(&str, Gains); 7] = [
    ("Flat", [0.0; 10]),
    (
        "Bass boost",
        [6.0, 6.0, 4.0, 2.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    ),
    (
        "Treble boost",
        [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0],
    ),
    (
        "Vocal",
        [-2.0, -2.0, -1.0, 1.0, 3.0, 4.0, 3.0, 2.0, 1.0, 0.0],
    ),
    ("Rock", [4.0, 4.0, 2.0, 0.0, -2.0, -1.0, 1.0, 3.0, 4.0, 4.0]),
    (
        "Electronic",
        [5.0, 5.0, 3.0, 1.0, 0.0, -1.0, 1.0, 3.0, 4.0, 4.0],
    ),
    (
        "Acoustic",
        [3.0, 3.0, 2.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0],
    ),
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
    /// The gains set, flat while off.
    target: Gains,
    /// The gains the filters are at, gliding toward `target`.
    playing: Gains,
    /// The format the filters were made for.
    rate: u32,
    channels: usize,
    /// Every band's filter, so the memory keeps its layout as gains change.
    filters: [Biquad; BANDS.len()],
    /// Kept through a change of gains, as zeroing it mid-song would click;
    /// sized for the most channels, so the device callback never allocates.
    memory: [[f32; 2]; BANDS.len() * MAX_CHANNELS],
    /// Lowers the whole signal by the peak of the combined boost, so the
    /// bands cannot push the music past full scale.
    preamp: f32,
    /// Frames the filters still run once glided to flat, so what rings in
    /// their memory dies away rather than being cut off with a click.
    ringing: usize,
}

impl Equaliser {
    /// `None` turns the equaliser off.
    pub(super) fn set(&mut self, gains: Option<Gains>) {
        self.target = gains.map_or([0.0; BANDS.len()], |g| {
            g.map(|db| db.clamp(-MAX_DB, MAX_DB))
        });
    }

    /// Glided to flat, or never moved from it.
    fn flat(&self) -> bool {
        self.playing == [0.0; BANDS.len()] && self.target == self.playing
    }

    pub(super) fn process(&mut self, samples: &mut [f32], rate: u32, channels: usize) {
        // Idle filters are kept at rest, as resuming with stale music in
        // their memory thumps.
        if self.flat() && self.ringing == 0 || rate == 0 || channels == 0 {
            self.memory.fill([0.0; 2]);
            return;
        }
        if rate != self.rate || channels != self.channels {
            if channels != self.channels {
                self.memory.fill([0.0; 2]);
            }
            self.filters = filters(self.playing, rate);
            self.preamp = headroom(&self.filters, rate);
            (self.rate, self.channels) = (rate, channels);
        }
        let frames = samples.len() / channels;
        let start = self.playing;
        let per_frame = GLIDE / rate as f32;
        // Frames until every band arrives, in whole blocks.
        let gliding = if start == self.target {
            0
        } else {
            let furthest = (start.iter().zip(&self.target))
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            ((furthest / per_frame).ceil() as usize)
                .next_multiple_of(BLOCK)
                .min(frames)
        };
        let (mut ratio, mut end_preamp) = (1.0, self.preamp);
        if gliding > 0 {
            let end = toward(start, self.target, per_frame * gliding as f32);
            end_preamp = headroom(&filters(end, rate), rate);
            // A steady ratio moves the preamp evenly in dB, as the gains move.
            ratio = (end_preamp / self.preamp).powf(1.0 / gliding as f32);
        }
        let (memories, _) = self.memory.as_chunks_mut::<{ BANDS.len() }>();
        let mut done = 0;
        for block in samples.chunks_mut(BLOCK * channels) {
            let step = if done < gliding {
                done += block.len() / channels;
                let gains = toward(start, self.target, per_frame * done as f32);
                for (band, (&db, was)) in gains.iter().zip(&self.playing).enumerate() {
                    if db != *was {
                        self.filters[band] = Biquad::new(band, db, rate);
                    }
                }
                self.playing = gains;
                ratio
            } else {
                1.0
            };
            for frame in block.chunks_exact_mut(channels) {
                self.preamp *= step;
                for (sample, memory) in frame.iter_mut().zip(memories.iter_mut()) {
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
        }
        // Exactly, past the ramp's rounding.
        self.preamp = end_preamp;
        // A tenth of a second takes even a full-scale bass note's ringing
        // down to about -60 dB.
        self.ringing = if self.flat() {
            self.ringing.saturating_sub(frames - gliding)
        } else {
            rate as usize / 10
        };
        // A filter that blew up anyway is put back at rest rather than left
        // sending infinities to the device.
        for z in self.memory.iter_mut().flatten() {
            if z.abs() < DENORMAL || !z.is_finite() {
                *z = 0.0;
            }
        }
    }
}

fn filters(gains: Gains, rate: u32) -> [Biquad; BANDS.len()] {
    std::array::from_fn(|band| Biquad::new(band, gains[band], rate))
}

/// Each gain moved up to `by` dB toward its target, landing on it exactly.
fn toward(from: Gains, to: Gains, by: f32) -> Gains {
    std::array::from_fn(|band| {
        let gap = to[band] - from[band];
        if gap.abs() <= by {
            to[band]
        } else {
            from[band] + by.copysign(gap)
        }
    })
}

/// The preamp that cancels the peak of the combined boost. Neighbouring
/// bands overlap and their boosts add, so the peak is found on a
/// sixth-octave grid from 20 Hz to below Nyquist.
fn headroom(filters: &[Biquad], rate: u32) -> f32 {
    // 1000^(1/63): 64 points from 20 Hz to 20 kHz.
    const STEP: f64 = 1.115_884;
    let top = f64::from(rate) * 0.45;
    let peak = std::iter::successors(Some(20.0), |hz| Some(hz * STEP))
        .take(64)
        .map(|hz: f64| response(filters, hz.min(top), rate))
        .fold(1.0, f64::max);
    (1.0 / peak) as f32
}

/// The filters' combined gain at `hz`, from their transfer functions.
fn response(filters: &[Biquad], hz: f64, rate: u32) -> f64 {
    let w = 2.0 * PI * hz / f64::from(rate);
    let [(s1, c1), (s2, c2)] = [w.sin_cos(), (2.0 * w).sin_cos()];
    // H(e^jw) = (b0 + b1 e^-jw + b2 e^-2jw) / (1 + a1 e^-jw + a2 e^-2jw),
    // each part's magnitude squared.
    let part = |k0: f32, k1: f32, k2: f32| {
        let (k0, k1, k2) = (f64::from(k0), f64::from(k1), f64::from(k2));
        let (re, im) = (k0 + k1 * c1 + k2 * c2, k1 * s1 + k2 * s2);
        re * re + im * im
    };
    filters
        .iter()
        .map(|f| part(f.b0, f.a1, f.b2) / part(1.0, f.a1, f.a2))
        .product::<f64>()
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT: Gains = [0.0; BANDS.len()];

    fn tone(hz: f64, rate: u32, len: usize, level: f64) -> Vec<f32> {
        (0..len)
            .map(|i| ((2.0 * PI * hz * i as f64 / f64::from(rate)).sin() * level) as f32)
            .collect()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn bands_reach_their_gain() {
        for rate in [44_100, 48_000] {
            let db_at = |f, hz| 20.0 * response(&[f], hz, rate).log10();
            for db in [-12.0, 6.0] {
                for (band, &hz) in BANDS.iter().enumerate() {
                    let f = Biquad::new(band, db, rate);
                    assert!((db_at(f, hz) - f64::from(db)).abs() < 0.01, "{hz} Hz");
                }
                let mid = Biquad::new(5, db, rate);
                assert!(db_at(mid, 20.0).abs() < 0.1, "far bands untouched");
                assert!(db_at(mid, 20_000.0).abs() < 0.1);
            }
        }
    }

    #[test]
    fn off_and_flat_leave_samples_alone() {
        let mut eq = Equaliser::default();
        let mut samples = [0.5, -0.25, 0.125, 1.0];
        eq.process(&mut samples, 48_000, 2);
        assert_eq!(samples, [0.5, -0.25, 0.125, 1.0], "off");
        eq.set(Some(FLAT));
        eq.process(&mut samples, 48_000, 2);
        assert_eq!(samples, [0.5, -0.25, 0.125, 1.0], "flat runs no filters");
    }

    #[test]
    fn boosts_leave_headroom() {
        let mut eq = Equaliser::default();
        let mut gains = FLAT;
        (gains[1], gains[7]) = (20.0, -3.0);
        eq.set(Some(gains));
        // A long 62 Hz tone at full scale stays within it once settled.
        let mut samples = tone(62.0, 48_000, 48_000, 1.0);
        eq.process(&mut samples, 48_000, 1);
        let settled = peak(&samples[24_000..]);
        assert!(
            settled <= 1.05,
            "boost clamped to 12 dB and cancelled: {settled}"
        );
        assert!(
            settled > 0.7,
            "the boosted band stays near full scale: {settled}"
        );

        // Overlapping boosts add up, and the preamp covers their sum.
        for gains in [[12.0; BANDS.len()], PRESETS[1].1] {
            eq.set(Some(gains));
            let mut tones: Vec<f32> = (tone(63.0, 48_000, 96_000, 0.5).iter())
                .zip(tone(1_000.0, 48_000, 96_000, 0.5))
                .map(|(a, b)| a + b)
                .collect();
            eq.process(&mut tones, 48_000, 1);
            let settled = peak(&tones[48_000..]);
            assert!(settled <= 1.02, "{gains:?} peaks at {settled}");
        }
    }

    #[test]
    fn bands_past_nyquist_pass_through() {
        let mut eq = Equaliser::default();
        eq.set(Some(PRESETS[2].1));
        let mut tone = tone(1_000.0, 22_050, 22_050, 1.0);
        for block in tone.chunks_mut(480) {
            eq.process(block, 22_050, 1);
        }
        let peak = peak(&tone);
        assert!(
            peak <= 1.02,
            "the 16 kHz band stays stable at 22 kHz: {peak}"
        );
    }

    #[test]
    fn a_gain_change_glides() {
        let rate = 48_000;
        let mut samples = tone(1_000.0, rate, 9_600, 0.25);
        let mut eq = Equaliser::default();
        eq.set(Some(FLAT));
        // Up to the tone's peak, where a jump in level would show most.
        let (before, mut rest) = samples.split_at_mut(492);
        eq.process(before, rate, 1);
        // A 12 dB boost far below lowers the tone by as much, through the
        // preamp, fed in callbacks of uneven sizes.
        let mut gains = FLAT;
        gains[0] = 12.0;
        eq.set(Some(gains));
        for size in [441, 0, 100, 7].into_iter().cycle() {
            let (now, later) = rest.split_at_mut(size.min(rest.len()));
            eq.process(now, rate, 1);
            rest = later;
            if rest.is_empty() {
                break;
            }
        }
        let largest = samples
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(largest < 0.034, "no step: {largest}");
        // 12 dB takes 75 ms, 3,600 frames: halfway there at 1,800.
        let halfway = peak(&samples[492 + 1_776..492 + 1_824]);
        assert!((0.1..0.16).contains(&halfway), "{halfway}");
        assert_eq!(eq.playing, gains, "arrived");
        assert_eq!(eq.preamp, headroom(&eq.filters, rate));
        let after = peak(&samples[4_800..]);
        assert!((0.055..0.07).contains(&after), "{after}");
    }

    #[test]
    fn changing_gains_keeps_the_filters_memory() {
        let mut eq = Equaliser::default();
        let mut six = FLAT;
        six[0] = 6.0;
        eq.set(Some(six));
        let mut samples = vec![0.5f32; 4_800];
        eq.process(&mut samples, 48_000, 1);
        let memory = eq.memory;

        // Flat only between two callbacks: the filters never stopped.
        eq.set(Some(FLAT));
        eq.set(Some([-1.0; BANDS.len()]));
        eq.process(&mut [], 48_000, 1);
        assert_eq!(eq.memory, memory, "a passing 0 dB keeps the memory");

        // Off or flat, they glide there and then rest.
        for gains in [None, Some(FLAT)] {
            eq.set(Some(six));
            eq.process(&mut samples, 48_000, 1);
            eq.set(gains);
            eq.process(&mut samples[..32], 48_000, 1);
            assert!(eq.memory[0][0] != 0.0, "{gains:?} still gliding");
            // Arriving, then ringing out.
            eq.process(&mut samples, 48_000, 1);
            eq.process(&mut samples, 48_000, 1);
            let mut untouched = [0.5; 4];
            eq.process(&mut untouched, 48_000, 1);
            assert!(
                eq.memory.iter().flatten().all(|&z| z == 0.0),
                "{gains:?} leaves the filters at rest"
            );
            assert_eq!(untouched, [0.5; 4], "{gains:?} passes through");
        }
    }

    #[test]
    fn filters_each_channel_alike() {
        let tone = tone(100.0, 48_000, 4_800, 1.0);
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

    #[test]
    fn turning_off_rings_out_without_a_click() {
        let rate = 48_000;
        let mut eq = Equaliser::default();
        let mut gains = FLAT;
        gains[0] = 12.0;
        eq.set(Some(gains));
        // The slowest band's ringing, at full scale, is the hardest to end.
        let mut samples = tone(31.0, rate, 96_000, 1.0);
        let (on, off) = samples.split_at_mut(48_000);
        for block in on.chunks_mut(480) {
            eq.process(block, rate, 1);
        }
        eq.set(None);
        for block in off.chunks_mut(480) {
            eq.process(block, rate, 1);
        }
        assert!(eq.memory.iter().flatten().all(|&z| z == 0.0), "at rest");
        // A 31 Hz sine moves at most 0.004 a sample, the glide adding little.
        let largest = (off.windows(2))
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(largest < 0.006, "no click: {largest}");
    }
}
