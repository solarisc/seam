use crate::protocol::{DEFAULT_PORT, Monitor};
use anyhow::{Context, Result, ensure};
use data_encoding::BASE32_NOPAD;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Most computers a single keyboard and mouse can control: one per screen edge.
pub const MAX_PEERS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    #[default]
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    pub fn opposite(self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Left => "Left",
            Side::Right => "Right",
            Side::Top => "Above",
            Side::Bottom => "Below",
        }
    }
}

/// Where the other computer's screen sits relative to this one.
///
/// Display ids are only valid for the current desktop session; the persistent
/// OS identities let a selection survive reboots and monitor renumbering.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Arrangement {
    #[serde(default)]
    pub side: Side,
    #[serde(default)]
    pub edge_offset: f64,
    #[serde(default)]
    pub local_monitor: Option<u32>,
    #[serde(default)]
    pub local_monitor_uuid: Option<String>,
    #[serde(default)]
    pub remote_monitor: Option<u32>,
    #[serde(default)]
    pub remote_monitor_uuid: Option<String>,
}

impl Arrangement {
    pub fn on(side: Side) -> Self {
        Self {
            side,
            ..Self::default()
        }
    }

    pub(crate) fn restore_local(&mut self, local: &[Monitor]) {
        restore_display_id(
            &mut self.local_monitor,
            self.local_monitor_uuid.as_deref(),
            local,
        );
    }

    pub(crate) fn restore_remote(&mut self, remote: &[Monitor]) {
        restore_display_id(
            &mut self.remote_monitor,
            self.remote_monitor_uuid.as_deref(),
            remote,
        );
    }

    pub(crate) fn remember(&mut self, local: &[Monitor], remote: &[Monitor]) {
        remember_identity(self.local_monitor, &mut self.local_monitor_uuid, local);
        remember_identity(self.remote_monitor, &mut self.remote_monitor_uuid, remote);
    }
}

/// A computer this one controls with its own keyboard and mouse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    #[serde(default)]
    pub name: String,
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// The other computer's pairing secret, received when the two were paired.
    pub code: String,
    #[serde(flatten)]
    pub arrangement: Arrangement,
    /// The hardware address it was last reached through, learned while it
    /// was awake, for waking it from sleep. An unreadable one is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

impl Peer {
    pub fn label(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.address
        } else {
            &self.name
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    #[serde(default = "default_port")]
    pub listen_port: u16,
    /// This computer's 128-bit secret. A controlling computer receives it once
    /// while pairing and uses it to authenticate every connection.
    pub pair_code: String,
    /// Authorizes local commands from the settings window and tray.
    pub control_token: String,
    /// Computers controlled from this one. Empty when this computer is
    /// controlled by another one, or not paired yet.
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// Placement of the controlling computer when another computer controls this one.
    #[serde(default)]
    pub arrangement: Arrangement,
    /// Display name of the computer allowed to control this one.
    #[serde(default)]
    pub controller_name: String,
    /// The person paused sharing in the settings window. Only the window
    /// honours it; signing in starts sharing regardless.
    #[serde(default)]
    pub paused: bool,
    /// Never move this computer's own pointer to the computer controlling it.
    #[serde(default)]
    pub receive_only: bool,
    #[serde(default = "default_true")]
    pub cursor_without_mouse: bool,
    /// Ctrl+Alt+Shift+arrow switches to the computer on that side (Windows).
    #[serde(default = "default_true")]
    pub switch_shortcut: bool,
    /// Screen corners on a shared edge don't switch computers.
    #[serde(default = "default_true")]
    pub keep_corners: bool,
    #[serde(default = "default_true")]
    pub map_mac_command_to_ctrl: bool,
    #[serde(default = "default_true")]
    pub map_app_switch_shortcut: bool,
    #[serde(default = "default_true")]
    pub clipboard: bool,
    #[serde(default = "default_true")]
    pub files: bool,
    #[serde(default = "default_true")]
    pub images: bool,
    /// This computer doesn't go to sleep on its own while Seam runs.
    #[serde(default)]
    pub keep_awake: bool,
    /// With `keep_awake`, the screen doesn't turn off on its own either.
    #[serde(default)]
    pub keep_screen_on: bool,
    /// Pushing the pointer against a sleeping computer's edge wakes it.
    #[serde(default = "default_true")]
    pub wake_at_edge: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: computer_name(),
            listen_port: DEFAULT_PORT,
            pair_code: random_secret(),
            control_token: random_secret(),
            peers: Vec::new(),
            arrangement: Arrangement::default(),
            controller_name: String::new(),
            paused: false,
            receive_only: false,
            cursor_without_mouse: true,
            switch_shortcut: true,
            keep_corners: true,
            map_mac_command_to_ctrl: true,
            map_app_switch_shortcut: true,
            clipboard: true,
            files: true,
            images: true,
            keep_awake: false,
            keep_screen_on: false,
            wake_at_edge: true,
        }
    }
}

