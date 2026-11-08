use crate::{
    clipboard,
    config::{Config, Link, config_dir, config_path},
    discovery,
    input::{Controller, InputQueue, Receiver},
    layout,
    network::{Admission, SecureConnection},
    protocol::{Input, PROTOCOL_VERSION, monitors},
    session,
    shutdown::Signal,
    sound,
    status::{self, LinkHandle, NearbyPeer, Stats, Status},
    wake,
};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

const INPUT_HEARTBEAT: Duration = Duration::from_millis(250);
const INPUT_DEADLINE: Duration = Duration::from_secs(1);
/// Most connections allowed to be mid-handshake on one listener.
const HANDSHAKE_LIMIT: usize = 4;
/// Reconnection backoff for an outgoing link. A computer that stays
/// unreachable is tried once a minute; hearing its beacon again, or pushing
/// the pointer against its edge, brings the next try forward.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// After a wake request, try this often for as long as a computer may take
/// to wake up and start Seam again.
const RETRY_WAKING: Duration = Duration::from_secs(1);
const WAKING: Duration = Duration::from_secs(60);
const INCOMPATIBLE: &str =
    "The other computer runs a different Seam version. Update Seam on both computers.";
/// The other computer accepted the pairing secret, then hung up before it
/// introduced itself. That says nothing about its version: it may be busy
/// with another computer or stuck closing an earlier connection. The window
/// looks for this text to name the computer.
pub const CLOSED_WHILE_CONNECTING: &str =
    "The other computer closed the connection while connecting";

struct AgentLock {
    _file: File,
}

impl AgentLock {
    fn acquire() -> Result<Self> {
        let directory = config_dir()?;
        fs::create_dir_all(&directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("agent.lock"))?;
        file.try_lock()
            .context("another Seam agent is already running or its lock is unavailable")?;
        Ok(Self { _file: file })
    }
}

/// Everything one connection's threads share.
#[derive(Clone)]
struct LinkContext {
    link: Link,
    controller: Arc<Controller>,
    stats: Arc<Stats>,
    layouts: Arc<layout::Service>,
    sessions: Arc<session::Control>,
    sound: Arc<sound::Link>,
}

