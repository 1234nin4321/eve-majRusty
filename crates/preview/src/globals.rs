//! Process-wide singletons reached from Win32 callbacks (window procedures, WinEvent hooks, EnumWindows).
//!
//! The Zig build kept these as global pointers (`g_painter_ptr`, `g_scout_ptr`, `g_config`, ...), because a
//! callback has no other way back to app state. This keeps that model: every slot is touched only from the
//! UI thread (the one running the message loop), so there's no cross-thread sharing. Win32 can re-enter a
//! callback while an outer one is still running (e.g. SetWindowPos sends WM_* messages synchronously), so a
//! `&mut` obtained from a slot is short-lived by convention: re-fetch after any call that can pump messages
//! rather than holding one across it. The chatlog worker thread never touches these; it talks through queues.

use std::cell::UnsafeCell;

pub struct Global<T>(UnsafeCell<Option<T>>);

// SAFETY: only ever accessed from the UI thread; see the module docs.
unsafe impl<T> Sync for Global<T> {}

impl<T: 'static> Global<T> {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    pub fn set(&self, value: T) {
        // SAFETY: UI-thread only; replacing the slot drops the previous value.
        unsafe { *self.0.get() = Some(value) };
    }

    pub fn take(&self) -> Option<T> {
        // SAFETY: UI-thread only.
        unsafe { (*self.0.get()).take() }
    }

    /// The slot's value, if set. See the module docs for the re-entrancy convention.
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> Option<&'static mut T> {
        // SAFETY: UI-thread only; the slot lives in a static, so the reference never dangles while set.
        unsafe { (*self.0.get()).as_mut().map(|v| &mut *(v as *mut T)) }
    }

    pub fn is_set(&self) -> bool {
        self.get().is_some()
    }
}

use eve_maj_core::config::{Config, GlobalSettings};
use windows_sys::Win32::Foundation::HWND;

use crate::hotkeys::HotkeyManager;
use crate::painter::Painter;
use crate::scout::Scout;

pub static CONFIG: Global<Config> = Global::new();
pub static GLOBAL_SETTINGS: Global<GlobalSettings> = Global::new();
pub static SCOUT: Global<Box<Scout>> = Global::new();
pub static PAINTER: Global<Box<Painter>> = Global::new();
pub static HOTKEY_MANAGER: Global<Box<HotkeyManager>> = Global::new();
pub static TIMER_HWND: Global<HWND> = Global::new();

/// The live profile; set before any window exists and kept for the whole run (a profile reload swaps its contents).
pub fn config() -> &'static mut Config {
    CONFIG.get().expect("config is loaded before any window or hook exists")
}

pub fn painter() -> Option<&'static mut Painter> {
    PAINTER.get().map(|b| &mut **b)
}

pub fn scout() -> Option<&'static mut Scout> {
    SCOUT.get().map(|b| &mut **b)
}

pub fn hotkey_manager() -> Option<&'static mut HotkeyManager> {
    HOTKEY_MANAGER.get().map(|b| &mut **b)
}

pub fn timer_hwnd() -> Option<HWND> {
    TIMER_HWND.get().copied()
}
