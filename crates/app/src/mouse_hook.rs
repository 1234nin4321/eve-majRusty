//! RegisterHotKey is keyboard-only and can't bind mouse buttons or the wheel, so this module
//! hooks WH_MOUSE_LL instead and re-posts matches as WM_HOTKEY, keeping hotkey dispatch agnostic
//! to whether a press came from the mouse or the keyboard.
//!
//! The binding table and swallow logic are in eve_maj_core::mouse_bindings. The hook callback runs on the
//! thread that installed it (whenever that thread pumps messages), and everything it reads sits behind one mutex,
//! which is never held while Windows could call back into the hook.

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use eve_maj_core::log::Scope;
use eve_maj_core::mouse_bindings::{HookAction, MouseBindings, MouseEvent};
use eve_maj_core::virtual_keys::{MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use eve_maj_win::input::{is_alt_pressed, is_ctrl_pressed, is_shift_pressed, is_win_pressed, wheel_delta, x_button};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_HOTKEY,
    WM_MOUSEWHEEL, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

const SLOG: Scope = Scope::new("mouse_hook");

/// SetWindowsHookExW failed; the binding that triggered the install was rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseHookInstallFailed;

impl fmt::Display for MouseHookInstallFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MouseHookInstallFailed")
    }
}

impl std::error::Error for MouseHookInstallFailed {}

struct HookState {
    bindings: MouseBindings,
    /// HHOOK, stored as an address so the state can live in a static.
    hook: usize,
    /// HWND that receives the re-posted WM_HOTKEY; 0 until the first register().
    target_hwnd: usize,
}

/// None until the first register() and again after deinit(), mirroring the Zig module's g_initialized.
static STATE: Mutex<Option<HookState>> = Mutex::new(None);

fn lock_state() -> MutexGuard<'static, Option<HookState>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ensure_init(state: &mut Option<HookState>) -> &mut HookState {
    state.get_or_insert_with(|| HookState { bindings: MouseBindings::new(), hook: 0, target_hwnd: 0 })
}

/// Register a mouse-button hotkey (combined vk from virtual_keys, e.g. XButton1+Ctrl); installs the low-level hook on first registration.
pub fn register(target_hwnd: HWND, combined_vk: u32, id: i32) -> Result<(), MouseHookInstallFailed> {
    let mut guard = lock_state();
    let state = ensure_init(&mut guard);
    state.target_hwnd = target_hwnd as usize;
    state.bindings.insert(combined_vk, id);
    if state.hook == 0 {
        if let Err(err) = install_hook(state) {
            state.bindings.remove(combined_vk);
            return Err(err);
        }
    }
    Ok(())
}

pub fn unregister(combined_vk: u32) {
    let mut guard = lock_state();
    let Some(state) = guard.as_mut() else { return };
    state.bindings.remove(combined_vk);
    if state.bindings.is_empty() {
        uninstall_hook(state);
    }
}

/// Remove all mouse-button bindings and uninstall the hook; safe to call even if nothing was ever registered.
pub fn unregister_all() {
    let mut guard = lock_state();
    let Some(state) = guard.as_mut() else { return };
    state.bindings.clear();
    uninstall_hook(state);
}

/// Drops the binding table; call only once at true process shutdown, never from a reload path that may register() again.
/// Unlike the Zig version a still-installed hook is removed too, so the callback never outlives its state.
pub fn deinit() {
    let mut guard = lock_state();
    if let Some(state) = guard.as_mut() {
        uninstall_hook(state);
    }
    *guard = None;
}

fn install_hook(state: &mut HookState) -> Result<(), MouseHookInstallFailed> {
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), GetModuleHandleW(std::ptr::null()), 0) };
    if hook.is_null() {
        SLOG.err(format_args!("Failed to install low-level mouse hook"));
        return Err(MouseHookInstallFailed);
    }
    state.hook = hook as usize;
    SLOG.debug(format_args!("Low-level mouse hook installed"));
    Ok(())
}

fn uninstall_hook(state: &mut HookState) {
    if state.hook != 0 {
        unsafe { UnhookWindowsHookEx(state.hook as HHOOK) };
        state.hook = 0;
        SLOG.debug(format_args!("Low-level mouse hook removed"));
    }
    state.bindings.reset_swallows();
}

fn current_modifiers() -> u32 {
    let mut mods = 0;
    if is_ctrl_pressed() {
        mods |= MOD_CONTROL;
    }
    if is_alt_pressed() {
        mods |= MOD_ALT;
    }
    if is_shift_pressed() {
        mods |= MOD_SHIFT;
    }
    if is_win_pressed() {
        mods |= MOD_WIN;
    }
    mods
}

unsafe extern "system" fn low_level_mouse_proc(n_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Per MSDN, a negative nCode must go straight to CallNextHookEx untouched, which the fallthrough below already does.
    if n_code >= 0 {
        let mouse_data = || (*(lparam as *const MSLLHOOKSTRUCT)).mouseData;
        let event = match wparam as u32 {
            WM_XBUTTONDOWN => Some(MouseEvent::XButtonDown(x_button(mouse_data()))),
            WM_XBUTTONUP => Some(MouseEvent::XButtonUp(x_button(mouse_data()))),
            WM_MOUSEWHEEL => Some(MouseEvent::Wheel(wheel_delta(mouse_data()))),
            _ => None,
        };
        if let Some(event) = event {
            let (action, target_hwnd) = match lock_state().as_mut() {
                Some(state) => (state.bindings.handle(event, current_modifiers), state.target_hwnd),
                None => (HookAction::Pass, 0),
            };
            match action {
                HookAction::Pass => {}
                HookAction::Swallow => return 1,
                HookAction::Dispatch(id) => {
                    if target_hwnd != 0 {
                        PostMessageW(target_hwnd as HWND, WM_HOTKEY, id as WPARAM, 0);
                    }
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam)
}
