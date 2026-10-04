//! Bulk window actions over the tracked EVE clients (hotkey and tray actions); holds no state.

use eve_maj_core::config::{Config, Position};
use eve_maj_core::log::Scope;
use eve_maj_win::window::is_window;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::scout::{is_generic_character_name, EveWindow};

const SLOG: Scope = Scope::new("manager");

/// Minimize all EVE client windows (hotkey action), regardless of their current state
pub fn minimize_all_clients(eve_windows: &[EveWindow]) {
    SLOG.info(format_args!("Minimizing all EVE clients (hotkey action)"));

    let mut minimized_count = 0;
    for w in eve_windows {
        if !is_window(w.hwnd) {
            continue;
        }
        unsafe { ShowWindowAsync(w.hwnd, SW_FORCEMINIMIZE) };
        minimized_count += 1;
        SLOG.debug(format_args!("Minimized: {}", w.character_name));
    }

    if minimized_count > 0 {
        SLOG.info(format_args!("Minimized {minimized_count} EVE client(s)"));
    } else {
        SLOG.debug(format_args!("No EVE clients to minimize"));
    }
}

/// Kept clear of the virtual screen edges so a restored window's title bar stays grabbable.
const SCREEN_EDGE_MARGIN: i32 = 30;

/// Clamps `pos` to the current virtual screen, in case the screen configuration changed since save.
pub fn clamp_to_virtual_screen(pos: Position) -> Position {
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let max_x = left.max(left + width - SCREEN_EDGE_MARGIN);
    let max_y = top.max(top + height - SCREEN_EDGE_MARGIN);
    Position { x: pos.x.clamp(left, max_x), y: pos.y.clamp(top, max_y) }
}

/// Moves a window's top-left corner to `pos`, restoring it first if minimized/maximized.
pub fn move_client_to_position(hwnd: HWND, pos: Position) {
    if !is_window(hwnd) {
        return;
    }
    unsafe {
        let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut placement) != 0
            && (placement.showCmd == SW_SHOWMINIMIZED as u32 || placement.showCmd == SW_SHOWMAXIMIZED as u32)
        {
            ShowWindowAsync(hwnd, SW_RESTORE);
        }
        let clamped = clamp_to_virtual_screen(pos);
        SetWindowPos(hwnd, HWND_NOTOPMOST, clamped.x, clamped.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Move every EVE client window with a saved position to that position (hotkey action / auto-move-on-login).
pub fn move_all_clients_to_saved_positions(eve_windows: &[EveWindow], config: &Config) {
    SLOG.info(format_args!("Moving all EVE clients to saved positions (hotkey action)"));

    let mut moved_count = 0;
    for w in eve_windows {
        if config.is_excluded_from_auto_move(&w.character_name) {
            continue;
        }
        let Some(pos) = config.character_window_position(&w.character_name) else { continue };
        move_client_to_position(w.hwnd, pos);
        moved_count += 1;
        SLOG.debug(format_args!("Moved {} to saved position ({}, {})", w.character_name, pos.x, pos.y));
    }

    if moved_count > 0 {
        SLOG.info(format_args!("Moved {moved_count} EVE client(s) to saved positions"));
    } else {
        SLOG.debug(format_args!("No EVE clients have a saved position"));
    }
}

/// Close all EVE client windows (hotkey action), except those in the exclude list
pub fn close_all_clients(eve_windows: &[EveWindow], config: &Config) {
    SLOG.info(format_args!("Closing all EVE clients (hotkey action)"));

    let mut closed_count = 0;
    let mut excluded_count = 0;
    for w in eve_windows {
        if !is_window(w.hwnd) {
            continue;
        }
        if config.is_excluded_from_close_all(&w.character_name) {
            SLOG.debug(format_args!("Skipping excluded character: {}", w.character_name));
            excluded_count += 1;
            continue;
        }
        if config.close_all.exclude_login_screen_clients && is_generic_character_name(&w.character_name) {
            SLOG.debug(format_args!("Skipping login-screen client (hwnd {:?})", w.hwnd));
            excluded_count += 1;
            continue;
        }
        unsafe { PostMessageW(w.hwnd, WM_CLOSE, 0, 0) };
        closed_count += 1;
        SLOG.debug(format_args!("Closing: {}", w.character_name));
    }

    if closed_count > 0 {
        SLOG.info(format_args!("Sent close message to {closed_count} EVE client(s) ({excluded_count} excluded)"));
    } else if excluded_count > 0 {
        SLOG.info(format_args!("No clients closed - all {excluded_count} client(s) are excluded"));
    } else {
        SLOG.debug(format_args!("No EVE clients to close"));
    }
}
