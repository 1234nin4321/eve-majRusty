//! Discovery of EVE client windows: periodic EnumWindows scans filtered by the profile's window filters, plus
//! WinEvent hooks that catch title (character) changes, new windows and closed windows between scans.

use std::collections::{HashMap, HashSet};

use eve_maj_core::log::Scope;
use eve_maj_win::window::{class_name, is_window, is_window_visible, process_exe_path, window_title};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_NAMECHANGE,
    WINEVENT_OUTOFCONTEXT,
};

use crate::globals;

const SLOG: Scope = Scope::new("scout");

#[derive(Debug, Clone)]
pub struct EveWindow {
    pub hwnd: HWND,
    pub title: String,
    pub character_name: String,
    pub process_id: u32,
}

#[derive(Debug, Clone)]
pub struct NameChange {
    pub hwnd: HWND,
    pub old_name: String,
    pub new_name: String,
}

/// character_name isn't unique (multiple windows can all report "EVE"), so hwnd travels with it.
#[derive(Debug, Clone)]
pub struct ClosedWindow {
    pub hwnd: HWND,
    pub character_name: String,
}

/// character_name before login or after logout while the client window stays open.
const GENERIC_CHARACTER_NAME: &str = "EVE";

pub fn is_generic_character_name(name: &str) -> bool {
    name == GENERIC_CHARACTER_NAME
}

pub struct UpdateResult {
    pub closed_windows: Vec<ClosedWindow>,
    pub name_changes: Vec<NameChange>,
}

#[derive(Debug)]
pub struct EnumWindowsFailed;

impl std::fmt::Display for EnumWindowsFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnumWindowsFailed")
    }
}

pub struct Scout {
    pub windows: Vec<EveWindow>,
    name_to_hwnd: HashMap<String, HWND>,
    // O(1) window lookups by HWND
    hwnd_to_index: HashMap<HWND, usize>,
    // Cache of known EVE process IDs to avoid re-checking executable path
    eve_pids: HashSet<u32>,
    // Pending closed windows detected by destroy event hook
    pending_closed: Vec<ClosedWindow>,
    // Pending name changes detected by title change event hook
    pending_name_changes: Vec<NameChange>,
    /// FIFO queue of windows currently not logged in, oldest-logged-out first; fed by update_window_title/enum_windows_callback, consumed by HotkeyManager::cycle_not_logged_in via not_logged_in_hwnds.
    not_logged_in_queue: Vec<HWND>,
    // Flag set by create event hook to trigger immediate scan
    pending_scan: bool,
    create_event_hook: HWINEVENTHOOK,
    name_change_hook: HWINEVENTHOOK,
    destroy_event_hook: HWINEVENTHOOK,
}

fn set_hook(event: u32, callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32)) -> HWINEVENTHOOK {
    // 0, 0 = all processes, all threads
    unsafe { SetWinEventHook(event, event, std::ptr::null_mut(), Some(callback), 0, 0, WINEVENT_OUTOFCONTEXT) }
}

impl Scout {
    pub fn new() -> Scout {
        let name_change_hook = set_hook(EVENT_OBJECT_NAMECHANGE, name_change_callback);
        if name_change_hook.is_null() {
            SLOG.warn(format_args!("Failed to set up title change event hook - character name changes will not be detected until full window rescan"));
        } else {
            SLOG.debug(format_args!("Title change event hook set up successfully"));
        }

        let create_event_hook = set_hook(EVENT_OBJECT_CREATE, window_create_callback);
        if create_event_hook.is_null() {
            SLOG.warn(format_args!("Failed to set up window creation event hook - new windows will only be detected via periodic scanning"));
        } else {
            SLOG.debug(format_args!("Window creation event hook set up successfully"));
        }

        let destroy_event_hook = set_hook(EVENT_OBJECT_DESTROY, window_destroy_callback);
        if destroy_event_hook.is_null() {
            SLOG.warn(format_args!("Failed to set up window destroy event hook - closed windows will not be detected until periodic validation"));
        } else {
            SLOG.debug(format_args!("Window destroy event hook set up successfully"));
        }

        Scout {
            windows: Vec::new(),
            name_to_hwnd: HashMap::new(),
            hwnd_to_index: HashMap::new(),
            eve_pids: HashSet::new(),
            pending_closed: Vec::new(),
            pending_name_changes: Vec::new(),
            not_logged_in_queue: Vec::new(),
            pending_scan: false,
            create_event_hook,
            name_change_hook,
            destroy_event_hook,
        }
    }

