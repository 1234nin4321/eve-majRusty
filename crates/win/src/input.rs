//! Keyboard/mouse state and message-parameter decoding.

use windows_sys::Win32::Foundation::LPARAM;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT};

fn is_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

pub fn is_ctrl_pressed() -> bool {
    is_down(VK_CONTROL)
}

pub fn is_alt_pressed() -> bool {
    is_down(VK_MENU)
}

pub fn is_shift_pressed() -> bool {
    is_down(VK_SHIFT)
}

/// Two physical Win keys, no single VK code covers both.
pub fn is_win_pressed() -> bool {
    is_down(VK_LWIN) || is_down(VK_RWIN)
}

/// The triggering virtual-key code from a WM_HOTKEY message's lParam (HIWORD; LOWORD holds the modifiers).
pub fn hotkey_vk_from_lparam(lparam: LPARAM) -> u32 {
    (lparam as usize as u32) >> 16
}

/// Which X button (1 or 2) a low-level mouse hook's mouseData refers to.
pub fn x_button(mouse_data: u32) -> u16 {
    (mouse_data >> 16) as u16
}

/// Signed wheel delta (multiples of 120 = WHEEL_DELTA; positive = scrolled up) from a WM_MOUSEWHEEL MSLLHOOKSTRUCT's mouseData.
pub fn wheel_delta(mouse_data: u32) -> i16 {
    (mouse_data >> 16) as u16 as i16
}
