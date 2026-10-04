//! Placeholder for hotkeys.zig's HotkeyManager, which is not ported yet (Milestone 2).
//!
//! The painter, input and tray code already call into the manager through these methods, exactly where the Zig
//! build did, so the real port can drop in without touching its callers. Until then no manager is ever
//! created (`globals::HOTKEY_MANAGER` stays empty), so none of these run; they only define the boundary.

use windows_sys::Win32::Foundation::HWND;

pub struct HotkeyManager {
    _private: (),
}

impl HotkeyManager {
    pub fn is_character_excluded(&self, _character_name: &str) -> bool {
        false
    }

    pub fn update_focused_character(&mut self, _character_name: &str, _hwnd: HWND) {}

    pub fn toggle_character_exclusion(&mut self, _character_name: &str) {}

    pub fn are_hotkeys_suspended(&self) -> bool {
        false
    }

    pub fn handle_suspend_hotkeys_request(&mut self) {}

    pub fn handle_move_to_saved_positions_request(&mut self) {}
}
