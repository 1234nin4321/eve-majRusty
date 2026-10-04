//! System tray icon and its context menu (profiles, toggles, exit).

use std::cell::RefCell;

use eve_maj_core::config::{GlobalSettings, Config};
use eve_maj_core::log::Scope;
use eve_maj_core::types::ViewMode;
use eve_maj_core::update::UPDATE_STATUS;
use eve_maj_win::{shell, wide, window};
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT};
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::globals;
use crate::manager;

const SLOG: Scope = Scope::new("tray");

pub const WM_TRAYICON: u32 = WM_USER + 1;
pub const WM_SWITCH_PROFILE: u32 = WM_APP + 4;
pub const WM_HOTKEYS_STATE_CHANGED: u32 = WM_APP + 12;
pub const WM_TOGGLE_VISIBILITY: u32 = WM_APP + 13;

pub const IDM_EXIT: u16 = 1001;
pub const IDM_UPDATE: u16 = 1002;
pub const IDM_TOGGLE_DRAGGING: u16 = 1003;
pub const IDM_SUSPEND_HOTKEYS: u16 = 1004;
pub const IDM_TOGGLE_AUTO_MINIMIZE: u16 = 1005;
pub const IDM_TOGGLE_VISIBILITY: u16 = 1006;
pub const IDM_OPEN_CONFIG: u16 = 1007;
pub const IDM_CLOSE_ALL_CLIENTS: u16 = 1008;
pub const IDM_TOGGLE_NOTIF_HISTORY: u16 = 1009;
pub const IDM_CLEAR_NOTIF_HISTORY: u16 = 1010;
pub const IDM_TOGGLE_TRAVEL_MODE: u16 = 1011;
pub const IDM_RESTORE_SAVED_POSITIONS: u16 = 1012;
pub const IDM_PROFILE_BASE: u16 = 2000;

thread_local! {
    /// The profile list as shown in the last menu, so a menu command id maps back to the name it showed.
    static PROFILE_LIST_CACHE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Set by the menu handler, taken by the timer window's WM_SWITCH_PROFILE handler.
    static PENDING_PROFILE_NAME: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub struct TrayIcon {
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
    owns_icon: bool,
}

#[derive(Debug)]
pub enum TrayError {
    LoadIconFailed,
    AddTrayIconFailed,
}

impl std::fmt::Display for TrayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::LoadIconFailed => "LoadIconFailed",
            Self::AddTrayIconFailed => "AddTrayIconFailed",
        })
    }
}

/// Copies `text` into a fixed UTF-16 field, truncating to leave room for the terminator.
fn copy_wide<const N: usize>(dest: &mut [u16; N], text: &str) {
    let units: Vec<u16> = text.encode_utf16().take(N - 1).collect();
    dest[..units.len()].copy_from_slice(&units);
    dest[units.len()] = 0;
}

impl TrayIcon {
    pub fn new(hwnd: HWND) -> Result<TrayIcon, TrayError> {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;

        let icon_path = wide("icon.ico");
        let custom_icon = unsafe { LoadImageW(std::ptr::null_mut(), icon_path.as_ptr(), IMAGE_ICON, 16, 16, LR_LOADFROMFILE) };
        let owns_icon = !custom_icon.is_null();
        if owns_icon {
            nid.hIcon = custom_icon;
        } else {
            SLOG.warn(format_args!("Failed to load custom tray icon, using default"));
            let icon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
            if icon.is_null() {
                SLOG.err(format_args!("Failed to load application icon"));
                return Err(TrayError::LoadIconFailed);
            }
            nid.hIcon = icon;
        }

        copy_wide(&mut nid.szTip, "EVE-Maj Preview");

        if unsafe { Shell_NotifyIconW(NIM_ADD, &nid) } == 0 {
            SLOG.err(format_args!("Failed to add system tray icon"));
            if owns_icon {
                unsafe { DestroyIcon(nid.hIcon) };
            }
            return Err(TrayError::AddTrayIconFailed);
        }
        SLOG.debug(format_args!("System tray icon created"));
        Ok(TrayIcon { hwnd, nid, owns_icon })
    }