/// One connection's view of the configuration: this computer's preferences
/// plus the endpoint and arrangement of the computer at the other end.
#[derive(Clone, Debug)]
pub struct Link {
    pub config: Config,
    /// Index into `config.peers` when this computer connects out and controls
    /// the other one; `None` for the incoming connection from a controller.
    pub peer: Option<usize>,
    pub arrangement: Arrangement,
}

impl Link {
    pub fn incoming(config: &Config) -> Self {
        Self {
            arrangement: config.arrangement.clone(),
            config: config.clone(),
            peer: None,
        }
    }

    pub fn outgoing(config: &Config, index: usize) -> Self {
        Self {
            arrangement: config.peers[index].arrangement.clone(),
            config: config.clone(),
            peer: Some(index),
        }
    }

    /// Every link a running agent maintains for this configuration.
    pub fn all(config: &Config) -> Vec<Self> {
        if config.peers.is_empty() {
            vec![Self::incoming(config)]
        } else {
            (0..config.peers.len())
                .map(|index| Self::outgoing(config, index))
                .collect()
        }
    }

    pub fn connects(&self) -> bool {
        self.peer.is_some()
    }

    pub fn remote(&self) -> Option<&Peer> {
        self.peer.and_then(|index| self.config.peers.get(index))
    }
}

