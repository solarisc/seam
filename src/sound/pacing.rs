//! How much sound to keep buffered against the network's timing, and how to
//! lean the playback rate so the two computers' audio clocks stay in step.
//!
//! Sound arrives in the sender's buffers and leaves in the speakers', about
//! 10 ms each. When packets arrive close to the moment the speakers take sound,
//! or when that moment isn't known, one arriving late now and then misses it,
//! so the buffer at a typical low point must then hold a packet in reserve plus
//! a margin. At its very lowest it must still hold the margin. Short of either,
//! the target grows at once; with room to spare, and the buffer settled, it
//! shrinks a step. Each time the buffer runs dry the margin widens, so a
//! network that is unsteady settles on a delay that suits it. Speakers that
//! tell when they take sound take a whole period at a time, so there the
//! reserve covers lateness and the margin starts narrower.
use anyhow::Result;
use rubato::{Adjustable, Async, FixedAsync, SincInterpolationParameters, Slip, WindowFunction};
use std::time::Duration;

/// The target when a connection starts, and the bounds it adapts between.
const START_DELAY: Duration = Duration::from_millis(20);
const MIN_DELAY: Duration = Duration::from_millis(2);
const MAX_DELAY: Duration = Duration::from_millis(250);
/// Added to the target when the buffer runs dry.
const LONGER: Duration = Duration::from_millis(10);
/// How often the buffer looks at how close it came to running dry, in time
/// played, and how many looks a shorter target must agree with.
const LOOK_EVERY: Duration = Duration::from_secs(2);
const LOOKS_REMEMBERED: usize = 5;
/// Low points counted per look: 2 s of them down to 2 ms apart.
const LOWS_PER_LOOK: usize = 1024;
/// The most a look shortens the target by.
const MOST_SHORTER: Duration = Duration::from_millis(2);
/// The buffer's mean moving less than this between looks counts as settled.
const SETTLED: Duration = Duration::from_millis(1);
/// The margin kept from empty: at first, its growth each time the buffer runs
/// dry, its most, and the looks without running dry before it narrows by 1 ms.
const MARGIN: Duration = Duration::from_millis(4);
/// The margin at first when the speakers tell when they take sound. They take
/// a whole period at a time, so a late packet needs the reserve, a whole
/// packet; the margin then only absorbs packets of uneven size and the
/// leaning of the rate.
#[cfg(any(windows, test))]
const TIMED_MARGIN: Duration = Duration::from_millis(1);
const WIDER: Duration = Duration::from_millis(3);
const MOST_MARGIN: Duration = Duration::from_millis(40);
const NARROW_AFTER: u32 = 150;
/// Buffered this far beyond the target, as when the network held packets back
/// and then delivered them together, the oldest are dropped: one skip instead
/// of playing late from then on.
pub(super) const EXCESS: Duration = Duration::from_millis(40);
/// Playback rate correction per second of distance from the target. At 0.1 a
/// clock 100 ppm off settles 1 ms from the target; the target itself follows
/// where the buffer actually runs, so that offset costs no margin.
const GAIN: f64 = 0.1;
/// The furthest playback leans from its nominal rate. 0.5 % is inaudible.
pub(super) const MOST_CORRECTION: f64 = 0.005;
/// The buffer's fill is averaged over about this long before it steers the
/// rate, so packets arriving in bursts don't waver the pitch.
const SMOOTHING: f64 = 1.0;
/// With nothing to play for this long the speakers pause, which costs
/// nothing, and resume with the next sound. Paused this long, they close.
pub(super) const PAUSE_AFTER: Duration = Duration::from_secs(2);
pub(super) const CLOSE_AFTER: Duration = Duration::from_secs(600);
/// How often running speakers check that they still play on the default
/// device, and how soon speakers that failed to open are tried again.
pub(super) const DEVICE_CHECK: Duration = Duration::from_secs(1);
pub(super) const REOPEN: Duration = Duration::from_secs(5);
/// Nothing to do until something happens.
pub(super) const UNTIL_WOKEN: Duration = Duration::from_secs(3600);

/// The buffer's target, adapted to the network's timing. Frames are counted
/// at the sender's sample rate.
pub(super) struct Pacing {
    rate: u32,
    target: usize,
    margin: usize,
    /// The narrowest the margin gets.
    least_margin: usize,
    /// Since the last look: the fewest frames left, and the sum and count of
    /// fills, for their mean.
    lowest: usize,
    fills: u64,
    takes: u64,
    /// The mean fill at the last look.
    last_mean: Option<usize>,
    /// The lowest fill of each recent look, newest first.
    lows: [usize; LOOKS_REMEMBERED],
    /// Low points since the last look: where the buffer stands each time the
    /// speakers have just taken what they need.
    low_points: [u32; LOWS_PER_LOOK],
    low_count: usize,
    /// The typical low point of each recent look, newest first.
    typicals: [usize; LOOKS_REMEMBERED],
    /// The most frames that have arrived at once.
    largest_packet: usize,
    /// Frames played since the last look.
    window: usize,
    /// Looks since the buffer last ran dry.
    calm: u32,
    /// A packet is held in reserve (see the module's notes).
    reserve: bool,
    pub(super) underruns: u64,
}

