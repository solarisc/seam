//! Control the Windows sign-in and lock screens from another computer.
//!
//! The secure desktop that shows them only accepts input from SYSTEM, so a
//! person's own agent cannot type a password there. The `SeamSignIn`
//! service runs as SYSTEM and, while the console session has no signed-in
//! user or is locked, starts a receive-only helper on the secure desktop with
//! a copy of this computer's pairing, which the service refreshes from the
//! signed-in person's settings. The helper listens on its own ports
//! (`helper_port`), the person's agent turns connections away while the
//! desktop is locked, and the controlling computer reconnects to whichever is
//! available. Once the person is signed in and unlocked, the helper stops.
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    path::PathBuf,
    ptr,
    time::{Duration, Instant},
};
use windows_service::{
    define_windows_service,
    service::*,
    service_control_handler, service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, STILL_ACTIVE},
    Security::{
        DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
        TokenPrimary, TokenSessionId,
    },
    System::{
        RemoteDesktop::{
            WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_LOCK, WTSFreeMemory,
            WTSGetActiveConsoleSessionId, WTSINFOEXW, WTSQuerySessionInformationW,
            WTSQueryUserToken, WTSSessionInfoEx,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
            GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW,
            TerminateProcess,
        },
    },
    UI::Shell::GetUserProfileDirectoryW,
};

pub const SERVICE_NAME: &str = "SeamSignIn";
/// Marks the helper process, which must not offer pairing or clipboard.
pub const HELPER_ENV: &str = "SEAM_SIGN_IN";

/// Where the sign-in copy of a receiving computer's pairing is kept. Only
/// SYSTEM and administrators can read it; the installer sets that access,
/// and only the service writes it.
pub fn directory() -> PathBuf {
    let data = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    PathBuf::from(data).join("Seam").join("sign-in")
}

pub fn is_helper() -> bool {
    std::env::var_os(HELPER_ENV).is_some()
}

/// The helper's listen port: next to, but separate from, the agent's ports.
pub fn helper_port(listen_port: u16) -> u16 {
    if listen_port <= u16::MAX - 20 {
        listen_port + 10
    } else {
        listen_port - 10
    }
}

/// `true` while this process's Windows session is locked. UAC prompts also
/// use a secure desktop but leave the session unlocked.
pub fn session_locked() -> bool {
    const CURRENT_SESSION: u32 = u32::MAX;
    locked(CURRENT_SESSION)
}

/// Refresh the sign-in copy from the signed-in person's settings. Only a
/// computer that another computer is paired to control needs one, so
/// unpairing also ends access to the sign-in screen. Another person's copy
/// is left alone: signing in to an unpaired account must not cut off the
/// paired person's controller. Returns `true` when the copy changed.
fn refresh_copy(config: &Config) -> bool {
    let path = directory().join("config.json");
    let current = std::fs::read(&path)
        .ok()
        .and_then(|raw| Config::parse(&raw).ok());
    // The control token identifies whose settings a copy came from.
    let own = current
        .as_ref()
        .filter(|copy| copy.control_token == config.control_token);
    let controlled = config.peers.is_empty() && !config.controller_name.is_empty();
    if !controlled {
        if own.is_none() {
            return false;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            log::warn!("cannot remove the sign-in copy: {error}");
        }
        return true;
    }
    let wanted = Config {
        listen_port: helper_port(config.listen_port),
        receive_only: true,
        clipboard: false,
        images: false,
        files: false,
        // The helper saves the layout and controller name it sees on its own
        // desktop. Treating that as a change would restart the helper on
        // every connection, so keep them.
        arrangement: own.map_or_else(
            || config.arrangement.clone(),
            |copy| copy.arrangement.clone(),
        ),
        controller_name: own.map_or_else(
            || config.controller_name.clone(),
            |copy| copy.controller_name.clone(),
        ),
        ..config.clone()
    };
    if current.as_ref() == Some(&wanted) {
        return false;
    }
    if let Err(error) = wanted.save_to(&path) {
        log::warn!("cannot update the sign-in copy: {error:#}");
    }
    true
}