pub fn config_dir() -> Result<PathBuf> {
    // The Windows sign-in helper runs as SYSTEM with its own copy.
    if let Some(directory) = std::env::var_os("SEAM_CONFIG_DIR") {
        return Ok(PathBuf::from(directory));
    }
    let base = dirs::config_dir().context("cannot find user config directory")?;
    Ok(base.join("Seam"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agent.log"))
}

/// Serializes read-modify-write updates of the saved configuration.
static EDIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply `change` to the latest saved configuration and save it.
pub fn edit(change: impl FnOnce(&mut Config) -> Result<()>) -> Result<Config> {
    edit_at(&config_path()?, change)
}

pub(crate) fn edit_at(
    path: &Path,
    change: impl FnOnce(&mut Config) -> Result<()>,
) -> Result<Config> {
    let _lock = EDIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut config = Config::load_or_create_at(path)?;
    let before = config.clone();
    change(&mut config)?;
    if config != before {
        config.save_to(path)?;
    }
    Ok(config)
}

impl Config {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_at(&config_path()?)
    }

    /// The saved configuration, or `None` when Seam was never set up for this
    /// person. For commands that must not create a pairing identity as a side
    /// effect, such as stopping Seam from an installer.
    pub fn load() -> Result<Option<Self>> {
        Self::load_at(&config_path()?)
    }

    fn load_at(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let value = Self::parse(&raw)?;
        value.validate()?;
        Ok(Some(value))
    }

    pub(crate) fn load_or_create_at(path: &Path) -> Result<Self> {
        if let Some(value) = Self::load_at(path)? {
            return Ok(value);
        }
        let value = Self::default();
        value.save_to(path)?;
        Ok(value)
    }

    pub(crate) fn parse(raw: &[u8]) -> Result<Self> {
        serde_json::from_slice(raw).context("invalid Seam config")
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty(),
            "Enter a name for this computer"
        );
        ensure!(self.name.len() <= 64, "Computer name is too long");
        ensure!(
            (1024..=u16::MAX - 3).contains(&self.listen_port),
            "Invalid listen port"
        );
        ensure!(
            is_secret(&self.pair_code),
            "Pairing code must contain 128 bits"
        );
        ensure!(is_secret(&self.control_token), "Invalid control token");
        ensure!(
            self.peers.len() <= MAX_PEERS,
            "At most {MAX_PEERS} computers can be controlled, one per screen edge"
        );
        let mut sides = Vec::new();
        let mut endpoints = Vec::new();
        for peer in &self.peers {
            ensure!(
                !peer.address.trim().is_empty(),
                "{} needs an address",
                peer.label()
            );
            ensure!(
                (1024..=u16::MAX - 3).contains(&peer.port),
                "{} has an invalid port",
                peer.label()
            );
            ensure!(
                is_secret(&peer.code),
                "{} has an invalid pairing code",
                peer.label()
            );
            ensure!(
                !sides.contains(&peer.arrangement.side),
                "Each computer must be on a different screen edge"
            );
            let endpoint = (peer.address.trim().to_ascii_lowercase(), peer.port);
            ensure!(
                !endpoints.contains(&endpoint),
                "{} is configured twice",
                peer.label()
            );
            validate_arrangement(&peer.arrangement)?;
            sides.push(peer.arrangement.side);
            endpoints.push(endpoint);
        }
        validate_arrangement(&self.arrangement)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path()?)
    }

    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        fs::create_dir_all(path.parent().context("config path has no directory")?)?;
        let tmp = path.with_extension("json.tmp");
        let mut saved = self.clone();
        saved.remember_display_identities();
        fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The first side not used by another controlled computer.
    pub fn free_side(&self) -> Option<Side> {
        [Side::Right, Side::Left, Side::Top, Side::Bottom]
            .into_iter()
            .find(|side| !self.peers.iter().any(|p| p.arrangement.side == *side))
    }

    /// Record persistent identities for displays chosen by number.
    fn remember_display_identities(&mut self) {
        #[cfg(target_os = "macos")]
        let ids = crate::mac_displays::identities();
        #[cfg(target_os = "windows")]
        let ids = crate::windows_displays::identities();
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let ids: Result<std::collections::BTreeMap<u32, String>> = Ok(Default::default());
        let Ok(ids) = ids else { return };
        for arrangement in std::iter::once(&mut self.arrangement)
            .chain(self.peers.iter_mut().map(|peer| &mut peer.arrangement))
        {
            if arrangement.local_monitor_uuid.is_none()
                && let Some(uuid) = arrangement.local_monitor.and_then(|id| ids.get(&id))
            {
                arrangement.local_monitor_uuid = Some(uuid.clone());
            }
        }
    }

    pub(crate) fn restore_display_identities(&mut self, local: &[Monitor]) {
        for arrangement in std::iter::once(&mut self.arrangement)
            .chain(self.peers.iter_mut().map(|peer| &mut peer.arrangement))
        {
            arrangement.restore_local(local);
            arrangement.remember(local, &[]);
        }
    }
}

fn validate_arrangement(arrangement: &Arrangement) -> Result<()> {
    ensure!(
        (-0.9..=0.9).contains(&arrangement.edge_offset),
        "Display offset must be between -0.9 and 0.9"
    );
    Ok(())
}

fn remember_identity(selected: Option<u32>, identity: &mut Option<String>, monitors: &[Monitor]) {
    if let Some(monitor) = monitors.iter().find(|m| Some(m.id) == selected) {
        // A display standing in for a remembered one that isn't connected
        // leaves that one remembered, so the edge goes back to it. Choosing a
        // display clears the identity first (`Layout::write_to`), so a choice
        // is always remembered.
        let standing_in = identity
            .as_deref()
            .is_some_and(|remembered| with_identity(monitors, remembered).is_none());
        if monitor.identity.is_some() && !standing_in {
            identity.clone_from(&monitor.identity);
        }
    } else if selected.is_none() {
        *identity = None;
    }
}

/// The one display with this persistent identity. An identity that several
/// displays share, as mirrored ones can, names none of them.
fn with_identity<'a>(monitors: &'a [Monitor], identity: &str) -> Option<&'a Monitor> {
    let mut matches = monitors
        .iter()
        .filter(|m| m.identity.as_deref() == Some(identity));
    match (matches.next(), matches.next()) {
        (Some(monitor), None) => Some(monitor),
        _ => None,
    }
}