pub fn run(mut config: Config) -> Result<()> {
    config.validate()?;
    let agent_lock = AgentLock::acquire()?;
    let local_monitors = monitors().context("reading local displays")?;
    if local_monitors.is_empty() {
        bail!("no local displays detected");
    }
    config.restore_display_identities(&local_monitors);
    config.save()?;
    // Own the local control port for the entire capture lifetime. A second
    // agent must fail before installing another global input hook/tap.
    let control_port = config.listen_port + 2;
    let control_owner = TcpListener::bind(("127.0.0.1", control_port)).map_err(|error| {
        anyhow::anyhow!(crate::ports::unavailable(
            control_port,
            crate::ports::Protocol::Tcp,
            &error
        ))
    })?;
    let control_server = control_owner.try_clone()?;
    log::info!(
        "Seam agent {} starting with {} display(s) and {} controlled computer(s)",
        env!("CARGO_PKG_VERSION"),
        local_monitors.len(),
        config.peers.len()
    );
    // Held until the agent stops, so pausing sharing lets the computer sleep.
    let _awake = if config.keep_awake {
        match crate::power::StayAwake::new(config.keep_screen_on) {
            Ok(awake) => {
                if config.keep_screen_on {
                    log::info!("keeping this computer awake, screen on, while Seam runs");
                } else {
                    log::info!("keeping this computer awake while Seam runs");
                }
                Some(awake)
            }
            Err(error) => {
                log::warn!("cannot keep this computer awake: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let controller = Controller::new(&config);
    let sessions = session::Control::new(controller.clone());
    let shutdown = Arc::new(Signal::default());
    let path = config_path()?;
    let links: Vec<LinkContext> = Link::all(&config)
        .into_iter()
        .enumerate()
        .map(|(index, link)| {
            let (controller, sessions) = if index == 0 {
                (controller.clone(), sessions.clone())
            } else {
                let peer_controller = controller.for_peer(index);
                let peer_sessions = session::Control::new(peer_controller.clone());
                sessions.add_peer(peer_sessions.clone());
                (peer_controller, peer_sessions)
            };
            LinkContext {
                layouts: layout::Service::new(
                    link.clone(),
                    local_monitors.clone(),
                    controller.clone(),
                    path.clone(),
                ),
                sound: sound::Link::new(link.remote().is_some_and(|peer| peer.sound)),
                link,
                controller,
                stats: Stats::new(),
                sessions,
            }
        })
        .collect();
    let beacons: Vec<_> = links.iter().map(|context| beacon(&context.link)).collect();
    let status = Status::new(
        &config,
        links
            .iter()
            .zip(&beacons)
            .map(|(context, beacon)| LinkHandle {
                peer: context.link.peer,
                stats: context.stats.clone(),
                controller: context.controller.clone(),
                layouts: context.layouts.clone(),
                beacon: beacon.clone(),
                sound: context.sound.clone(),
            })
            .collect(),
    );
    #[cfg(windows)]
    let sign_in_helper = crate::windows_signin::is_helper();
    #[cfg(not(windows))]
    let sign_in_helper = false;
    watch_displays(
        links.iter().map(|c| c.layouts.clone()).collect(),
        shutdown.clone(),
    );
    {
        let (config, status, sessions, shutdown) = (
            config.clone(),
            status.clone(),
            sessions.clone(),
            shutdown.clone(),
        );
        thread::spawn(move || {
            if let Err(e) = status::serve(control_server, config, status, sessions, shutdown) {
                log::error!("local control: {e:#}");
            }
        });
    }
    crate::watchdog::spawn(config.clone(), shutdown.clone());
    {
        let (config, status) = (config.clone(), status.clone());
        thread::spawn(move || {
            if let Err(e) = discovery::run(config, status, !sign_in_helper) {
                log::warn!("LAN discovery disabled: {e:#}");
            }
        });
    }
    if config.peers.is_empty() && !sign_in_helper {
        let (config, status, shutdown) = (config.clone(), status.clone(), shutdown.clone());
        thread::spawn(move || {
            if let Err(e) = crate::pairing::serve(config, status, shutdown) {
                log::warn!("pairing unavailable: {e:#}");
            }
        });
    }
    #[cfg(windows)]
    if config.peers.is_empty() && !sign_in_helper {
        watch_lock(
            links.iter().map(|c| c.layouts.clone()).collect(),
            shutdown.clone(),
        );
    }
    for (context, beacon) in links.into_iter().zip(beacons) {
        if let Some((_, alarm)) = beacon {
            let (status, shutdown) = (status.clone(), shutdown.clone());
            thread::spawn(move || connect_loop(context, alarm, status, shutdown));
        } else {
            let (data, data_status) = (context.clone(), status.clone());
            thread::spawn(move || listen_data(data, &data_status));
            let (sound, sound_status) = (context.clone(), status.clone());
            thread::spawn(move || listen_sound(sound, &sound_status, sign_in_helper));
            let status = status.clone();
            thread::spawn(move || listen_input(context, &status));
        }
    }
    let result = crate::input::run_capture(controller, shutdown.clone());
    shutdown.stop();
    // Exiting ends any cleanup that is stuck. Only a capture failure is a
    // crash: a requested stop must not look like one and be restarted.
    if let Err(error) = sessions.retire(true) {
        log::warn!("exiting before cleanup finished: {error:#}");
    }
    drop(control_owner);
    drop(agent_lock);
    result
}

/// Follow display changes; a changed desktop reconnects every link so both
/// computers exchange fresh geometry.
///
/// A cheap look at the layout each second decides whether to read the
/// displays in full. macOS also announces changes, which are then read at
/// once, but some versions never deliver the announcement. Waiting for it
/// alone left a Mac describing the displays it had when Seam started, such as
/// its built-in screen after an external one took over, with part of the
/// real screen out of the pointer's reach.
fn watch_displays(layouts: Vec<Arc<layout::Service>>, shutdown: Arc<Signal>) {
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let changes = crate::mac_displays::Watch::new(&shutdown)
            .map_err(|error| {
                log::warn!("Display change announcements unavailable: {error:#}");
            })
            .ok();
        let mut watch = crate::display_watch::LayoutWatch::default();
        while !shutdown.is_stopped() {
            let now = Instant::now();
            #[cfg(target_os = "windows")]
            let placement = crate::windows_displays::layout();
            #[cfg(target_os = "macos")]
            let placement = crate::mac_displays::active().ok();
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let placement: Option<()> = None;
            if watch.needs_read(placement.as_ref(), now) {
                match crate::protocol::monitor_geometry(&layouts[0].local_monitors()) {
                    // macOS lists no display while they all sleep. That is no
                    // desktop to describe: keep the last one, so the pointer
                    // can still cross to the computer and wake its screen.
                    Ok(displays) if displays.is_empty() => watch.read(placement, now),
                    Ok(displays) => {
                        watch.read(placement, now);
                        for layout in &layouts {
                            layout.update_monitors(displays.clone());
                        }
                    }
                    Err(error) => {
                        // Try again in a second, whatever the layout.
                        watch.read(None, now);
                        for layout in &layouts {
                            layout.disconnect(&format!("Cannot read displays: {error}"));
                        }
                    }
                }
            }
            #[cfg(target_os = "macos")]
            if let Some(changes) = &changes {
                if changes.wait(Duration::from_secs(1)) {
                    // Announced: read in full even if the layout looks the same.
                    watch = crate::display_watch::LayoutWatch::default();
                }
                continue;
            }
            shutdown.wait(Duration::from_secs(1));
        }
    });
}

/// End an incoming session when Windows locks, so the controlling computer
/// reconnects to the sign-in helper and can type the password.
#[cfg(windows)]
fn watch_lock(layouts: Vec<Arc<layout::Service>>, shutdown: Arc<Signal>) {
    thread::spawn(move || {
        let mut was_locked = false;
        while !shutdown.wait(Duration::from_millis(500)) {
            let locked = crate::windows_signin::session_locked();
            if locked && !was_locked {
                for layout in &layouts {
                    layout.disconnect("This computer is locked");
                }
            }
            was_locked = locked;
        }
    });
}

/// Record why this computer cannot be connected to, for the log and window.
fn listen_failed(context: &LinkContext, status: &Status, port: u16, error: &std::io::Error) {
    let problem = crate::ports::unavailable(port, crate::ports::Protocol::Tcp, error);
    log::error!("{problem}");
    context.stats.set_error(problem.clone());
    status.report_problem(&problem);
}

fn listen_input(context: LinkContext, status: &Status) {
    let port = context.link.config.listen_port;
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(v) => v,
        Err(e) => {
            listen_failed(&context, status, port, &e);
            return;
        }
    };
    log::info!("listening for input on port {port}");
    let admission = Admission::new(HANDSHAKE_LIMIT);
    for accepted in listener.incoming() {
        let Ok(socket) = accepted else {
            continue;
        };
        let peer = socket.peer_addr().ok();
        if context.stats.connected.load(Ordering::Acquire) {
            continue;
        }
        // While Windows is locked the sign-in helper takes input instead;
        // this agent cannot type into the lock screen.
        #[cfg(windows)]
        if !crate::windows_signin::is_helper() && crate::windows_signin::session_locked() {
            continue;
        }
        let Some(admitted) = admission.enter() else {
            continue;
        };
        let context = context.clone();
        thread::spawn(move || {
            log::info!("input connection from {peer:?}");
            let accepted = SecureConnection::accept(socket, &context.link.config.pair_code);
            drop(admitted);
            match accepted {
                Ok(conn) => {
                    if let Err(e) = run_input(conn, &context) {
                        log::warn!("input session: {e:#}");
                    }
                }
                Err(e) => log::warn!("rejected input connection: {e:#}"),
            }
        });
    }
}

fn listen_data(context: LinkContext, status: &Status) {
    let port = context.link.config.listen_port + 1;
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(v) => v,
        Err(e) => {
            listen_failed(&context, status, port, &e);
            return;
        }
    };
    let admission = Admission::new(HANDSHAKE_LIMIT);
    for accepted in listener.incoming() {
        let Ok(socket) = accepted else {
            continue;
        };
        let Some(admitted) = admission.enter() else {
            continue;
        };
        let config = context.link.config.clone();
        let signals =
            clipboard::InputSignals::new(context.controller.clone(), context.stats.clone());
        let sessions = context.sessions.clone();
        thread::spawn(move || {
            let result = (|| {
                let conn = SecureConnection::accept(socket, &config.pair_code);
                drop(admitted);
                let conn = conn?;
                let (reader, writer, lease) = sessions.accept_data(conn)?;
                clipboard::run_session(reader, writer, config, false, lease.stop.clone(), signals)
            })();
            if let Err(e) = result {
                log::info!("clipboard session ended: {e:#}");
            }
        });
    }
}

/// Accept the controlling computer's sound connection, and record and send
/// this computer's sound while it stays open. The Windows sign-in screen has
/// no sound of the person's to share, and says so.
fn listen_sound(context: LinkContext, status: &Status, sign_in_screen: bool) {
    let port = context.link.config.listen_port + sound::PORT_OFFSET;
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(v) => v,
        Err(e) => {
            // Keyboard, mouse and clipboard work on without it.
            let problem = crate::ports::unavailable(port, crate::ports::Protocol::Tcp, &e);
            log::warn!("this computer's sound can't be shared: {problem}");
            status.report_problem(&problem);
            return;
        }
    };
    let admission = Admission::new(HANDSHAKE_LIMIT);
    for accepted in listener.incoming() {
        let Ok(socket) = accepted else {
            continue;
        };
        let Some(admitted) = admission.enter() else {
            continue;
        };
        let code = context.link.config.pair_code.clone();
        let (sessions, link) = (context.sessions.clone(), context.sound.clone());
        thread::spawn(move || {
            let result = (|| {
                let conn = SecureConnection::accept(socket, &code);
                drop(admitted);
                let (reader, writer, lease) = sessions.accept(conn?, session::Channel::Sound)?;
                sound::serve(reader, writer, lease.stop.clone(), link, sign_in_screen)
            })();
            match result {
                Ok(()) => log::info!("sound sharing stopped"),
                Err(e) => log::info!("sound sharing ended: {e:#}"),
            }
        });
    }
}

/// The pairing hint an outgoing link's computer puts in its beacons, and the
/// alarm its connection thread waits on: the input hooks ring it to wake the
/// computer, discovery when the computer is heard again. `None` for the link
/// a controlled computer listens on.
fn beacon(link: &Link) -> Option<(String, Arc<wake::Alarm>)> {
    let peer = link.remote()?;
    let alarm = wake::Alarm::new(
        peer.mac.as_deref().and_then(wake::parse_mac),
        peer.address.trim().parse().ok(),
    );
    Some((discovery::pairing_id(&peer.code), Arc::new(alarm)))
}

