//! Live agent state and the token-protected local control channel used by the
//! settings window, tray and command line.
use crate::{
    config::Config,
    input::Controller,
    layout::{self, Layout},
    pairing,
    protocol::Monitor,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub links: Vec<LinkStatus>,
    /// `false` when the OS has not granted input control (macOS Accessibility).
    pub input_permission_ready: bool,
    pub nearby: Vec<NearbyPeer>,
    /// A computer asking to control this one, awaiting the user's decision.
    pub pairing: Option<pairing::Prompt>,
    pub transfers: Vec<crate::transfers::Snapshot>,
    /// Problems that keep part of Seam from working, worded for the person.
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LinkStatus {
    /// Index into `Config::peers`; `None` for the link from a controlling computer.
    pub peer: Option<usize>,
    pub connected: bool,
    pub name: String,
    pub address: String,
    pub os: String,
    pub remote_monitors: Vec<Monitor>,
    pub layout: Option<layout::Report>,
    /// This computer's keyboard and mouse currently drive the other one.
    pub controlling: bool,
    /// The other computer's keyboard and mouse currently drive this one.
    pub controlled: bool,
    pub rtt_us: Option<u64>,
    pub sent_events: u64,
    pub received_events: u64,
    pub last_error: String,
    /// The controlled computer's sound: played here, or sent from here.
    #[serde(default)]
    pub sound: crate::sound::Status,
}

impl LinkStatus {
    pub fn ready(&self) -> bool {
        self.connected && self.layout.as_ref().is_some_and(|r| r.ready && !r.pending)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NearbyPeer {
    pub name: String,
    pub address: String,
    pub port: u16,
    /// Hash of the computer's pairing secret, used to follow address changes.
    #[serde(default)]
    pub pairing_id: String,
    #[serde(default)]
    pub os: String,
    /// The computer can be paired: it does not control other computers.
    #[serde(default)]
    pub accepts_pairing: bool,
    /// Name of the computer currently allowed to control it.
    #[serde(default)]
    pub controller: String,
}

impl Snapshot {
    pub fn connection_count(&self) -> usize {
        self.links.iter().filter(|link| link.connected).count()
    }

    pub fn has_remote_control(&self) -> bool {
        self.links
            .iter()
            .any(|link| link.controlling || link.controlled)
    }

    pub fn link(&self, peer: Option<usize>) -> Option<&LinkStatus> {
        self.links.iter().find(|link| link.peer == peer)
    }
}

/// Counters and identity of one connection, shared by its worker threads.
pub struct Stats {
    pub connected: AtomicBool,
    pub peer_name: RwLock<String>,
    pub peer_address: RwLock<String>,
    pub peer_os: RwLock<String>,
    pub remote_monitors: RwLock<Vec<Monitor>>,
    pub rtt_us: AtomicU64,
    pub(crate) best_rtt_us: AtomicU64,
    pub(crate) rtt_samples: AtomicU64,
    pub received_events: AtomicU64,
    pub last_error: RwLock<String>,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connected: AtomicBool::new(false),
            peer_name: RwLock::new(String::new()),
            peer_address: RwLock::new(String::new()),
            peer_os: RwLock::new(String::new()),
            remote_monitors: RwLock::new(Vec::new()),
            rtt_us: AtomicU64::new(u64::MAX),
            best_rtt_us: AtomicU64::new(u64::MAX),
            rtt_samples: AtomicU64::new(0),
            received_events: AtomicU64::new(0),
            last_error: RwLock::new(String::new()),
        })
    }

    pub(crate) fn set_error(&self, error: impl Into<String>) {
        *self.last_error.write().unwrap() = error.into();
    }

    fn snapshot(&self, peer: Option<usize>, controller: &Controller) -> LinkStatus {
        let rtt = self.rtt_us.load(Ordering::Relaxed);
        LinkStatus {
            peer,
            connected: self.connected.load(Ordering::Acquire),
            name: self.peer_name.read().unwrap().clone(),
            address: self.peer_address.read().unwrap().clone(),
            os: self.peer_os.read().unwrap().clone(),
            remote_monitors: self.remote_monitors.read().unwrap().clone(),
            layout: None,
            controlling: controller.is_controlling_peer(),
            controlled: peer.is_none() && controller.peer_control_active.load(Ordering::Acquire),
            rtt_us: (rtt != u64::MAX).then_some(rtt),
            sent_events: controller.sent_events.load(Ordering::Relaxed),
            received_events: self.received_events.load(Ordering::Relaxed),
            last_error: self.last_error.read().unwrap().clone(),
            sound: crate::sound::Status::Off,
        }
    }
}

