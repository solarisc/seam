//! Windows suppresses the desktop cursor when no mouse is attached. Mouse Keys
//! supplies native pointer presence, including application-selected cursor
//! shapes. Lease only that setting; keep the user's preferences and recover the
//! lease after a crash. This never runs in the per-event input path.
//!
//! Mouse Keys also claims the numeric keypad in one Num Lock state, and which
//! one is a preference. Only the cursor is wanted, so whenever Windows reports
//! the keypad claimed, the lease moves to the other state.
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    ptr,
    sync::OnceLock,
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows_sys::Win32::UI::{
    Accessibility::MOUSEKEYS,
    Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME,
        RIM_TYPEMOUSE,
    },
    WindowsAndMessaging::{
        GetSystemMetrics, MKF_MOUSEKEYSON, MKF_MOUSEMODE, MKF_REPLACENUMBERS, SM_MOUSEPRESENT,
        SPI_GETMOUSEKEYS, SPI_SETMOUSEKEYS, SPIF_SENDCHANGE, SystemParametersInfoW,
    },
};

/// How soon a press of Num Lock stops the keypad moving the pointer. Runs only
/// on a computer with no mouse: one with a mouse has no lease and no timer.
const KEYPAD_CHECK: Duration = Duration::from_millis(250);
/// While another computer's pointer is here, losing the cursor may announce
/// itself no other way. Only reads a number from Windows, and stops when the
/// pointer leaves.
const CONTROLLED_CHECK: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(1)
};
/// Windows refuses these calls while a secure screen is up.
const RETRY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(2)
};
/// A device's notification can arrive before Windows counts the change, and
/// devices come in bursts. Look soon for a mouse that went, then once they have
/// settled for one that came.
const DEVICES_QUICK_LOOK: Duration = Duration::from_millis(300);
const RECORD_RETRIES: u32 = 10;
const RECORD_RETRY_DELAY: Duration = Duration::from_millis(50);
const DEVICES_SETTLED: Duration = if cfg!(test) {
    Duration::from_millis(600)
} else {
    Duration::from_secs(3)
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Settings {
    flags: u32,
    max_speed: u32,
    time_to_max_speed: u32,
    ctrl_speed: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reading {
    settings: Settings,
    /// Windows is treating the numeric keypad as mouse commands.
    keypad_claimed: bool,
}

/// What the lease needs from Windows, apart so that its decisions can be
/// tested without changing this computer's settings.
trait System {
    fn read(&self) -> Result<Reading>;
    fn apply(&self, settings: Settings) -> Result<()>;
    /// Windows' own answer, which Mouse Keys makes true with nothing plugged in.
    fn reports_mouse(&self) -> bool;
    /// The mouse devices listed. One can be virtual, such as Remote Desktop's,
    /// on a computer Windows considers to have no mouse.
    fn listed_mice(&self) -> Result<Vec<String>>;
}

struct Native;
impl System for Native {
    fn read(&self) -> Result<Reading> {
        let mut keys = MOUSEKEYS {
            cbSize: std::mem::size_of::<MOUSEKEYS>() as u32,
            ..Default::default()
        };
        if unsafe {
            SystemParametersInfoW(
                SPI_GETMOUSEKEYS,
                keys.cbSize,
                ptr::from_mut(&mut keys).cast(),
                0,
            )
        } == 0
        {
            bail!(
                "reading native pointer settings: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Reading {
            settings: Settings {
                // The high bits describe transient button/Num Lock state and are
                // explicitly ignored by SPI_SETMOUSEKEYS.
                flags: keys.dwFlags & 0xff,
                max_speed: keys.iMaxSpeed,
                time_to_max_speed: keys.iTimeToMaxSpeed,
                ctrl_speed: keys.iCtrlSpeed,
            },
            keypad_claimed: keys.dwFlags & MKF_MOUSEMODE != 0,
        })
    }

    fn apply(&self, settings: Settings) -> Result<()> {
        let mut keys = MOUSEKEYS {
            cbSize: std::mem::size_of::<MOUSEKEYS>() as u32,
            dwFlags: settings.flags,
            iMaxSpeed: settings.max_speed,
            iTimeToMaxSpeed: settings.time_to_max_speed,
            iCtrlSpeed: settings.ctrl_speed,
            ..Default::default()
        };
        // Do not persist a new user preference in the registry.
        if unsafe {
            SystemParametersInfoW(
                SPI_SETMOUSEKEYS,
                keys.cbSize,
                ptr::from_mut(&mut keys).cast(),
                SPIF_SENDCHANGE,
            )
        } == 0
        {
            bail!(
                "applying native pointer settings: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn reports_mouse(&self) -> bool {
        unsafe { GetSystemMetrics(SM_MOUSEPRESENT) != 0 }
    }

    fn listed_mice(&self) -> Result<Vec<String>> {
        listed_mice()
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Lease {
    original: Settings,
    applied: Settings,
}

struct PointerPresence<S: System> {
    enabled: bool,
    journal: PathBuf,
    lease: Option<Lease>,
    /// The lease came from a record left by an earlier run, and hasn't been
    /// compared with Windows yet.
    inherited: bool,
    /// Windows kept the keypad after the lease moved. Moving it at every check
    /// would only make the cursor flicker.
    keypad_stuck: bool,
    /// Mouse Keys was turned on again for a cursor Windows wasn't showing.
    revived: bool,
    /// Devices listed when Windows said there was no mouse. Not asked about
    /// again, or a computer listing a virtual mouse would blink its cursor at
    /// every notification.
    not_mice: Vec<String>,
    system: S,
}

impl<S: System> PointerPresence<S> {
    fn new(enabled: bool, journal: PathBuf, system: S) -> Self {
        // A record that can't be read must not cost the cursor for good.
        let lease = match fs::read(&journal) {
            Ok(data) => serde_json::from_slice(&data)
                .map_err(|error| log::warn!("ignoring pointer settings record: {error}"))
                .ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                log::warn!("cannot read pointer settings record: {error}");
                None
            }
        };
        Self {
            enabled,
            journal,
            inherited: lease.is_some(),
            lease,
            keypad_stuck: false,
            revived: false,
            not_mice: Vec::new(),
            system,
        }
    }

    /// Mouse devices have settled after a change, or Seam just started: decide
    /// afresh.
    fn reconcile(&mut self) -> Result<()> {
        self.check_ownership()?;
        let listed = self.system.listed_mice()?;
        // A device that went away is a new one if it comes back.
        self.not_mice.retain(|name| listed.contains(name));
        let unknown = listed.iter().any(|name| !self.not_mice.contains(name));
        // With the lease on, Windows reports a mouse whatever is plugged in, so
        // the only way to ask it is without the lease. Going by the device list
        // alone turned the lease off at every other notification on a computer
        // listing a virtual mouse, leaving no cursor.
        if self.lease.is_some() && (!self.enabled || unknown) {
            self.restore()?;
        }
        self.ensure()?;
        // Windows has now said there is no mouse with these listed.
        if self.lease.is_some() {
            self.not_mice = listed;
        }
        Ok(())
    }

    /// Keep the cursor showing and the keypad free. Cheap enough to run often.
    fn ensure(&mut self) -> Result<()> {
        self.check_ownership()?;
        if self.lease.is_none() && self.enabled && !self.system.reports_mouse() {
            self.take()?;
        }
        self.revive()?;
        self.free_keypad()
    }

    /// Mouse Keys is on and Windows still says there is no mouse, so it isn't
    /// showing a cursor. Turning it on is what makes Windows show one: do that
    /// again, once.
    fn revive(&mut self) -> Result<()> {
        let Some(lease) = self.lease else {
            return Ok(());
        };
        if self.system.reports_mouse() {
            self.revived = false;
        } else if !self.revived {
            log::warn!("Windows reports no mouse with Mouse Keys on; turning it on again");
            self.turn_on_again(lease.applied)?;
            self.revived = true;
        }
        Ok(())
    }

    /// The cursor is gone for just these two calls.
    fn turn_on_again(&self, settings: Settings) -> Result<()> {
        self.system.apply(Settings {
            flags: settings.flags & !MKF_MOUSEKEYSON,
            ..settings
        })?;
        self.system.apply(settings)
    }

    /// Whether anything can change that nothing would announce.
    fn wants_checks(&self) -> bool {
        self.enabled && (self.lease.is_some() || !self.system.reports_mouse())
    }

    fn check_ownership(&mut self) -> Result<()> {
        let Some(lease) = self.lease else {
            return Ok(());
        };
        let current = self.system.read()?.settings;
        if std::mem::take(&mut self.inherited) {
            // From before a crash, and perhaps a restart. Anything but what it
            // says is someone's own Mouse Keys, not to be taken for a lease. A
            // crash can have come just as the lease moved off the keypad.
            let moved = Settings {
                flags: lease.applied.flags ^ MKF_REPLACENUMBERS,
                ..lease.applied
            };
            if current != lease.applied && current != moved {
                return self.forget();
            }
        }
        if current == lease.applied {
            return Ok(());
        }
        if current.flags & MKF_MOUSEKEYSON == 0 {
            // The user turned it off, or Windows reset the session at sign-out.
            // All that can be left to undo is moving the lease off the keypad.
            if (current.flags ^ lease.original.flags) & MKF_REPLACENUMBERS != 0 {
                self.system.apply(Settings {
                    flags: current.flags ^ MKF_REPLACENUMBERS,
                    ..current
                })?;
            }
            return self.forget();
        }
        // Still on, so the user changed some other Mouse Keys preference. Keep
        // that and the lease: forgetting would leave Mouse Keys on for good,
        // and the keypad with it.
        let ours = MKF_MOUSEKEYSON | MKF_REPLACENUMBERS;
        self.record(Lease {
            original: Settings {
                flags: (current.flags & !ours) | (lease.original.flags & ours),
                ..current
            },
            applied: current,
        })
    }

    fn take(&mut self) -> Result<()> {
        let original = self.system.read()?.settings;
        // Changing nothing, to find out if Windows allows it. It can refuse for
        // as long as a secure screen is up: don't rewrite the record each time.
        self.system.apply(original)?;
        let lease = Lease {
            original,
            applied: Settings {
                flags: original.flags | MKF_MOUSEKEYSON,
                ..original
            },
        };
        self.record(lease)?;
        self.system.apply(lease.applied)?;
        if self.system.read()?.settings.flags & MKF_MOUSEKEYSON == 0 {
            self.forget()?;
            bail!("Windows did not turn on Mouse Keys");
        }
        self.keypad_stuck = false;
        self.revived = false;
        log::info!("Native cursor enabled for a computer without a mouse; previous settings saved");
        Ok(())
    }

    /// Mouse Keys claims the keypad with Num Lock on or off, as its preference
    /// says, and pressing Num Lock changes which applies now: choose the other.
    /// Windows' report decides, not Num Lock's state, which a thread that reads
    /// no keyboard input can see late.
    fn free_keypad(&mut self) -> Result<()> {
        let Some(lease) = self.lease else {
            return Ok(());
        };
        if !self.system.read()?.keypad_claimed {
            self.keypad_stuck = false;
            return Ok(());
        }
        if self.keypad_stuck {
            return Ok(());
        }
        let applied = Settings {
            flags: lease.applied.flags ^ MKF_REPLACENUMBERS,
            ..lease.applied
        };
        self.system.apply(applied)?;
        if self.system.read()?.keypad_claimed {
            // Windows can settle this only as Mouse Keys turns on.
            self.turn_on_again(applied)?;
        }
        // After the change: if it is refused there is nothing to record, and a
        // crash before this is only another preference changed during the lease.
        self.record(Lease { applied, ..lease })?;
        if self.system.read()?.keypad_claimed {
            self.keypad_stuck = true;
            log::warn!("Windows keeps the numeric keypad for Mouse Keys");
        } else {
            log::info!("Numeric keypad freed from Mouse Keys");
        }
        Ok(())
    }

    fn record(&mut self, lease: Lease) -> Result<()> {
        let directory = self
            .journal
            .parent()
            .context("pointer settings directory")?;
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&serde_json::to_vec(&lease)?)?;
        file.as_file().sync_all()?;
        // Antivirus or the search indexer can hold a file just written for a
        // moment, and Windows then refuses to replace it.
        let mut tries = 0;
        loop {
            match file.persist(&self.journal) {
                Ok(_) => break,
                Err(error)
                    if tries < RECORD_RETRIES
                        && error.error.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    tries += 1;
                    file = error.file;
                    std::thread::sleep(RECORD_RETRY_DELAY);
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.lease = Some(lease);
        Ok(())
    }

    fn forget(&mut self) -> Result<()> {
        match fs::remove_file(&self.journal) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.lease = None;
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        self.check_ownership()?;
        if let Some(lease) = self.lease {
            self.system.apply(lease.original)?;
            log::info!("Restored previous Windows pointer settings");
            self.forget()?;
        }
        Ok(())
    }
}

impl<S: System> Drop for PointerPresence<S> {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            log::error!("cannot restore Windows pointer preferences: {error:#}");
        }
    }
}

/// What a look at the devices is due, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceLook {
    NotDue,
    Quick,
    Settled,
}

/// When to look again after a device notification.
#[derive(Default)]
struct DeviceChecks {
    soon: Option<Instant>,
    later: Option<Instant>,
}

impl DeviceChecks {
    /// A device that keeps coming and going can't put off the first look.
    fn devices_changed(&mut self, now: Instant) {
        self.soon.get_or_insert(now + DEVICES_QUICK_LOOK);
        self.later = Some(now + DEVICES_SETTLED);
    }

    fn next(&self) -> Option<Instant> {
        self.soon.or(self.later)
    }

    fn take_due(&mut self, now: Instant) -> DeviceLook {
        if !self.next().is_some_and(|at| at <= now) {
            DeviceLook::NotDue
        } else if self.soon.take().is_some() {
            DeviceLook::Quick
        } else {
            self.later = None;
            DeviceLook::Settled
        }
    }
}

enum Command {
    DevicesChanged,
    PointerArrived,
    Stop,
}

/// Windows refuses these calls for as long as a secure screen is up: say so
/// once. Returns whether the last call failed.
fn note_outcome(outcome: Result<()>, failing: bool) -> bool {
    match outcome {
        Ok(()) => false,
        Err(error) => {
            if !failing {
                log::warn!("native cursor for a mouse-less computer unavailable: {error:#}");
            }
            true
        }
    }
}

fn keep<S: System>(
    mut presence: PointerPresence<S>,
    commands: Receiver<Command>,
    controlled: impl Fn() -> bool,
) {
    let mut devices = DeviceChecks::default();
    let first = presence.reconcile();
    // A look at the devices that failed is still owed, whatever works meanwhile.
    let mut look_owed = first.is_err();
    let mut failing = note_outcome(first, false);
    loop {
        let check = if failing || look_owed {
            Some(RETRY)
        } else if presence.wants_checks() {
            Some(KEYPAD_CHECK)
        } else if controlled() {
            Some(CONTROLLED_CHECK)
        } else {
            None
        };
        let check = check.map(|wait| Instant::now() + wait);
        let deadline = match (check, devices.next()) {
            (Some(check), Some(device)) => Some(check.min(device)),
            (check, device) => check.or(device),
        };
        let command = match deadline {
            Some(deadline) => commands.recv_deadline(deadline),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let outcome = match command {
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Command::DevicesChanged) => {
                devices.devices_changed(Instant::now());
                continue;
            }
            Ok(Command::PointerArrived) => presence.ensure(),
            Err(RecvTimeoutError::Timeout) => {
                look_owed |= devices.take_due(Instant::now()) == DeviceLook::Settled;
                if look_owed {
                    let outcome = presence.reconcile();
                    look_owed = outcome.is_err();
                    outcome
                } else {
                    presence.ensure()
                }
            }
        };
        failing = note_outcome(outcome, failing);
    }
}

/// The lease's own thread. Changing the setting broadcasts to every window,
/// which has no place on the thread answering Windows' input hooks.
pub(crate) struct Keeper {
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

static ARRIVALS: OnceLock<Sender<Command>> = OnceLock::new();

impl Keeper {
    /// `controlled` says whether another computer's pointer is here now.
    pub(crate) fn start(
        enabled: bool,
        controlled: impl Fn() -> bool + Send + 'static,
    ) -> Result<Self> {
        let journal = crate::config::config_dir()?.join("pointer-presence.json");
        let keeper = Self::spawn(PointerPresence::new(enabled, journal, Native), controlled);
        let _ = ARRIVALS.set(keeper.commands.clone());
        Ok(keeper)
    }

    fn spawn<S: System + Send + 'static>(
        presence: PointerPresence<S>,
        controlled: impl Fn() -> bool + Send + 'static,
    ) -> Self {
        let (commands, receiver) = crossbeam_channel::unbounded();
        let thread = std::thread::spawn(move || keep(presence, receiver, controlled));
        Self {
            commands,
            thread: Some(thread),
        }
    }

    pub(crate) fn devices_changed(&self) {
        let _ = self.commands.send(Command::DevicesChanged);
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Another computer's pointer has just arrived here. Whatever went unnoticed
/// since it last did, this is when the cursor has to show. Never waits.
pub(crate) fn pointer_arrived() {
    if let Some(commands) = ARRIVALS.get() {
        let _ = commands.send(Command::PointerArrived);
    }
}

fn listed_mice() -> Result<Vec<String>> {
    let size = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
    // The device list can change between calls during hotplug.
    for _ in 0..3 {
        let mut count = 0;
        if unsafe { GetRawInputDeviceList(ptr::null_mut(), &mut count, size) } == u32::MAX {
            bail!(
                "listing pointer devices: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut devices = vec![RAWINPUTDEVICELIST::default(); count as usize];
        let read = unsafe { GetRawInputDeviceList(devices.as_mut_ptr(), &mut count, size) };
        if read != u32::MAX {
            return Ok(devices[..read as usize]
                .iter()
                .filter(|device| device.dwType == RIM_TYPEMOUSE)
                .filter_map(device_name)
                .collect());
        }
    }
    bail!("pointer devices kept changing while checking native cursor support")
}

/// Windows can enumerate anonymous synthetic mouse handles even on a desktop
/// with SM_MOUSEPRESENT == false. Only a device with an actual interface name
/// is worth lifting the lease to check.
fn device_name(device: &RAWINPUTDEVICELIST) -> Option<String> {
    let mut characters = 0;
    let asked = unsafe {
        GetRawInputDeviceInfoW(
            device.hDevice,
            RIDI_DEVICENAME,
            ptr::null_mut(),
            &mut characters,
        )
    };
    if asked == u32::MAX || characters <= 1 {
        return None;
    }
    let mut name = vec![0u16; characters as usize];
    let read = unsafe {
        GetRawInputDeviceInfoW(
            device.hDevice,
            RIDI_DEVICENAME,
            name.as_mut_ptr().cast(),
            &mut characters,
        )
    };
    if read == u32::MAX {
        return None;
    }
    let length = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..length]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    const DEFAULT_FLAGS: u32 = 0x3e;

    fn user_settings(flags: u32) -> Settings {
        Settings {
            flags,
            max_speed: 80,
            time_to_max_speed: 3000,
            ctrl_speed: 80,
        }
    }

    /// Windows as Microsoft documents it: `MKF_REPLACENUMBERS` means the keypad
    /// moves the pointer with Num Lock on, otherwise with it off.
    struct World {
        settings: Settings,
        num_lock: bool,
        mouse_plugged: bool,
        virtual_mouse_listed: bool,
        /// Gone, but the list hasn't caught up.
        mouse_still_listed: bool,
        /// Windows hasn't counted a device change yet.
        report_lags: Option<bool>,
        keypad_claimed: bool,
        /// Windows works out the keypad only as Mouse Keys turns on.
        settles_only_at_turn_on: bool,
        /// Whatever the flags say, as if the documentation were wrong.
        keypad_never_freed: bool,
        refused: bool,
        /// Windows lets the settings be read but not changed.
        changes_refused: bool,
        /// Changes Windows will still make before refusing the rest.
        changes_left: Option<usize>,
        /// Windows reports success without turning Mouse Keys on.
        mouse_keys_ignored: bool,
        calls_to_apply: usize,
    }

    impl World {
        fn new() -> Self {
            Self {
                settings: user_settings(DEFAULT_FLAGS),
                num_lock: true,
                mouse_plugged: false,
                virtual_mouse_listed: false,
                mouse_still_listed: false,
                report_lags: None,
                keypad_claimed: false,
                settles_only_at_turn_on: false,
                keypad_never_freed: false,
                refused: false,
                changes_refused: false,
                changes_left: None,
                mouse_keys_ignored: false,
                calls_to_apply: 0,
            }
        }

        fn mouse_keys_on(&self) -> bool {
            self.settings.flags & MKF_MOUSEKEYSON != 0
        }

        fn settle_keypad(&mut self) {
            let on_with_num_lock = self.settings.flags & MKF_REPLACENUMBERS != 0;
            self.keypad_claimed = self.keypad_never_freed
                || self.mouse_keys_on() && self.num_lock == on_with_num_lock;
        }

        /// A physical press: both change, whatever the flags say.
        fn press_num_lock(&mut self) {
            self.num_lock = !self.num_lock;
            if self.mouse_keys_on() {
                self.keypad_claimed = !self.keypad_claimed;
            }
        }
    }

    #[derive(Clone)]
    struct Fake(Arc<Mutex<World>>);

    impl Fake {
        fn new(change: impl FnOnce(&mut World)) -> Self {
            let mut world = World::new();
            change(&mut world);
            world.settle_keypad();
            Self(Arc::new(Mutex::new(world)))
        }

        fn world(&self) -> std::sync::MutexGuard<'_, World> {
            self.0.lock().unwrap()
        }
    }

    impl System for Fake {
        fn read(&self) -> Result<Reading> {
            let world = self.world();
            if world.refused {
                bail!("refused on this desktop");
            }
            Ok(Reading {
                settings: world.settings,
                keypad_claimed: world.keypad_claimed,
            })
        }

        fn apply(&self, settings: Settings) -> Result<()> {
            let mut world = self.world();
            if world.refused || world.changes_refused || world.changes_left == Some(0) {
                bail!("refused on this desktop");
            }
            if let Some(left) = world.changes_left.as_mut() {
                *left -= 1;
            }
            world.calls_to_apply += 1;
            let was_on = world.mouse_keys_on();
            world.settings = settings;
            if world.mouse_keys_ignored {
                world.settings.flags &= !MKF_MOUSEKEYSON;
            }
            if !world.settles_only_at_turn_on || (!was_on && world.mouse_keys_on()) {
                world.settle_keypad();
            }
            Ok(())
        }

        fn reports_mouse(&self) -> bool {
            let world = self.world();
            world
                .report_lags
                .unwrap_or(world.mouse_plugged || world.mouse_keys_on())
        }

        fn listed_mice(&self) -> Result<Vec<String>> {
            let world = self.world();
            let mut listed = Vec::new();
            if world.mouse_plugged || world.mouse_still_listed {
                listed.push("mouse".to_string());
            }
            if world.virtual_mouse_listed {
                listed.push("virtual mouse".to_string());
            }
            Ok(listed)
        }
    }

    struct Bench {
        _folder: tempfile::TempDir,
        journal: PathBuf,
        fake: Fake,
    }

    impl Bench {
        fn new(change: impl FnOnce(&mut World)) -> Self {
            let folder = tempfile::tempdir().unwrap();
            Self {
                journal: folder.path().join("pointer-presence.json"),
                _folder: folder,
                fake: Fake::new(change),
            }
        }

        fn presence(&self, enabled: bool) -> PointerPresence<Fake> {
            PointerPresence::new(enabled, self.journal.clone(), self.fake.clone())
        }

        fn cursor_shows(&self) -> bool {
            self.fake.reports_mouse()
        }

        fn keypad_free(&self) -> bool {
            !self.fake.world().keypad_claimed
        }
    }

    #[test]
    fn a_computer_without_a_mouse_shows_the_cursor() {
        let bench = Bench::new(|_| {});
        assert!(!bench.cursor_shows());
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(bench.cursor_shows());
        assert!(bench.journal.exists());
    }

    #[test]
    fn a_computer_with_a_mouse_is_left_alone() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        presence.ensure().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, 0);
        assert!(!presence.wants_checks());
        assert!(!bench.journal.exists());
    }

    #[test]
    fn turned_off_in_settings_it_never_touches_windows() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(false);
        presence.reconcile().unwrap();
        presence.ensure().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, 0);
        assert!(!presence.wants_checks());
    }

    /// Every other notification used to take the lease away for good.
    #[test]
    fn a_virtual_mouse_never_costs_the_cursor() {
        let bench = Bench::new(|world| world.virtual_mouse_listed = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let calls = bench.fake.world().calls_to_apply;
        for _ in 0..5 {
            presence.reconcile().unwrap();
            assert!(bench.cursor_shows());
            assert!(presence.lease.is_some());
        }
        // Not even a blink: lifting the lease to ask could itself bring
        // another notification, and so on for good.
        assert_eq!(bench.fake.world().calls_to_apply, calls);
    }

    #[test]
    fn a_virtual_mouse_appearing_later_is_asked_about_once() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        bench.fake.world().virtual_mouse_listed = true;
        presence.reconcile().unwrap();
        assert!(presence.lease.is_some());
        assert!(bench.cursor_shows());
        let calls = bench.fake.world().calls_to_apply;
        presence.reconcile().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, calls);
    }

    #[test]
    fn a_lease_recovered_from_a_crash_is_asked_about_once_with_a_virtual_mouse() {
        let bench = Bench::new(|world| world.virtual_mouse_listed = true);
        let presence = {
            let mut presence = bench.presence(true);
            presence.reconcile().unwrap();
            presence
        };
        std::mem::forget(presence);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(bench.cursor_shows());
        let calls = bench.fake.world().calls_to_apply;
        presence.reconcile().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, calls);
    }

    /// Carried to the other computer and back, past a moment when Windows
    /// still listed it.
    #[test]
    fn a_mouse_that_comes_back_ends_the_lease_again() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        {
            let mut world = bench.fake.world();
            world.mouse_plugged = false;
            world.virtual_mouse_listed = true;
        }
        presence.reconcile().unwrap();
        assert!(presence.lease.is_some());
        for plugged in [true, false, true] {
            bench.fake.world().mouse_plugged = plugged;
            presence.reconcile().unwrap();
            assert_eq!(presence.lease.is_some(), !plugged);
            assert!(bench.cursor_shows());
        }
        assert!(!bench.fake.world().mouse_keys_on());
    }

    #[test]
    fn plugging_in_a_mouse_ends_the_lease() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        bench.fake.world().mouse_plugged = true;
        presence.reconcile().unwrap();
        assert!(presence.lease.is_none());
        assert!(!bench.fake.world().mouse_keys_on());
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
        assert!(bench.cursor_shows());
        assert!(!bench.journal.exists());
    }

    /// The mouse carried over to the other computer: Windows can report it
    /// for a moment after saying it has gone, which used to be the only look.
    #[test]
    fn a_mouse_unplugged_shows_the_cursor_once_windows_has_counted_it() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        {
            let mut world = bench.fake.world();
            world.mouse_plugged = false;
            world.report_lags = Some(true);
        }
        presence.reconcile().unwrap();
        assert!(presence.lease.is_none());
        bench.fake.world().report_lags = None;
        assert!(presence.wants_checks());
        presence.reconcile().unwrap();
        assert!(presence.lease.is_some());
        assert!(bench.cursor_shows());
    }

    #[test]
    fn a_pointer_arriving_brings_back_a_cursor_something_else_turned_off() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let mouse_keys_off = user_settings(DEFAULT_FLAGS);
        bench.fake.apply(mouse_keys_off).unwrap();
        assert!(!bench.cursor_shows());
        presence.ensure().unwrap();
        assert!(bench.cursor_shows());
    }

    #[test]
    fn refused_at_first_it_works_once_windows_allows_it() {
        let bench = Bench::new(|world| world.refused = true);
        let mut presence = bench.presence(true);
        assert!(presence.reconcile().is_err());
        assert!(!bench.cursor_shows());
        bench.fake.world().refused = false;
        presence.reconcile().unwrap();
        assert!(bench.cursor_shows());
    }

    /// Windows' quick look: the list can still name a mouse that has gone.
    #[test]
    fn a_mouse_still_listed_when_the_lease_was_taken_can_end_it() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        {
            let mut world = bench.fake.world();
            world.mouse_plugged = false;
            world.mouse_still_listed = true;
        }
        presence.ensure().unwrap();
        assert!(presence.lease.is_some());
        {
            let mut world = bench.fake.world();
            world.mouse_plugged = true;
            world.mouse_still_listed = false;
        }
        presence.reconcile().unwrap();
        assert!(presence.lease.is_none());
        assert!(!bench.fake.world().mouse_keys_on());
    }

    #[test]
    fn a_lease_without_a_cursor_is_turned_on_again_once() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let calls = bench.fake.world().calls_to_apply;
        bench.fake.world().report_lags = Some(false);
        presence.ensure().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, calls + 2);
        assert!(bench.fake.world().mouse_keys_on());
        presence.ensure().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, calls + 2);
        // It worked for a while, so it is worth doing again.
        bench.fake.world().report_lags = None;
        presence.ensure().unwrap();
        bench.fake.world().report_lags = Some(false);
        presence.ensure().unwrap();
        assert_eq!(bench.fake.world().calls_to_apply, calls + 4);
    }

    #[test]
    fn windows_not_turning_it_on_counts_as_a_failure() {
        let bench = Bench::new(|world| world.mouse_keys_ignored = true);
        let mut presence = bench.presence(true);
        assert!(presence.reconcile().is_err());
        assert!(presence.lease.is_none());
        assert!(!bench.journal.exists());
    }

    fn write_record(bench: &Bench, original: Settings, applied: Settings) {
        std::fs::write(
            &bench.journal,
            serde_json::to_vec(&Lease { original, applied }).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn someones_own_mouse_keys_are_not_taken_for_a_lease_left_by_a_crash() {
        let own = Settings {
            max_speed: 200,
            ..user_settings(DEFAULT_FLAGS | MKF_MOUSEKEYSON)
        };
        let bench = Bench::new(|world| {
            world.settings = own;
            world.num_lock = false;
        });
        write_record(
            &bench,
            user_settings(DEFAULT_FLAGS),
            user_settings(DEFAULT_FLAGS | MKF_MOUSEKEYSON),
        );
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(presence.lease.is_none());
        assert!(!bench.journal.exists());
        presence.ensure().unwrap();
        drop(presence);
        let world = bench.fake.world();
        assert_eq!(world.settings, own);
        assert_eq!(world.calls_to_apply, 0);
        assert!(world.keypad_claimed);
    }

    #[test]
    fn a_crash_just_as_the_lease_moved_is_recovered() {
        let bench = Bench::new(|world| {
            world.settings = user_settings(DEFAULT_FLAGS | MKF_MOUSEKEYSON | MKF_REPLACENUMBERS);
            world.num_lock = false;
        });
        write_record(
            &bench,
            user_settings(DEFAULT_FLAGS),
            user_settings(DEFAULT_FLAGS | MKF_MOUSEKEYSON),
        );
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(presence.lease.is_some());
        drop(presence);
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
    }

    #[test]
    fn mouse_keys_turned_off_during_the_lease_keeps_the_num_lock_preference() {
        let bench = Bench::new(|world| world.num_lock = false);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let moved = bench.fake.world().settings;
        assert_ne!(moved.flags & MKF_REPLACENUMBERS, 0);
        bench.fake.world().mouse_plugged = true;
        bench
            .fake
            .apply(Settings {
                flags: moved.flags & !MKF_MOUSEKEYSON,
                ..moved
            })
            .unwrap();
        presence.ensure().unwrap();
        assert!(presence.lease.is_none());
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
    }

    #[test]
    fn a_refusal_between_turning_off_and_on_loses_nothing() {
        let bench = Bench::new(|world| world.settles_only_at_turn_on = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        {
            let mut world = bench.fake.world();
            world.press_num_lock();
            // Moving the lease and turning off, then no more.
            world.changes_left = Some(2);
        }
        assert!(presence.ensure().is_err());
        assert!(!bench.fake.world().mouse_keys_on());
        bench.fake.world().changes_left = None;
        presence.ensure().unwrap();
        assert!(bench.cursor_shows());
        assert!(bench.keypad_free());
        drop(presence);
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
    }

    #[test]
    fn being_refused_does_not_rewrite_the_record() {
        let bench = Bench::new(|world| world.changes_refused = true);
        let mut presence = bench.presence(true);
        assert!(presence.reconcile().is_err());
        assert!(presence.lease.is_none());
        assert!(!bench.journal.exists());
        bench.fake.world().changes_refused = false;
        presence.reconcile().unwrap();
        let record = std::fs::read(&bench.journal).unwrap();
        {
            let mut world = bench.fake.world();
            world.changes_refused = true;
            world.press_num_lock();
        }
        assert!(presence.ensure().is_err());
        assert_eq!(std::fs::read(&bench.journal).unwrap(), record);
        // And the keypad comes back as soon as Windows allows it.
        bench.fake.world().changes_refused = false;
        presence.ensure().unwrap();
        assert!(bench.keypad_free());
    }

    #[test]
    fn being_refused_keeps_the_lease() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        bench.fake.world().refused = true;
        assert!(presence.ensure().is_err());
        assert!(presence.lease.is_some());
        assert!(bench.journal.exists());
        bench.fake.world().refused = false;
        presence.ensure().unwrap();
        assert!(presence.lease.is_some());
    }

    /// Windows' default: Mouse Keys takes the keypad with Num Lock off.
    #[test]
    fn the_keypad_stays_free_with_num_lock_off() {
        let bench = Bench::new(|world| world.num_lock = false);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(bench.cursor_shows());
        assert!(bench.keypad_free());
    }

    #[test]
    fn the_keypad_stays_free_with_num_lock_on() {
        let bench = Bench::new(|world| {
            world.settings.flags |= MKF_REPLACENUMBERS;
        });
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(bench.cursor_shows());
        assert!(bench.keypad_free());
    }

    #[test]
    fn the_keypad_comes_back_after_each_press_of_num_lock() {
        for settles_only_at_turn_on in [false, true] {
            let bench = Bench::new(|world| world.settles_only_at_turn_on = settles_only_at_turn_on);
            let mut presence = bench.presence(true);
            presence.reconcile().unwrap();
            for _ in 0..4 {
                bench.fake.world().press_num_lock();
                assert!(!bench.keypad_free());
                presence.ensure().unwrap();
                assert!(bench.keypad_free());
                assert!(bench.cursor_shows());
            }
        }
    }

    #[test]
    fn a_free_keypad_needs_no_changes() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let calls = bench.fake.world().calls_to_apply;
        for _ in 0..20 {
            presence.ensure().unwrap();
        }
        assert_eq!(bench.fake.world().calls_to_apply, calls);
    }

    #[test]
    fn it_stops_trying_when_windows_keeps_the_keypad() {
        let bench = Bench::new(|world| world.keypad_never_freed = true);
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let calls = bench.fake.world().calls_to_apply;
        for _ in 0..20 {
            presence.ensure().unwrap();
        }
        assert_eq!(bench.fake.world().calls_to_apply, calls);
        assert!(bench.cursor_shows());
    }

    #[test]
    fn stopping_gives_back_the_users_num_lock_preference() {
        for flags in [DEFAULT_FLAGS, DEFAULT_FLAGS | MKF_REPLACENUMBERS] {
            // Num Lock as Mouse Keys wants it, so that the lease has to move.
            let bench = Bench::new(|world| {
                world.settings.flags = flags;
                world.num_lock = flags & MKF_REPLACENUMBERS != 0;
            });
            let mut presence = bench.presence(true);
            presence.reconcile().unwrap();
            assert!(bench.keypad_free());
            assert_ne!(
                bench.fake.world().settings.flags & MKF_REPLACENUMBERS,
                flags & MKF_REPLACENUMBERS
            );
            drop(presence);
            assert_eq!(bench.fake.world().settings, user_settings(flags));
            assert!(!bench.journal.exists());
        }
    }

    #[test]
    fn another_preference_changed_during_the_lease_is_kept_without_leaving_it_on() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        let mut changed = bench.fake.world().settings;
        changed.max_speed = 200;
        bench.fake.apply(changed).unwrap();
        presence.ensure().unwrap();
        assert!(presence.lease.is_some());
        drop(presence);
        let world = bench.fake.world();
        assert!(!world.mouse_keys_on());
        assert_eq!(world.settings.max_speed, 200);
        assert_eq!(world.settings.flags, DEFAULT_FLAGS);
    }

    #[test]
    fn turning_the_setting_off_gives_the_settings_back() {
        let bench = Bench::new(|_| {});
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        std::mem::forget(presence);
        let mut presence = bench.presence(false);
        presence.reconcile().unwrap();
        assert!(!bench.fake.world().mouse_keys_on());
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
    }

    #[test]
    fn a_crash_is_recovered_when_a_mouse_has_been_plugged_in_since() {
        let bench = Bench::new(|world| world.num_lock = false);
        let presence = {
            let mut presence = bench.presence(true);
            presence.reconcile().unwrap();
            presence
        };
        std::mem::forget(presence);
        bench.fake.world().mouse_plugged = true;
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
        assert!(!bench.journal.exists());
    }

    #[test]
    fn a_crash_keeps_the_lease_with_no_mouse() {
        let bench = Bench::new(|_| {});
        let presence = {
            let mut presence = bench.presence(true);
            presence.reconcile().unwrap();
            presence
        };
        std::mem::forget(presence);
        let calls = bench.fake.world().calls_to_apply;
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(presence.lease.is_some());
        assert_eq!(bench.fake.world().calls_to_apply, calls);
        drop(presence);
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
    }

    #[test]
    fn windows_resetting_the_session_retires_the_record() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        std::fs::write(
            &bench.journal,
            serde_json::to_vec(&Lease {
                original: user_settings(DEFAULT_FLAGS),
                applied: user_settings(DEFAULT_FLAGS | MKF_MOUSEKEYSON),
            })
            .unwrap(),
        )
        .unwrap();
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(presence.lease.is_none());
        assert!(!bench.journal.exists());
        assert_eq!(bench.fake.world().calls_to_apply, 0);
    }

    #[test]
    fn a_record_from_the_previous_version_still_reads() {
        let bench = Bench::new(|_| {});
        std::fs::write(
            &bench.journal,
            r#"{"original":{"flags":62,"max_speed":80,"time_to_max_speed":3000,"ctrl_speed":80},
                "applied":{"flags":63,"max_speed":80,"time_to_max_speed":3000,"ctrl_speed":80}}"#,
        )
        .unwrap();
        let presence = bench.presence(true);
        assert!(presence.lease.is_some());
        std::mem::forget(presence);
    }

    #[test]
    fn a_damaged_record_does_not_cost_the_cursor() {
        let bench = Bench::new(|_| {});
        std::fs::write(&bench.journal, b"not a record").unwrap();
        let mut presence = bench.presence(true);
        presence.reconcile().unwrap();
        assert!(bench.cursor_shows());
    }

    #[test]
    fn device_checks_wait_for_a_burst_and_look_twice() {
        let start = Instant::now();
        let burst_ended = start + Duration::from_millis(100);
        let mut checks = DeviceChecks::default();
        assert_eq!(checks.next(), None);
        assert_eq!(checks.take_due(start), DeviceLook::NotDue);
        checks.devices_changed(start);
        checks.devices_changed(burst_ended);
        assert_eq!(checks.next(), Some(start + DEVICES_QUICK_LOOK));
        assert_eq!(
            checks.take_due(start + Duration::from_millis(200)),
            DeviceLook::NotDue
        );
        assert_eq!(
            checks.take_due(start + DEVICES_QUICK_LOOK),
            DeviceLook::Quick
        );
        assert_eq!(checks.next(), Some(burst_ended + DEVICES_SETTLED));
        assert_eq!(
            checks.take_due(burst_ended + DEVICES_SETTLED - Duration::from_millis(1)),
            DeviceLook::NotDue
        );
        assert_eq!(
            checks.take_due(burst_ended + DEVICES_SETTLED),
            DeviceLook::Settled
        );
        assert_eq!(checks.next(), None);
    }

    #[test]
    fn devices_that_keep_changing_do_not_put_off_the_quick_look() {
        let start = Instant::now();
        let mut checks = DeviceChecks::default();
        checks.devices_changed(start);
        checks.devices_changed(start + DEVICES_QUICK_LOOK);
        assert_eq!(
            checks.take_due(start + DEVICES_QUICK_LOOK),
            DeviceLook::Quick
        );
    }

    fn wait_until(condition: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn the_thread_keeps_the_keypad_free_and_gives_everything_back() {
        let bench = Bench::new(|_| {});
        let keeper = Keeper::spawn(bench.presence(true), || false);
        assert!(wait_until(|| bench.cursor_shows()));
        bench.fake.world().press_num_lock();
        assert!(wait_until(|| bench.keypad_free()));
        drop(keeper);
        assert_eq!(bench.fake.world().settings, user_settings(DEFAULT_FLAGS));
        assert!(!bench.journal.exists());
    }

    #[test]
    fn the_thread_notices_a_mouse_going_after_the_notification() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        let keeper = Keeper::spawn(bench.presence(true), || false);
        {
            let mut world = bench.fake.world();
            world.mouse_plugged = false;
            world.report_lags = Some(true);
        }
        keeper.devices_changed();
        // Past the first look, which still sees the old answer.
        std::thread::sleep(DEVICES_QUICK_LOOK + Duration::from_millis(100));
        assert!(!bench.fake.world().mouse_keys_on());
        bench.fake.world().report_lags = None;
        assert!(wait_until(|| bench.fake.world().mouse_keys_on()));
        drop(keeper);
    }

    /// Nothing announces someone's own Mouse Keys being turned off.
    #[test]
    fn the_thread_keeps_watching_while_another_computers_pointer_is_here() {
        let bench = Bench::new(|world| {
            world.settings = user_settings(DEFAULT_FLAGS | MKF_MOUSEKEYSON);
        });
        let controlled = Arc::new(AtomicBool::new(true));
        let watched = controlled.clone();
        let keeper = Keeper::spawn(bench.presence(true), move || {
            watched.load(Ordering::Relaxed)
        });
        std::thread::sleep(Duration::from_millis(50));
        bench.fake.apply(user_settings(DEFAULT_FLAGS)).unwrap();
        assert!(wait_until(|| bench.cursor_shows()));
        controlled.store(false, Ordering::Relaxed);
        drop(keeper);
    }

    #[test]
    fn the_thread_still_owes_a_look_that_failed_after_a_pointer_arrives() {
        let bench = Bench::new(|_| {});
        let keeper = Keeper::spawn(bench.presence(true), || false);
        assert!(wait_until(|| bench.cursor_shows()));
        {
            let mut world = bench.fake.world();
            world.refused = true;
            world.mouse_plugged = true;
        }
        keeper.devices_changed();
        std::thread::sleep(DEVICES_SETTLED + Duration::from_millis(100));
        assert!(bench.fake.world().mouse_keys_on());
        bench.fake.world().refused = false;
        keeper.commands.send(Command::PointerArrived).unwrap();
        assert!(wait_until(|| !bench.fake.world().mouse_keys_on()));
        drop(keeper);
    }

    #[test]
    fn the_thread_waits_silently_on_a_computer_with_a_mouse() {
        let bench = Bench::new(|world| world.mouse_plugged = true);
        let keeper = Keeper::spawn(bench.presence(true), || false);
        keeper.commands.send(Command::PointerArrived).unwrap();
        keeper.devices_changed();
        drop(keeper);
        assert_eq!(bench.fake.world().calls_to_apply, 0);
    }
}