    /// Bumps hwnd to the back of the not-logged-in FIFO used by the cycle-not-logged-in hotkey; re-logout bumps instead of duplicating.
    fn track_not_logged_in(&mut self, hwnd: HWND) {
        self.not_logged_in_queue.retain(|&h| h != hwnd);
        self.not_logged_in_queue.push(hwnd);
    }

    /// Removes hwnd from the not-logged-in FIFO when its title changes away from "EVE", so a stale entry doesn't keep matching a since-logged-in character.
    fn untrack_not_logged_in(&mut self, hwnd: HWND) {
        if let Some(i) = self.not_logged_in_queue.iter().position(|&h| h == hwnd) {
            self.not_logged_in_queue.remove(i);
        }
    }

    /// Snapshot of the not-logged-in FIFO, oldest first.
    pub fn not_logged_in_hwnds(&self) -> Vec<HWND> {
        self.not_logged_in_queue.clone()
    }

    pub fn scan_for_eve_windows(&mut self) -> Result<(), EnumWindowsFailed> {
        // Enumerate all windows - callback will only add new ones not already tracked
        let result = unsafe { EnumWindows(Some(enum_windows_callback), self as *mut Scout as LPARAM) };
        if result == 0 {
            return Err(EnumWindowsFailed);
        }
        Ok(())
    }

    /// Only called when windows are closed; drops PIDs for processes that no longer exist.
    pub fn cleanup_stale_pids(&mut self) {
        let active: HashSet<u32> = self.windows.iter().map(|w| w.process_id).filter(|&pid| pid != 0).collect();
        let before = self.eve_pids.len();
        self.eve_pids.retain(|pid| active.contains(pid));
        let removed = before - self.eve_pids.len();
        if removed > 0 {
            SLOG.debug(format_args!("Cleaned up {removed} stale PIDs from cache"));
        }
    }

    /// Re-reads a window's title (EVENT_OBJECT_NAMECHANGE hook and the periodic refresh), recording a NameChange when the character changed.
    fn update_window_title(&mut self, hwnd: HWND) {
        let Some(&index) = self.hwnd_to_index.get(&hwnd) else { return };
        let Some(current_title) = window_title(hwnd) else {
            SLOG.err(format_args!("Failed to get window title for '{}': NoWindowTitle", self.windows[index].character_name));
            return;
        };
        if self.windows[index].title == current_title {
            return;
        }

        let new_char_name = extract_character_name(&current_title).to_owned();
        let eve_window = &mut self.windows[index];
        eve_window.title = current_title;

        if eve_window.character_name == new_char_name {
            return;
        }

        let old_name = std::mem::replace(&mut eve_window.character_name, new_char_name.clone());
        self.name_to_hwnd.remove(&old_name);
        self.name_to_hwnd.insert(new_char_name.clone(), hwnd);

        if is_generic_character_name(&new_char_name) {
            self.track_not_logged_in(hwnd);
        } else {
            self.untrack_not_logged_in(hwnd);
        }

        SLOG.info(format_args!("Character changed: {old_name} -> {new_char_name}"));
        self.pending_name_changes.push(NameChange { hwnd, old_name, new_name: new_char_name });
    }

    /// Re-reads titles for all tracked windows as a fallback for EVENT_OBJECT_NAMECHANGE events dropped before the HWND/PID was cached.
    /// Runs only on force_scan ticks (~1s), since GetWindowText on another process's window is a synchronous cross-process call.
    fn refresh_tracked_window_titles(&mut self) {
        // update_window_title may rename entries but never adds/removes them, so iterating a snapshot of the HWNDs is safe.
        let hwnds: Vec<HWND> = self.windows.iter().map(|w| w.hwnd).collect();
        for hwnd in hwnds {
            self.update_window_title(hwnd);
        }
    }