pub(crate) struct LinkHandle {
    pub peer: Option<usize>,
    pub stats: Arc<Stats>,
    pub controller: Arc<Controller>,
    pub layouts: Arc<layout::Service>,
    /// For a computer this one connects to: the pairing hint in its beacons,
    /// and the alarm its connection thread waits on.
    pub beacon: Option<(String, Arc<crate::wake::Alarm>)>,
    pub sound: Arc<crate::sound::Link>,
}

/// Agent-wide state reported to the settings window.
pub struct Status {
    pub(crate) name: String,
    pub(crate) links: Vec<LinkHandle>,
    pub nearby: RwLock<Vec<NearbyPeer>>,
    pub(crate) pairing: pairing::Inbox,
    pub controller_name: RwLock<String>,
    problems: RwLock<Vec<String>>,
}

impl Status {
    pub(crate) fn new(config: &Config, links: Vec<LinkHandle>) -> Arc<Self> {
        Arc::new(Self {
            name: config.name.clone(),
            links,
            nearby: RwLock::new(Vec::new()),
            pairing: pairing::Inbox::default(),
            controller_name: RwLock::new(config.controller_name.clone()),
            problems: RwLock::default(),
        })
    }

    /// A computer's beacon was heard after a silence. A link to it that is
    /// waiting to try again tries now: the computer is back on the network.
    pub(crate) fn heard(&self, pairing_id: &str) {
        for link in &self.links {
            if let Some((hint, alarm)) = &link.beacon
                && hint == pairing_id
            {
                alarm.heard();
            }
        }
    }

    /// Show `problem` in the settings window until the agent restarts.
    pub(crate) fn report_problem(&self, problem: &str) {
        let mut problems = self.problems.write().unwrap();
        if !problems.iter().any(|known| known == problem) {
            problems.push(problem.to_string());
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            name: self.name.clone(),
            links: self
                .links
                .iter()
                .map(|link| {
                    let mut status = link.stats.snapshot(link.peer, &link.controller);
                    status.layout = Some(link.layouts.report());
                    status.sound = link.sound.status();
                    status
                })
                .collect(),
            input_permission_ready: crate::input::accessibility_ready(),
            nearby: self.nearby.read().unwrap().clone(),
            pairing: self.pairing.prompt(),
            transfers: crate::transfers::snapshots(),
            problems: self.problems.read().unwrap().clone(),
        }
    }

    fn layouts(&self, peer: Option<usize>) -> Result<&Arc<layout::Service>> {
        self.links
            .iter()
            .find(|link| link.peer == peer)
            .map(|link| &link.layouts)
            .context("That computer is not part of the running configuration")
    }

