//! Plays a controlled computer's sound through speakers that ask for it,
//! as macOS's do.
//!
//! Samples wait in a short buffer that rides out the network's timing (see
//! `pacing`). The speakers' callback takes them out through a converter that
//! matches the two sample rates and leans on the playback rate, by an
//! inaudible fraction of a percent, to hold the buffer at its target. That
//! follows the drift between the two computers' audio clocks, so the delay
//! neither creeps up nor runs dry over a long film.
//!
//! The network thread puts samples in the buffer itself; the speakers run only
//! while there is sound, and nothing else wakes up for each packet.
#[cfg(not(windows))]
use super::pacing::{CLOSE_AFTER, DEVICE_CHECK, PAUSE_AFTER, REOPEN, UNTIL_WOKEN};
use super::{
    dsp,
    pacing::{self, Pacing, Steering},
};
#[cfg(not(windows))]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(windows))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{Adjustable, FixedAsync, audioadapter_buffers::direct::InterleavedSlice};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
#[cfg(not(windows))]
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

/// Frames converted at a time: under 3 ms at 48 kHz.
const CHUNK: usize = 128;
/// Most held, beyond the target, while the speakers haven't started.
const WAITING: Duration = Duration::from_millis(500);

/// Samples received and not yet played, shared by the network thread, the
/// speakers' callback and the player.
pub(super) struct Jitter {
    /// Interleaved stereo at the sender's rate.
    samples: VecDeque<f32>,
    pub(super) pacing: Pacing,
    playing: bool,
    /// The sender stopped because nothing plays. What remains is the end of
    /// the sound, not a gap.
    paused: bool,
    /// The speakers are running. While they aren't, arriving sound asks the
    /// player to start them, once.
    running: bool,
    asked: bool,
    /// Packets received, so the player can tell sound is arriving.
    pub(super) received: u64,
}

impl Jitter {
    pub(super) fn new(rate: u32) -> Self {
        Self {
            samples: VecDeque::new(),
            pacing: Pacing::new(rate),
            playing: false,
            paused: false,
            running: false,
            asked: false,
            received: 0,
        }
    }

    fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Sound follows at `rate`. A new rate starts afresh.
    pub(super) fn set_rate(&mut self, rate: u32) {
        if rate.max(1) != self.pacing.rate() {
            let (running, asked) = (self.running, self.asked);
            *self = Self::new(rate);
            self.running = running;
            self.asked = asked;
        }
    }