    /// Main update cycle - performs all Scout operations for a single tick
    pub fn update(&mut self, force_scan: bool) -> Result<UpdateResult, EnumWindowsFailed> {
        let closed_windows = std::mem::take(&mut self.pending_closed);
        let name_changes = std::mem::take(&mut self.pending_name_changes);

        if !closed_windows.is_empty() {
            self.cleanup_stale_pids();
        }

        if self.pending_scan || force_scan {
            self.scan_for_eve_windows()?;
            self.pending_scan = false;
        }

        // Catches missed name-change events; see refresh_tracked_window_titles() doc comment.
        if force_scan {
            self.refresh_tracked_window_titles();
        }

        Ok(UpdateResult { closed_windows, name_changes })
    }

    /// Lookup HWND by character name (validates window before returning)
    pub fn hwnd_by_name(&self, name: &str) -> Option<HWND> {
        self.name_to_hwnd.get(name).copied().filter(|&h| is_window(h))
    }

    /// Clears the cached HWND for a character, forcing re-lookup on next hwnd_by_name; call when a HWND is known stale.
    pub fn clear_hwnd_for_character(&mut self, name: &str) {
        self.name_to_hwnd.remove(name);
    }

    /// Call this after removing windows to keep indices consistent.
    fn rebuild_hwnd_index(&mut self) {
        self.hwnd_to_index = self.windows.iter().enumerate().map(|(i, w)| (w.hwnd, i)).collect();
    }

    /// Re-checks a tracked window's class+exe against the current filter set from scratch,
    /// unlike enum_windows_callback's class-first fast path which only applies to new windows.
    fn matches_current_filters(hwnd: HWND, process_id: u32) -> bool {
        let Some(class) = class_name(hwnd) else { return false };
        let Some(path) = process_exe_path(process_id) else { return false };
        globals::config().window_filters.iter().any(|f| f.matches_class(&class) && f.matches_executable(&path))
    }

    /// Drops tracked windows that no longer match any filter (e.g. the filter that once matched them was edited
    /// or deleted); scan_for_eve_windows() alone won't catch this since it skips already-tracked HWNDs. Call after
    /// a config reload, before recreating thumbnails.
    pub fn prune_non_matching_windows(&mut self) {
        let mut i = self.windows.len();
        while i > 0 {
            i -= 1;
            let (hwnd, pid) = (self.windows[i].hwnd, self.windows[i].process_id);
            if Self::matches_current_filters(hwnd, pid) {
                continue;
            }
            let removed = self.windows.remove(i);
            self.name_to_hwnd.remove(&removed.character_name);
            self.eve_pids.remove(&removed.process_id);
            self.untrack_not_logged_in(removed.hwnd);
        }
        self.rebuild_hwnd_index();
    }

    fn add_window_if_matching(&mut self, hwnd: HWND) {
        if !is_window_visible(hwnd) {
            return;
        }
        // Already tracked: skip the class-name lookup and filter-match loop below entirely.
        if self.hwnd_to_index.contains_key(&hwnd) {
            return;
        }

        // Fast filter: Check window class name first (much faster than OpenProcess)
        let Some(class) = class_name(hwnd) else { return };
        let filters = &globals::config().window_filters;
        let Some(mut matching) = filters.iter().position(|f| f.matches_class(&class)) else { return };

        let mut process_id = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };

        if !self.eve_pids.contains(&process_id) {
            let Some(path) = process_exe_path(process_id) else { return };
            if !filters[matching].matches_executable(&path) {
                // A class-less filter can vacuously win the pick above; fall back to the rest.
                let first = matching;
                let Some(other) = filters
                    .iter()
                    .enumerate()
                    .position(|(i, f)| i != first && f.matches_class(&class) && f.matches_executable(&path))
                else {
                    return;
                };
                matching = other;
            }
            self.eve_pids.insert(process_id);
        }

        let Some(title) = window_title(hwnd) else {
            SLOG.err(format_args!("Failed to get window title for hwnd {hwnd:?}: NoWindowTitle"));
            return;
        };