    /// Play a controlled computer's sound here or stop, at once, and keep
    /// the choice for next time.
    fn set_sound(&self, peer: usize, on: bool, code: &str) -> Result<()> {
        let link = self
            .links
            .iter()
            .find(|link| link.peer == Some(peer))
            .context("That computer is not part of the running configuration")?;
        link.sound.set(on);
        crate::config::edit(|saved| {
            // Matched by its secret: the list may have changed since.
            if let Some(saved) = saved.peers.iter_mut().find(|saved| saved.code == code) {
                saved.sound = on;
            }
            Ok(())
        })?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
enum Command {
    Status,
    Stop,
    ResetInput,
    ApplyLayout {
        peer: Option<usize>,
        layout: Layout,
        expected_revision: u64,
    },
    AnswerPairing {
        id: u64,
        accept: bool,
    },
    SetSound {
        peer: usize,
        on: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    #[serde(flatten)]
    command: Command,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Reply<T> {
    Ok { ok: T },
    Error { error: String },
}

fn reply<T: Serialize>(result: Result<T>) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&match result {
        Ok(ok) => Reply::Ok { ok },
        Err(error) => Reply::Error {
            error: format!("{error:#}"),
        },
    })?)
}

pub(crate) fn serve(
    listener: TcpListener,
    config: Config,
    status: Arc<Status>,
    sessions: Arc<crate::session::Control>,
    shutdown: Arc<crate::shutdown::Signal>,
) -> Result<()> {
    for accepted in listener.incoming() {
        let Ok(mut stream) = accepted else {
            continue;
        };
        // A malformed or abandoned local request must not stop the server.
        if stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(2))))
            .is_err()
        {
            continue;
        }
        // One deadline for the whole request: a client that trickles bytes
        // must not hold the channel the window and tray depend on.
        let deadline = Instant::now() + Duration::from_secs(2);
        let request = Request::deserialize(&mut serde_json::Deserializer::from_reader(
            DeadlineReader {
                stream: &stream,
                deadline,
            }
            .take(16_384),
        ));
        let mut stopping = false;
        let response = match request {
            Ok(request) if same_secret(&request.token, &config.control_token) => match request
                .command
            {
                Command::Status => reply(Ok(status.snapshot()))?,
                Command::Stop => {
                    // Stop even when cleanup is stuck. Staying up would
                    // refuse every connection until something killed this
                    // process; exiting ends whatever is stuck.
                    if let Err(error) = sessions.retire(true) {
                        log::warn!("stopping anyway: {error:#}");
                    }
                    stopping = true;
                    reply(Ok(()))?
                }
                Command::ResetInput => reply(sessions.retire(false))?,
                Command::ApplyLayout {
                    peer,
                    layout,
                    expected_revision,
                } => reply((|| {
                    status
                        .layouts(peer)?
                        .request(layout, expected_revision)?
                        .recv_timeout(Duration::from_secs(5))?
                        .map_err(anyhow::Error::msg)
                })())?,
                Command::AnswerPairing { id, accept } => reply(status.pairing.answer(id, accept))?,
                Command::SetSound { peer, on } => reply(
                    config
                        .peers
                        .get(peer)
                        .context("That computer is not part of the running configuration")
                        .and_then(|saved| status.set_sound(peer, on, &saved.code)),
                )?,
            },
            _ => reply::<()>(Err(anyhow::anyhow!("unauthorized")))?,
        };
        let _ = stream.write_all(&response);
        if stopping {
            shutdown.stop();
            return Ok(());
        }
    }
    Ok(())
}

/// Reads from a socket until a fixed deadline, however the bytes arrive.
struct DeadlineReader<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        (&mut &*self.stream).read(buffer)
    }
}

/// Compare secrets without revealing through timing how much matched.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Client for the local control channel of the agent configured by `config`.
pub struct Agent<'a> {
    config: &'a Config,
}

pub fn agent(config: &Config) -> Agent<'_> {
    Agent { config }
}