    /// Interleaved 16-bit stereo, little-endian: four bytes a frame.
    pub(super) fn push(&mut self, samples: &[u8]) {
        self.paused = false;
        self.received += 1;
        self.pacing.packet(samples.len() / 4);
        dsp::to_float(&samples[..samples.len() / 4 * 4], &mut self.samples);
        let target = self.pacing.target();
        let (limit, keep) = if self.playing {
            (self.pacing.excess(), target)
        } else {
            // Waiting for the speakers to start, which may first ask for
            // their whole buffer: keep the start of the sound.
            let limit = target + self.pacing.frames_in(WAITING);
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

    /// Sound arrived while the speakers aren't running: whether to ask the
    /// player to start them. Asks once until they run.
    pub(super) fn wants_speakers(&mut self) -> bool {
        let ask = !self.running && !self.asked;
        self.asked |= ask;
        ask
    }

    fn set_running(&mut self, running: bool) {
        self.running = running;
        self.asked = false;
    }

    /// Something is playing or waiting to.
    pub(super) fn active(&self) -> bool {
        self.playing || !self.samples.is_empty()
    }

    /// Enough has gathered to start playing, with `wanted` frames asked for
    /// at once. Speakers ask for their whole buffer when they start, and the
    /// target is kept beyond that.
    fn ready(&self, wanted: usize) -> bool {
        self.frames() >= self.pacing.target() + wanted || (self.paused && !self.samples.is_empty())
    }

    fn ran_dry(&mut self) {
        self.playing = false;
        self.pacing.ran_dry();
    }

    /// A speaker callback took what it needed while sound streams in.
    fn callback_end(&mut self) {
        if self.playing && !self.paused {
            let fill = self.frames();
            self.pacing.low_point(fill);
        }
    }
}

fn lock(jitter: &Mutex<Jitter>) -> MutexGuard<'_, Jitter> {
    jitter.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs in the speakers' callback: takes samples from the buffer, converts
/// them and writes them out, steering the rate to hold the buffer's target.
pub(super) struct Renderer {
    jitter: Arc<Mutex<Jitter>>,
    converter: Box<dyn Adjustable<f32>>,
    channels: usize,
    rate: u32,
    source_per_output: f64,
    input: Vec<f32>,
    /// One converted chunk, interleaved stereo.
    output: Vec<f32>,
    /// Frames of `output` already written out.
    position: usize,
    steering: Steering,
}

impl Renderer {
    pub(super) fn new(
        jitter: Arc<Mutex<Jitter>>,
        source_rate: u32,
        output_rate: u32,
        channels: usize,
    ) -> Result<Self> {
        let converter = pacing::converter(source_rate, output_rate, CHUNK, FixedAsync::Output)?;
        Ok(Self {
            input: vec![0.0; converter.input_frames_max() * 2],
            output: vec![0.0; CHUNK * 2],
            position: CHUNK,
            converter,
            jitter,
            channels: channels.max(1),
            rate: source_rate,
            source_per_output: f64::from(source_rate) / f64::from(output_rate),
            steering: Steering::default(),
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
        lock(&self.jitter).callback_end();
    }

    /// Convert the next chunk, with `wanted` source frames still asked for in
    /// this callback. `false` when there is nothing to play.
    fn convert(&mut self, wanted: usize) -> bool {
        let needed = self.converter.input_frames_next();
        let (taken, fill, target, streaming) = {
            let mut jitter = lock(&self.jitter);
            // Sound at another rate waits for speakers set up for it.
            if jitter.pacing.rate() != self.rate {
                return false;
            }
            if !jitter.playing {
                if !jitter.ready(wanted) {
                    return false;
                }
                jitter.playing = true;
                self.steering.average = None;
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
            // The end of a sound drains the buffer on purpose: it neither
            // steers nor counts as coming close to running dry.
            let streaming = !jitter.paused;
            let fill = jitter.frames();
            if streaming {
                jitter.pacing.played(taken, fill);
            } else if taken < needed {
                // Finish the sound, then wait for the next one.
                jitter.playing = false;
            }
            (taken, fill, jitter.pacing.target(), streaming)
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
        if streaming {
            let ratio = self.steering.ratio(fill, target, taken, self.rate);
            let _ = self.converter.set_resample_ratio_relative(ratio, true);
        }
        true
    }
}

/// The speakers, playing one controlled computer's sound.
#[cfg(not(windows))]
struct Output {
    stream: cpal::Stream,
    device: cpal::Device,
    /// The sender's rate this output converts from.
    rate: u32,
    running: bool,
    failed: Arc<AtomicBool>,
    /// Callbacks the system reports as late.
    late: Arc<AtomicU64>,
}

#[cfg(not(windows))]
impl Output {
    fn open(host: &cpal::Host, jitter: &Arc<Mutex<Jitter>>, rate: u32) -> Result<Self> {
        let device = host
            .default_output_device()
            .context("no speakers or headphones are connected")?;
        let supported = device
            .default_output_config()
            .context("reading the speakers' format")?;
        // Windows' shared mode keeps the same buffer whatever is asked for.
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut renderer = Renderer::new(
            jitter.clone(),
            rate,
            config.sample_rate,
            usize::from(config.channels),
        )?;
        let failed = Arc::new(AtomicBool::new(false));
        let late = Arc::new(AtomicU64::new(0));
        let (failing, lateness) = (failed.clone(), late.clone());
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| renderer.render(data),
                move |error: cpal::Error| {
                    // A late callback is a click, not a broken device.
                    if error.kind() == cpal::ErrorKind::Xrun {
                        lateness.fetch_add(1, Ordering::Relaxed);
                    } else {
                        failing.store(true, Ordering::Release);
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
            stream,
            device,
            rate,
            running: true,
            failed,
            late,
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
#[cfg(not(windows))]
pub(super) struct Player {
    host: cpal::Host,
    jitter: Arc<Mutex<Jitter>>,
    /// The sender's sample rate, once it said.
    rate: Option<u32>,
    output: Option<Output>,
    /// Running speakers had nothing to play since then.
    quiet_since: Option<Instant>,
    /// The speakers paused then.
    paused_at: Option<Instant>,
    checked: Instant,
    /// The speakers failed to open; try again then.
    retry: Option<Instant>,
    /// Why the speakers can't play, for the window.
    pub(super) problem: Option<String>,
    /// Sound arrived since the last look, or is still playing out.
    playing: bool,
    received: u64,
    underruns: u64,
    late: u64,
    /// The buffer's target last written to the log.
    logged_delay: Duration,
}

#[cfg(not(windows))]
impl Player {
    pub(super) fn new() -> Self {
        Self {
            host: cpal::default_host(),
            jitter: Arc::new(Mutex::new(Jitter::new(48_000))),
            rate: None,
            output: None,
            quiet_since: None,
            paused_at: None,
            checked: Instant::now(),
            retry: None,
            problem: None,
            playing: false,
            received: 0,
            underruns: 0,
            late: 0,
            logged_delay: Duration::ZERO,
        }
    }

    /// The buffer, for the network thread to fill.
    pub(super) fn buffer(&self) -> Arc<Mutex<Jitter>> {
        self.jitter.clone()
    }

    /// Sound follows at `rate`, which the network thread already gave the
    /// buffer. The speakers open now, so the first sound isn't held up.
    pub(super) fn format(&mut self, rate: u32) {
        self.rate = Some(rate);
        if self
            .output
            .as_ref()
            .is_some_and(|output| output.rate != rate)
        {
            self.output = None;
        }
        self.retry = None;
        if self.output.is_none() {
            self.open(Instant::now());
        }
    }

    /// Sound arrived while the speakers weren't running.
    pub(super) fn start(&mut self) {
        let now = Instant::now();
        if let Some(output) = &mut self.output
            && !output.running
        {
            // Checked here rather than each second while paused.
            let resumed = !output.stale(&self.host) && output.stream.play().is_ok();
            if resumed {
                output.running = true;
                lock(&self.jitter).set_running(true);
                self.paused_at = None;
                self.quiet_since = None;
                self.checked = now;
                return;
            }
            self.output = None;
        }
        if self.output.is_none() {
            self.open(now);
        } else {
            lock(&self.jitter).set_running(true);
        }
    }

    /// Sound arrived since the last look, or is still playing out.
    pub(super) fn playing(&self) -> bool {
        self.playing
    }

    /// Pause or close idle speakers, follow a change of speakers and report.
    /// Returns how long until this is needed again.
    pub(super) fn tick(&mut self, name: &str) -> Duration {
        let now = Instant::now();
        let (active, received, underruns, delay) = {
            let jitter = lock(&self.jitter);
            (
                jitter.active(),
                jitter.received,
                jitter.pacing.underruns,
                jitter.pacing.delay(),
            )
        };
        self.playing = active || received != self.received;
        self.received = received;
        if underruns > self.underruns {
            log::info!(
                "sound from {name} ran short; now buffering {} ms",
                delay.as_millis()
            );
            self.underruns = underruns;
            self.logged_delay = delay;
        } else if delay.abs_diff(self.logged_delay) >= Duration::from_millis(5) {
            log::info!("sound from {name}: buffering {} ms", delay.as_millis());
            self.logged_delay = delay;
        }
        if let Some(output) = &self.output {
            let late = output.late.load(Ordering::Relaxed);
            if late > self.late {
                log::debug!(
                    "the speakers reported {} late callback(s)",
                    late - self.late
                );
                self.late = late;
            }
        }
        match self.output.as_ref().map(|output| output.running) {
            None => {
                // Retry speakers that failed to open, if there's sound.
                if active && self.retry.is_none_or(|retry| now >= retry) {
                    self.open(now);
                }
                match (&self.output, self.retry) {
                    (Some(_), _) => DEVICE_CHECK,
                    (None, Some(_)) if active => REOPEN,
                    (None, _) => UNTIL_WOKEN,
                }
            }
            Some(true) => {
                if self.playing {
                    self.quiet_since = None;
                } else if now - *self.quiet_since.get_or_insert(now) >= PAUSE_AFTER {
                    if let Some(output) = &mut self.output {
                        let _ = output.stream.pause();
                        output.running = false;
                    }
                    lock(&self.jitter).set_running(false);
                    self.paused_at = Some(now);
                    self.quiet_since = None;
                    return CLOSE_AFTER;
                }
                if now - self.checked >= DEVICE_CHECK {
                    self.checked = now;
                    if self.output.as_ref().is_some_and(|o| o.stale(&self.host)) {
                        log::info!("the speakers changed; following them");
                        self.output = None;
                        self.open(now);
                    }
                }
                DEVICE_CHECK
            }
            Some(false) => {
                let paused = now - *self.paused_at.get_or_insert(now);
                if paused >= CLOSE_AFTER {
                    log::debug!("sound from {name} has been quiet; closing the speakers");
                    self.output = None;
                    self.paused_at = None;
                    UNTIL_WOKEN
                } else {
                    CLOSE_AFTER - paused
                }
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
                self.late = 0;
                self.output = Some(output);
                lock(&self.jitter).set_running(true);
                self.checked = now;
                self.quiet_since = None;
                self.paused_at = None;
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
    fn tone(rate: u32, frequency: f64, start: usize, frames: usize) -> Vec<u8> {
        (start..start + frames)
            .flat_map(|index| {
                let value = (index as f64 * frequency * std::f64::consts::TAU / f64::from(rate))
                    .sin()
                    * 0.5
                    * 32_767.0;
                let [low, high] = (value as i16).to_le_bytes();
                [low, high, low, high]
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
        /// Packets arrive up to this many milliseconds late, at random.
        lateness: usize,
        /// Packets sent and when each arrives, in milliseconds.
        in_flight: VecDeque<(usize, Vec<u8>)>,
        random: u32,
        /// Milliseconds simulated so far.
        now: usize,
        sent: usize,
        rendered: Vec<f32>,
        /// The buffer's fill summed each millisecond, and the count.
        fill_sum: f64,
        fill_count: f64,
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
                lateness: 0,
                in_flight: VecDeque::new(),
                random: 0x1234_5678,
                now: 0,
                sent: 0,
                rendered: Vec::new(),
                fill_sum: 0.0,
                fill_count: 0.0,
            }
        }

        /// The buffer's mean fill over the next `seconds`, in milliseconds.
        fn mean_fill_ms(&mut self, seconds: f64) -> f64 {
            (self.fill_sum, self.fill_count) = (0.0, 0.0);
            self.run(seconds, false);
            self.fill_sum / self.fill_count * 1000.0 / f64::from(self.source_rate)
        }

        /// Run for `seconds`: the sender records 10 ms packets on its own
        /// clock, the network may hold each back, and the speakers ask for
        /// 10 ms buffers on their clock.
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
                    self.random ^= self.random << 13;
                    self.random ^= self.random >> 17;
                    self.random ^= self.random << 5;
                    let delay = self.random as usize % (self.lateness + 1);
                    // TCP keeps packets in order.
                    let arrives = self
                        .in_flight
                        .back()
                        .map_or(step + delay, |(last, _)| (*last).max(step + delay));
                    self.in_flight.push_back((arrives, packet));
                    self.sent = due;
                }
                while self.in_flight.front().is_some_and(|(at, _)| *at <= step) {
                    let (_, packet) = self.in_flight.pop_front().unwrap();
                    lock(&self.jitter).push(&packet);
                }
                if step % 10 == 9 {
                    self.renderer.render(&mut buffer);
                    if keep {
                        self.rendered.extend_from_slice(&buffer);
                    }
                }
                self.fill_sum += lock(&self.jitter).frames() as f64;
                self.fill_count += 1.0;
            }
        }

        fn underruns(&self) -> u64 {
            lock(&self.jitter).pacing.underruns
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
            simulation.run(300.0, false);
            assert_eq!(simulation.underruns(), 0, "ran dry at clock {clock}");
        }
    }

    #[test]
    fn the_delay_settles_near_the_least_that_never_runs_dry() {
        // Clocks slower, equal and faster, on a steady network and on one
        // that holds packets back up to 15 ms. With 10 ms packets and 10 ms
        // callbacks, never running dry as the clocks drift takes about one
        // and a half packets plus the margin: some 19 ms.
        for (clock, lateness, most) in [
            (0.9997, 0, 26.0),
            (1.0, 0, 24.0),
            (1.0003, 0, 22.0),
            (1.0001, 3, 26.0),
            (1.0001, 15, 40.0),
        ] {
            let mut simulation = Simulation::new(48_000, 48_000, clock);
            simulation.lateness = lateness;
            simulation.run(120.0, false);
            let mean = simulation.mean_fill_ms(480.0);
            assert_eq!(
                simulation.underruns(),
                0,
                "ran dry at clock {clock}, {lateness} ms late"
            );
            assert!(
                mean < most,
                "{mean:.1} ms buffered at clock {clock}, {lateness} ms late"
            );
        }
    }

    #[test]
    fn a_different_sample_rate_keeps_the_pitch() {
        let mut simulation = Simulation::new(44_100, 48_000, 1.0);
        // Once the buffer has settled; until then the rate leans a little.
        simulation.run(30.0, false);
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
        lock(&jitter).push(&sent);
        lock(&jitter).pause();
        let mut played = vec![0.0f32; 2_400 * 2];
        renderer.render(&mut played);
        for (sample, &original) in played.iter().zip(sent.as_chunks::<2>().0) {
            let original = i16::from_le_bytes(original);
            assert!((sample - f32::from(original) / 32_768.0).abs() < 1e-6);
        }
    }

    #[test]
    fn sound_at_a_new_rate_waits_for_speakers_set_up_for_it() {
        let jitter = Arc::new(Mutex::new(Jitter::new(48_000)));
        let mut renderer = Renderer::new(jitter.clone(), 48_000, 48_000, 2).unwrap();
        lock(&jitter).set_rate(44_100);
        lock(&jitter).push(&tone(44_100, 440.0, 0, 4_410));
        lock(&jitter).pause();
        let mut played = vec![1.0f32; 960];
        renderer.render(&mut played);
        assert!(played.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn the_target_is_kept_beyond_the_speakers_first_request() {
        let jitter = Arc::new(Mutex::new(Jitter::new(48_000)));
        let mut renderer = Renderer::new(jitter.clone(), 48_000, 48_000, 2).unwrap();
        let target = lock(&jitter).pacing.target();
        // Speakers that start by asking for 22 ms at once.
        let mut first = vec![0.0f32; 1_056 * 2];
        let mut sent = 0;
        while sent < target + 1_056 {
            renderer.render(&mut first);
            assert!(first.iter().all(|&sample| sample == 0.0), "started early");
            lock(&jitter).push(&tone(48_000, 1000.0, sent, 480));
            sent += 480;
        }
        renderer.render(&mut first);
        assert!(first.iter().any(|&sample| sample != 0.0));
        assert!(lock(&jitter).frames() >= target - CHUNK);
    }

    #[test]
    fn a_sound_shorter_than_the_delay_still_plays_once_the_sender_pauses() {
        let jitter = Arc::new(Mutex::new(Jitter::new(48_000)));
        let mut renderer = Renderer::new(jitter.clone(), 48_000, 48_000, 1).unwrap();
        // 5 ms, less than the delay gathered before playing.
        lock(&jitter).push(&tone(48_000, 1000.0, 0, 240));
        let mut buffer = vec![0.0f32; 480];
        renderer.render(&mut buffer);
        assert!(buffer.iter().all(|&sample| sample == 0.0), "waits for more");
        lock(&jitter).pause();
        renderer.render(&mut buffer);
        let energy: f32 = buffer.iter().map(|sample| sample * sample).sum();
        assert!(energy > 10.0, "{energy}");
        let jitter = lock(&jitter);
        assert!(!jitter.active());
        assert_eq!(
            jitter.pacing.underruns, 0,
            "the end of a sound is not a gap"
        );
    }

    #[test]
    fn a_backlog_while_playing_is_cut_back_to_the_target() {
        let mut jitter = Jitter::new(48_000);
        jitter.playing = true;
        jitter.push(&vec![0; 48_000 * 4]);
        assert_eq!(jitter.frames(), jitter.pacing.target());
    }

    #[test]
    fn arriving_sound_asks_for_the_speakers_once() {
        let mut jitter = Jitter::new(48_000);
        assert!(jitter.wants_speakers());
        assert!(!jitter.wants_speakers());
        jitter.set_running(true);
        assert!(!jitter.wants_speakers());
        jitter.set_running(false);
        assert!(jitter.wants_speakers());
    }
}