impl Pacing {
    pub(super) fn new(rate: u32) -> Self {
        let mut pacing = Self {
            rate: rate.max(1),
            target: 0,
            margin: 0,
            least_margin: 0,
            lowest: usize::MAX,
            fills: 0,
            takes: 0,
            last_mean: None,
            lows: [0; LOOKS_REMEMBERED],
            low_points: [0; LOWS_PER_LOOK],
            low_count: 0,
            typicals: [0; LOOKS_REMEMBERED],
            largest_packet: 0,
            window: 0,
            calm: 0,
            reserve: true,
            underruns: 0,
        };
        pacing.target = pacing.frames_in(START_DELAY);
        pacing.margin = pacing.frames_in(MARGIN);
        pacing.least_margin = pacing.margin;
        pacing
    }

    /// The speakers tell when they take sound: keep the narrower margin that
    /// allows (see `TIMED_MARGIN`). A margin already widened by running dry
    /// stays as wide.
    #[cfg(any(windows, test))]
    pub(super) fn takes_known(&mut self) {
        let timed = self.frames_in(TIMED_MARGIN);
        if self.least_margin != timed {
            if self.margin == self.least_margin {
                self.margin = timed;
            }
            self.least_margin = timed;
        }
    }

    pub(super) fn rate(&self) -> u32 {
        self.rate
    }

    pub(super) fn frames_in(&self, duration: Duration) -> usize {
        (duration.as_secs_f64() * f64::from(self.rate)).round() as usize
    }

    pub(super) fn target(&self) -> usize {
        self.target
    }

    /// The delay currently kept against the network's timing.
    pub(super) fn delay(&self) -> Duration {
        Duration::from_secs_f64(self.target as f64 / f64::from(self.rate))
    }

    /// Beyond this many frames, the buffer drops its oldest.
    pub(super) fn excess(&self) -> usize {
        self.target + self.target.max(self.frames_in(EXCESS))
    }

    pub(super) fn packet(&mut self, frames: usize) {
        self.largest_packet = self.largest_packet.max(frames);
    }

    /// The speakers have just taken what they need; `fill` frames remain.
    pub(super) fn low_point(&mut self, fill: usize) {
        if self.low_count < self.low_points.len() {
            self.low_points[self.low_count] = fill.min(u32::MAX as usize) as u32;
            self.low_count += 1;
        }
    }

    /// Hold a packet in reserve, or not. Holding starts at once.
    #[cfg(any(windows, test))]
    pub(super) fn hold_reserve(&mut self, hold: bool) {
        if hold && !self.reserve {
            self.target = self
                .target
                .max(self.largest_packet + self.margin)
                .min(self.frames_in(MAX_DELAY));
            self.lows = [0; LOOKS_REMEMBERED];
            self.typicals = [0; LOOKS_REMEMBERED];
        }
        self.reserve = hold;
    }

    pub(super) fn ran_dry(&mut self) {
        self.underruns += 1;
        self.target = (self.target + self.frames_in(LONGER)).min(self.frames_in(MAX_DELAY));
        self.margin = (self.margin + self.frames_in(WIDER)).min(self.frames_in(MOST_MARGIN));
        self.calm = 0;
        self.last_mean = None;
        self.lows = [0; LOOKS_REMEMBERED];
        self.typicals = [0; LOOKS_REMEMBERED];
        self.start_window();
    }

    fn start_window(&mut self) {
        self.lowest = usize::MAX;
        self.fills = 0;
        self.takes = 0;
        self.low_count = 0;
        self.window = 0;
    }

