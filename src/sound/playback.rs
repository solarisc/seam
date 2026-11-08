//! Plays a controlled computer's sound on this computer.
//!
//! Samples wait in a short buffer that rides out the network's timing. The
//! speakers' callback takes them out through a converter that matches the two
//! sample rates and leans on the playback rate, by an inaudible fraction of a
//! percent, to hold the buffer at its target. That follows the drift between
//! the two computers' audio clocks, so the delay neither creeps up nor runs dry
//! over a long film.
use super::dsp;
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{
    Adjustable, Async, FixedAsync, SincInterpolationParameters, Slip, WindowFunction,
    audioadapter_buffers::direct::InterleavedSlice,
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Frames converted at a time: under 3 ms at 48 kHz.
const CHUNK: usize = 128;
/// Delay buffered against the network's timing at first, and the bounds it
/// adapts between: longer each time the buffer runs dry, shorter again after a
/// while without. Each time it runs dry, the wait before shortening doubles,
/// so an unsteady network settles on a delay that suits it instead of running
/// dry again every so often.
const START_DELAY: Duration = Duration::from_millis(25);
const MIN_DELAY: Duration = Duration::from_millis(15);
const MAX_DELAY: Duration = Duration::from_millis(250);
const LONGER: Duration = Duration::from_millis(10);
const SHORTER: Duration = Duration::from_millis(5);
const SHORTEN_AFTER: Duration = Duration::from_secs(30);
const MOST_SHORTEN_AFTER: Duration = Duration::from_secs(600);
/// Buffered this far beyond the target, as when the network held packets back
/// and then delivered them together, the oldest are dropped: one skip instead
/// of playing late from then on.
const EXCESS: Duration = Duration::from_millis(40);
/// Most held, beyond the target, while the speakers haven't started.
const WAITING: Duration = Duration::from_millis(500);
/// Playback rate correction per second of distance from the target. At 0.1 a
/// clock 100 ppm off settles 1 ms from the target.
const GAIN: f64 = 0.1;
/// The furthest playback leans from its nominal rate. 0.5 % is inaudible.
const MOST_CORRECTION: f64 = 0.005;
/// The buffer's fill is averaged over about this long before it steers the
/// rate, so packets arriving in bursts don't waver the pitch.
const SMOOTHING: f64 = 1.0;
/// The speakers close after this long with nothing to play, and open again
/// with the next sound.
const IDLE: Duration = Duration::from_secs(30);
/// How often an open output checks that it still plays on the default device.
const DEVICE_CHECK: Duration = Duration::from_secs(1);
const REOPEN: Duration = Duration::from_secs(5);

/// Samples received and not yet played, shared with the speakers' callback.
pub(super) struct Jitter {
    /// Interleaved stereo at `rate`.
    samples: VecDeque<f32>,
    rate: u32,
    /// Frames gathered before playing starts: the delay that rides out the
    /// network's timing.
    target: usize,
    playing: bool,
    /// The sender stopped because nothing plays. What remains is the end of
    /// the sound, not a gap.
    paused: bool,
    /// Frames played since the target last changed.
    steady: usize,
    /// Frames to play without running dry before the target shortens.
    shorten_after: usize,
    pub(super) underruns: u64,
}

impl Jitter {
    pub(super) fn new(rate: u32) -> Self {
        let mut jitter = Self {
            samples: VecDeque::new(),
            rate: rate.max(1),
            target: 0,
            playing: false,
            paused: false,
            steady: 0,
            shorten_after: 0,
            underruns: 0,
        };
        jitter.target = jitter.frames_in(START_DELAY);
        jitter.shorten_after = jitter.frames_in(SHORTEN_AFTER);
        jitter
    }

    fn frames_in(&self, duration: Duration) -> usize {
        (duration.as_secs_f64() * f64::from(self.rate)).round() as usize
    }

    fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// The delay currently kept against the network's timing.
    pub(super) fn delay(&self) -> Duration {
        Duration::from_secs_f64(self.target as f64 / f64::from(self.rate))
    }

    pub(super) fn push(&mut self, samples: &[i16]) {
        self.paused = false;
        dsp::to_float(&samples[..samples.len() / 2 * 2], &mut self.samples);
        let (limit, keep) = if self.playing {
            (
                self.target + self.target.max(self.frames_in(EXCESS)),
                self.target,
            )
        } else {
            // Waiting for the speakers to start, which may first ask for
            // their whole buffer: keep the start of the sound.
            let limit = self.target + self.frames_in(WAITING);
            (limit, limit)
        };
        if self.frames() > limit {
            let drop = (self.frames() - keep) * 2;
            self.samples.drain(..drop);
        }
    }

    pub(super) fn pause(&mut self) {
        self.paused = true;
    }

    /// Something is playing or waiting to.
    pub(super) fn active(&self) -> bool {
        self.playing || !self.samples.is_empty()
    }

    /// Enough has gathered to start playing, with `wanted` frames asked for
    /// at once. Speakers ask for their whole buffer when they start, and the
    /// target is the margin kept beyond that.
    fn ready(&self, wanted: usize) -> bool {
        self.frames() >= self.target + wanted || (self.paused && !self.samples.is_empty())
    }

    fn ran_dry(&mut self) {
        self.playing = false;
        self.underruns += 1;
        self.steady = 0;
        self.target = (self.target + self.frames_in(LONGER)).min(self.frames_in(MAX_DELAY));
        self.shorten_after = (self.shorten_after * 2).min(self.frames_in(MOST_SHORTEN_AFTER));
    }

    fn played(&mut self, frames: usize) {
        self.steady += frames;
        if self.steady >= self.shorten_after {
            self.steady = 0;
            self.target = self
                .target
                .saturating_sub(self.frames_in(SHORTER))
                .max(self.frames_in(MIN_DELAY));
        }
    }
}

/// Runs in the speakers' callback: takes samples from the buffer, converts
/// them and writes them out, steering the rate to hold the buffer's delay.
pub(super) struct Renderer {
    jitter: Arc<Mutex<Jitter>>,
    converter: Box<dyn Adjustable<f32>>,
    channels: usize,
    source_rate: f64,
    source_per_output: f64,
    input: Vec<f32>,
    /// One converted chunk, interleaved stereo.
    output: Vec<f32>,
    /// Frames of `output` already written out.
    position: usize,
    /// Smoothed buffer fill in frames, from when playing started.
    average: Option<f64>,
    /// Weight of each new fill measurement in the average.
    weight: f64,
}

impl Renderer {
    pub(super) fn new(
        jitter: Arc<Mutex<Jitter>>,
        source_rate: u32,
        output_rate: u32,
        channels: usize,
    ) -> Result<Self> {
        let converter: Box<dyn Adjustable<f32>> = if source_rate == output_rate {
            // Only the clocks differ: slip a frame now and then, crossfaded.
            Box::new(Slip::new(CHUNK, 2, FixedAsync::Output)?)
        } else {
            Box::new(Async::new_sinc(
                f64::from(output_rate) / f64::from(source_rate),
                1.0 + 2.0 * MOST_CORRECTION,
                &SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2),
                CHUNK,
                2,
                FixedAsync::Output,
            )?)
        };
        Ok(Self {
            input: vec![0.0; converter.input_frames_max() * 2],
            output: vec![0.0; CHUNK * 2],
            position: CHUNK,
            converter,
            jitter,
            channels: channels.max(1),
            source_rate: f64::from(source_rate),
            source_per_output: f64::from(source_rate) / f64::from(output_rate),
            average: None,
            weight: CHUNK as f64 / (SMOOTHING * f64::from(output_rate)),
        })
    }

    /// Fill one device buffer of interleaved frames.
    pub(super) fn render(&mut self, data: &mut [f32]) {
        let channels = self.channels;
        let total = data.len() / channels;
        let mut written = 0;
        while written < total {
            let wanted = ((total - written) as f64 * self.source_per_output).ceil() as usize;
            if self.position == CHUNK && !self.convert(wanted) {
                break;
            }
            let count = (total - written).min(CHUNK - self.position);
            let frames = data[written * channels..(written + count) * channels]
                .chunks_exact_mut(channels)
                .zip(self.output[self.position * 2..].as_chunks::<2>().0);
            for (frame, &[left, right]) in frames {
                match frame {
                    [mono] => *mono = (left + right) * 0.5,
                    [first, second, rest @ ..] => {
                        *first = left;
                        *second = right;
                        rest.fill(0.0);
                    }
                    [] => {}
                }
            }
            written += count;
            self.position += count;
        }
        data[written * channels..].fill(0.0);
    }

    /// Convert the next chunk, with `wanted` source frames still asked for in
    /// this callback. `false` when there is nothing to play.
    fn convert(&mut self, wanted: usize) -> bool {
        let needed = self.converter.input_frames_next();
        let (taken, fill, target, paused) = {
            let mut jitter = self.jitter.lock().unwrap_or_else(PoisonError::into_inner);
            if !jitter.playing {
                if !jitter.ready(wanted) {
                    return false;
                }
                jitter.playing = true;
                self.average = None;
                self.converter.reset();
            }
            let available = jitter.frames();
            if available < needed && !jitter.paused {
                jitter.ran_dry();
                return false;
            }
            let taken = available.min(needed);
            for (slot, sample) in self.input.iter_mut().zip(jitter.samples.drain(..taken * 2)) {
                *slot = sample;
            }
            if taken < needed {
                // The end of a sound: finish it, then wait for the next one.
                jitter.playing = false;
            }
            jitter.played(taken);
            (taken, jitter.frames(), jitter.target, jitter.paused)
        };
        self.input[taken * 2..needed * 2].fill(0.0);
        let converted = InterleavedSlice::new(&self.input[..needed * 2], 2, needed)
            .ok()
            .zip(InterleavedSlice::new_mut(&mut self.output, 2, CHUNK).ok())
            .is_some_and(|(input, mut output)| {
                self.converter
                    .process_into_buffer(&input, &mut output, None)
                    .is_ok()
            });
        if !converted {
            self.output.fill(0.0);
        }
        self.position = 0;
        // The end of a sound drains the buffer on purpose.
        if !paused {
            self.steer(fill as f64, target as f64);
        }
        true
    }

    /// Lean the playback rate toward holding the buffer at its target.
    fn steer(&mut self, fill: f64, target: f64) {
        let average = self
            .average
            .map_or(fill, |average| average + (fill - average) * self.weight);
        self.average = Some(average);
        let error = (average - target) / self.source_rate;
        let correction = (GAIN * error).clamp(-MOST_CORRECTION, MOST_CORRECTION);
        // Fuller than the target: take input faster, a ratio below one.
        let _ = self
            .converter
            .set_resample_ratio_relative(1.0 - correction, true);
    }
}