    /// Show a Windows tray balloon notification; `title`/`text` are truncated to fit szInfoTitle/szInfo if longer.
    pub fn show_balloon(&mut self, title: &str, text: &str, info_flags: u32) {
        copy_wide(&mut self.nid.szInfoTitle, title);
        copy_wide(&mut self.nid.szInfo, text);
        self.nid.dwInfoFlags = info_flags;
        self.nid.uFlags |= NIF_INFO;
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.nid) } == 0 {
            SLOG.warn(format_args!("Failed to show tray balloon notification"));
        }
    }

    pub fn handle_tray_message(&self, lparam: LPARAM) {
        let event = lparam as u32;
        if event == WM_RBUTTONUP {
            self.show_context_menu();
        } else if event == WM_LBUTTONDBLCLK {
            SLOG.info(format_args!("Opening configuration dialog from system tray double-click"));
            open_config_dialog();
        }
    }

    fn show_context_menu(&self) {
        let config = globals::config();
        let mut cursor = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut cursor) } == 0 {
            SLOG.err(format_args!("Failed to get cursor position"));
            return;
        }
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            SLOG.err(format_args!("Failed to create popup menu"));
            return;
        }
        let profile_submenu = unsafe { CreatePopupMenu() };
        if profile_submenu.is_null() {
            SLOG.err(format_args!("Failed to create profile submenu"));
            unsafe { DestroyMenu(menu) };
            return;
        }
        // The submenu is destroyed automatically along with its parent.

        let profiles = GlobalSettings::enumerate_profiles().unwrap_or_else(|err| {
            SLOG.err(format_args!("Failed to enumerate profiles: {err}"));
            Vec::new()
        });
        let append = |m: HMENU, flags: u32, id: usize, text: Option<&str>| {
            let w = text.map(wide);
            unsafe { AppendMenuW(m, flags, id, w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr())) };
        };
        let checked = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };

        // Safety limit - keeps IDs within the IDM_PROFILE_BASE range.
        for (i, profile) in profiles.iter().enumerate().take(1000) {
            append(profile_submenu, checked(*profile == config.profile_name), (IDM_PROFILE_BASE + i as u16) as usize, Some(profile));
        }
        if profiles.is_empty() {
            append(profile_submenu, MF_STRING, 0, Some("(No profiles found)"));
        }
        PROFILE_LIST_CACHE.with(|c| *c.borrow_mut() = profiles);

        append(menu, MF_POPUP, profile_submenu as usize, Some("Load Profile"));
        append(menu, MF_STRING, IDM_OPEN_CONFIG as usize, Some("Open Configuration..."));
        append(menu, MF_SEPARATOR, 0, None);
        append(menu, checked(config.interaction.enable_dragging), IDM_TOGGLE_DRAGGING as usize, Some("Enable Dragging"));
        append(menu, checked(config.auto_minimize.enabled), IDM_TOGGLE_AUTO_MINIMIZE as usize, Some("Enable Auto-Minimize"));
        append(menu, checked(config.travel.enabled), IDM_TOGGLE_TRAVEL_MODE as usize, Some("Enable Travel Mode"));

        let visibility_flags = if config.display.view_mode == ViewMode::Nothing {
            MF_STRING | MF_GRAYED
        } else {
            // No thumbnails: default to visible state.
            let visible = globals::painter().is_none_or(|p| p.thumbnails.first().is_none_or(|t| t.visibility_state.is_visible()));
            checked(visible)
        };
        append(menu, visibility_flags, IDM_TOGGLE_VISIBILITY as usize, Some("Show Thumbnails"));
        append(menu, MF_STRING, IDM_RESTORE_SAVED_POSITIONS as usize, Some("Restore Saved Positions"));
        append(menu, MF_SEPARATOR, 0, None);

        let history_visible = globals::painter().map_or(config.display.show_notif_info_panel, |p| p.is_notif_info_panel_visible());
        append(menu, checked(history_visible), IDM_TOGGLE_NOTIF_HISTORY as usize, Some("Show History Panel"));
        append(menu, MF_STRING, IDM_CLEAR_NOTIF_HISTORY as usize, Some("Clear Notification History"));
        append(menu, MF_SEPARATOR, 0, None);

        if let Some(hkm) = globals::hotkey_manager() {
            append(menu, checked(hkm.are_hotkeys_suspended()), IDM_SUSPEND_HOTKEYS as usize, Some("Suspend Hotkeys"));
            append(menu, MF_SEPARATOR, 0, None);
        }

        if UPDATE_STATUS.is_available() {
            SLOG.debug(format_args!("Adding update menu item"));
            append(menu, MF_STRING, IDM_UPDATE as usize, Some("Update Available!"));
            append(menu, MF_SEPARATOR, 0, None);
        }

        append(menu, MF_STRING, IDM_CLOSE_ALL_CLIENTS as usize, Some("Close All Clients"));
        append(menu, MF_SEPARATOR, 0, None);
        append(menu, MF_STRING, IDM_EXIT as usize, Some("Exit"));

        unsafe {
            // Required to make menu disappear when clicking outside
            SetForegroundWindow(self.hwnd);
            TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, cursor.x, cursor.y, 0, self.hwnd, std::ptr::null());
            DestroyMenu(menu);
        }
    }

    pub fn take_pending_profile_name() -> Option<String> {
        PENDING_PROFILE_NAME.with(|p| p.borrow_mut().take())
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &self.nid);
            if self.owns_icon {
                DestroyIcon(self.nid.hIcon);
            }
        }
        PROFILE_LIST_CACHE.with(|c| c.borrow_mut().clear());
        SLOG.debug(format_args!("System tray icon removed"));
    }
}

