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
//! network that is unsteady settles on a delay that suits it.
//!
//! Speakers that tell when they take sound take a whole period at a time, so
//! there protection comes in whole periods: holding part of a period more
//! protects no better than holding none. How many each packet needs is
//! measured from how late packets come (see `push`), and each packet's own
//! fill, less the periods it needs, must keep the margin, which then only
//! absorbs the leaning of the rate. Running dry there adds the period that
//! was missing, not a fixed 10 ms and a wider margin.
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
/// The margin when the speakers tell when they take sound. They take a whole
/// period at a time, so a late packet needs whole periods held for it; the
/// margin only absorbs the leaning of the rate, and running dry leaves it as
/// it is: a wider margin would protect no better.
#[cfg(any(windows, test))]
const TIMED_MARGIN: Duration = Duration::from_millis(2);
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
/// Seconds the steering takes to bring the buffer most of the way to a new
/// target: two of its time constants (`1 / GAIN`, plus the smoothing), which
/// close 86 % of the distance, the margin covering the rest.
#[cfg(any(windows, test))]
pub(super) const SETTLE: f64 = 2.0 * (1.0 / GAIN + SMOOTHING);
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
    /// The narrowest the margin gets, how much running dry widens it, and the
    /// looks without running dry before it narrows by 1 ms.
    least_margin: usize,
    wider: usize,
    narrow_after: u32,
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
    /// The frames the speakers take at a time, when they tell.
    period: Option<usize>,
    /// Where they tell: since the last look, the least any packet's fill
    /// held beyond the periods it needs, and that least for each recent
    /// look, newest first.
    cover: Option<isize>,
    covers: [isize; LOOKS_REMEMBERED],
    /// The most periods any packet needed this look and in each recent
    /// look: they stay held until no packet of the recent looks needs them.
    needed: [usize; LOOKS_REMEMBERED],
    /// Frames played since the last look.
    window: usize,
    /// Looks since the buffer last ran dry.
    calm: u32,
    pub(super) underruns: u64,
}

impl Pacing {
    pub(super) fn new(rate: u32) -> Self {
        let mut pacing = Self {
            rate: rate.max(1),
            target: 0,
            margin: 0,
            least_margin: 0,
            wider: 0,
            narrow_after: NARROW_AFTER,
            lowest: usize::MAX,
            fills: 0,
            takes: 0,
            last_mean: None,
            lows: [0; LOOKS_REMEMBERED],
            low_points: [0; LOWS_PER_LOOK],
            low_count: 0,
            typicals: [0; LOOKS_REMEMBERED],
            largest_packet: 0,
            period: None,
            cover: None,
            covers: [0; LOOKS_REMEMBERED],
            needed: [0; LOOKS_REMEMBERED],
            window: 0,
            calm: 0,
            underruns: 0,
        };
        pacing.target = pacing.frames_in(START_DELAY);
        pacing.margin = pacing.frames_in(MARGIN);
        pacing.least_margin = pacing.margin;
        pacing.wider = pacing.frames_in(WIDER);
        pacing
    }

    /// The speakers tell when they take sound, `period` frames at a time:
    /// keep the narrower margin that allows (see `TIMED_MARGIN`), and start
    /// from one period held, however much arrives at once. A margin already
    /// widened by running dry stays as wide.
    #[cfg(any(windows, test))]
    pub(super) fn takes_known(&mut self, period: usize) {
        self.period = Some(period.max(1));
        let timed = self.frames_in(TIMED_MARGIN);
        if self.least_margin != timed {
            if self.margin == self.least_margin {
                self.margin = timed;
            }
            self.least_margin = timed;
            // Start from a period held and the margin, not the general
            // starting delay; the target grows at once if short.
            self.target = self
                .target
                .min(self.held() + 2 * timed)
                .max(self.frames_in(MIN_DELAY));
            self.needed = [1; LOOKS_REMEMBERED];
            self.wider = 0;
        }
    }

    /// The periods held: the most any packet of the recent looks needed.
    fn periods_held(&self) -> usize {
        self.needed.iter().copied().max().unwrap_or(0)
    }