/// The speakers, playing one controlled computer's sound.
struct Output {
    _stream: cpal::Stream,
    device: cpal::Device,
    failed: Arc<AtomicBool>,
}

impl Output {
    fn open(host: &cpal::Host, jitter: &Arc<Mutex<Jitter>>, source_rate: u32) -> Result<Self> {
        let device = host
            .default_output_device()
            .context("no speakers or headphones are connected")?;
        let supported = device
            .default_output_config()
            .context("reading the speakers' format")?;
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut renderer = Renderer::new(
            jitter.clone(),
            source_rate,
            config.sample_rate,
            usize::from(config.channels),
        )?;
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| renderer.render(data),
                move |error: cpal::Error| {
                    // A late callback is a click, not a broken device.
                    if error.kind() != cpal::ErrorKind::Xrun {
                        flag.store(true, Ordering::Release);
                    }
                },
                None,
            )
            .context("opening the speakers")?;
        stream.play().context("starting the speakers")?;
        log::info!(
            "sound playing on {device} at {} Hz, {} channel(s), {} frames a callback",
            config.sample_rate,
            config.channels,
            stream
                .buffer_size()
                .map_or_else(|_| "unknown".into(), |frames| frames.to_string())
        );
        Ok(Self {
            _stream: stream,
            device,
            failed,
        })
    }

    /// The device stopped working, or another one became the default.
    fn stale(&self, host: &cpal::Host) -> bool {
        self.failed.load(Ordering::Acquire)
            || host
                .default_output_device()
                .is_some_and(|device| device != self.device)
    }
}

