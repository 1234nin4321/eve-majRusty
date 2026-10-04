//! eve-maj-preview: live DWM thumbnails of EVE Online client windows.
//!
//! Startup, the hidden timer window that dispatches tray/IPC/timer messages, the per-tick scan → paint pipeline,
//! profile reloads, the config dialog's live-preview patches, and crash logging.
//!
//! Not ported yet (Milestone 2): hotkeys, chatlog monitoring and the combat/mining/bounty/resource overlays that
//! feed from it, the client list and history panels, and region selection. Those settings load and save normally
//! but have no effect in this build; each logs a warning when enabled.

#![cfg_attr(windows, windows_subsystem = "windows")]
// Helpers ported ahead of the Milestone 2 modules that call them (list/history panels, hotkeys, chatlog) would
// otherwise warn as unused until those land.
#![allow(dead_code)]

#[cfg(windows)]
mod fonts;
#[cfg(windows)]
mod gdi_overlay;
#[cfg(windows)]
mod globals;
#[cfg(windows)]
mod hotkeys;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod manager;
#[cfg(windows)]
mod painter;
#[cfg(windows)]
mod scout;
#[cfg(windows)]
mod tray;

#[cfg(windows)]
mod app;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("eve-maj-preview only runs on Windows");
}