/// Point `selected` at the remembered display (`uuid`) in this desktop
/// session's numbering. Only the identity says which display that is: a
/// number that another display took over is never trusted. Without an
/// identity, a chosen number holds while a display has it.
///
/// While the remembered display isn't connected, the main display stands in,
/// so the pointer keeps crossing. The identity stays remembered (see
/// `remember_identity`), and the edge goes back to its display once it's
/// connected again.
fn restore_display_id(selected: &mut Option<u32>, uuid: Option<&str>, monitors: &[Monitor]) {
    let found = match (uuid, *selected) {
        (Some(uuid), _) => with_identity(monitors, uuid).map(|m| m.id),
        // "Main display": whichever it is when the computers connect.
        (None, None) => return,
        (None, Some(id)) => monitors.iter().any(|m| m.id == id).then_some(id),
    };
    // Zero is no display: with none connected, nothing can stand in.
    *selected = Some(found.or_else(|| main_display(monitors)).unwrap_or(0));
}

/// The display used when none was chosen, or the chosen one is gone: the
/// main display, as `Layout::resolve` and `Geometry::new` pick it.
fn main_display(monitors: &[Monitor]) -> Option<u32> {
    monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())
        .map(|m| m.id)
}

pub fn random_secret() -> String {
    let mut secret = [0u8; 16];
    OsRng.fill_bytes(&mut secret);
    BASE32_NOPAD.encode(&secret)
}

fn is_secret(value: &str) -> bool {
    BASE32_NOPAD
        .decode(normalize_pair_code(value).as_bytes())
        .is_ok_and(|raw| raw.len() == 16)
}

pub fn normalize_pair_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '-')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The name people know this computer by, as shown in its system settings.
pub fn computer_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(output) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        && output.status.success()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return truncate_name(name);
        }
    }
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|name| name.trim().to_string())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "My computer".into());
    truncate_name(name.trim_end_matches(".local").to_string())
}

