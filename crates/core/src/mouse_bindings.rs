//! Platform-neutral half of the low-level mouse hook (the Windows side lives in eve_maj_app::mouse_hook): the bound
//! mouse-button/wheel hotkeys and the per-event decision of whether to dispatch, swallow or pass an event on.

use std::collections::HashMap;

use crate::virtual_keys::{combine_key, VK_WHEELDOWN, VK_WHEELUP, VK_XBUTTON1, VK_XBUTTON2};

/// MSLLHOOKSTRUCT mouseData's HIWORD for the first and second X buttons.
pub const XBUTTON1: u16 = 1;
pub const XBUTTON2: u16 = 2;

/// The hook events this module cares about, already decoded from MSLLHOOKSTRUCT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    /// WM_XBUTTONDOWN with the X button number.
    XButtonDown(u16),
    /// WM_XBUTTONUP with the X button number.
    XButtonUp(u16),
    /// WM_MOUSEWHEEL with its signed delta.
    Wheel(i16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Hand the event to CallNextHookEx.
    Pass,
    /// Eat the event without posting anything.
    Swallow,
    /// Eat the event and re-post it as WM_HOTKEY with this id.
    Dispatch(i32),
}

/// Combined vk (from virtual_keys, e.g. XButton1+Ctrl) -> hotkey id, plus the pending button-up swallows.
#[derive(Debug, Default)]
pub struct MouseBindings {
    bindings: HashMap<u32, i32>,
    swallow_xbutton1_up: bool,
    swallow_xbutton2_up: bool,
}

impl MouseBindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, combined_vk: u32, id: i32) {
        self.bindings.insert(combined_vk, id);
    }

    pub fn remove(&mut self, combined_vk: u32) {
        self.bindings.remove(&combined_vk);
    }

    pub fn clear(&mut self) {
        self.bindings.clear();
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Forget any armed button-up swallows (the hook is going away).
    pub fn reset_swallows(&mut self) {
        self.swallow_xbutton1_up = false;
        self.swallow_xbutton2_up = false;
    }

    /// Look up a bound base virtual key with the currently-held modifiers.
    fn bound(&self, base_vk: u32, modifiers: u32) -> Option<i32> {
        self.bindings.get(&combine_key(base_vk, modifiers)).copied()
    }

    /// Decides what the hook does with `event`; `modifiers` (MOD_* flags currently held) is only queried for a button press or a scroll.
    pub fn handle(&mut self, event: MouseEvent, modifiers: impl FnOnce() -> u32) -> HookAction {
        match event {
            MouseEvent::XButtonDown(button) => {
                let base_vk = match button {
                    XBUTTON1 => VK_XBUTTON1,
                    XBUTTON2 => VK_XBUTTON2,
                    _ => return HookAction::Pass,
                };
                // Swallow the click, matching RegisterHotKey's exclusive-capture semantics.
                let Some(id) = self.bound(base_vk, modifiers()) else { return HookAction::Pass };
                // Arm the matching release swallow so the newly-focused client doesn't see a phantom button-up.
                if button == XBUTTON1 {
                    self.swallow_xbutton1_up = true;
                } else {
                    self.swallow_xbutton2_up = true;
                }
                HookAction::Dispatch(id)
            }
            MouseEvent::XButtonUp(button) => {
                let armed = match button {
                    XBUTTON1 => &mut self.swallow_xbutton1_up,
                    XBUTTON2 => &mut self.swallow_xbutton2_up,
                    _ => return HookAction::Pass,
                };
                if std::mem::take(armed) {
                    HookAction::Swallow
                } else {
                    HookAction::Pass
                }
            }
            MouseEvent::Wheel(delta) => {
                let wheel_vk = if delta > 0 { VK_WHEELUP } else { VK_WHEELDOWN };
                // Swallow the scroll, matching RegisterHotKey's exclusive-capture semantics.
                match self.bound(wheel_vk, modifiers()) {
                    Some(id) => HookAction::Dispatch(id),
                    None => HookAction::Pass,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtual_keys::{MOD_CONTROL, MOD_SHIFT};

    #[test]
    fn xbutton_press_dispatches_and_swallows_its_release() {
        let mut b = MouseBindings::new();
        b.insert(combine_key(VK_XBUTTON1, MOD_CONTROL), 7);
        assert_eq!(b.handle(MouseEvent::XButtonDown(XBUTTON1), || 0), HookAction::Pass);
        assert_eq!(b.handle(MouseEvent::XButtonUp(XBUTTON1), || 0), HookAction::Pass);
        assert_eq!(b.handle(MouseEvent::XButtonDown(XBUTTON1), || MOD_CONTROL), HookAction::Dispatch(7));
        assert_eq!(b.handle(MouseEvent::XButtonUp(XBUTTON2), || 0), HookAction::Pass);
        assert_eq!(b.handle(MouseEvent::XButtonUp(XBUTTON1), || 0), HookAction::Swallow);
        assert_eq!(b.handle(MouseEvent::XButtonUp(XBUTTON1), || 0), HookAction::Pass);
    }

    #[test]
    fn unknown_buttons_pass_without_reading_modifiers() {
        let mut b = MouseBindings::new();
        b.insert(VK_XBUTTON2, 1);
        assert_eq!(b.handle(MouseEvent::XButtonDown(3), || unreachable!()), HookAction::Pass);
        assert_eq!(b.handle(MouseEvent::XButtonUp(3), || unreachable!()), HookAction::Pass);
        assert_eq!(b.handle(MouseEvent::XButtonUp(XBUTTON2), || unreachable!()), HookAction::Pass);
    }

    #[test]
    fn wheel_direction_picks_the_binding() {
        let mut b = MouseBindings::new();
        b.insert(VK_WHEELUP, 1);
        b.insert(combine_key(VK_WHEELDOWN, MOD_SHIFT), 2);
        assert_eq!(b.handle(MouseEvent::Wheel(120), || 0), HookAction::Dispatch(1));
        assert_eq!(b.handle(MouseEvent::Wheel(-120), || 0), HookAction::Pass);
        assert_eq!(b.handle(MouseEvent::Wheel(-120), || MOD_SHIFT), HookAction::Dispatch(2));
        // Zero delta counts as down, like the Zig `> 0` test.
        assert_eq!(b.handle(MouseEvent::Wheel(0), || MOD_SHIFT), HookAction::Dispatch(2));
    }

    #[test]
    fn reset_disarms_pending_swallows() {
        let mut b = MouseBindings::new();
        b.insert(VK_XBUTTON2, 4);
        assert_eq!(b.handle(MouseEvent::XButtonDown(XBUTTON2), || 0), HookAction::Dispatch(4));
        b.reset_swallows();
        assert_eq!(b.handle(MouseEvent::XButtonUp(XBUTTON2), || 0), HookAction::Pass);
        b.remove(VK_XBUTTON2);
        assert!(b.is_empty());
    }
}
