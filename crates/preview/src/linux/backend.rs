//! The two seams between the desktop-independent Linux app and a particular compositor.
//!
//! * [`WindowManager`]: which client windows exist, which one is focused, and how to focus one; the Linux counterpart
//!   of the Windows build's Scout + GetForegroundWindow/SetForegroundWindow. Hyprland implements it over its IPC
//!   sockets ([`super::wm_hyprland`]); a standards-based backend would use the ext/wlr foreign-toplevel protocols.
//! * Capture ([`super::capture`]): how a window's pixels get into a buffer we own. Every protocol plugs into the same
//!   buffer pipeline (GBM dmabuf or wl_shm, double-buffered, imported as a GL texture); Hyprland's
//!   hyprland-toplevel-export is the one implemented, ext-image-copy-capture is the standard one to add.
//!
//! Display (wlr-layer-shell overlays) and input are shared by every Wayland compositor that has layer-shell.

use std::os::fd::RawFd;

use eve_maj_core::config::WindowFilter;
use eve_maj_core::display_grid::Rect;
use eve_maj_core::layout::GENERIC_CHARACTER_NAME;

/// A window manager's handle for one toplevel. Hyprland: the window address.
pub type WindowId = u64;

/// One monitor in the compositor's global layout coordinates (logical pixels).
#[derive(Debug, Clone)]
pub struct Monitor {
    /// Connector name, matching wl_output's `name` (e.g. "DP-1").
    pub name: String,
    pub bounds: Rect,
    /// Bounds minus bars' reserved space (the Windows "work area").
    pub work_area: Rect,
    pub scale: f32,
}

/// A tracked client window: the counterpart of the Windows Scout's EveWindow.
#[derive(Debug, Clone)]
pub struct TrackedWindow {
    pub id: WindowId,
    pub title: String,
    pub character_name: String,
    pub pid: i32,
    /// Hidden the way minimizing hides a window on Windows.
    pub minimized: bool,
}

/// A raw toplevel as a window manager reports it, before filtering.
#[derive(Debug, Clone)]
pub struct Toplevel {
    pub id: WindowId,
    pub title: String,
    pub class: String,
    pub initial_class: String,
    pub pid: i32,
    pub minimized: bool,
    /// Top-left in layout coordinates.
    pub at: (i32, i32),
    pub floating: bool,
    /// Monitor (connector name) the window is on.
    pub monitor: String,
}

/// What a window manager's event stream reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmEvent {
    /// Keyboard focus moved to this window, or to no window at all.
    Focus(Option<WindowId>),
    /// Something that can change the window list, titles, monitors or minimized state; rescan.
    Changed,
    /// One of our registered hotkeys was pressed.
    Hotkey(u32),
    /// The compositor reloaded its config, dropping runtime binds; register them again.
    ConfigReloaded,
}

/// A hotkey to bind in the compositor: xkb key name (or mouse button) plus modifier names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBind {
    pub id: u32,
    /// SUPER/CTRL/ALT/SHIFT.
    pub modifiers: Vec<&'static str>,
    /// xkb key name ("F12", "Prior", "KP_7") or a mouse button ("mouse:275", "mouse_up").
    pub key: String,
    pub repeating: bool,
    pub description: String,
}

pub trait WindowManager {
    fn name(&self) -> &'static str;
    /// Monitors in a stable order (the order display.monitorIndex counts in).
    fn monitors(&self) -> Vec<Monitor>;
    /// Every mapped toplevel, in a stable order.
    fn toplevels(&self) -> Vec<Toplevel>;
    /// The focused window, if any.
    fn active(&self) -> Option<WindowId>;
    /// Focuses (and raises/unhides) a window.
    fn focus(&self, id: WindowId);
    /// A pollable fd that becomes readable when [`WindowManager::read_events`] has something; None if the backend can't notify.
    fn event_fd(&self) -> Option<RawFd>;
    /// Drains pending events without blocking.
    fn read_events(&mut self) -> Vec<WmEvent>;

