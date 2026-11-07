//! Machine setup used by the Windows installer: sign-in tasks for the agent
//! and tray, and a firewall rule for the local network. Runs elevated.
use anyhow::{Context, Result, bail};
use std::{
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};

const NO_WINDOW: u32 = 0x0800_0000;
const AGENT_TASK: &str = crate::service::AGENT_TASK;
const TRAY_TASK: &str = "Seam Tray";
const FIREWALL_RULE: &str = "Seam";
/// Built-in Users group: the tasks run in the session of whoever signs in.
const USERS: &str = "S-1-5-32-545";
/// SYSTEM and Administrators manage the tasks; signed-in users may start
/// them, so the settings window can resume sharing after a pause.
const TASK_ACCESS: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;AU)";

fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .creation_flags(NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))
}

pub(crate) fn checked(program: &str, args: &[&str]) -> Result<()> {
    let output = run(program, args)?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A sign-in task. `elevated` lets the agent control elevated windows.
fn task_xml(description: &str, command: &Path, arguments: &str, elevated: bool) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Seam</Author>
    <Description>{description}</Description>
    <SecurityDescriptor>{TASK_ACCESS}</SecurityDescriptor>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Users">
      <GroupId>{USERS}</GroupId>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Users">
    <Exec>
      <Command>"{command}"</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        command = escape(&command.to_string_lossy()),
        run_level = if elevated {
            "HighestAvailable"
        } else {
            "LeastPrivilege"
        },
    )
}

fn register(name: &str, xml: &str) -> Result<()> {
    let file = std::env::temp_dir().join(format!("seam-{}.xml", name.replace(' ', "-")));
    // Task Scheduler expects UTF-16 to match the declared encoding.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&file, bytes)?;
    let result = checked(
        "schtasks.exe",
        &[
            "/Create",
            "/TN",
            name,
            "/XML",
            &file.to_string_lossy(),
            "/F",
        ],
    );
    let _ = std::fs::remove_file(&file);
    result.with_context(|| format!("registering the {name} task"))
}

fn remove_task(name: &str) {
    let _ = run("schtasks.exe", &["/End", "/TN", name]);
    let _ = run("schtasks.exe", &["/Delete", "/TN", name, "/F"]);
}

fn remove_rule(name: &str) {
    let _ = run(
        "netsh.exe",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={name}"),
        ],
    );
}

pub fn install() -> Result<()> {
    let agent = std::env::current_exe()?;
    let gui = agent.with_file_name("seam.exe");
    remove_rule(FIREWALL_RULE);
    register(
        AGENT_TASK,
        &task_xml(
            "Shares this keyboard and mouse with your other computers.",
            &agent,
            "supervise",
            true,
        ),
    )?;
    register(
        TRAY_TASK,
        &task_xml(
            "Shows Seam in the notification area.",
            &gui,
            "--tray",
            false,
        ),
    )?;
    // Program-based, so a custom port needs no new rule. Limited to the
    // local subnet: Seam only talks to computers on the same network.
    checked(
        "netsh.exe",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={FIREWALL_RULE}"),
            "dir=in",
            "action=allow",
            &format!("program={}", agent.display()),
            "remoteip=localsubnet",
            "profile=any",
            "enable=yes",
        ],
    )
    .context("adding the firewall rule")?;
    crate::windows_signin::install().context("installing the sign-in screen service")
}

/// Start the tasks now so the person does not have to sign out first.
pub fn start() -> Result<()> {
    checked("schtasks.exe", &["/Run", "/TN", AGENT_TASK])?;
    checked("schtasks.exe", &["/Run", "/TN", TRAY_TASK])
}

/// Machine-wide Seam data: the sign-in copy and this log.
fn machine_directory() -> std::path::PathBuf {
    crate::windows_signin::directory()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::windows_signin::directory)
}

/// Where `setup` and `uninstall` record what they did.
pub fn log_path() -> std::path::PathBuf {
    machine_directory().join("setup.log")
}

/// Remove everything `install` added. With `purge`, also delete every
/// person's Seam settings, including pairing secrets.
pub fn uninstall(purge: bool) -> Result<()> {
    for task in [AGENT_TASK, TRAY_TASK] {
        remove_task(task);
    }
    remove_rule(FIREWALL_RULE);
    crate::windows_signin::uninstall();
    if purge {
        for directory in user_seam_directories() {
            match std::fs::remove_dir_all(&directory) {
                Ok(()) => log::info!("removed {}", directory.display()),
                Err(error) => log::warn!("cannot remove {}: {error}", directory.display()),
            }
        }
    }
    log::info!("uninstall complete");
    // Last, because this also deletes the log being written.
    let _ = std::fs::remove_dir_all(machine_directory());
    Ok(())
}

/// `%APPDATA%\Seam` of every profile on this computer. The uninstaller runs
/// elevated, possibly as a different administrator, so the current user's
/// folder alone is not enough.
fn user_seam_directories() -> Vec<std::path::PathBuf> {
    let Some(profiles) = std::env::var_os("PUBLIC")
        .map(std::path::PathBuf::from)
        .and_then(|public| public.parent().map(Path::to_path_buf))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(profiles) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("AppData").join("Roaming").join("Seam"))
        .filter(|directory| directory.is_dir())
        .collect()
}