fn connect_loop(
    context: LinkContext,
    alarm: Arc<wake::Alarm>,
    status: Arc<Status>,
    shutdown: Arc<Signal>,
) {
    let peer = context
        .link
        .remote()
        .expect("outgoing link has a peer")
        .clone();
    let _stop_waiting = {
        let alarm = alarm.clone();
        shutdown.register(move || alarm.stop())
    };
    // Retry quickly after a working session ends; back off while the other
    // computer stays unreachable so an offline peer costs almost nothing.
    let mut delay = RETRY_MIN;
    let mut wake_request = None;
    while !shutdown.is_stopped() {
        if let Some((mac, address)) = wake_request.take() {
            match wake::send(&mac, address) {
                Ok(()) => log::info!(
                    "waking {}: sent Wake-on-LAN to {}",
                    peer.label(),
                    wake::format_mac(&mac)
                ),
                Err(error) => log::warn!("cannot wake {}: {error}", peer.label()),
            }
        }
        // Read again each time: the displays may have changed meanwhile.
        if context.link.config.wake_at_edge {
            context
                .controller
                .watch_for_sleep(sleeping_edge(&context), &alarm);
        }
        let addresses = connection_candidates(&peer, &status.nearby.read().unwrap());
        for address in addresses {
            if shutdown.is_stopped() {
                break;
            }
            match SecureConnection::connect(&address, &peer.code)
                .with_context(|| format!("Cannot reach {} at {address}", peer.label()))
            {
                Ok(input) => {
                    // Connected, hearing it again before now means nothing.
                    alarm.take_heard();
                    if let Ok(remote) = input.peer_addr() {
                        let (code, alarm) = (peer.code.clone(), alarm.clone());
                        thread::spawn(move || remember_wake_address(&code, remote.ip(), &alarm));
                    }
                    let started = Instant::now();
                    if let Err(error) = run_input(input, &context) {
                        context.stats.set_error(format!("{error:#}"));
                        log::warn!("connection to {} at {address}: {error:#}", peer.label());
                    }
                    if started.elapsed() >= Duration::from_secs(10) {
                        delay = RETRY_MIN;
                    }
                    break;
                }
                Err(error) => {
                    context.stats.set_error(format!("{error:#}"));
                    log::debug!("connection: {error:#}");
                }
            }
        }
        let waking = alarm.rung_within(WAKING);
        wake_request = alarm.wait(if waking { RETRY_WAKING } else { delay });
        // Heard again, the computer may still be starting Seam: try soon
        // and often before backing off once more.
        delay = if alarm.take_heard() {
            RETRY_MIN
        } else {
            (delay * 2).min(RETRY_MAX)
        };
    }
}

/// The edge a controlled computer sits on, as last arranged, for waking it.
fn sleeping_edge(context: &LinkContext) -> Option<crate::geometry::Geometry> {
    let mut arrangement = crate::config::Arrangement::default();
    context.layouts.report().layout.write_to(&mut arrangement);
    crate::geometry::Geometry::local_edge(&arrangement, &context.layouts.local_monitors())
        .map(|geometry| geometry.with_kept_corners(context.link.config.keep_corners))
}

/// Learn the hardware address a controlled computer is reached through while
/// it is awake and connected, and save it for waking it later. A private
/// Wi-Fi address can change, so this is read on every connection.
fn remember_wake_address(code: &str, address: std::net::IpAddr, alarm: &wake::Alarm) {
    let address = address.to_canonical();
    let Some(mac) = wake::neighbor_mac(address) else {
        log::debug!("no hardware address known for {address}");
        return;
    };
    alarm.aim(mac, address);
    if cfg!(test) {
        return;
    }
    let text = wake::format_mac(&mac);
    if let Err(error) = crate::config::edit(|saved| {
        if let Some(peer) = saved.peers.iter_mut().find(|peer| peer.code == code)
            && peer.mac.as_deref() != Some(text.as_str())
        {
            log::info!("{} can be woken through {text}", peer.label());
            peer.mac = Some(text.clone());
        }
        Ok(())
    }) {
        log::warn!("cannot save a computer's wake address: {error:#}");
    }
}

/// The saved address first, then addresses discovery saw announcing this
/// computer's secret, so a computer whose LAN address changed is still found.
/// Beacons are not authenticated: a forged one must never delay the saved
/// address, and only a few are tried.
fn connection_candidates(peer: &crate::config::Peer, nearby: &[NearbyPeer]) -> Vec<String> {
    let hint = discovery::pairing_id(&peer.code);
    let mut addresses = vec![address_with_port(&peer.address, peer.port)];
    for found in nearby.iter().filter(|found| found.pairing_id == hint) {
        let address = address_with_port(&found.address, found.port);
        if !addresses.contains(&address) {
            addresses.push(address);
        }
        if addresses.len() > 3 {
            break;
        }
    }
    addresses
}

fn connect_data(
    address: SocketAddr,
    config: Config,
    code: String,
    lease: session::DataLease,
    signals: clipboard::InputSignals,
) {
    while !lease.stop.is_stopped() {
        let result = (|| {
            let socket = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
            let (reader, writer) = lease.connect(socket, &code)?;
            clipboard::run_session(
                reader,
                writer,
                config.clone(),
                true,
                lease.stop.clone(),
                signals.clone(),
            )
        })();
        if lease.stop.is_stopped() {
            break;
        }
        // One wording for a hang-up, whether it arrives as an end of stream
        // or a reset, so a computer refusing every second logs one line.
        match result {
            Err(error) if closed_by_peer(&error) => {
                log::info!("clipboard reconnecting: the other computer closed the connection");
            }
            Err(error) => log::info!("clipboard reconnecting: {error:#}"),
            Ok(()) => {}
        }
        lease.stop.wait(Duration::from_secs(1));
    }
}

/// The other computer closed or reset the connection.
fn closed_by_peer(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            )
        })
    })
}

