// The agent runs in the background without a console window on Windows.
// Command-line use attaches to the console of the shell that started it.
#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::Result;
use seam::{config::Config, protocol::monitors, service, status};

const USAGE: &str = "usage: seam-agent [run|supervise|status|check|stop|reset-input|wake|setup [--no-start]|uninstall [--purge]]";

fn main() -> Result<()> {
    let command = std::env::args().nth(1);
    // Exit status for a running agent that wants macOS's current answer.
    if command.as_deref() == Some("accessibility") {
        std::process::exit(if seam::input::accessibility_ready() {
            0
        } else {
            1
        });
    }
    #[cfg(target_os = "macos")]
    stay_out_of_dock();
    #[cfg(windows)]
    {
        if command
            .as_deref()
            .is_some_and(|c| !matches!(c, "run" | "supervise" | "sign-in-service"))
        {
            attach_console();
        }
        seam::windows_dpi::enable_per_monitor_awareness();
    }
    // Secure the machine-wide folder before setup writes its log there.
    #[cfg(windows)]
    if command.as_deref() == Some("setup")
        && let Err(error) = seam::windows_signin::prepare_directory()
    {
        // The machine-wide folder is not safe to write to; use this
        // account's temporary folder for the reason instead.
        seam::logging::init(Some(&std::env::temp_dir().join("seam-setup.log")));
        log::error!("setup failed: {error:#}");
        return Err(error);
    }
    // Background processes write a size-capped log file; commands print.
    let log_file = match command.as_deref() {
        None | Some("run") => seam::config::log_path().ok(),
        Some("supervise") => seam::config::config_dir()
            .ok()
            .map(|directory| directory.join("supervisor.log")),
        #[cfg(windows)]
        Some("sign-in-service") => Some(seam::windows_signin::directory().join("service.log")),
        // The installer runs these hidden; keep their errors for support.
        #[cfg(windows)]
        Some("setup" | "uninstall" | "stop-service") => Some(seam::windows_setup::log_path()),
        _ => None,
    };
    seam::logging::init(log_file.as_deref());
    #[cfg(windows)]
    let flag = |name: &str| std::env::args().any(|arg| arg == name);
    // Commands that do not belong to one person's configuration.
    match command.as_deref() {
        Some("supervise") => return service::supervise(),
        Some("--version" | "-V") => {
            println!("seam-agent {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        #[cfg(windows)]
        Some("setup") => {
            let result = seam::windows_setup::install().and_then(|()| {
                if flag("--no-start") {
                    Ok(())
                } else {
                    seam::windows_setup::start()
                }
            });
            match &result {
                Ok(()) => log::info!("setup {} complete", env!("CARGO_PKG_VERSION")),
                Err(error) => log::error!("setup failed: {error:#}"),
            }
            return result;
        }
        #[cfg(windows)]
        Some("uninstall") => return seam::windows_setup::uninstall(flag("--purge")),
        #[cfg(windows)]
        Some("sign-in-service") => return seam::windows_signin::run_service(),
        #[cfg(windows)]
        Some("stop-service") => return seam::windows_signin::stop(),
        _ => {}
    }
    if matches!(command.as_deref(), None | Some("run")) {
        // Background agents have no console: keep the reason in the log.
        let result = Config::load_or_create().and_then(seam::agent::run);
        if let Err(error) = &result {
            log::error!("agent stopped: {error:#}");
        }
        return result;
    }
    // The remaining commands only inspect or stop an existing setup.
    if !matches!(
        command.as_deref(),
        Some("status" | "stop" | "reset-input" | "check" | "wake")
    ) {
        anyhow::bail!(USAGE);
    }
    let Some(config) = Config::load()? else {
        if command.as_deref() == Some("stop") {
            return Ok(());
        }
        anyhow::bail!("Seam is not set up for this user. Open Seam once to set it up.");
    };
    match command.as_deref() {
        Some("status") => println!(
            "{}",
            serde_json::to_string_pretty(&status::agent(&config).status()?)?
        ),
        Some("stop") => service::stop(&config)?,
        Some("reset-input") => status::agent(&config).reset_input()?,
        Some("wake") => wake(&config)?,
        Some("check") => {
            println!("Config: {}", seam::config::config_path()?.display());
            println!("Log: {}", seam::config::log_path()?.display());
            println!(
                "Agent: {}",
                if service::is_running(&config) {
                    "running"
                } else {
                    "stopped"
                }
            );
            println!("Displays: {}", serde_json::to_string_pretty(&monitors()?)?);
        }
        _ => unreachable!("commands are checked above"),
    }
    Ok(())
}

/// Send Wake-on-LAN to every controlled computer whose address Seam learned,
/// to check that a sleeping one wakes without moving the pointer.
fn wake(config: &Config) -> Result<()> {
    if config.peers.is_empty() {
        anyhow::bail!("This computer doesn't control other computers.");
    }
    for peer in &config.peers {
        let Some(mac) = peer.mac.as_deref().and_then(seam::wake::parse_mac) else {
            println!(
                "{}: no address yet; connect to it once while it's awake",
                peer.label()
            );
            continue;
        };
        let address = peer.address.trim().parse().ok();
        match seam::wake::send(&mac, address) {
            Ok(()) => println!(
                "{}: sent Wake-on-LAN to {}",
                peer.label(),
                seam::wake::format_mac(&mac)
            ),
            Err(error) => println!("{}: {error}", peer.label()),
        }
    }
    Ok(())
}

/// The agent lives inside Seam.app, so macOS gives it the bundle's identity
/// and a second Seam icon in the Dock once it talks to the window server.
/// Make it an accessory, like an LSUIElement helper, before that happens.
#[cfg(target_os = "macos")]
fn stay_out_of_dock() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

/// Print to the console of the shell that started this windowless program,
/// unless its output is already redirected to a file or pipe.
#[cfg(windows)]
fn attach_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process: u32) -> i32;
        fn GetStdHandle(kind: u32) -> isize;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if output == 0 || output == -1 {
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}
