//! Starting, stopping and auto-starting the background agent, and bringing the
//! settings window forward when the agent needs the person's attention.
use crate::{
    config::{Config, log_path},
    status,
};
use anyhow::{Context, Result, bail};
use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const AGENT_TASK: &str = "Seam Agent";

fn sibling(name: &str) -> Result<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = std::env::current_exe()?.with_file_name(exe);
    if !path.exists() {
        bail!(
            "Part of Seam is missing ({}). Reinstall Seam.",
            path.display()
        );
    }
    Ok(path)
}

pub fn agent_binary() -> Result<PathBuf> {
    sibling("seam-agent")
}

pub fn is_running(config: &Config) -> bool {
    status::agent(config).status().is_ok()
}

/// Save `config` and restart the agent with it. Refuses while the pointer is
/// on another computer so a restart cannot strand keys or buttons there.
pub fn apply(previous: &Config, config: &Config) -> Result<()> {
    config.validate()?;
    if let Ok(snapshot) = status::agent(previous).status() {
        if snapshot.has_remote_control() {
            bail!("Move the pointer back to this computer, then try again.");
        }
        status::agent(previous).stop()?;
        wait_until_stopped(previous)?;
    }
    config.save()?;
    // Changing a setting while paused keeps sharing paused.
    if config.paused {
        return Ok(());
    }
    start(config)
}

pub fn start(config: &Config) -> Result<()> {
    if is_running(config) {
        return Ok(());
    }
    let log = log_path()?;
    let log_start = std::fs::metadata(&log).map_or(0, |meta| meta.len());
    let installed = start_installed()?;
    if !installed {
        spawn_agent()?;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut retried = Instant::now();
    while !is_running(config) {
        if Instant::now() >= deadline {
            return Err(match stop_reason(&log, log_start) {
                Some(reason) => anyhow::anyhow!(reason),
                None => anyhow::anyhow!("See {} for details.", log.display()),
            });
        }
        // A task instance that was still exiting ignores a new start, so
        // ask again until the agent answers.
        if installed && retried.elapsed() >= Duration::from_secs(3) {
            start_installed()?;
            retried = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Why the agent stopped, from what it logged after byte `from` of its log.
fn stop_reason(log: &std::path::Path, from: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(log).ok()?;
    // A rotated log starts again from zero.
    let from = if file.metadata().ok()?.len() < from {
        0
    } else {
        from
    };
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut text = String::new();
    file.take(256 * 1024).read_to_string(&mut text).ok()?;
    text.lines()
        .rev()
        .find_map(|line| line.split_once("agent stopped: ").map(|(_, reason)| reason))
        .map(str::to_string)
}

pub fn stop(config: &Config) -> Result<()> {
    let agent = status::agent(config);
    let Ok(snapshot) = agent.status() else {
        return Ok(());
    };
    if snapshot.has_remote_control() {
        bail!("Move the pointer back to this computer, then try again.");
    }
    agent.stop()?;
    wait_until_stopped(config)
}

/// The agent holds its local control port for its whole lifetime.
fn wait_until_stopped(config: &Config) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match TcpListener::bind(("127.0.0.1", config.listen_port + 2)) {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                return Err(error).context("Seam is still stopping. Try again in a moment.");
            }
        }
    }
}

fn spawn_agent() -> Result<()> {
    // The agent writes its own log file.
    let mut command = Command::new(agent_binary()?);
    command
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(NO_WINDOW);
    }
    command.spawn().context("starting the Seam agent")?;
    Ok(())
}

/// Start through the installed sign-in task or login item, so the agent keeps
/// its privileges and crash recovery. Returns `false` when none is installed.
fn start_installed() -> Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let query = Command::new("schtasks.exe")
            .args(["/Query", "/TN", AGENT_TASK])
            .creation_flags(NO_WINDOW)
            .output()?;
        if !query.status.success() {
            return Ok(false);
        }
        let run = Command::new("schtasks.exe")
            .args(["/Run", "/TN", AGENT_TASK])
            .creation_flags(NO_WINDOW)
            .output()?;
        if !run.status.success() {
            bail!(
                "Seam's background task did not start. Restart this computer or reinstall Seam. ({})",
                String::from_utf8_lossy(&run.stderr).trim()
            );
        }
        Ok(true)
    }
    #[cfg(target_os = "macos")]
    {
        let Some(plist) = mac::installed_plist() else {
            return Ok(false);
        };
        mac::load(&plist)?;
        Ok(true)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    Ok(false)
}