fn truncate_name(name: String) -> String {
    name.chars().take(64).collect()
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_saved_by_0_2_0_still_load() {
        // 0.2.0 wrote speed settings that no longer exist and lacked the
        // shortcut and pause settings that do.
        let saved = format!(
            r#"{{"name":"Desk PC","listen_port":47843,"pair_code":"{}","control_token":"{}",
                "peers":[],"controller_name":"","receive_only":false,"pointer_speed":1.6,
                "cursor_without_mouse":true,"trackpad_scroll_speed":0.4,
                "map_mac_command_to_ctrl":true,"map_app_switch_shortcut":true,
                "clipboard":true,"files":true,"images":true}}"#,
            random_secret(),
            random_secret()
        );
        let config = Config::parse(saved.as_bytes()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.name, "Desk PC");
        assert!(config.switch_shortcut);
        assert!(!config.paused);
        assert!(!config.keep_awake);
        assert!(!config.keep_screen_on);
        assert!(config.wake_at_edge);
    }

    fn peer(address: &str, side: Side) -> Peer {
        Peer {
            name: address.into(),
            address: address.into(),
            port: DEFAULT_PORT,
            code: random_secret(),
            arrangement: Arrangement::on(side),
            mac: None,
        }
    }

    fn display(id: u32, identity: &str, primary: bool) -> Monitor {
        Monitor {
            id,
            identity: Some(identity.into()),
            name: format!("Display {id}"),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
        }
    }

    #[test]
    fn display_identity_survives_reboot_and_never_selects_a_reused_id() {
        let monitors = [
            display(1, "built-in", false),
            display(2, "external", true),
            display(3, "different", false),
        ];
        let mut selected = Some(3);
        restore_display_id(&mut selected, Some("external"), &monitors);
        assert_eq!(selected, Some(2));
        restore_display_id(&mut selected, None, &monitors);
        assert_eq!(selected, Some(2));
        // The remembered display's old number now belongs to another one:
        // the main display stands in, never the display with that number.
        let mut selected = Some(3);
        restore_display_id(&mut selected, Some("unplugged"), &monitors);
        assert_eq!(selected, Some(2));
        // Back again: its own display.
        restore_display_id(&mut selected, Some("built-in"), &monitors);
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn the_main_display_stands_in_while_the_remembered_one_is_gone() {
        let monitors = [display(1, "built-in", false), display(4, "external", true)];
        // Without an identity, a chosen number holds while a display has it.
        let mut selected = Some(1);
        restore_display_id(&mut selected, None, &monitors);
        assert_eq!(selected, Some(1));
        let mut selected = Some(9);
        restore_display_id(&mut selected, None, &monitors);
        assert_eq!(selected, Some(4));
        // "Main display" is resolved when the computers connect.
        let mut selected = None;
        restore_display_id(&mut selected, None, &monitors);
        assert_eq!(selected, None);
        // Mirrored displays sharing an identity name neither of them.
        let mirrored = [display(1, "twin", false), display(2, "twin", true)];
        let mut selected = Some(1);
        restore_display_id(&mut selected, Some("twin"), &mirrored);
        assert_eq!(selected, Some(2));
        // With no display at all, nothing can stand in.
        restore_display_id(&mut selected, Some("built-in"), &[]);
        assert_eq!(selected, Some(0));

        // Standing in leaves the gone display remembered, so the edge goes
        // back to it; a display that is chosen is remembered.
        let mut remembered = Some("lid closed".to_string());
        remember_identity(Some(4), &mut remembered, &monitors);
        assert_eq!(remembered.as_deref(), Some("lid closed"));
        let mut remembered = Some("built-in".to_string());
        remember_identity(Some(1), &mut remembered, &monitors);
        assert_eq!(remembered.as_deref(), Some("built-in"));
        // `Layout::write_to` clears the identity when another is chosen.
        let mut remembered = None;
        remember_identity(Some(4), &mut remembered, &monitors);
        assert_eq!(remembered.as_deref(), Some("external"));
        remember_identity(None, &mut remembered, &monitors);
        assert_eq!(remembered, None);
    }

    #[test]
    fn peers_require_distinct_edges_endpoints_and_valid_pairing() {
        let mut config = Config {
            peers: vec![
                peer("192.0.2.1", Side::Left),
                peer("192.0.2.2", Side::Right),
            ],
            ..Config::default()
        };
        config.validate().unwrap();
        config.peers[1].arrangement.side = Side::Left;
        assert!(config.validate().is_err());
        config.peers[1].arrangement.side = Side::Right;
        config.peers[1].address = "192.0.2.1".into();
        assert!(config.validate().is_err());
        config.peers[1].address = "192.0.2.2".into();
        config.peers[1].code = "bad".into();
        assert!(config.validate().is_err());
        config.peers[1].code = random_secret();
        config
            .peers
            .extend([peer("a", Side::Top), peer("b", Side::Bottom)]);
        config.validate().unwrap();
        assert_eq!(config.free_side(), None);
        config.peers.push(peer("c", Side::Top));
        assert!(config.validate().is_err());
    }

    #[test]
    fn config_round_trips() {
        let mut config = Config {
            peers: vec![peer("192.0.2.1", Side::Top)],
            keep_awake: true,
            keep_screen_on: true,
            ..Config::default()
        };
        let parsed = Config::parse(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(parsed, config);
        config.peers[0].mac = Some("04:42:1a:f1:c8:e4".into());
        let parsed = Config::parse(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn generated_secret_has_128_bits_and_accepts_group_separators() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(
            BASE32_NOPAD
                .decode(config.pair_code.as_bytes())
                .unwrap()
                .len(),
            16
        );
        let grouped = config
            .pair_code
            .as_bytes()
            .chunks(4)
            .map(|part| std::str::from_utf8(part).unwrap())
            .collect::<Vec<_>>()
            .join("-");
        assert!(is_secret(&grouped));
        assert_eq!(normalize_pair_code(&grouped), config.pair_code);
    }

    #[test]
    fn links_follow_the_peer_list() {
        let mut config = Config::default();
        let links = Link::all(&config);
        assert_eq!(links.len(), 1);
        assert!(!links[0].connects());
        config.peers = vec![
            peer("192.0.2.1", Side::Left),
            peer("192.0.2.2", Side::Right),
        ];
        let links = Link::all(&config);
        assert_eq!(links.len(), 2);
        assert_eq!(links[1].remote().unwrap().address, "192.0.2.2");
        assert_eq!(links[1].arrangement.side, Side::Right);
    }
}