pub(crate) fn address_with_port(host: &str, port: u16) -> String {
    let host = host.trim();
    if let Ok(mut socket) = host.parse::<std::net::SocketAddr>() {
        socket.set_port(port);
        socket.to_string()
    } else if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn run_input(conn: SecureConnection, context: &LinkContext) -> Result<()> {
    let stats = &context.stats;
    let _lease = context.sessions.begin(&conn)?;
    stats.set_error("");
    let result = input_session(conn, context);
    context.layouts.detach();
    if let Err(error) = &result {
        stats.set_error(format!("{error:#}"));
    }
    stats.connected.store(false, Ordering::Release);
    stats.rtt_us.store(u64::MAX, Ordering::Relaxed);
    stats.best_rtt_us.store(u64::MAX, Ordering::Relaxed);
    stats.rtt_samples.fetch_add(1, Ordering::Release);
    result
}

/// Record which computer controls this one, shown in the window and used
/// for the Windows sign-in screen.
fn remember_controller(name: &str) {
    if cfg!(test) {
        return;
    }
    let name = name.chars().take(64).collect::<String>();
    if let Err(error) = crate::config::edit(|saved| {
        saved.controller_name = name;
        Ok(())
    }) {
        log::warn!("cannot save the controlling computer's name: {error:#}");
    }
}

/// Keep a controlled computer's name current, e.g. after it was renamed.
fn remember_peer_name(code: &str, name: &str) {
    if cfg!(test) || name.trim().is_empty() {
        return;
    }
    let name = name.chars().take(64).collect::<String>();
    if let Err(error) = crate::config::edit(|saved| {
        if let Some(peer) = saved.peers.iter_mut().find(|peer| peer.code == code) {
            peer.name = name;
        }
        Ok(())
    }) {
        log::warn!("cannot save a computer's name: {error:#}");
    }
}

/// Skip obsolete absolute positions already received together. A button, key,
/// scroll or session message is an ordering boundary and is retained for the
/// next iteration. The callback must never wait for more network input.
#[cfg(any(target_os = "macos", test))]
fn latest_pointer(
    mut current: Input,
    mut read_buffered: impl FnMut() -> Result<Option<Input>>,
    pending: &mut Option<Input>,
    stats: &Stats,
) -> Result<Input> {
    // Kernel read-ahead can keep finding new arrivals. Bound each catch-up so
    // continuous motion cannot starve native posting indefinitely.
    for _ in 0..256 {
        let Some(next) = read_buffered()? else { break };
        if matches!(next, Input::Pointer { .. }) {
            stats.received_events.fetch_add(1, Ordering::Relaxed);
            current = next;
        } else {
            *pending = Some(next);
            break;
        }
    }
    Ok(current)
}

fn input_session(conn: SecureConnection, context: &LinkContext) -> Result<()> {
    let LinkContext {
        link,
        controller,
        stats,
        layouts,
        sessions,
        sound,
    } = context;
    let config = &link.config;
    #[cfg(target_os = "macos")]
    let _priority = crate::input_priority::InputPriority::enter();
    #[cfg(target_os = "windows")]
    let _priority = crate::windows_priority::InputPriority::enter();
    let _traffic = match conn.classify_input() {
        Ok(traffic) => Some(traffic),
        Err(error) => {
            log::warn!("input traffic classification unavailable; using default traffic: {error}");
            None
        }
    };
    let peer_address = conn.peer_addr()?.to_string();
    let binding = conn.binding();
    // Clipboard and sound use the ports after the one input reached, which
    // may be the Windows sign-in screen's rather than the computer's own.
    let beside_input = |offset: u16| -> Result<Option<SocketAddr>> {
        if !link.connects() {
            return Ok(None);
        }
        let mut address = conn.peer_addr()?;
        address.set_port(address.port().checked_add(offset).context("invalid port")?);
        Ok(Some(address))
    };
    let data_target = beside_input(1)?;
    let sound_target = beside_input(sound::PORT_OFFSET)?;
    let (mut reader, mut writer) = conn.split()?;
    let queue = Arc::new(InputQueue::new());
    let ping_time = Arc::new(Mutex::new(None::<(u64, Instant)>));
    let writer_queue = queue.clone();
    let writer_ping = ping_time.clone();
    let name = config.name.clone();
    let hub = config.peers.len() > 1;
    let local = layouts.local_monitors();
    let announced_local = local.clone();
    let writer_layouts = layouts.clone();
    writer.set_timeout(Duration::from_secs(3))?;
    reader.set_timeout(INPUT_DEADLINE)?;
    let send_thread = thread::spawn(move || -> Result<()> {
        #[cfg(target_os = "macos")]
        let _priority = crate::input_priority::InputPriority::enter();
        #[cfg(target_os = "windows")]
        let _priority = crate::windows_priority::InputPriority::enter();
        let result = (|| {
            writer.write(&Input::Hello {
                version: PROTOCOL_VERSION,
                name,
                os: std::env::consts::OS.into(),
                monitors: local,
                hub,
            })?;
            let mut seq = 0u64;
            let mut last_ping = Instant::now();
            loop {
                // Queue notifications wake input immediately. When idle, sleep
                // until the next heartbeat instead of polling between deadlines.
                let remaining = INPUT_HEARTBEAT.saturating_sub(last_ping.elapsed());
                if let Some(event) = writer_queue.pop(remaining) {
                    writer.write(&event)?;
                } else if writer_queue.is_closed() {
                    break;
                }
                if last_ping.elapsed() >= INPUT_HEARTBEAT {
                    writer_layouts.tick();
                    seq += 1;
                    *writer_ping.lock().unwrap() = Some((seq, Instant::now()));
                    writer.write(&Input::Ping(seq))?;
                    last_ping = Instant::now();
                }
            }
            Ok(())
        })();
        let _ = writer.shutdown();
        result
    });
    // Apply input on the reader thread with no extra timer or worker wakeup.
    // On macOS, collapse consecutive positions already buffered after a burst
    // so WindowServer does not have to present an obsolete cursor trail.
    let mut receiver = Receiver::new(controller.clone());
    let mut introduced = false;
    let mut pending = None;
    let result = loop {
        let message: Input = match pending.take().map_or_else(|| reader.read(), Ok) {
            Ok(v) => v,
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
                }) =>
            {
                break Err(error.context("Input peer stopped responding; releasing shared input"));
            }
            // Another version's introduction may not decode at all, but a
            // computer that hangs up first said nothing about its version.
            Err(e) if !introduced && closed_by_peer(&e) => {
                break Err(anyhow::anyhow!(CLOSED_WHILE_CONNECTING));
            }
            Err(e) if !introduced => break Err(e.context(INCOMPATIBLE)),
            Err(e) => break Err(e),
        };
        if !introduced && !matches!(message, Input::Hello { .. }) {
            break Err(anyhow::anyhow!(
                "Input arrived before the peer introduction"
            ));
        }
        match message {
            Input::Hello {
                version,
                name,
                os,
                monitors: remote,
                hub,
            } => {
                if version != PROTOCOL_VERSION {
                    break Err(anyhow::anyhow!(INCOMPATIBLE));
                }
                match link.remote() {
                    Some(peer) if peer.name != name => remember_peer_name(&peer.code, &name),
                    Some(_) => {}
                    None => {
                        controller.set_controller_is_hub(hub);
                        remember_controller(&name);
                    }
                }
                *stats.peer_name.write().unwrap() = name;
                *stats.peer_os.write().unwrap() = os.clone();
                *stats.remote_monitors.write().unwrap() = remote.clone();
                if let Err(error) = layouts.attach(&announced_local, remote, &os, queue.clone()) {
                    break Err(error);
                }
                *stats.peer_address.write().unwrap() = peer_address.clone();
                stats.connected.store(true, Ordering::Release);
                introduced = true;
                // A clipboard still stopping from the last connection keeps
                // this one's clipboard off; keyboard and mouse go on.
                if let Some(address) = data_target
                    && (config.clipboard || config.files || config.images)
                    && !sessions.data_lingering()
                {
                    let lease = match sessions.reserve_data(binding) {
                        Ok(lease) => lease,
                        Err(error) => break Err(error),
                    };
                    let config = config.clone();
                    let code = link
                        .remote()
                        .map(|peer| peer.code.clone())
                        .unwrap_or_default();
                    let signals = clipboard::InputSignals::new(controller.clone(), stats.clone());
                    thread::spawn(move || connect_data(address, config, code, lease, signals));
                }
                // The controlled computer's sound, played here while it's on.
                if let (Some(address), Some(peer)) = (sound_target, link.remote())
                    && !sessions.data_lingering()
                {
                    let lease = match sessions.reserve(binding, session::Channel::Sound) {
                        Ok(lease) => lease,
                        Err(error) => break Err(error),
                    };
                    let (sound, code, name) =
                        (sound.clone(), peer.code.clone(), peer.label().to_string());
                    thread::spawn(move || sound::play(sound, lease, address, code, name));
                }
            }
            Input::Layout(control) => {
                if let Err(error) = layouts.receive(control) {
                    break Err(error);
                }
            }

            Input::Ping(seq) => {
                queue.push(Input::Pong(seq));
            }
            Input::PointerLock { locked, x, y } => {
                controller.set_peer_pointer_lock(locked, x, y);
            }
            Input::Pong(seq) => {
                if let Some((sent, at)) = *ping_time.lock().unwrap()
                    && sent == seq
                {
                    let elapsed = at.elapsed().as_micros() as u64;
                    stats.rtt_us.store(elapsed, Ordering::Relaxed);
                    stats.best_rtt_us.fetch_min(elapsed, Ordering::Relaxed);
                    stats.rtt_samples.fetch_add(1, Ordering::Release);
                }
            }
            other => {
                stats.received_events.fetch_add(1, Ordering::Relaxed);
                #[cfg(target_os = "macos")]
                let other = if matches!(other, Input::Pointer { .. }) {
                    match latest_pointer(other, || reader.read_ready(), &mut pending, stats) {
                        Ok(input) => input,
                        Err(error) => break Err(error),
                    }
                } else {
                    other
                };
                receiver.apply(other);
                if let Some(report) = receiver.take_report() {
                    queue.push(report);
                }
            }
        }
    };
    // Wake a writer blocked in send before waiting for it, and discard stale
    // input. A stalled connection must not delay receiver/source restoration.
    let _ = reader.shutdown();
    queue.discard_and_close();
    receiver.release_all();
    // Give this computer its keyboard and mouse back now: the writer can take
    // seconds to notice a dead connection.
    context.layouts.detach();
    let _ = send_thread.join();
    if queue.overloaded() {
        bail!("Input queue overloaded; shared input was released and the connection will restart");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Side, layout::Layout, protocol::Monitor};
    use std::{
        io::{Read, Write},
        net::{Shutdown, SocketAddr, TcpStream},
        sync::atomic::{AtomicBool, AtomicU8},
    };

    #[test]
    fn buffered_motion_keeps_the_latest_position_before_every_input_boundary() {
        use crate::protocol::{MouseButton, ScrollUnit};
        for boundary in [
            Input::Button {
                button: MouseButton::Left,
                down: true,
            },
            Input::Button {
                button: MouseButton::Left,
                down: false,
            },
            Input::Key {
                key: rdev::Key::ShiftLeft,
                down: true,
            },
            Input::Key {
                key: rdev::Key::ShiftLeft,
                down: false,
            },
            Input::Wheel {
                dx: 0,
                dy: 1,
                unit: ScrollUnit::Lines,
            },
            Input::Leave,
            Input::ReleaseAll,
            Input::Ping(1),
        ] {
            let expected = serde_json::to_value(&boundary).unwrap();
            let mut buffered = [
                Input::Pointer { x: 2.0, y: 3.0 },
                Input::Pointer { x: 4.0, y: 5.0 },
                boundary,
                Input::Pointer { x: 6.0, y: 7.0 },
            ]
            .into_iter();
            let mut pending = None;
            let stats = Stats::new();
            let latest = latest_pointer(
                Input::Pointer { x: 0.0, y: 1.0 },
                || Ok(buffered.next()),
                &mut pending,
                &stats,
            )
            .unwrap();
            assert!(matches!(latest, Input::Pointer { x: 4.0, y: 5.0 }));
            assert_eq!(serde_json::to_value(pending.unwrap()).unwrap(), expected);
            assert!(matches!(
                buffered.next(),
                Some(Input::Pointer { x: 6.0, y: 7.0 })
            ));
            assert_eq!(stats.received_events.load(Ordering::Relaxed), 2);
        }
        let mut pending = None;
        let stats = Stats::new();
        let latest = latest_pointer(
            Input::Pointer { x: 8.0, y: 9.0 },
            || Ok(None),
            &mut pending,
            &stats,
        )
        .unwrap();
        assert!(matches!(latest, Input::Pointer { x: 8.0, y: 9.0 }));
        assert!(pending.is_none());
        assert_eq!(stats.received_events.load(Ordering::Relaxed), 0);
        assert!(
            latest_pointer(
                latest,
                || anyhow::bail!("bad encrypted frame"),
                &mut pending,
                &stats
            )
            .is_err()
        );
    }

    #[test]
    fn continuous_ready_motion_cannot_starve_native_posting() {
        let stats = Stats::new();
        let mut pending = None;
        let mut count = 0;
        let latest = latest_pointer(
            Input::Pointer { x: 0.0, y: 0.0 },
            || {
                count += 1;
                assert!(count <= 256, "catch-up never yielded to native posting");
                Ok(Some(Input::Pointer {
                    x: f64::from(count),
                    y: 0.0,
                }))
            },
            &mut pending,
            &stats,
        )
        .unwrap();
        assert!(matches!(latest, Input::Pointer { x: 256.0, y: 0.0 }));
        assert!(pending.is_none());
    }

    fn display(id: u32) -> Vec<Monitor> {
        vec![Monitor {
            id,
            identity: None,
            name: format!("Display {id}"),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        }]
    }

    fn quiet(config: Config) -> Config {
        Config {
            clipboard: false,
            files: false,
            images: false,
            ..config
        }
    }

    fn context(
        link: Link,
        controller: Arc<Controller>,
        layouts: Arc<layout::Service>,
    ) -> LinkContext {
        LinkContext {
            link,
            sessions: session::Control::new(controller.clone()),
            controller,
            stats: Stats::new(),
            layouts,
            sound: sound::Link::new(false),
        }
    }

    #[test]
    fn three_computers_negotiate_independent_layouts_and_retire_together() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("center.json");
        let satellites = [0, 1].map(|_| quiet(Config::default()));
        let center = quiet(Config {
            peers: [Side::Left, Side::Right]
                .into_iter()
                .zip(&satellites)
                .map(|(side, satellite)| crate::config::Peer {
                    name: format!("{side:?}"),
                    address: format!("127.0.0.{}", side as u8 + 1),
                    port: 47843,
                    code: satellite.pair_code.clone(),
                    arrangement: crate::config::Arrangement::on(side),
                    mac: None,
                    sound: false,
                })
                .collect(),
            ..Config::default()
        });
        center.save_to(&path).unwrap();
        let controller = Controller::new(&center);
        let root = session::Control::new(controller.clone());
        let mut layouts = Vec::new();
        let mut tasks = Vec::new();
        for (index, destination) in satellites.iter().enumerate() {
            let peer_controller = if index == 0 {
                controller.clone()
            } else {
                controller.for_peer(index)
            };
            let link = Link::outgoing(&center, index);
            let source_layout = layout::Service::new(
                link.clone(),
                display(1),
                peer_controller.clone(),
                path.clone(),
            );
            let mut source = context(link, peer_controller, source_layout.clone());
            if index == 0 {
                source.sessions = root.clone();
            } else {
                root.add_peer(source.sessions.clone());
            }
            let destination = destination.clone();
            let target_controller = Controller::new(&destination);
            let target_layout = layout::Service::new(
                Link::incoming(&destination),
                display(index as u32 + 2),
                target_controller.clone(),
                directory.path().join(format!("peer-{index}.json")),
            );
            let target = context(
                Link::incoming(&destination),
                target_controller,
                target_layout.clone(),
            );
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            tasks.push(thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                let conn = SecureConnection::accept(socket, &target.link.config.pair_code).unwrap();
                run_input(conn, &target)
            }));
            let conn =
                SecureConnection::connect(&address.to_string(), &destination.pair_code).unwrap();
            tasks.push(thread::spawn(move || run_input(conn, &source)));
            layouts.push(source_layout);
            layouts.push(target_layout);
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        while !layouts.iter().all(|layout| layout.report().ready) {
            assert!(
                Instant::now() < deadline,
                "both connections must negotiate their layouts"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let saved = Config::parse(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.peers[0].arrangement.side, Side::Left);
        assert_eq!(saved.peers[0].arrangement.remote_monitor, Some(2));
        assert_eq!(saved.peers[1].arrangement.side, Side::Right);
        assert_eq!(saved.peers[1].arrangement.remote_monitor, Some(3));
        root.retire(true).unwrap();
        for task in tasks {
            let _ = task.join().unwrap();
        }
        assert!(layouts.iter().all(|layout| !layout.report().ready));
    }

    #[test]
    fn rediscovery_prefers_paired_pc_at_new_address_and_ignores_other_peers() {
        let paired = crate::config::Peer {
            name: "PC".into(),
            address: "192.0.2.10".into(),
            port: 47843,
            code: crate::config::random_secret(),
            arrangement: Default::default(),
            mac: None,
            sound: false,
        };
        let found = |address: &str, code: &str| NearbyPeer {
            name: "PC".into(),
            address: address.into(),
            port: 47843,
            pairing_id: discovery::pairing_id(code),
            os: "windows".into(),
            accepts_pairing: true,
            controller: String::new(),
        };
        let nearby = vec![
            found("192.0.2.20", &paired.code),
            found("192.0.2.20", &paired.code),
            found("192.0.2.99", &crate::config::random_secret()),
        ];
        assert_eq!(
            connection_candidates(&paired, &nearby),
            ["192.0.2.10:47843", "192.0.2.20:47843"]
        );
        assert_eq!(connection_candidates(&paired, &[]), ["192.0.2.10:47843"]);
    }

    #[test]
    fn hearing_an_unreachable_computer_tries_it_at_once_and_backs_off_afresh() {
        // Accepts and hangs up, like a computer still starting Seam.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = quiet(Config {
            peers: vec![crate::config::Peer {
                name: "Mac".into(),
                address: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
                code: crate::config::random_secret(),
                arrangement: Default::default(),
                mac: None,
                sound: false,
            }],
            ..Config::default()
        });
        let attempts = thread::spawn(move || {
            (0..3)
                .map(|_| {
                    drop(listener.accept().unwrap());
                    Instant::now()
                })
                .collect::<Vec<_>>()
        });
        let directory = tempfile::tempdir().unwrap();
        let link = Link::outgoing(&config, 0);
        let (hint, alarm) = beacon(&link).unwrap();
        let controller = Controller::new(&config);
        let layouts = layout::Service::new(
            link.clone(),
            display(1),
            controller.clone(),
            directory.path().join("config.json"),
        );
        let context = context(link, controller.clone(), layouts.clone());
        let status = Status::new(
            &config,
            vec![LinkHandle {
                peer: Some(0),
                stats: context.stats.clone(),
                controller,
                layouts,
                beacon: Some((hint.clone(), alarm.clone())),
                sound: sound::Link::new(false),
            }],
        );
        let shutdown = Arc::new(Signal::default());
        let connecting = {
            let (status, shutdown) = (status.clone(), shutdown.clone());
            thread::spawn(move || connect_loop(context, alarm, status, shutdown))
        };
        // The first try fails at once and the next waits RETRY_MIN.
        thread::sleep(RETRY_MIN / 4);
        let heard = Instant::now();
        status.heard(&hint);
        status.heard("another computer");
        let attempts = attempts.join().unwrap();
        shutdown.stop();
        connecting.join().unwrap();
        assert!(attempts[1] - heard < RETRY_MIN / 2, "tried when heard");
        assert!(
            attempts[2] - attempts[1] < RETRY_MIN * 2 - RETRY_MIN / 4,
            "the wait after being heard starts again from RETRY_MIN"
        );
    }

    // An opaque TCP relay: it never parses or fabricates protocol messages.
    // Mode 1 drops client bytes; mode 2 delivers them one byte per 100 ms.
    struct FaultLink {
        address: SocketAddr,
        mode: Arc<AtomicU8>,
        stopped: Arc<AtomicBool>,
        sockets: Arc<Mutex<Vec<TcpStream>>>,
        task: Option<thread::JoinHandle<()>>,
    }

    impl FaultLink {
        fn new(target: SocketAddr) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let mode = Arc::new(AtomicU8::new(0));
            let stopped = Arc::new(AtomicBool::new(false));
            let sockets = Arc::new(Mutex::new(Vec::new()));
            let (gate, stop, handles) = (mode.clone(), stopped.clone(), sockets.clone());
            let task = thread::spawn(move || {
                let (mut client, mut server) = loop {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    if let Ok((client, _)) = listener.accept() {
                        break (client, TcpStream::connect(target).unwrap());
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                client.set_nonblocking(false).unwrap();
                client.set_nodelay(true).unwrap();
                server.set_nodelay(true).unwrap();
                *handles.lock().unwrap() =
                    vec![client.try_clone().unwrap(), server.try_clone().unwrap()];
                let (mut reply_source, mut reply_target) =
                    (server.try_clone().unwrap(), client.try_clone().unwrap());
                let replies = thread::spawn(move || {
                    let _ = std::io::copy(&mut reply_source, &mut reply_target);
                    let _ = reply_source.shutdown(Shutdown::Both);
                    let _ = reply_target.shutdown(Shutdown::Both);
                });
                let mut bytes = [0u8; 4096];
                'forward: while !stop.load(Ordering::Acquire) {
                    let count = match client.read(&mut bytes) {
                        Ok(count) if count > 0 => count,
                        _ => break,
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    match gate.load(Ordering::Acquire) {
                        1 => {}
                        2 => {
                            for byte in &bytes[..count] {
                                if stop.load(Ordering::Acquire)
                                    || server.write_all(&[*byte]).is_err()
                                {
                                    break 'forward;
                                }
                                thread::sleep(Duration::from_millis(100));
                            }
                        }
                        _ => {
                            if server.write_all(&bytes[..count]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = server.shutdown(Shutdown::Both);
                let _ = client.shutdown(Shutdown::Both);
                replies.join().unwrap();
            });
            Self {
                address,
                mode,
                stopped,
                sockets,
                task: Some(task),
            }
        }
    }

    impl Drop for FaultLink {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            for socket in self.sockets.lock().unwrap().iter() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            self.task.take().unwrap().join().unwrap();
        }
    }

    struct LayoutPair {
        configs: [Config; 2],
        layouts: [Arc<layout::Service>; 2],
        controllers: [Arc<Controller>; 2],
        stats: [Arc<Stats>; 2],
        sessions: [Arc<crate::session::Control>; 2],
        paths: [std::path::PathBuf; 2],
        tasks: Vec<thread::JoinHandle<Result<()>>>,
        _directory: tempfile::TempDir,
    }

    impl LayoutPair {
        fn new() -> Self {
            Self::new_via(|address| address)
        }

        fn link(configs: &[Config; 2], side: usize) -> Link {
            if side == 0 {
                Link::incoming(&configs[0])
            } else {
                Link::outgoing(&configs[1], 0)
            }
        }

        fn context(&self, side: usize) -> LinkContext {
            LinkContext {
                link: Self::link(&self.configs, side),
                controller: self.controllers[side].clone(),
                stats: self.stats[side].clone(),
                layouts: self.layouts[side].clone(),
                sessions: self.sessions[side].clone(),
                sound: sound::Link::new(false),
            }
        }

        fn new_via(route: impl FnOnce(SocketAddr) -> SocketAddr) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let paths = [
                directory.path().join("left/config.json"),
                directory.path().join("right/config.json"),
            ];
            let listener = quiet(Config::default());
            let connector = quiet(Config {
                peers: vec![crate::config::Peer {
                    name: "Left".into(),
                    address: "127.0.0.1".into(),
                    port: 47843,
                    code: listener.pair_code.clone(),
                    arrangement: Default::default(),
                    mac: None,
                    sound: false,
                }],
                ..Config::default()
            });
            let configs = [listener, connector];
            let controllers = [Controller::new(&configs[0]), Controller::new(&configs[1])];
            let layouts = std::array::from_fn(|i| {
                layout::Service::new(
                    Self::link(&configs, i),
                    vec![Monitor {
                        id: (i + 1) as u32,
                        identity: None,
                        name: format!("Display {i}"),
                        x: 0,
                        y: 0,
                        width: if i == 0 { 1920 } else { 2560 },
                        height: if i == 0 { 1080 } else { 1440 },
                        primary: true,
                    }],
                    controllers[i].clone(),
                    paths[i].clone(),
                )
            });
            let mut pair = Self {
                configs,
                layouts,
                sessions: std::array::from_fn(|i| {
                    crate::session::Control::new(controllers[i].clone())
                }),
                controllers,
                paths,
                stats: [Stats::new(), Stats::new()],
                tasks: Vec::new(),
                _directory: directory,
            };
            pair.connect_via(route);
            pair
        }

        fn connect(&mut self) {
            self.connect_via(|address| address);
        }

        fn connect_via(&mut self, route: impl FnOnce(SocketAddr) -> SocketAddr) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let listening = self.context(0);
            self.tasks.push(thread::spawn(move || {
                let (socket, _) = listener.accept()?;
                let connection =
                    SecureConnection::accept(socket, &listening.link.config.pair_code)?;
                run_input(connection, &listening)
            }));
            let connection =
                SecureConnection::connect(&route(address).to_string(), &self.configs[0].pair_code)
                    .unwrap();
            let connecting = self.context(1);
            self.tasks
                .push(thread::spawn(move || run_input(connection, &connecting)));
            self.wait_for(|p| p.layouts.iter().all(|s| s.report().revision != 0));
        }

        fn wait_for(&self, condition: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(4);
            while !condition(self) {
                assert!(
                    Instant::now() < deadline,
                    "layout state did not converge: {:?} / {:?}",
                    self.layouts[0].report(),
                    self.layouts[1].report()
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn saved_arrangement(&self, side: usize) -> crate::config::Arrangement {
            let config = Config::parse(&std::fs::read(&self.paths[side]).unwrap()).unwrap();
            if side == 0 {
                config.arrangement
            } else {
                config.peers[0].arrangement.clone()
            }
        }

        fn saved(&self, side: usize) -> Layout {
            Layout::of(&self.saved_arrangement(side))
        }

        fn join(&mut self) {
            for task in self.tasks.drain(..) {
                let _ = task.join().unwrap();
            }
        }
    }

    impl Drop for LayoutPair {
        fn drop(&mut self) {
            for service in &self.layouts {
                service.disconnect("test finished");
            }
            self.join();
        }
    }

    #[test]
    fn live_layout_changes_from_either_peer_are_mirrored_persisted_and_revision_checked() {
        let pair = LayoutPair::new();
        for (initiator, side, opposite, offset) in [
            (0, Side::Top, Side::Bottom, 0.25),
            (1, Side::Left, Side::Right, -0.5),
        ] {
            let prior_revision = pair.layouts[initiator].report().revision;
            let requested = Layout {
                side,
                edge_offset: offset,
                local_monitor: Some(initiator as u32 + 1),
                remote_monitor: Some(2 - initiator as u32),
            };
            let receipt = pair.layouts[initiator]
                .request(requested.clone(), prior_revision)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert!(
                !receipt.pending,
                "a confirmed request must no longer be pending"
            );
            pair.wait_for(|p| {
                p.layouts
                    .iter()
                    .all(|s| s.report().ready && s.report().revision == receipt.revision)
            });
            assert_eq!(pair.saved(initiator), requested);
            assert_eq!(
                pair.saved(1 - initiator),
                Layout {
                    side: opposite,
                    edge_offset: -offset,
                    local_monitor: Some(2 - initiator as u32),
                    remote_monitor: Some(initiator as u32 + 1)
                }
            );
            assert!(
                pair.stats
                    .iter()
                    .all(|s| s.connected.load(Ordering::Acquire))
            );
            let stale = pair.layouts[initiator]
                .request(requested.clone(), prior_revision)
                .unwrap_err();
            assert!(stale.to_string().contains("layout changed"));
            let invalid = Layout {
                remote_monitor: Some(999),
                ..requested.clone()
            };
            let error = pair.layouts[initiator]
                .request(invalid, receipt.revision)
                .unwrap_err();
            assert!(error.to_string().contains("display is no longer available"));
            assert_eq!(pair.saved(initiator), requested);
        }
    }

    #[test]
    fn interrupted_layout_commit_disables_handoffs_and_reconciles_after_reconnection() {
        let mut pair = LayoutPair::new();
        // A real filesystem error after the coordinator persists its decision.
        let saved_peer_config = std::fs::read(&pair.paths[0]).unwrap();
        std::fs::remove_file(&pair.paths[0]).unwrap();
        std::fs::create_dir(&pair.paths[0]).unwrap();
        let requested = Layout {
            side: Side::Bottom,
            edge_offset: 0.4,
            local_monitor: Some(1),
            remote_monitor: Some(2),
        };
        let revision = pair.layouts[0].report().revision;
        let response = pair.layouts[0]
            .request(requested.clone(), revision)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(
            response.is_err(),
            "partial commit must never report success"
        );
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
        assert!(pair.layouts.iter().all(|s| !s.report().ready));
        pair.join();
        std::fs::remove_dir(&pair.paths[0]).unwrap();
        std::fs::write(&pair.paths[0], saved_peer_config).unwrap();
        pair.connect();
        assert_eq!(pair.saved(0), requested);
        assert_eq!(
            pair.saved(1),
            Layout {
                side: Side::Top,
                edge_offset: -0.4,
                local_monitor: Some(2),
                remote_monitor: Some(1)
            }
        );
        assert_eq!(
            pair.layouts[0].report().revision,
            pair.layouts[1].report().revision
        );
    }

    #[test]
    fn reboot_remaps_both_displays_by_identity_even_when_old_handles_are_reused() {
        let mut pair = LayoutPair::new();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        // Upgrade a legacy, successfully selected layout to persistent identities.
        for side in 0..2 {
            let mut displays = pair.layouts[side].local_monitors();
            displays[0].identity = Some(format!("physical-{side}"));
            pair.layouts[side].update_monitors(displays);
        }
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
        pair.join();
        pair.connect();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        pair.layouts[0].disconnect("simulated shutdown");
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
        pair.join();
        for side in 0..2 {
            let saved = pair.saved_arrangement(side);
            assert_eq!(
                saved.local_monitor_uuid.as_deref(),
                Some(format!("physical-{side}").as_str())
            );
            assert_eq!(
                saved.remote_monitor_uuid.as_deref(),
                Some(format!("physical-{}", 1 - side).as_str())
            );
            let saved = Config::parse(&fs::read(&pair.paths[side]).unwrap()).unwrap();
            let mut displays = pair.layouts[side].local_monitors();
            let mut reused = displays[0].clone();
            reused.identity = Some(format!("unrelated-{side}"));
            displays[0].id = 50 + side as u32;
            displays.push(reused);
            pair.configs[side] = saved;
            pair.layouts[side] = layout::Service::new(
                LayoutPair::link(&pair.configs, side),
                displays,
                pair.controllers[side].clone(),
                pair.paths[side].clone(),
            );
        }
        pair.connect();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        for side in 0..2 {
            assert_eq!(pair.saved(side).local_monitor, Some(50 + side as u32));
            assert_eq!(pair.saved(side).remote_monitor, Some(51 - side as u32));
        }
        // While the remembered display is gone, the main display stands in
        // and the identity stays remembered on both computers.
        let all = pair.layouts[1].local_monitors();
        pair.layouts[1].update_monitors(all.iter().filter(|m| m.id != 51).cloned().collect());
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
        pair.join();
        pair.connect();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        assert_eq!(pair.saved(1).local_monitor, Some(2));
        assert_eq!(pair.saved(0).remote_monitor, Some(2));
        assert_eq!(
            pair.saved_arrangement(1).local_monitor_uuid.as_deref(),
            Some("physical-1")
        );
        assert_eq!(
            pair.saved_arrangement(0).remote_monitor_uuid.as_deref(),
            Some("physical-1")
        );
        pair.layouts[1].update_monitors(all);
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
        pair.join();
        pair.connect();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        assert_eq!(pair.saved(1).local_monitor, Some(51));
    }

    #[test]
    fn main_display_stands_in_for_a_removed_seam_display_until_it_returns() {
        // Unlike a normal layout edit, topology changes terminate the input
        // session. Exercise removal at each end through real encrypted sessions.
        let reconnect = |pair: &mut LayoutPair| {
            pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
            pair.join();
            pair.connect();
            pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        };
        for removed_side in 0..2 {
            let mut pair = LayoutPair::new();
            pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
            for side in 0..2 {
                let mut displays = pair.layouts[side].local_monitors();
                displays[0].identity = Some(format!("physical-{side}"));
                pair.layouts[side].update_monitors(displays);
            }
            reconnect(&mut pair);
            let original = pair.saved(removed_side);
            let remembered = pair.saved_arrangement(removed_side).local_monitor_uuid;
            assert!(remembered.is_some());

            // A laptop's lid closes: another display takes over.
            let before = pair.layouts[removed_side].local_monitors();
            let mut replacement = before.clone();
            replacement[0].id = 50;
            replacement[0].x = -1920;
            replacement[0].identity = Some("external".into());
            pair.layouts[removed_side].update_monitors(replacement);
            reconnect(&mut pair);
            assert!(pair.layouts.iter().all(|s| s.report().error.is_empty()));
            assert_eq!(pair.saved(removed_side).local_monitor, Some(50));
            assert_eq!(pair.saved(1 - removed_side).remote_monitor, Some(50));
            assert_eq!(
                pair.saved_arrangement(removed_side).local_monitor_uuid,
                remembered,
                "a display standing in must not replace the remembered one"
            );
            assert_eq!(
                pair.saved_arrangement(1 - removed_side).remote_monitor_uuid,
                remembered
            );

            // The lid opens: the edge goes back to its own display.
            pair.layouts[removed_side].update_monitors(before);
            reconnect(&mut pair);
            assert_eq!(pair.saved(removed_side), original);
        }
    }

    #[test]
    fn without_identities_the_main_display_takes_over_a_removed_seam_display() {
        for removed_side in 0..2 {
            let mut pair = LayoutPair::new();
            pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
            let mut replacement = pair.layouts[removed_side].local_monitors();
            replacement[0].id = 50;
            replacement[0].x = -1920;
            pair.layouts[removed_side].update_monitors(replacement);
            pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
            pair.join();
            pair.connect();
            pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
            assert_eq!(pair.saved(removed_side).local_monitor, Some(50));
            assert_eq!(pair.saved(1 - removed_side).remote_monitor, Some(50));
        }
    }

    #[test]
    fn stalled_or_partial_input_stream_retires_promptly_and_allows_reconnect() {
        let mut failures = Vec::new();
        for mode in [1, 2] {
            let mut link = None;
            let mut pair = LayoutPair::new_via(|address| {
                let relay = FaultLink::new(address);
                let address = relay.address;
                link = Some(relay);
                address
            });
            pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
            // An idle but responsive session must survive the same deadline.
            thread::sleep(Duration::from_millis(1250));
            assert!(
                pair.stats
                    .iter()
                    .all(|s| s.connected.load(Ordering::Acquire))
            );
            link.as_ref().unwrap().mode.store(mode, Ordering::Release);
            let deadline = Instant::now() + Duration::from_millis(1800);
            while pair.stats[0].connected.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            let retired =
                !pair.stats[0].connected.load(Ordering::Acquire) && !pair.layouts[0].report().ready;
            let reason = pair.stats[0].last_error.read().unwrap().clone();
            drop(link);
            pair.join();
            if !retired || !reason.contains("peer stopped responding") {
                failures.push(format!("mode {mode}: retired={retired}, reason={reason:?}"));
            }
            pair.connect();
            pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        }
        assert!(
            failures.is_empty(),
            "stalled input kept the session alive: {failures:?}"
        );
    }

    #[test]
    fn local_reset_retires_the_input_session_before_acknowledging_and_allows_reconnect() {
        let mut pair = LayoutPair::new();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = Config {
            listen_port: port - 2,
            ..pair.configs[0].clone()
        };
        let agent_status = Status::new(
            &config,
            vec![LinkHandle {
                peer: None,
                stats: pair.stats[0].clone(),
                controller: pair.controllers[0].clone(),
                layouts: pair.layouts[0].clone(),
                beacon: None,
                sound: sound::Link::new(false),
            }],
        );
        let (server_config, sessions) = (config.clone(), pair.sessions[0].clone());
        let shutdown = Arc::new(Signal::default());
        let control_server = TcpListener::bind(("127.0.0.1", config.listen_port + 2)).unwrap();
        let server = thread::spawn(move || {
            status::serve(
                control_server,
                server_config,
                agent_status,
                sessions,
                shutdown,
            )
            .unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while status::agent(&config).status().is_err() {
            assert!(Instant::now() < deadline, "control endpoint did not start");
            thread::sleep(Duration::from_millis(5));
        }
        status::agent(&config).reset_input().unwrap();
        assert!(
            !pair.stats[0].connected.load(Ordering::Acquire),
            "reset acknowledged while the old receiver can still inject input"
        );
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
        pair.join();
        pair.connect();
        pair.wait_for(|p| p.layouts.iter().all(|s| s.report().ready));
        status::agent(&config).stop().unwrap();
        server.join().unwrap();
        assert!(!pair.stats[0].connected.load(Ordering::Acquire));
        pair.wait_for(|p| p.stats.iter().all(|s| !s.connected.load(Ordering::Acquire)));
    }

    #[test]
    fn address_formats() {
        assert_eq!(address_with_port("192.168.1.2", 47843), "192.168.1.2:47843");
        assert_eq!(address_with_port("::1", 47843), "[::1]:47843");
    }

    #[test]
    fn hanging_up_is_not_mistaken_for_another_version() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
        ] {
            let error = anyhow::Error::from(Error::from(kind)).context("reading input");
            assert!(closed_by_peer(&error), "{kind:?}");
        }
        // An introduction that does not decode is another version.
        let undecodable = serde_json::from_str::<Input>("{\"Hello\":7}").unwrap_err();
        assert!(!closed_by_peer(&anyhow::Error::from(undecodable)));
        assert!(!closed_by_peer(&anyhow::Error::from(Error::from(
            ErrorKind::TimedOut
        ))));
    }
}