/// Keep the agent running: restart it after a crash with a short backoff,
/// and stop when it exits cleanly (the person paused sharing).
pub fn supervise() -> Result<()> {
    let agent = std::env::current_exe()?;
    let mut delay = Duration::from_secs(2);
    loop {
        let started = Instant::now();
        let mut command = Command::new(&agent);
        command
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(NO_WINDOW);
        }
        let status = command.status().context("starting the Seam agent")?;
        if status.success() {
            log::info!("agent stopped");
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(60) {
            delay = Duration::from_secs(2);
        }
        log::warn!(
            "agent exited ({status}); restarting in {}s",
            delay.as_secs()
        );
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// Bring the settings window forward, opening it if necessary.
pub fn show_settings() {
    #[cfg(all(windows, feature = "gui"))]
    if crate::windows_tray::request_settings() {
        return;
    }
    #[cfg(target_os = "macos")]
    if let Some(bundle) = mac::bundle() {
        let _ = Command::new("/usr/bin/open").arg(bundle).spawn();
        return;
    }
    if let Ok(gui) = sibling("seam") {
        let _ = Command::new(gui).spawn();
    }
}

/// Whether the agent starts automatically when this person signs in.
pub fn starts_at_login() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("schtasks.exe")
            .args(["/Query", "/TN", AGENT_TASK])
            .creation_flags(NO_WINDOW)
            .output()
            .is_ok_and(|output| output.status.success())
    }
    #[cfg(target_os = "macos")]
    {
        mac::installed_plist().is_some()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    false
}

/// Register the agent to start at login when running from an installed app.
/// Windows registers its sign-in task in the installer instead.
pub fn ensure_login_item() -> Result<()> {
    #[cfg(target_os = "macos")]
    mac::ensure()?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::path::Path;

    const LABEL: &str = "dev.seam.agent";

    fn plist_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(format!("Library/LaunchAgents/{LABEL}.plist")))
    }

    /// The enclosing `.app` when running from an installed bundle.
    pub fn bundle() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let bundle = exe.parent()?.parent()?.parent()?;
        (bundle.extension()? == "app").then(|| bundle.to_path_buf())
    }

    /// The login item, when it starts this copy of the agent.
    pub fn installed_plist() -> Option<PathBuf> {
        let agent = agent_binary().ok()?;
        plist_path().filter(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|plist| plist.contains(&*agent.to_string_lossy()))
        })
    }

    fn domain() -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }

    pub fn load(plist: &Path) -> Result<()> {
        let target = format!("{}/{LABEL}", domain());
        let loaded = Command::new("/bin/launchctl")
            .args(["print", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success();
        if !loaded {
            let status = Command::new("/bin/launchctl")
                .args(["bootstrap", &domain()])
                .arg(plist)
                .status()?;
            if !status.success() {
                bail!("launchctl could not load the Seam agent");
            }
        }
        // RunAtLoad starts a freshly loaded agent; kickstart covers a paused one.
        let _ = Command::new("/bin/launchctl")
            .args(["kickstart", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }

    fn render(agent: &Path) -> String {
        fn escape(value: &str) -> String {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }
        let agent = escape(&agent.to_string_lossy());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{LABEL}</string>
	<key>ProgramArguments</key><array><string>{agent}</string><string>run</string></array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
	<key>ThrottleInterval</key><integer>10</integer>
	<key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
        )
    }

    /// Point the login item at this bundle's agent, replacing a stale one
    /// left by an app that was moved or updated.
    pub fn ensure() -> Result<()> {
        if bundle().is_none() {
            return Ok(());
        }
        let plist = plist_path().context("no home directory")?;
        let contents = render(&agent_binary()?);
        if std::fs::read_to_string(&plist).is_ok_and(|current| current == contents) {
            return Ok(());
        }
        std::fs::create_dir_all(plist.parent().unwrap())?;
        let target = format!("{}/{LABEL}", domain());
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &target])
            .stderr(Stdio::null())
            .status();
        // bootout returns before the job is gone; loading too early fails.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && Command::new("/bin/launchctl")
                .args(["print", &target])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success()
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::write(&plist, contents)?;
        load(&plist)
    }
}