/// One controlled computer's sound on this computer: its buffer, and the
/// speakers while there is something to play.
pub(super) struct Player {
    host: cpal::Host,
    jitter: Arc<Mutex<Jitter>>,
    /// The sender's sample rate, once it said.
    rate: Option<u32>,
    output: Option<Output>,
    /// When sound last arrived.
    heard: Option<Instant>,
    /// When the speakers last had sound to play, or opened.
    used: Instant,
    checked: Instant,
    /// The speakers failed to open; try again then.
    retry: Option<Instant>,
    /// Why the speakers can't play, for the window.
    pub(super) problem: Option<String>,
    underruns: u64,
}

impl Player {
    pub(super) fn new() -> Self {
        Self {
            host: cpal::default_host(),
            jitter: Arc::new(Mutex::new(Jitter::new(48_000))),
            rate: None,
            output: None,
            heard: None,
            used: Instant::now(),
            checked: Instant::now(),
            retry: None,
            problem: None,
            underruns: 0,
        }
    }

    fn jitter(&self) -> std::sync::MutexGuard<'_, Jitter> {
        self.jitter.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sound follows at `rate`. The speakers open now, so the first sound
    /// isn't held up by opening them.
    pub(super) fn format(&mut self, rate: u32) {
        if self.rate != Some(rate) {
            self.output = None;
            *self.jitter() = Jitter::new(rate);
            self.rate = Some(rate);
        }
        self.retry = None;
        self.open(Instant::now());
    }

