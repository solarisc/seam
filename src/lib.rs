pub mod agent;
pub mod clipboard;
pub mod config;
pub mod discovery;
mod display_watch;
pub mod geometry;
pub mod input;
#[cfg(target_os = "macos")]
mod input_priority;
mod input_traffic;
mod keymap;
pub mod layout;
pub mod logging;
#[cfg(target_os = "macos")]
mod mac_capture;
#[cfg(target_os = "macos")]
mod mac_displays;
pub mod network;
pub mod pairing;
mod ports;
mod power;
pub mod protocol;
pub mod service;
mod session;
pub mod shutdown;
pub mod status;
#[cfg(target_os = "windows")]
mod windows_capture;
#[cfg(target_os = "windows")]
mod windows_cursor;
#[cfg(target_os = "windows")]
mod windows_displays;
#[cfg(target_os = "windows")]
pub mod windows_dpi;
#[cfg(target_os = "windows")]
mod windows_input;
#[cfg(target_os = "windows")]
mod windows_raw;
#[cfg(target_os = "windows")]
mod windows_scroll;
#[cfg(target_os = "windows")]
pub mod windows_setup;
#[cfg(target_os = "windows")]
pub mod windows_signin;

#[cfg(feature = "gui")]
pub mod gui;

#[cfg(all(target_os = "windows", feature = "gui"))]
pub mod windows_tray;

#[cfg(target_os = "windows")]
mod windows_pointer_lock;
#[cfg(target_os = "windows")]
mod windows_pointer_presence;
#[cfg(target_os = "windows")]
mod windows_priority;

pub mod transfers;
pub mod wake;
mod watchdog;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows_stacks;