/// Saves `config` to its current profile file, logging (but not propagating) any failure.
fn save_current_profile(config: &Config, context: &str) {
    if let Err(err) = config.save_current_profile() {
        SLOG.err(format_args!("Failed to save config after toggling {context}: {err}"));
    }
}

/// Handles a tray menu WM_COMMAND; returns whether the id was one of ours.
pub fn handle_menu_command(command_id: u16) -> bool {
    let config = globals::config();
    match command_id {
        IDM_EXIT => {
            SLOG.info(format_args!("Exit requested from system tray"));
            unsafe { PostQuitMessage(0) };
        }
        IDM_TOGGLE_DRAGGING => {
            config.interaction.enable_dragging = !config.interaction.enable_dragging;
            SLOG.info(format_args!("Thumbnail dragging toggled: {}", if config.interaction.enable_dragging { "enabled" } else { "disabled" }));
            save_current_profile(config, "dragging");
        }
        IDM_OPEN_CONFIG => {
            SLOG.info(format_args!("Opening configuration dialog from system tray"));
            open_config_dialog();
        }
        IDM_TOGGLE_AUTO_MINIMIZE => {
            // Toggle the auto-minimize setting temporarily (not saved)
            config.auto_minimize.enabled = !config.auto_minimize.enabled;
            SLOG.info(format_args!("Auto-minimize toggled: {}", if config.auto_minimize.enabled { "enabled" } else { "disabled" }));
        }
        IDM_TOGGLE_TRAVEL_MODE => {
            config.travel.enabled = !config.travel.enabled;
            SLOG.info(format_args!("Travel Mode toggled: {}", if config.travel.enabled { "enabled" } else { "disabled" }));
            save_current_profile(config, "Travel Mode");
        }
        IDM_TOGGLE_VISIBILITY => {
            SLOG.info(format_args!("Toggle visibility requested from system tray"));
            match globals::timer_hwnd() {
                Some(hwnd) => unsafe {
                    PostMessageW(hwnd, WM_TOGGLE_VISIBILITY, 0, 0);
                },
                None => SLOG.err(format_args!("Timer window not available for toggle visibility")),
            }
        }
        IDM_TOGGLE_NOTIF_HISTORY => {
            SLOG.warn(format_args!("The History Panel is not available in this build yet"));
        }
        IDM_CLEAR_NOTIF_HISTORY => {
            SLOG.info(format_args!("Clear notification history requested from system tray"));
            match globals::painter() {
                Some(p) => p.clear_notification_history(),
                None => SLOG.err(format_args!("Painter not available for clear notification history")),
            }
        }
        IDM_SUSPEND_HOTKEYS => {
            if let Some(hkm) = globals::hotkey_manager() {
                hkm.handle_suspend_hotkeys_request();
            }
        }
        IDM_RESTORE_SAVED_POSITIONS => {
            SLOG.info(format_args!("Restore saved positions requested from system tray"));
            match globals::hotkey_manager() {
                Some(hkm) => hkm.handle_move_to_saved_positions_request(),
                // Not in the Zig build (which always had a hotkey manager): do the move directly until hotkeys are ported.
                None => {
                    if let Some(scout) = globals::scout() {
                        manager::move_all_clients_to_saved_positions(&scout.windows, config);
                    }
                }
            }
        }
        IDM_CLOSE_ALL_CLIENTS => {
            SLOG.info(format_args!("Close all clients requested from system tray"));
            match globals::scout() {
                Some(scout) => manager::close_all_clients(&scout.windows, config),
                None => SLOG.err(format_args!("Scout not available for close all clients")),
            }
        }
        IDM_UPDATE => {
            SLOG.info(format_args!("Opening releases page from tray menu"));
            eve_maj_app::updater::open_releases_page();
        }
        id if (IDM_PROFILE_BASE..IDM_PROFILE_BASE + 1000).contains(&id) => {
            let index = (id - IDM_PROFILE_BASE) as usize;
            let Some(selected) = PROFILE_LIST_CACHE.with(|c| c.borrow().get(index).cloned()) else { return false };
            SLOG.info(format_args!("Profile selected from menu: {selected}"));
            PENDING_PROFILE_NAME.with(|p| *p.borrow_mut() = Some(selected));
            match globals::timer_hwnd() {
                Some(hwnd) => unsafe {
                    PostMessageW(hwnd, WM_SWITCH_PROFILE, 0, 0);
                },
                None => SLOG.err(format_args!("Timer window not available for profile switch")),
            }
        }
        _ => return false,
    }
    true
}

/// Launch config.exe, which is installed alongside the main executable
fn open_config_dialog() {
    let Some(exe_dir) = window::self_exe_dir() else {
        SLOG.err(format_args!("Failed to determine executable directory"));
        return;
    };
    let config_exe = exe_dir.join("config.exe");
    SLOG.info(format_args!("Launching configuration dialog: {}", config_exe.display()));
    if !shell::shell_open(&config_exe.to_string_lossy(), Some(&exe_dir.to_string_lossy())) {
        SLOG.err(format_args!("Failed to launch config.exe"));
    }
}