    pub(super) fn samples(&mut self, samples: &[i16]) {
        let now = Instant::now();
        self.heard = Some(now);
        self.used = now;
        self.jitter().push(samples);
        if self.output.is_none() {
            self.open(now);
        }
    }

    pub(super) fn pause(&mut self) {
        self.jitter().pause();
    }

    /// Sound arrived within the last moment, or is still playing out.
    pub(super) fn playing(&self) -> bool {
        self.heard
            .is_some_and(|heard| heard.elapsed() < Duration::from_millis(300))
            || (self.output.is_some() && self.jitter().active())
    }

    /// Close idle speakers, and follow a change of speakers.
    pub(super) fn tick(&mut self, name: &str) {
        let now = Instant::now();
        let (active, underruns, delay) = {
            let jitter = self.jitter();
            (jitter.active(), jitter.underruns, jitter.delay())
        };
        if underruns > self.underruns {
            log::info!(
                "sound from {name} ran short; buffering {} ms",
                delay.as_millis()
            );
            self.underruns = underruns;
        }
        let Some(output) = &self.output else {
            return;
        };
        if active {
            self.used = now;
        }
        if now - self.used >= IDLE {
            log::debug!("sound from {name} is quiet; closing the speakers");
            self.output = None;
        } else if now - self.checked >= DEVICE_CHECK {
            self.checked = now;
            if output.stale(&self.host) {
                log::info!("the speakers changed; following them");
                self.output = None;
                self.open(now);
            }
        }
    }