        // Non-EVE titles aren't a stable per-window identity, so fall back to the filter's own name.
        let character_name =
            if class == "trinityWindow" { extract_character_name(&title).to_owned() } else { filters[matching].name.clone() };

        self.windows.push(EveWindow { hwnd, title, character_name: character_name.clone(), process_id });
        self.hwnd_to_index.insert(hwnd, self.windows.len() - 1);
        self.name_to_hwnd.insert(character_name.clone(), hwnd);

        // Window was already not-logged-in when first discovered (e.g. app launch), so no name-change transition fires for it; queue it here instead.
        if is_generic_character_name(&character_name) {
            self.track_not_logged_in(hwnd);
        }
    }

    fn handle_destroy(&mut self, hwnd: HWND) {
        // Uses hwnd_to_index before checking class name, since a partially-destroyed window can fail GetClassName.
        let Some(&index) = self.hwnd_to_index.get(&hwnd) else { return };
        let removed = self.windows.remove(index);
        SLOG.debug(format_args!("Window destroyed: '{}' (hwnd {hwnd:?})", removed.character_name));
        self.name_to_hwnd.remove(&removed.character_name);
        self.untrack_not_logged_in(removed.hwnd);
        self.pending_closed.push(ClosedWindow { hwnd, character_name: removed.character_name });
        // Rebuild hwnd_to_index since removal shifts array indices
        self.rebuild_hwnd_index();
    }
}

impl Drop for Scout {
    fn drop(&mut self) {
        for hook in [self.create_event_hook, self.name_change_hook, self.destroy_event_hook] {
            if !hook.is_null() {
                unsafe { UnhookWinEvent(hook) };
            }
        }
    }
}

/// EVE window titles are typically: "EVE - CharacterName"; falls back to the full title if there's no " - " separator or nothing follows it.
fn extract_character_name(title: &str) -> &str {
    split_character_name(title).unwrap_or(title)
}

/// Splits an EVE window title ("EVE - CharacterName") on " - "; None if there's no such separator or nothing follows it.
pub fn split_character_name(title: &str) -> Option<&str> {
    let dash_pos = title.find(" - ")?;
    let name = &title[dash_pos + 3..];
    (!name.is_empty()).then_some(name)
}

/// EnumWindows callback: returning TRUE continues enumeration, FALSE stops it.
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scout = &mut *(lparam as *mut Scout);
    scout.add_window_if_matching(hwnd);
    TRUE
}

/// OBJID_WINDOW; anything else is a child object (caret, scrollbar, ...) of the window.
const OBJID_WINDOW: i32 = 0;

unsafe extern "system" fn name_change_callback(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    // Only process main window title changes (not child controls)
    if id_object != OBJID_WINDOW {
        return;
    }
    let Some(scout) = globals::scout() else { return };

    let mut process_id = 0u32;
    GetWindowThreadProcessId(hwnd, &mut process_id);
    if !scout.eve_pids.contains(&process_id) {
        // Not a known EVE process, skip the expensive class name check below
        return;
    }

    // Kept as safety check in case PID cache is stale or process reuses PID
    if class_name(hwnd).as_deref() != Some("trinityWindow") {
        return;
    }
    scout.update_window_title(hwnd);
}

unsafe extern "system" fn window_destroy_callback(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    // Only process main window destruction (not child controls)
    if id_object != OBJID_WINDOW {
        return;
    }
    if let Some(scout) = globals::scout() {
        scout.handle_destroy(hwnd);
    }
}

unsafe extern "system" fn window_create_callback(_: HWINEVENTHOOK, _: u32, _: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    // Only process main window creation (not child controls)
    if id_object != OBJID_WINDOW {
        return;
    }
    // EVENT_OBJECT_CREATE fires for ALL windows, so this just flags a scan rather than validating expensively here.
    if let Some(scout) = globals::scout() {
        scout.pending_scan = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_name_from_title() {
        assert_eq!(split_character_name("EVE - Pilot One"), Some("Pilot One"));
        assert_eq!(split_character_name("EVE - "), None);
        assert_eq!(extract_character_name("EVE"), "EVE");
        assert!(is_generic_character_name("EVE"));
    }
}