    /// `frames` were played while sound streams in, leaving `fill`. Every so
    /// often, move the target.
    pub(super) fn played(&mut self, frames: usize, fill: usize) {
        self.lowest = self.lowest.min(fill);
        self.fills += fill as u64;
        self.takes += 1;
        self.window += frames;
        if self.window < self.frames_in(LOOK_EVERY) {
            return;
        }
        let mean = (self.fills / self.takes) as usize;
        let settled = self
            .last_mean
            .replace(mean)
            .is_some_and(|last| last.abs_diff(mean) <= self.frames_in(SETTLED));
        let lowest = self.lowest;
        // The low point nine in ten stay above.
        let typical = if self.low_count >= 10 {
            let points = &mut self.low_points[..self.low_count];
            *points.select_nth_unstable(points.len() / 10).1 as usize
        } else {
            lowest
        };
        let packet = if self.reserve { self.largest_packet } else { 0 };
        let (margin, reserve) = (self.margin, packet + self.margin);
        // Shorten only on what the last several looks all allow: a dip that
        // comes now and then must not be forgotten between two looks.
        self.lows.rotate_right(1);
        self.lows[0] = lowest;
        self.typicals.rotate_right(1);
        self.typicals[0] = typical;
        let lowest_lately = self.lows.iter().copied().min().unwrap_or(lowest);
        let typical_lately = self.typicals.iter().copied().min().unwrap_or(typical);
        let short = (margin.saturating_sub(lowest)).max(reserve.saturating_sub(typical));
        if short > margin / 2 {
            self.target = (self.target + short).min(self.frames_in(MAX_DELAY));
        } else if settled && lowest_lately > margin && typical_lately > reserve {
            let spare = (lowest_lately - margin).min(typical_lately - reserve);
            let shorter = (spare / 2).min(self.frames_in(MOST_SHORTER));
            self.target = self
                .target
                .saturating_sub(shorter)
                .max(self.frames_in(MIN_DELAY));
        }
        self.calm += 1;
        if self.calm >= NARROW_AFTER {
            self.calm = 0;
            self.margin = self
                .margin
                .saturating_sub(self.frames_in(Duration::from_millis(1)))
                .max(self.least_margin);
        }
        self.start_window();
    }
}

/// Leans the playback rate toward holding the buffer at its target.
#[derive(Default)]
pub(super) struct Steering {
    /// Smoothed fill, in frames, since sound last started.
    pub(super) average: Option<f64>,
}

impl Steering {
    /// The relative rate to play at, given the buffer's `fill` after `frames`
    /// more were played, at `rate`. Fuller than the target: take input
    /// faster, a ratio below one.
    pub(super) fn ratio(&mut self, fill: usize, target: usize, frames: usize, rate: u32) -> f64 {
        let rate = f64::from(rate.max(1));
        let weight = (frames as f64 / (SMOOTHING * rate)).min(1.0);
        let fill = fill as f64;
        let average = self
            .average
            .map_or(fill, |average| average + (fill - average) * weight);
        self.average = Some(average);
        let error = (average - target as f64) / rate;
        1.0 - (GAIN * error).clamp(-MOST_CORRECTION, MOST_CORRECTION)
    }
}

/// Converts the sender's rate to the speakers', with the ratio adjustable for
/// clock drift. Equal rates only slip a crossfaded frame now and then.
pub(super) fn converter(
    source_rate: u32,
    output_rate: u32,
    chunk: usize,
    fixed: FixedAsync,
) -> Result<Box<dyn Adjustable<f32>>> {
    Ok(if source_rate == output_rate {
        Box::new(Slip::new(chunk, 2, fixed)?)
    } else {
        Box::new(Async::new_sinc(
            f64::from(output_rate) / f64::from(source_rate),
            1.0 + 2.0 * MOST_CORRECTION,
            &SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2),
            chunk,
            2,
            fixed,
        )?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_dry_buffers_longer_and_keeps_a_wider_margin() {
        let mut pacing = Pacing::new(48_000);
        let (target, margin) = (pacing.target, pacing.margin);
        pacing.ran_dry();
        assert_eq!(pacing.target, target + 480);
        assert_eq!(pacing.margin, margin + 144);
        for _ in 0..100 {
            pacing.ran_dry();
        }
        assert_eq!(pacing.delay(), MAX_DELAY);
        assert_eq!(pacing.margin, pacing.frames_in(MOST_MARGIN));
    }

    #[test]
    fn speakers_that_tell_their_takes_keep_a_narrower_margin() {
        let mut pacing = Pacing::new(48_000);
        pacing.takes_known();
        assert_eq!(pacing.margin, 48);
        pacing.ran_dry();
        pacing.takes_known();
        assert_eq!(pacing.margin, 48 + 144, "a widened margin stays wide");
        let mut pacing = Pacing::new(48_000);
        pacing.ran_dry();
        pacing.takes_known();
        assert_eq!(
            pacing.margin,
            192 + 144,
            "already widened before the takes were known"
        );
    }

    #[test]
    fn steering_leans_toward_the_target_within_bounds() {
        let mut steering = Steering::default();
        assert!(steering.ratio(4_800, 960, 480, 48_000) < 1.0);
        let mut steering = Steering::default();
        assert!(steering.ratio(0, 960, 480, 48_000) > 1.0);
        let mut steering = Steering::default();
        let ratio = steering.ratio(48_000 * 10, 0, 480, 48_000);
        assert!((ratio - (1.0 - MOST_CORRECTION)).abs() < 1e-12);
    }
}