    fn open(&mut self, now: Instant) {
        let Some(rate) = self.rate else {
            return;
        };
        if self.output.is_some() || self.retry.is_some_and(|retry| now < retry) {
            return;
        }
        match Output::open(&self.host, &self.jitter, rate) {
            Ok(output) => {
                self.output = Some(output);
                self.used = now;
                self.checked = now;
                self.retry = None;
                self.problem = None;
            }
            Err(error) => {
                if self.problem.is_none() {
                    log::warn!("cannot play sound here: {error:#}");
                }
                self.problem = Some(format!("Can't play sound on this computer: {error}"));
                self.retry = Some(now + REOPEN);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine tone at `frequency`, as the sender would put on the wire.
    fn tone(rate: u32, frequency: f64, start: usize, frames: usize) -> Vec<i16> {
        (start..start + frames)
            .flat_map(|index| {
                let value = (index as f64 * frequency * std::f64::consts::TAU / f64::from(rate))
                    .sin()
                    * 0.5
                    * 32_767.0;
                [value as i16, value as i16]
            })
            .collect()
    }

    struct Simulation {
        jitter: Arc<Mutex<Jitter>>,
        renderer: Renderer,
        source_rate: u32,
        output_rate: u32,
        /// How fast the sender's clock runs against nominal.
        source_clock: f64,
        /// Milliseconds simulated so far.
        now: usize,
        sent: usize,
        rendered: Vec<f32>,
    }

    impl Simulation {
        fn new(source_rate: u32, output_rate: u32, source_clock: f64) -> Self {
            let jitter = Arc::new(Mutex::new(Jitter::new(source_rate)));
            Self {
                renderer: Renderer::new(jitter.clone(), source_rate, output_rate, 2).unwrap(),
                jitter,
                source_rate,
                output_rate,
                source_clock,
                now: 0,
                sent: 0,
                rendered: Vec::new(),
            }
        }

        /// Run for `seconds`: the sender delivers 10 ms packets on its own
        /// clock, the speakers ask for 10 ms buffers on theirs.
        fn run(&mut self, seconds: f64, keep: bool) {
            let end = self.now + (seconds * 1000.0) as usize;
            let mut buffer = vec![0.0f32; self.output_rate as usize / 100 * 2];
            while self.now < end {
                let step = self.now;
                self.now += 1;
                let due = ((step + 1) as f64 / 1000.0
                    * f64::from(self.source_rate)
                    * self.source_clock) as usize;
                if due >= self.sent + self.source_rate as usize / 100 {
                    let frames = due - self.sent;
                    let packet = tone(self.source_rate, 1000.0, self.sent, frames);
                    self.jitter.lock().unwrap().push(&packet);
                    self.sent = due;
                }
                if step % 10 == 9 {
                    self.renderer.render(&mut buffer);
                    if keep {
                        self.rendered.extend_from_slice(&buffer);
                    }
                }
            }
        }

        fn delay_ms(&self) -> f64 {
            let jitter = self.jitter.lock().unwrap();
            jitter.frames() as f64 * 1000.0 / f64::from(self.source_rate)
        }
    }

    /// Frequency of the left channel, from its rising zero crossings.
    fn frequency(samples: &[f32], rate: u32) -> f64 {
        let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let crossings: Vec<usize> = (1..left.len())
            .filter(|&index| left[index - 1] < 0.0 && left[index] >= 0.0)
            .collect();
        let span = (crossings[crossings.len() - 1] - crossings[0]) as f64;
        (crossings.len() - 1) as f64 * f64::from(rate) / span
    }

    #[test]
    fn clock_drift_is_followed_without_running_dry() {
        for clock in [1.0003, 0.9997] {
            let mut simulation = Simulation::new(48_000, 48_000, clock);
            simulation.run(60.0, false);
            let underruns = simulation.jitter.lock().unwrap().underruns;
            simulation.run(240.0, false);
            let jitter = simulation.jitter.lock().unwrap();
            assert_eq!(
                jitter.underruns, underruns,
                "ran dry after settling at clock {clock}"
            );
            let target = jitter.delay().as_secs_f64() * 1000.0;
            drop(jitter);
            let delay = simulation.delay_ms();
            // Within a packet and a speaker buffer of the target.
            assert!(
                (delay - target).abs() < 15.0,
                "delay {delay} ms against {target} ms at clock {clock}"
            );
        }
    }

    #[test]
    fn a_different_sample_rate_keeps_the_pitch() {
        let mut simulation = Simulation::new(44_100, 48_000, 1.0);
        simulation.run(2.0, false);
        simulation.run(2.0, true);
        let measured = frequency(&simulation.rendered, 48_000);
        assert!((measured - 1000.0).abs() < 1.0, "{measured} Hz");
        let peak = simulation
            .rendered
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!((peak - 0.5).abs() < 0.02, "peak {peak}");
    }

    #[test]
    fn the_same_rate_plays_the_samples_unchanged() {
        let jitter = Arc::new(Mutex::new(Jitter::new(48_000)));
        let mut renderer = Renderer::new(jitter.clone(), 48_000, 48_000, 2).unwrap();
        let sent = tone(48_000, 440.0, 0, 2_400);
        jitter.lock().unwrap().push(&sent);
        jitter.lock().unwrap().pause();
        let mut played = vec![0.0f32; 2_400 * 2];
        renderer.render(&mut played);
        for (sample, original) in played.iter().zip(&sent) {
            assert!((sample - f32::from(*original) / 32_768.0).abs() < 1e-6);
        }
    }

    #[test]
    fn the_margin_is_kept_beyond_the_speakers_first_request() {
        let jitter = Arc::new(Mutex::new(Jitter::new(48_000)));
        let mut renderer = Renderer::new(jitter.clone(), 48_000, 48_000, 2).unwrap();
        let target = jitter.lock().unwrap().target;
        // Speakers that start by asking for 22 ms at once.
        let mut first = vec![0.0f32; 1_056 * 2];
        let mut sent = 0;
        while sent < target + 1_056 {
            renderer.render(&mut first);
            assert!(first.iter().all(|&sample| sample == 0.0), "started early");
            jitter
                .lock()
                .unwrap()
                .push(&tone(48_000, 1000.0, sent, 480));
            sent += 480;
        }
        renderer.render(&mut first);
        assert!(first.iter().any(|&sample| sample != 0.0));
        assert!(jitter.lock().unwrap().frames() >= target - CHUNK);
    }

    #[test]
    fn a_sound_shorter_than_the_delay_still_plays_once_the_sender_pauses() {
        let jitter = Arc::new(Mutex::new(Jitter::new(48_000)));
        let mut renderer = Renderer::new(jitter.clone(), 48_000, 48_000, 1).unwrap();
        // 5 ms, less than the delay gathered before playing.
        jitter.lock().unwrap().push(&tone(48_000, 1000.0, 0, 240));
        let mut buffer = vec![0.0f32; 480];
        renderer.render(&mut buffer);
        assert!(buffer.iter().all(|&sample| sample == 0.0), "waits for more");
        jitter.lock().unwrap().pause();
        renderer.render(&mut buffer);
        let energy: f32 = buffer.iter().map(|sample| sample * sample).sum();
        assert!(energy > 10.0, "{energy}");
        let jitter = jitter.lock().unwrap();
        assert!(!jitter.active());
        assert_eq!(jitter.underruns, 0, "the end of a sound is not a gap");
    }

    #[test]
    fn running_dry_buffers_longer_and_a_backlog_is_dropped() {
        let mut jitter = Jitter::new(48_000);
        let start = jitter.target;
        jitter.ran_dry();
        assert_eq!(jitter.target, start + 480);
        // Shortening again waits twice as long after each time it ran dry.
        jitter.played(48_000 * 59);
        assert_eq!(jitter.target, start + 480);
        jitter.played(48_000);
        assert_eq!(jitter.target, start + 240);
        for _ in 0..100 {
            jitter.ran_dry();
        }
        assert_eq!(jitter.delay(), MAX_DELAY);
        jitter.played(48_000 * 599);
        assert_eq!(jitter.delay(), MAX_DELAY);
        jitter.played(48_000);
        assert_eq!(jitter.delay(), MAX_DELAY - SHORTER);

        let mut jitter = Jitter::new(48_000);
        jitter.playing = true;
        jitter.push(&vec![0; 48_000 * 2]);
        assert_eq!(jitter.frames(), jitter.target);
    }
}