/// The settings of the person signed in to `session`, if any.
fn user_config(session: u32) -> Option<Config> {
    let mut token: HANDLE = ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        return None;
    }
    let mut profile = [0u16; 512];
    let mut size = profile.len() as u32;
    let found = unsafe { GetUserProfileDirectoryW(token, profile.as_mut_ptr(), &mut size) } != 0;
    unsafe { CloseHandle(token) };
    if !found {
        return None;
    }
    let end = profile.iter().position(|&c| c == 0)?;
    let path = PathBuf::from(String::from_utf16_lossy(&profile[..end]))
        .join("AppData\\Roaming\\Seam\\config.json");
    Config::parse(&std::fs::read(path).ok()?).ok()
}

define_windows_service!(ffi_service_main, service_main);

pub fn run_service() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("starting service")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = serve() {
        log::error!("sign-in service: {error:#}");
    }
}

struct Helper {
    process: HANDLE,
    session: u32,
}

impl Helper {
    fn running(&self) -> bool {
        let mut code = 0;
        unsafe { GetExitCodeProcess(self.process, &mut code) != 0 && code == STILL_ACTIVE as u32 }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.process, 0);
            CloseHandle(self.process);
        }
    }
}

fn serve() -> Result<()> {
    let (events, wake) = crossbeam_channel::unbounded::<bool>();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = events.send(true);
            service_control_handler::ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = events.send(false);
            service_control_handler::ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => {
            service_control_handler::ServiceControlHandlerResult::NoError
        }
        _ => service_control_handler::ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let report = |state| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP
                    | ServiceControlAccept::SHUTDOWN
                    | ServiceControlAccept::SESSION_CHANGE
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        })
    };
    report(ServiceState::Running)?;
    let mut helper: Option<Helper> = None;
    let mut retry_after = Instant::now();
    loop {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        // Follow the signed-in person's pairing; restart the helper when it
        // changes so an unpaired computer loses access right away.
        if let Some(config) = user_config(session)
            && refresh_copy(&config)
        {
            helper = None;
        }
        let wanted = session != u32::MAX
            && (!signed_in(session) || locked(session))
            && directory().join("config.json").exists();
        if let Some(current) = &helper
            && (!wanted || current.session != session || !current.running())
        {
            if !current.running() {
                // Back off when the helper keeps exiting.
                retry_after = Instant::now() + Duration::from_secs(5);
            }
            helper = None;
        }
        if wanted && helper.is_none() && Instant::now() >= retry_after {
            match launch(session) {
                Ok(started) => helper = Some(started),
                Err(error) => {
                    log::warn!("cannot start sign-in helper: {error:#}");
                    retry_after = Instant::now() + Duration::from_secs(10);
                }
            }
        }
        match wake.recv_timeout(Duration::from_secs(2)) {
            Ok(true) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            _ => {}
        }
    }
    drop(helper);
    report(ServiceState::Stopped)?;
    Ok(())
}

/// A console session has a user once someone has signed in to it.
fn signed_in(session: u32) -> bool {
    let mut token: HANDLE = ptr::null_mut();
    let found = unsafe { WTSQueryUserToken(session, &mut token) } != 0;
    if found {
        unsafe { CloseHandle(token) };
    }
    found
}