    /// Replaces every hotkey we registered with `binds`; presses arrive as [`WmEvent::Hotkey`]. Returns how many took.
    fn set_hotkeys(&mut self, binds: &[KeyBind]) -> usize;
    /// Closes a window the way its close button would.
    fn close(&self, id: WindowId);
    /// Hides a window the way minimizing does on Windows.
    fn minimize(&self, id: WindowId);
    /// Undoes [`WindowManager::minimize`] onto the current workspace and focuses the window.
    fn restore(&self, id: WindowId);
    /// Places a window's top-left at (x, y) in layout coordinates, floating it first if the layout tiles it.
    fn move_to(&self, id: WindowId, x: i32, y: i32);
}

/// The Windows-side image path of a Wine/Proton process (argv[0], e.g. `C:\EVE\...\exefile.exe`), else the native executable.
pub fn process_image_path(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        if let Some(argv0) = cmdline.split(|&b| b == 0).next().filter(|a| !a.is_empty()) {
            let argv0 = String::from_utf8_lossy(argv0).into_owned();
            if argv0.to_ascii_lowercase().ends_with(".exe") {
                return Some(argv0);
            }
        }
    }
    std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Whether a toplevel counts as a tracked window under `filters`.
///
/// Windows matches on window class *and* executable. On Linux a filter's `classNames` are Windows class names (EVE's
/// `trinityWindow`) that never show up as a Linux window class, so a class match is optional: the window's class (or
/// initial class) matching a listed class name is enough on its own, and otherwise the executable check decides,
/// tested against the process's Windows image path (Wine/Proton argv[0]) and the window class (Wine sets it to the
/// exe name, e.g. `exefile.exe`).
pub fn matches_filters(t: &Toplevel, filters: &[WindowFilter], image_path: Option<&str>) -> bool {
    filters.iter().filter(|f| f.enabled).any(|f| {
        if f.class_names.iter().any(|c| c.eq_ignore_ascii_case(&t.class) || c.eq_ignore_ascii_case(&t.initial_class)) {
            return true;
        }
        if f.executable_names.is_empty() {
            return false;
        }
        image_path.is_some_and(|p| f.matches_executable(p)) || f.matches_executable(&t.class) || f.matches_executable(&t.initial_class)
    })
}

/// "EVE - Character Name" -> "Character Name"; anything without the separator (the login screen's "EVE") stays whole.
pub fn character_name_from_title(title: &str) -> String {
    match title.find(" - ") {
        Some(i) if i + 3 < title.len() => title[i + 3..].to_owned(),
        _ if title.is_empty() => GENERIC_CHARACTER_NAME.to_owned(),
        _ => title.to_owned(),
    }
}

/// The tracked windows among `toplevels`, in the window manager's order.
pub fn tracked_windows(toplevels: &[Toplevel], filters: &[WindowFilter]) -> Vec<TrackedWindow> {
    toplevels
        .iter()
        .filter(|t| matches_filters(t, filters, process_image_path(t.pid).as_deref()))
        .map(|t| TrackedWindow { id: t.id, title: t.title.clone(), character_name: character_name_from_title(&t.title), pid: t.pid, minimized: t.minimized })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toplevel(class: &str, title: &str) -> Toplevel {
        Toplevel { id: 1, title: title.into(), class: class.into(), initial_class: class.into(), pid: 0, minimized: false, at: (0, 0), floating: false, monitor: String::new() }
    }

    #[test]
    fn eve_filter_matches_wine_and_proton_clients() {
        let f = vec![WindowFilter::eve_default()];
        assert!(matches_filters(&toplevel("exefile.exe", "EVE - A"), &f, None));
        assert!(matches_filters(&toplevel("steam_app_8500", "EVE - A"), &f, Some("C:\\EVE\\SharedCache\\tq\\bin64\\exefile.exe")));
        assert!(!matches_filters(&toplevel("steam_app_8500", "EVE"), &f, Some("C:\\EVE\\Launcher\\evelauncher.exe")));
        assert!(!matches_filters(&toplevel("firefox", "EVE - fake"), &f, Some("/usr/lib/firefox/firefox")));
    }

    #[test]
    fn names_from_titles() {
        assert_eq!(character_name_from_title("EVE - Some Pilot"), "Some Pilot");
        assert_eq!(character_name_from_title("EVE"), "EVE");
        assert_eq!(character_name_from_title(""), "EVE");
    }
}