    /// Where the speakers tell when they take sound: a packet arrived with
    /// `fill` frames buffered had it come when packets usually do. It needs
    /// `up` periods held for it to come as late as packets have come lately,
    /// with the guard; `down`, with half the guard again. More periods than
    /// held are held at once; held ones stay while `down` still asks for
    /// them, and are let go by the looks.
    #[cfg(any(windows, test))]
    pub(super) fn need(&mut self, up: usize, down: usize, fill: isize) {
        let Some(period) = self.period else {
            return;
        };
        let held = self.periods_held();
        let periods = up.max(down.min(held));
        let cover = fill - (periods * period) as isize;
        if periods > held {
            // The target must hold the periods and the margin; one that
            // already does, as when a held period was let go only lately,
            // needs nothing more, and growing it anyway would leave it longer
            // each time packets drift past a take.
            self.target = self
                .target
                .max(periods * period + self.margin)
                .min(self.frames_in(MAX_DELAY));
            self.forget_lows();
        }
        self.needed[0] = self.needed[0].max(periods);
        self.cover = Some(self.cover.map_or(cover, |least| least.min(cover)));
    }

    /// Low points measured against an older target no longer tell how far
    /// the target may shrink.
    fn forget_lows(&mut self) {
        self.lows = [0; LOOKS_REMEMBERED];
        self.typicals = [0; LOOKS_REMEMBERED];
        self.covers = [0; LOOKS_REMEMBERED];
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

    /// The frames of a packet in reserve: a period where the speakers tell
    /// it, else the most that has arrived at once.
    fn held(&self) -> usize {
        self.period.unwrap_or(self.largest_packet)
    }

    /// The speakers have just taken what they need; `fill` frames remain.
    pub(super) fn low_point(&mut self, fill: usize) {
        if self.low_count < self.low_points.len() {
            self.low_points[self.low_count] = fill.min(u32::MAX as usize) as u32;
            self.low_count += 1;
        }
    }

    /// The speakers no longer tell when they take sound: back to the general
    /// rules at once, with a packet in reserve and the general margin, as the
    /// periods held for measured lateness no longer stand for anything.
    #[cfg(any(windows, test))]
    pub(super) fn takes_unknown(&mut self) {
        if self.period.take().is_none() {
            return;
        }
        let general = self.frames_in(MARGIN);
        self.margin = self.margin.max(general);
        self.least_margin = general;
        self.wider = self.frames_in(WIDER);
        self.needed = [0; LOOKS_REMEMBERED];
        self.cover = None;
        self.target = self
            .target
            .max(self.held() + self.margin)
            .min(self.frames_in(MAX_DELAY));
        self.forget_lows();
    }

    /// The buffer ran dry. Where the speakers tell when they take sound, a
    /// packet came later than the periods held for it, so one period more is
    /// held for the looks remembered (10 s), one more for each further gap
    /// within them; longer only if `push` sees a second gap within 5 minutes.
    pub(super) fn ran_dry(&mut self) {
        self.underruns += 1;
        let longer = match self.period {
            Some(period) => {
                self.needed[0] = self.periods_held() + 1;
                period
            }
            None => self.frames_in(LONGER),
        };
        self.target = (self.target + longer).min(self.frames_in(MAX_DELAY));
        self.margin = (self.margin + self.wider).min(self.frames_in(MOST_MARGIN));
        self.calm = 0;
        self.last_mean = None;
        self.forget_lows();
        self.start_window();
    }

    fn start_window(&mut self) {
        self.lowest = usize::MAX;
        self.fills = 0;
        self.takes = 0;
        self.low_count = 0;
        self.window = 0;
        self.cover = None;
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
        let last = self.last_mean.replace(mean);
        let settled = last.is_some_and(|last| last.abs_diff(mean) <= self.frames_in(SETTLED));
        let lowest = self.lowest;
        if let (Some(period), Some(cover)) = (self.period, self.cover) {
            self.timed_look(settled, last, mean, lowest, cover, period);
            return;
        }
        // The low point nine in ten stay above.
        let typical = if self.low_count >= 10 {
            let points = &mut self.low_points[..self.low_count];
            *points.select_nth_unstable(points.len() / 10).1 as usize
        } else {
            lowest
        };
        let (margin, reserve) = (self.margin, self.held() + self.margin);
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
        self.end_look();
    }

    /// A look where the speakers tell when they take sound. Every packet's
    /// fill must keep the margin beyond the periods it needs (`cover`), and
    /// the very lowest fill must keep the margin too. Short of either, the
    /// target grows at once; with room to spare in every recent look, and
    /// the buffer settled, it shrinks a step.
    ///
    /// While the buffer is still on its way to the target, its mean moving
    /// toward it since the last look (`last`), both are judged with it there.
    /// Falling from above, as just after the target shrank, it has room it is
    /// about to lose, so what it runs above counts against both. Rising from
    /// below, as just after more periods are held or a held one was spent as
    /// the packets drifted past a take, the cover it lacks only for that is
    /// already asked for, and asking again each look would leave the target
    /// periods too long once it arrives: so what it runs below counts toward
    /// the cover, but never more than the periods held, and not toward the
    /// lowest fill. A packet that comes late meanwhile runs the buffer dry at
    /// worst, which tops it up to the whole target at once and holds a period
    /// more. A buffer that stays off the target, as the leaning of the rate
    /// keeps it against the clocks' drift, is judged where it runs, so the
    /// target follows it.
    fn timed_look(
        &mut self,
        settled: bool,
        last: Option<usize>,
        mean: usize,
        lowest: usize,
        cover: isize,
        period: usize,
    ) {
        let margin = self.margin as isize;
        let falling = last.is_some_and(|last| last > mean);
        let rising = last.is_some_and(|last| last < mean);
        let ahead = if falling {
            mean.saturating_sub(self.target) as isize
        } else {
            0
        };
        let behind = if rising {
            self.target
                .saturating_sub(mean)
                .min(self.periods_held() * period) as isize
        } else {
            0
        };
        let lowest = (lowest as isize - ahead).max(0);
        let cover = cover - ahead + behind;
        self.lows.rotate_right(1);
        self.lows[0] = lowest as usize;
        self.covers.rotate_right(1);
        self.covers[0] = cover;
        let lowest_lately = self.lows.iter().copied().min().unwrap_or(0) as isize;
        let cover_lately = self.covers.iter().copied().min().unwrap_or(cover);
        let short = (margin - lowest).max(margin - cover);
        if short > margin / 2 {
            self.target = (self.target + short as usize).min(self.frames_in(MAX_DELAY));
        } else if settled && lowest_lately > margin && cover_lately > margin {
            let spare = (lowest_lately - margin).min(cover_lately - margin) as usize;
            let shorter = (spare / 2).min(self.frames_in(MOST_SHORTER));
            self.target = self
                .target
                .saturating_sub(shorter)
                .max(self.frames_in(MIN_DELAY));
        }
        self.needed.rotate_right(1);
        self.needed[0] = 0;
        self.end_look();
    }

    fn end_look(&mut self) {
        self.calm += 1;
        if self.calm >= self.narrow_after {
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
        pacing.takes_known(480);
        assert_eq!(pacing.margin, 96);
        let target = pacing.target;
        pacing.ran_dry();
        pacing.takes_known(480);
        assert_eq!(pacing.margin, 96, "running dry leaves the margin");
        assert_eq!(pacing.target, target + 480, "and adds the missing period");
        assert_eq!(pacing.periods_held(), 2);
        let mut pacing = Pacing::new(48_000);
        pacing.ran_dry();
        pacing.takes_known(480);
        assert_eq!(
            pacing.margin,
            192 + 144,
            "already widened before the takes were known"
        );
    }

    /// End a look in which every packet needed `periods` and had `fill`.
    fn look(pacing: &mut Pacing, periods: usize, fill: usize) {
        pacing.need(periods, periods, fill as isize);
        pacing.window = pacing.frames_in(LOOK_EVERY);
        pacing.played(0, fill);
    }

    #[test]
    fn a_gap_holds_a_period_more_for_a_few_looks() {
        let mut pacing = Pacing::new(48_000);
        pacing.takes_known(480);
        for _ in 0..LOOKS_REMEMBERED {
            look(&mut pacing, 0, 2_000);
        }
        assert_eq!(pacing.periods_held(), 0);
        pacing.ran_dry();
        assert_eq!(pacing.periods_held(), 1);
        for _ in 0..LOOKS_REMEMBERED - 1 {
            look(&mut pacing, 0, 2_000);
            assert_eq!(pacing.periods_held(), 1);
        }
        look(&mut pacing, 0, 2_000);
        assert_eq!(pacing.periods_held(), 0, "let go 10 s later");
    }

    #[test]
    fn speakers_that_stop_telling_hold_a_packet_again_at_once() {
        let mut pacing = Pacing::new(48_000);
        pacing.packet(480);
        pacing.takes_known(480);
        // Packets well clear of the takes: no period held.
        pacing.target = 200;
        pacing.needed = [0; LOOKS_REMEMBERED];
        pacing.takes_unknown();
        assert_eq!(pacing.margin, pacing.frames_in(MARGIN));
        assert!(pacing.target >= 480 + pacing.frames_in(MARGIN));
        assert_eq!(pacing.periods_held(), 0);
        // And they may tell again later.
        pacing.takes_known(480);
        assert_eq!(pacing.margin, pacing.frames_in(TIMED_MARGIN));
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