fn locked(session: u32) -> bool {
    let mut buffer: windows_sys::core::PWSTR = ptr::null_mut();
    let mut size = 0;
    unsafe {
        if WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session,
            WTSSessionInfoEx,
            &mut buffer,
            &mut size,
        ) == 0
            || buffer.is_null()
        {
            return false;
        }
        let info = &*buffer.cast::<WTSINFOEXW>();
        let locked = info.Level == 1
            && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32;
        WTSFreeMemory(buffer.cast());
        locked
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Start the helper as SYSTEM in `session`, on the secure sign-in desktop.
fn launch(session: u32) -> Result<Helper> {
    let exe = std::env::current_exe()?;
    unsafe {
        let mut own: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) == 0 {
            bail!("OpenProcessToken: {}", std::io::Error::last_os_error());
        }
        let mut token: HANDLE = ptr::null_mut();
        let duplicated = DuplicateTokenEx(
            own,
            TOKEN_ALL_ACCESS,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        CloseHandle(own);
        if duplicated == 0 {
            bail!("DuplicateTokenEx: {}", std::io::Error::last_os_error());
        }
        let result = (|| {
            if SetTokenInformation(
                token,
                TokenSessionId,
                (&raw const session).cast(),
                size_of::<u32>() as u32,
            ) == 0
            {
                bail!("SetTokenInformation: {}", std::io::Error::last_os_error());
            }
            let mut environment: Vec<u16> = Vec::new();
            for (key, value) in std::env::vars_os()
                .filter(|(key, _)| key != "SEAM_CONFIG_DIR" && key != HELPER_ENV)
                .chain([
                    ("SEAM_CONFIG_DIR".into(), directory().into_os_string()),
                    (HELPER_ENV.into(), "1".into()),
                ])
            {
                environment.extend(
                    format!("{}={}", key.to_string_lossy(), value.to_string_lossy()).encode_utf16(),
                );
                environment.push(0);
            }
            environment.push(0);
            let mut desktop = wide("winsta0\\winlogon");
            let mut command = wide(&format!("\"{}\" run", exe.display()));
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: desktop.as_mut_ptr(),
                ..std::mem::zeroed()
            };
            let mut process: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessAsUserW(
                token,
                ptr::null(),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                environment.as_ptr().cast(),
                ptr::null(),
                &startup,
                &mut process,
            ) == 0
            {
                bail!("CreateProcessAsUserW: {}", std::io::Error::last_os_error());
            }
            CloseHandle(process.hThread);
            log::info!("sign-in helper started in session {session}");
            Ok(Helper {
                process: process.hProcess,
                session,
            })
        })();
        CloseHandle(token);
        result
    }
}

fn manager() -> Result<ServiceManager> {
    ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("opening the service manager")
}

/// Stop the service and wait until it has, so its files can be replaced.
pub fn stop() -> Result<()> {
    let Ok(service) = manager()?.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    ) else {
        return Ok(());
    };
    let _ = service.stop();
    let deadline = Instant::now() + Duration::from_secs(15);
    while service.query_status()?.current_state != ServiceState::Stopped {
        if Instant::now() >= deadline {
            bail!("the sign-in service did not stop");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Stop the service and recreate `%ProgramData%\Seam` from scratch, readable
/// only by SYSTEM and administrators. Runs elevated, before anything else
/// writes there: anyone can create folders in ProgramData, and a folder (or
/// link) planted before Seam was installed must not be reused, since its
/// creator could keep a handle that changes its access back.
pub fn prepare_directory() -> Result<()> {
    stop()?;
    let directory = directory();
    let root = directory
        .parent()
        .context("sign-in directory has no parent")?;
    // Removes a link itself, never what it points to. Everything here is
    // recreated: the copy below, logs and locks on next use.
    match std::fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("replacing {}", root.display()));
        }
    }
    std::fs::create_dir_all(&directory)?;
    let root = root.to_string_lossy();
    // icacls accepts /setowner only on its own.
    super::windows_setup::checked(
        "icacls.exe",
        &[
            &root,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F",
            "/T",
            "/Q",
        ],
    )?;
    super::windows_setup::checked(
        "icacls.exe",
        &[&root, "/setowner", "*S-1-5-32-544", "/T", "/Q"],
    )
}

/// Register (or update) and start the service. Runs elevated, after
/// `prepare_directory`.
pub fn install() -> Result<()> {
    // Seed the copy from the installing person's settings, so the sign-in
    // screen works before anyone signs in again. Never create settings here:
    // the installer may run as an administrator who does not use Seam.
    if let Ok(Some(config)) = Config::load() {
        refresh_copy(&config);
    }
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Seam sign-in screen".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec!["sign-in-service".into()],
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let manager = manager()?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            service.change_config(&info)?;
            service
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    let _ =
        service.set_description("Lets a paired computer's keyboard and mouse sign in to this PC.");
    let _ = service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(5),
        }]),
    });
    service
        .start::<&str>(&[])
        .context("starting the sign-in service")
}

pub fn uninstall() {
    let _ = stop();
    if let Ok(manager) = manager()
        && let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)
    {
        let _ = service.delete();
    }
}