impl Agent<'_> {
    fn call<T: DeserializeOwned>(&self, command: Command, timeout: Duration) -> Result<T> {
        let mut stream = TcpStream::connect_timeout(
            &([127, 0, 0, 1], self.config.listen_port + 2).into(),
            Duration::from_millis(300),
        )?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(&serde_json::to_vec(&Request {
            token: self.config.control_token.clone(),
            command,
        })?)?;
        let mut raw = Vec::new();
        // `timeout` bounds the whole reply, so whatever answers on this port
        // cannot keep the settings window waiting.
        DeadlineReader {
            stream: &stream,
            deadline: Instant::now() + timeout,
        }
        .take(64 * 1024 * 1024)
        .read_to_end(&mut raw)?;
        match serde_json::from_slice::<Reply<T>>(&raw).context("invalid agent reply")? {
            Reply::Ok { ok } => Ok(ok),
            Reply::Error { error } => bail!("{error}"),
        }
    }

    pub fn status(&self) -> Result<Snapshot> {
        self.call(Command::Status, Duration::from_millis(800))
    }

    pub fn stop(&self) -> Result<()> {
        self.call(Command::Stop, Duration::from_secs(5))
    }

    pub fn reset_input(&self) -> Result<()> {
        self.call(Command::ResetInput, Duration::from_secs(5))
    }

    pub fn apply_layout(
        &self,
        peer: Option<usize>,
        layout: Layout,
        expected_revision: u64,
    ) -> Result<layout::Report> {
        self.call(
            Command::ApplyLayout {
                peer,
                layout,
                expected_revision,
            },
            Duration::from_secs(6),
        )
    }

    pub fn answer_pairing(&self, id: u64, accept: bool) -> Result<()> {
        self.call(
            Command::AnswerPairing { id, accept },
            Duration::from_secs(3),
        )
    }

    /// Play controlled computer `peer`'s sound here, or stop.
    pub fn set_sound(&self, peer: usize, on: bool) -> Result<()> {
        self.call(Command::SetSound { peer, on }, Duration::from_secs(3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_agent(response: &'static str) -> (Config, std::thread::JoinHandle<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            listen_port: listener.local_addr().unwrap().port() - 2,
            ..Config::default()
        };
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request = serde_json::Value::deserialize(
                &mut serde_json::Deserializer::from_reader(&mut socket),
            )
            .unwrap();
            socket.write_all(response.as_bytes()).unwrap();
            request
        });
        (config, server)
    }

    #[test]
    fn layout_client_decodes_confirmations_and_rejections() {
        let (config, server) = fake_agent(
            r#"{"ok":{"layout":{"side":"right","edge_offset":0.0,"local_monitor":1,"remote_monitor":2},"revision":18446744073709551614,"ready":true,"pending":false,"error":""}}"#,
        );
        let report = agent(&config)
            .apply_layout(Some(1), Layout::of(&config.arrangement), 42)
            .unwrap();
        let request = server.join().unwrap();
        assert_eq!(request["command"], "apply-layout");
        assert_eq!(request["peer"], 1);
        assert_eq!(request["expected_revision"], 42);
        assert_eq!(request["token"], config.control_token);
        assert_eq!(report.revision, 18_446_744_073_709_551_614);
        assert!(report.ready && !report.pending);
        assert_eq!(report.layout.remote_monitor, Some(2));

        let (config, server) =
            fake_agent(r#"{"error":"Return control before changing the arrangement"}"#);
        let error = agent(&config)
            .apply_layout(None, Layout::of(&config.arrangement), 1)
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            error.to_string(),
            "Return control before changing the arrangement"
        );
    }

    #[test]
    fn unit_commands_use_kebab_case_names() {
        let (config, server) = fake_agent(r#"{"ok":null}"#);
        agent(&config).reset_input().unwrap();
        assert_eq!(server.join().unwrap()["command"], "reset-input");
        let (config, server) = fake_agent(r#"{"ok":null}"#);
        agent(&config).answer_pairing(7, true).unwrap();
        let request = server.join().unwrap();
        assert_eq!(request["command"], "answer-pairing");
        assert_eq!(request["id"], 7);
        assert_eq!(request["accept"], true);
        let (config, server) = fake_agent(r#"{"ok":null}"#);
        agent(&config).set_sound(1, true).unwrap();
        let request = server.join().unwrap();
        assert_eq!(request["command"], "set-sound");
        assert_eq!(request["peer"], 1);
        assert_eq!(request["on"], true);
    }
}
