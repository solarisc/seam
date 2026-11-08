//! A controlled computer's sound, played on the computer that controls it.
//!
//! While sound is on for a computer, the controlling computer keeps a
//! connection open to that computer's sound port, bound to the input session
//! like the clipboard's. The controlled computer records what it plays only
//! while that connection is open, and sends it as lossless 16-bit stereo at its
//! own sample rate. Nothing is sent while it is silent. Turning sound off
//! closes the connection, and the recording stops with it.
//!
//! Sound has its own connection so that neither a large copy nor the pointer
//! ever waits for it, nor it for them.
mod capture;
mod dsp;
mod playback;

use crate::{
    network::{SecureReader, SecureWriter},
    session::DataLease,
    shutdown::Signal,
};
use anyhow::{Context, Result};
use crossbeam_channel::RecvTimeoutError;
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, TcpStream},
    sync::{Arc, Condvar, Mutex, PoisonError},
    thread,
    time::{Duration, Instant},
};

/// The sound port is `listen_port` plus this.
pub const PORT_OFFSET: u16 = 4;
/// Version of the messages below, checked when a connection opens.
const VERSION: u16 = 1;
/// Silence lasting this long ends sending, so a quiet computer sends nothing.
/// Long enough not to cut into pauses within a sound.
const HANGOVER: std::time::Duration = Duration::from_millis(250);
/// No recording for this long while sending means the sound stopped.
/// Windows delivers nothing at all while nothing plays.
const GAP: Duration = Duration::from_millis(100);
/// How often a controlled computer that can't record tries again.
const RETRY_RECORDING: Duration = Duration::from_secs(5);
/// Longest wait between attempts to reach a computer's sound.
const MOST_BACKOFF: Duration = Duration::from_secs(30);
/// Buffers waiting for the network beyond this are dropped, oldest first, so
/// a stalled network doesn't add delay once it recovers.
const BACKLOG: usize = 4;

/// Sent by the controlling computer when the connection opens.
#[derive(Serialize, Deserialize)]
struct Request {
    version: u16,
}

/// Sent by the controlled computer.
#[derive(Debug, Serialize, Deserialize)]
enum Message {
    /// Sound follows as interleaved stereo at this sample rate.
    Format { rate: u32 },
    /// Interleaved 16-bit stereo samples.
    Samples(Vec<i16>),
    /// Nothing is playing. Samples resume with the next `Samples`.
    Pause,
    /// This computer can't share its sound right now, for the reason given,
    /// worded for the person. It keeps trying, and sends `Format` once it can.
    Unavailable(String),
}

/// What the window shows about one connection's sound.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "reason")]
pub enum Status {
    /// Not shared.
    #[default]
    Off,
    /// Reaching the controlled computer for its sound.
    Connecting,
    /// Shared, with nothing playing right now.
    Quiet,
    /// Playing: arriving here, or on a controlled computer, being sent.
    Playing,
    /// Sound can't be shared, for the reason given.
    Unavailable(String),
}

/// One connection's sound switch and state, shared by its workers, the
/// window's commands and the status the window reads.
#[derive(Default)]
pub struct Link {
    state: Mutex<LinkState>,
    changed: Condvar,
}

#[derive(Default)]
struct LinkState {
    on: bool,
    status: Status,
    /// Counts wake-ups, so a waiter can't miss one between checking and waiting.
    wakes: u64,
}

