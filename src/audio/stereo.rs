//! Crossfeed, mono and balance for headphones, run in the device callback
//! after the equaliser so a change is heard at once.

use std::f32::consts::PI;

/// Bauer's bs2b default level: the other side's sound under about 700 Hz,
/// fed in 4.5 dB under the direct side's.
const CUT_HZ: f32 = 700.0;
/// G / (1 - G) = 10^(-4.5 / 20), the feed's share of a side's lows.
const FEED: f32 = 0.373_3;
/// How long a change takes, so turning one on or off never clicks.
const RAMP_SECS: f32 = 0.05;
/// As in the equaliser: denormal filter memory would slow every sample.
const DENORMAL: f32 = 1e-20;

#[derive(Debug, Default)]
pub(super) struct Stereo {
    pub(super) crossfeed: bool,
    pub(super) mono: bool,
    /// -1 is the left side alone, 1 the right.
    pub(super) balance: f32,
    /// How far crossfeed, mono and balance are in, moving towards the
    /// settings a step a frame; all zero is idle.
    now: [f32; 3],
    /// Each side low-passed, for the crossfeed.
    lows: [f32; 2],
    /// The low-pass's coefficient, for `rate`.
    pole: f32,
    rate: u32,
}

impl Stereo {
    pub(super) fn reset(&mut self) {
        self.lows = [0.0; 2];
    }

    pub(super) fn process(&mut self, samples: &mut [f32], rate: u32, channels: usize) {
        let target = [
            f32::from(u8::from(self.crossfeed)),
            f32::from(u8::from(self.mono)),
            self.balance,
        ];
        // shortcut: a mono track opens a 1-channel device, so none of this
        // applies to it; open a stereo device for it should balance matter.
        if channels != 2 || rate == 0 || (target == [0.0; 3] && self.now == [0.0; 3]) {
            self.lows = [0.0; 2];
            return;
        }
        if rate != self.rate {
            self.pole = 1.0 - (-2.0 * PI * CUT_HZ / rate as f32).exp();
            self.rate = rate;
        }
        let step = 1.0 / (RAMP_SECS * rate as f32);
        for frame in samples.as_chunks_mut::<2>().0 {
            for (now, target) in self.now.iter_mut().zip(target) {
                *now = if *now < target {
                    (*now + step).min(target)
                } else {
                    (*now - step).max(target)
                };
            }
            let [feed, mono, balance] = self.now;
            let [l, r] = *frame;
            for (low, x) in self.lows.iter_mut().zip([l, r]) {
                *low += self.pole * (x - *low);
            }
            // Both sides' lows move the same way, so a centred sound is kept.
            let shift = feed * FEED * (self.lows[1] - self.lows[0]);
            let (l, r) = (l + shift, r - shift);
            let shift = mono * 0.5 * (r - l);
            let (l, r) = (l + shift, r - shift);
            *frame = [l * (1.0 - balance.max(0.0)), r * (1.0 + balance.min(0.0))];
        }
        for z in &mut self.lows {
            if z.abs() < DENORMAL || !z.is_finite() {
                *z = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// A second of stereo frames, settled past any ramp.
    fn run(stereo: &mut Stereo, frame: [f32; 2]) -> [f32; 2] {
        let mut samples = [frame; 48_000];
        stereo.process(samples.as_flattened_mut(), RATE, 2);
        samples[47_999]
    }

    #[test]
    fn idle_leaves_samples_alone() {
        let music: Vec<f32> = (0..960).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut samples = music.clone();
        let mut stereo = Stereo::default();
        stereo.process(&mut samples, RATE, 2);
        assert_eq!(samples, music);

        stereo.mono = true;
        stereo.process(&mut samples, RATE, 1);
        assert_eq!(samples, music, "a mono track is left alone");
    }

    #[test]
    fn mono_sums_the_sides() {
        let mut stereo = Stereo {
            mono: true,
            ..Default::default()
        };
        assert_eq!(run(&mut stereo, [0.5, 0.25]), [0.375, 0.375]);
    }

    #[test]
    fn balance_lowers_the_other_side() {
        for (balance, gains) in [
            (0.0, [1.0, 1.0]),
            (-1.0, [1.0, 0.0]),
            (1.0, [0.0, 1.0]),
            (0.5, [0.5, 1.0]),
            (-0.25, [1.0, 0.75]),
        ] {
            let mut stereo = Stereo {
                balance,
                ..Default::default()
            };
            assert_eq!(run(&mut stereo, [1.0, 1.0]), gains, "{balance}");
        }
    }

    #[test]
    fn crossfeed_keeps_the_centre_and_leaks_the_sides() {
        let mut stereo = Stereo {
            crossfeed: true,
            ..Default::default()
        };
        let [l, r] = run(&mut stereo, [0.5, 0.5]);
        assert!((l - 0.5).abs() < 1e-6 && (r - 0.5).abs() < 1e-6, "{l} {r}");

        // A steady sound on the left alone is low enough to be fed across.
        let [l, r] = run(&mut stereo, [0.5, 0.0]);
        let feed_db = 20.0 * (r / l).log10();
        assert!((feed_db + 4.5).abs() < 0.05, "{feed_db} dB");

        // Highs stay where they are.
        let mut samples: Vec<f32> = (0..9_600)
            .flat_map(|i| [if i % 2 == 0 { 0.5 } else { -0.5 }, 0.0])
            .collect();
        stereo.process(&mut samples, RATE, 2);
        let tail = &samples[9_600..];
        let peak = |side: usize| {
            tail.iter()
                .skip(side)
                .step_by(2)
                .fold(0f32, |m, s| m.max(s.abs()))
        };
        assert!(peak(0) > 0.45, "{}", peak(0));
        assert!(peak(1) < 0.05, "{}", peak(1));
    }

    #[test]
    fn turning_on_ramps_and_off_passes_through_again() {
        let mut stereo = Stereo {
            mono: true,
            ..Default::default()
        };
        let mut samples = [[1.0, 0.0]; 4_800];
        stereo.process(samples.as_flattened_mut(), RATE, 2);
        let steps = samples.windows(2).map(|w| (w[1][0] - w[0][0]).abs());
        assert!(steps.fold(0f32, f32::max) < 0.001, "no step");
        assert_eq!(samples[0][0], 1.0 - 0.5 / 2_400.0, "a frame's step in");
        assert_eq!(samples[4_799], [0.5, 0.5], "all the way in after 50 ms");

        stereo.mono = false;
        run(&mut stereo, [0.5, 0.0]);
        let music = [0.3, -0.7, 0.1, 0.9];
        let mut samples = music;
        stereo.process(&mut samples, RATE, 2);
        assert_eq!(samples, music, "idle again once ramped out");
    }
}