impl Link {
    pub fn new(on: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(LinkState {
                on,
                ..LinkState::default()
            }),
            changed: Condvar::new(),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn is_on(&self) -> bool {
        self.state().on
    }

    /// Turn sound on or off; takes effect at once.
    pub fn set(&self, on: bool) {
        let mut state = self.state();
        if state.on != on {
            state.on = on;
            if !on {
                state.status = Status::Off;
            }
            state.wakes += 1;
            self.changed.notify_all();
        }
    }

    pub fn status(&self) -> Status {
        self.state().status.clone()
    }

    fn set_status(&self, status: Status) {
        self.state().status = status;
    }

    fn wake(&self) {
        self.state().wakes += 1;
        self.changed.notify_all();
    }

    /// A mark to wait from, taken before checking what to wait for.
    fn mark(&self) -> u64 {
        self.state().wakes
    }

    /// Wait until woken or switched after `mark`, or for `timeout`.
    fn wait_since(&self, mark: u64, timeout: Duration) {
        let _ = self
            .changed
            .wait_timeout_while(self.state(), timeout, |state| state.wakes == mark)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// On the controlling computer, for one input session: while sound is on,
/// keep a connection to the controlled computer's sound port and play what
/// arrives.
pub(crate) fn play(
    link: Arc<Link>,
    lease: DataLease,
    address: SocketAddr,
    code: String,
    name: String,
) {
    let _wake = {
        let link = link.clone();
        lease.stop.register(move || link.wake())
    };
    let mut failures = 0u32;
    let mut retry_at: Option<Instant> = None;
    loop {
        let mark = link.mark();
        if lease.stop.is_stopped() {
            break;
        }
        if !link.is_on() {
            link.set_status(Status::Off);
            failures = 0;
            retry_at = None;
            link.wait_since(mark, Duration::from_secs(3600));
            continue;
        }
        if let Some(at) = retry_at {
            let now = Instant::now();
            if now < at {
                link.wait_since(mark, at - now);
                continue;
            }
        }
        if failures == 0 {
            link.set_status(Status::Connecting);
        }
        let started = Instant::now();
        match connect(&link, &lease, address, &code, &name) {
            Ok(()) => {
                failures = 0;
                retry_at = None;
            }
            Err(error) => {
                // A connection that worked for a while starts the backoff afresh.
                if started.elapsed() >= Duration::from_secs(10) {
                    failures = 0;
                }
                failures += 1;
                let unreachable = error.is::<Unreachable>();
                if unreachable {
                    // The input connection works, so Seam there is older and
                    // has no sound port.
                    if failures == 1 {
                        log::info!("sound from {name}: {error:#}");
                    }
                    link.set_status(Status::Unavailable(format!(
                        "Update Seam on {name} to hear its sound here"
                    )));
                } else {
                    log::info!("sound from {name}: {error:#}");
                }
                let backoff = Duration::from_secs(1 << (failures - 1).min(5)).min(MOST_BACKOFF);
                retry_at = Some(Instant::now() + if unreachable { MOST_BACKOFF } else { backoff });
            }
        }
    }
    link.set_status(Status::Off);
}

/// Sound moves every 10 ms. Schedule its network threads as input's are, so
/// a busy computer, such as one running a game, doesn't hold them back.
fn responsive() -> impl Sized {
    #[cfg(target_os = "macos")]
    return crate::input_priority::InputPriority::enter();
    #[cfg(target_os = "windows")]
    return crate::windows_priority::InputPriority::enter();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    ()
}

/// The sound port didn't take the connection.
#[derive(Debug)]
struct Unreachable(std::io::Error);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the sound port did not answer: {}", self.0)
    }
}

impl std::error::Error for Unreachable {}

/// One connection to a controlled computer's sound, until sound is turned off,
/// the input session ends or the connection fails.
fn connect(
    link: &Link,
    lease: &DataLease,
    address: SocketAddr,
    code: &str,
    name: &str,
) -> Result<()> {
    let socket =
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(Unreachable)?;
    let (mut reader, mut writer) = lease.connect(socket, code)?;
    writer.write(&Request { version: VERSION })?;
    reader.set_cancellation(lease.stop.clone())?;
    let (messages, received) = crossbeam_channel::bounded(256);
    let reading = thread::Builder::new()
        .name("seam-sound-receive".into())
        .spawn(move || -> Result<()> {
            let _priority = responsive();
            loop {
                if messages.send(reader.read::<Message>()?).is_err() {
                    return Ok(());
                }
            }
        })?;
    let _priority = responsive();
    let mut player = playback::Player::new();
    let mut unavailable = None;
    let result = loop {
        if lease.stop.is_stopped() || !link.is_on() {
            break Ok(());
        }
        match received.recv_timeout(Duration::from_millis(200)) {
            Ok(Message::Format { rate }) => {
                unavailable = None;
                log::info!("sound from {name}: {rate} Hz");
                player.format(rate);
            }
            Ok(Message::Samples(samples)) => player.samples(&samples),
            Ok(Message::Pause) => player.pause(),
            Ok(Message::Unavailable(reason)) => {
                if unavailable.as_ref() != Some(&reason) {
                    log::info!("sound from {name} unavailable: {reason}");
                }
                unavailable = Some(reason);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
        player.tick(name);
        link.set_status(match (&unavailable, &player.problem) {
            (Some(reason), _) => Status::Unavailable(reason.clone()),
            (None, Some(problem)) => Status::Unavailable(problem.clone()),
            (None, None) if player.playing() => Status::Playing,
            (None, None) => Status::Quiet,
        });
    };
    // Closing the connection stops the recording on the other computer.
    let _ = writer.shutdown();
    drop(player);
    let read = reading.join().unwrap_or(Ok(()));
    // A read that ended because this side closed is no failure.
    match (&result, read) {
        (Ok(()), Err(error)) if !lease.stop.is_stopped() && link.is_on() => {
            Err(error.context("the sound connection closed"))
        }
        _ => result,
    }
}

/// On the controlled computer, for one connection from the controlling
/// computer: record this computer's sound and send it until the connection
/// closes or the input session ends. On the Windows sign-in screen, only say
/// that the computer is locked.
pub(crate) fn serve(
    mut reader: SecureReader,
    mut writer: SecureWriter,
    stop: Arc<Signal>,
    link: Arc<Link>,
    sign_in_screen: bool,
) -> Result<()> {
    reader.set_timeout(Duration::from_secs(3))?;
    let request: Request = reader.read().context("reading the sound request")?;
    if request.version != VERSION {
        let _ = writer.write(&Message::Unavailable(
            "Install the same Seam version on both computers to share sound".into(),
        ));
        anyhow::bail!("sound version {} is not supported", request.version);
    }
    let (closed, watching) = watch_hang_up(reader, stop)?;
    let result = if sign_in_screen {
        writer
            .write(&Message::Unavailable(
                "Its sound plays here again once it's unlocked".into(),
            ))
            .map(|()| while !closed.wait(Duration::from_secs(3600)) {})
    } else {
        send(&mut writer, &closed, &link)
    };
    let _ = writer.shutdown();
    let _ = watching.join();
    link.set_status(Status::Off);
    result
}

/// The controlling computer sends nothing after its request: stop once it
/// hangs up or the input session ends, however long the silence before that.
fn watch_hang_up(
    mut reader: SecureReader,
    stop: Arc<Signal>,
) -> Result<(Arc<Signal>, thread::JoinHandle<()>)> {
    reader.clear_timeout()?;
    reader.set_cancellation(stop)?;
    let closed = Arc::new(Signal::default());
    let watching = {
        let closed = closed.clone();
        thread::Builder::new()
            .name("seam-sound-watch".into())
            .spawn(move || {
                let _ = reader.read::<Request>();
                closed.stop();
            })?
    };
    Ok((closed, watching))
}

fn send(writer: &mut SecureWriter, closed: &Signal, link: &Link) -> Result<()> {
    let _priority = responsive();
    let host = cpal::default_host();
    while !closed.is_stopped() {
        let capture = match capture::Capture::start(&host) {
            Ok(capture) => capture,
            Err(reason) => {
                if link.status() != Status::Unavailable(reason.clone()) {
                    log::warn!("cannot share this computer's sound: {reason}");
                }
                writer.write(&Message::Unavailable(reason.clone()))?;
                link.set_status(Status::Unavailable(reason));
                closed.wait(RETRY_RECORDING);
                continue;
            }
        };
        writer.write(&Message::Format { rate: capture.rate })?;
        link.set_status(Status::Quiet);
        let mut transmit = Transmit::new(capture.rate);
        let mut checked = Instant::now();
        while !closed.is_stopped() {
            // While quiet, only the speakers' check needs a wake-up.
            let wait = if transmit.paused {
                Duration::from_secs(1)
            } else {
                GAP
            };
            match capture.packets.recv_timeout(wait) {
                Ok(packet) => {
                    transmit.packet(writer, &packet)?;
                    capture.recycle(packet);
                    // Catch up after the network held things back.
                    while capture.packets.len() > BACKLOG {
                        if let Ok(stale) = capture.packets.try_recv() {
                            capture.recycle(stale);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => transmit.idle(writer)?,
                Err(RecvTimeoutError::Disconnected) => break,
            }
            capture.set_quiet(transmit.paused);
            link.set_status(if transmit.paused {
                Status::Quiet
            } else {
                Status::Playing
            });
            if checked.elapsed() >= Duration::from_secs(1) {
                checked = Instant::now();
                if !capture.current(&host) {
                    log::info!("the speakers changed or stopped; recording again");
                    transmit.idle(writer)?;
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Sends one recording's buffers, leaving out silence.
struct Transmit {
    quantizer: dsp::Quantizer,
    samples: Vec<i16>,
    /// Frames of silence in a row.
    silent: usize,
    hangover: usize,
    /// Told the other computer nothing is playing; nothing sent since.
    paused: bool,
}

impl Transmit {
    fn new(rate: u32) -> Self {
        Self {
            quantizer: dsp::Quantizer::new(),
            samples: Vec::new(),
            silent: 0,
            hangover: (HANGOVER.as_secs_f64() * f64::from(rate)) as usize,
            paused: true,
        }
    }

    /// One buffer of interleaved stereo.
    fn packet(&mut self, writer: &mut SecureWriter, stereo: &[f32]) -> Result<()> {
        if dsp::silent(stereo) {
            self.silent += stereo.len() / 2;
            if self.paused {
                return Ok(());
            }
            if self.silent >= self.hangover {
                return self.pause(writer);
            }
        } else {
            self.silent = 0;
        }
        self.paused = false;
        // Well under the largest message, even for a long buffer.
        for chunk in stereo.chunks(8_192) {
            let mut samples = std::mem::take(&mut self.samples);
            samples.clear();
            self.quantizer.quantize(chunk, &mut samples);
            let message = Message::Samples(samples);
            let sent = writer.write(&message);
            if let Message::Samples(samples) = message {
                self.samples = samples;
            }
            sent?;
        }
        Ok(())
    }

    /// Nothing recorded for a while.
    fn idle(&mut self, writer: &mut SecureWriter) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        self.pause(writer)
    }

    fn pause(&mut self, writer: &mut SecureWriter) -> Result<()> {
        self.paused = true;
        writer.write(&Message::Pause)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::SecureConnection;
    use std::net::TcpListener;

    fn connected() -> (SecureReader, SecureWriter) {
        let code = crate::config::random_secret();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.clone();
        let server = thread::spawn(move || {
            SecureConnection::accept(listener.accept().unwrap().0, &server_code).unwrap()
        });
        let client = SecureConnection::connect(&address.to_string(), &code).unwrap();
        let (_, writer) = client.split().unwrap();
        let (reader, _) = server.join().unwrap().split().unwrap();
        // Both halves of each side stay open through their remaining half.
        (reader, writer)
    }

    #[test]
    fn silence_is_sent_only_until_the_hangover_and_sound_resumes_at_once() {
        let (mut reader, mut writer) = connected();
        reader.set_timeout(Duration::from_secs(2)).unwrap();
        let rate = 48_000;
        let mut transmit = Transmit::new(rate);
        let tone: Vec<f32> = (0..960).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let quiet = vec![0.0f32; 960];

        // Silence before any sound sends nothing.
        transmit.packet(&mut writer, &quiet).unwrap();
        transmit.packet(&mut writer, &tone).unwrap();
        assert!(matches!(reader.read().unwrap(), Message::Samples(s) if s.len() == 960));
        // Silence within the hangover is still sent, so pauses keep their length.
        let within = (HANGOVER.as_millis() / 10 - 1) as usize;
        for _ in 0..within {
            transmit.packet(&mut writer, &quiet).unwrap();
            assert!(matches!(reader.read().unwrap(), Message::Samples(_)));
        }
        transmit.packet(&mut writer, &quiet).unwrap();
        assert!(matches!(reader.read().unwrap(), Message::Pause));
        for _ in 0..50 {
            transmit.packet(&mut writer, &quiet).unwrap();
        }
        transmit.idle(&mut writer).unwrap();
        transmit.packet(&mut writer, &tone).unwrap();
        // The next message after the pause is the sound itself.
        assert!(matches!(reader.read().unwrap(), Message::Samples(s) if s.len() == 960));
        // A recording that stops delivering ends with one pause.
        transmit.idle(&mut writer).unwrap();
        transmit.idle(&mut writer).unwrap();
        assert!(matches!(reader.read().unwrap(), Message::Pause));
        writer.write(&Message::Format { rate }).unwrap();
        assert!(matches!(
            reader.read().unwrap(),
            Message::Format { rate: 48_000 }
        ));
    }

    #[test]
    fn a_full_buffer_fits_in_one_message() {
        let (mut reader, mut writer) = connected();
        reader.set_timeout(Duration::from_secs(2)).unwrap();
        let mut transmit = Transmit::new(48_000);
        // Longer than any capture callback delivers, split as needed.
        let long: Vec<f32> = (0..40_000)
            .map(|i| ((i % 100) as f32 - 50.0) / 100.0)
            .collect();
        transmit.packet(&mut writer, &long).unwrap();
        let mut received = 0;
        while received < long.len() {
            match reader.read().unwrap() {
                Message::Samples(samples) => received += samples.len(),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(received, long.len());
    }

    #[test]
    fn a_long_silence_from_the_controller_is_not_a_hang_up() {
        for hang_up in [true, false] {
            let (mut reader, writer) = connected();
            // The deadline the request was read under must not carry over.
            reader.set_timeout(Duration::from_millis(100)).unwrap();
            let stop = Arc::new(Signal::default());
            let (closed, watching) = watch_hang_up(reader, stop.clone()).unwrap();
            assert!(!closed.wait(Duration::from_millis(600)));
            if hang_up {
                writer.shutdown().unwrap();
            } else {
                // The input session ended.
                stop.stop();
            }
            assert!(closed.wait(Duration::from_secs(2)));
            watching.join().unwrap();
        }
    }

    #[test]
    fn switching_wakes_a_waiting_worker_at_once() {
        let link = Link::new(false);
        let waiter = {
            let link = link.clone();
            thread::spawn(move || {
                let started = Instant::now();
                let mark = link.mark();
                if !link.is_on() {
                    link.wait_since(mark, Duration::from_secs(10));
                }
                (link.is_on(), started.elapsed())
            })
        };
        thread::sleep(Duration::from_millis(50));
        link.set(true);
        let (on, waited) = waiter.join().unwrap();
        assert!(on);
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn status_reads_as_a_tagged_state_for_the_window() {
        let json = serde_json::to_value(Status::Unavailable("why".into())).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "unavailable", "reason": "why"})
        );
        assert_eq!(
            serde_json::to_value(Status::Playing).unwrap(),
            serde_json::json!({"state": "playing"})
        );
    }
}
