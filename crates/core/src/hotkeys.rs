//! Hotkey bindings and the actions behind them, independent of how a platform delivers key presses.
//!
//! Platform-neutral port of the Zig build's `hotkeys.zig` HotkeyManager: [`build_bindings`] turns a profile plus
//! global settings into the list of (id, key, action) to register (same id bands as the Zig build), and
//! [`HotkeyManager::handle_press`] runs an action: cycling groups/characters/accounts/excluded/notified/all/not-logged-in
//! clients through a [`HotkeyHost`], and returning an [`Effect`] for whatever the front end has to do itself
//! (minimize all, switch profile, ...). [`xkb_key_name`] maps a profile's Windows virtual-key code to the key name
//! Linux compositors use.

use crate::accounts_store::{self, Membership};
use crate::config::{build_character_order_map, order_map_less_than, Config, GlobalSettings};
use crate::layout::is_generic_character_name;
use crate::log::Scope;
use crate::virtual_keys::*;

const SLOG: Scope = Scope::new("hotkeys");

const ID_CYCLE_GROUP_BASE: u32 = 1000;
const ID_GLOBAL_ACTION_BASE: u32 = 2000;
const ID_PER_CHARACTER_BASE: u32 = 3000;
const ID_PROFILE_SWITCH_BASE: u32 = 4000;
const ID_APP_HOTKEY_BASE: u32 = 5000;
const ID_URL_HOTKEY_BASE: u32 = 6000;
const ID_ACCOUNT_BASE: u32 = 7000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    CycleGroup { group_index: usize, forward: bool },
    /// Characters-list indices sharing one per-character hotkey; `current` is the cycle cursor.
    ActivateCharacter { character_indices: Vec<usize>, current: Option<usize> },
    AssignGroup { group_index: usize },
    MinimizeAll,
    CloseAll,
    ToggleVisibility,
    NextProfile,
    PreviousProfile,
    SwitchToProfile { profile_index: usize },
    ToggleExclusion,
    NextExcluded,
    PreviousExcluded,
    SuspendHotkeys,
    ToggleAutoMinimize,
    CycleNotified,
    PreviousNotified,
    NextAllClients,
    PreviousAllClients,
    NextNotLoggedIn,
    PreviousNotLoggedIn,
    MoveToSavedPositions,
    ReturnToLastApp,
    ActivateApp { app_index: usize },
    OpenUrl { url_index: usize },
    /// accountHotkeys indices sharing this key.
    ActivateAccount { binding_indices: Vec<usize> },
}

/// One hotkey to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: u32,
    /// Combined virtual key + MOD_* flags (see virtual_keys).
    pub key: u32,
    pub action: HotkeyAction,
    pub description: String,
}

/// What a press asks the front end to do, beyond activating a client through the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    MinimizeAll,
    CloseAll,
    ToggleVisibility,
    ToggleAutoMinimize,
    MoveToSavedPositions,
    SwitchProfile(String),
    /// Toggle cycle exclusion for the focused client (same as shift-clicking its thumbnail).
    ToggleExclusionOfFocused,
    ReturnToLastApp,
    ActivateApp(String),
    OpenUrl { url: String, upload_clipboard: bool },
    /// Suspend toggled; the front end re-registers bindings (only the suspend key while suspended).
    SuspendChanged(bool),
    /// A group's membership changed for this character (badge/reflow refresh).
    GroupMembershipChanged(String),
}

/// What the hotkey logic needs from the running app. `W` is a client window handle.
pub trait HotkeyHost<W: Copy + Eq> {
    /// Tracked clients (handle, character name) in discovery order.
    fn windows(&self) -> Vec<(W, String)>;
    /// The client currently showing `name`.
    fn window_by_name(&self, name: &str) -> Option<W> {
        self.windows().into_iter().find(|(_, n)| n == name).map(|(w, _)| w)
    }
    /// Clients at the login screen, oldest logout first.
    fn not_logged_in_windows(&self) -> Vec<W>;
    /// The focused window if it's a tracked client.
    fn focused(&self) -> Option<W>;
    /// Focus a client (the same path as clicking its thumbnail).
    fn activate(&mut self, window: W);
    /// Character of the thumbnail under the pointer.
    fn thumbnail_under_cursor(&self) -> Option<String>;
    /// Recently notified characters, newest first, within `retention_ms`.
    fn notified_character_names(&self, retention_ms: u64) -> Vec<String>;
    /// Character -> account membership, freshly read.
    fn account_membership(&mut self) -> Membership;
}

fn push_grouped(groups: &mut Vec<(u32, Vec<usize>)>, key: u32, index: usize) {
    match groups.iter_mut().find(|(k, _)| *k == key) {
        Some((_, v)) => v.push(index),
        None => groups.push((key, vec![index])),
    }
}

/// Every configured hotkey, with the Zig build's id bands. Per-character and account keys shared by several entries collapse into one binding that cycles among them.
pub fn build_bindings(cfg: &Config, gs: &GlobalSettings) -> Vec<Binding> {
    let mut out = Vec::new();
    let mut add = |id: u32, key: Option<u32>, action: HotkeyAction, description: String| {
        if let Some(key) = key {
            out.push(Binding { id, key, action, description });
        }
    };

    for (gi, g) in cfg.hotkey_groups.iter().enumerate() {
        let first = g.characters.first().map_or("(empty)", String::as_str);
        let base = ID_CYCLE_GROUP_BASE + gi as u32 * 3;
        add(base, g.forward_key, HotkeyAction::CycleGroup { group_index: gi, forward: true }, format!("group {gi} [{first}...] forward"));
        add(base + 1, g.backward_key, HotkeyAction::CycleGroup { group_index: gi, forward: false }, format!("group {gi} [{first}...] backward"));
        add(base + 2, g.assign_key, HotkeyAction::AssignGroup { group_index: gi }, format!("group {gi} [{}] assign", g.name));
    }

    let mut per_char: Vec<(u32, Vec<usize>)> = Vec::new();
    for (ci, c) in cfg.characters.iter().enumerate() {
        if let Some(key) = c.hotkey {
            push_grouped(&mut per_char, key, ci);
        }
    }
    for (i, (key, indices)) in per_char.into_iter().enumerate() {
        let first = &cfg.characters[indices[0]].name;
        let desc = if indices.len() == 1 { format!("activate character [{first}]") } else { format!("activate character [{first}...] ({} sharing hotkey)", indices.len()) };
        add(ID_PER_CHARACTER_BASE + i as u32, Some(key), HotkeyAction::ActivateCharacter { character_indices: indices, current: None }, desc);
    }

    let mut accounts: Vec<(u32, Vec<usize>)> = Vec::new();
    for (bi, b) in cfg.account_hotkeys.iter().enumerate() {
        if let (Some(key), false) = (b.hotkey, b.account_id.is_empty()) {
            push_grouped(&mut accounts, key, bi);
        }
    }
    for (i, (key, indices)) in accounts.into_iter().enumerate() {
        let desc = format!("activate account [{}] ({} binding(s))", cfg.account_hotkeys[indices[0]].account_id, indices.len());
        add(ID_ACCOUNT_BASE + i as u32, Some(key), HotkeyAction::ActivateAccount { binding_indices: indices }, desc);
    }

    for (i, p) in gs.profile_switch_hotkeys.iter().enumerate() {
        add(ID_PROFILE_SWITCH_BASE + i as u32, p.hotkey, HotkeyAction::SwitchToProfile { profile_index: i }, format!("switch to profile [{}]", p.target_profile));
    }
    for (i, a) in gs.app_hotkeys.iter().enumerate() {
        add(ID_APP_HOTKEY_BASE + i as u32, a.hotkey, HotkeyAction::ActivateApp { app_index: i }, format!("activate app [{}]", a.executable_name));
    }
    for (i, u) in gs.url_hotkeys.iter().enumerate() {
        add(ID_URL_HOTKEY_BASE + i as u32, u.hotkey, HotkeyAction::OpenUrl { url_index: i }, format!("open url [{}]", u.url));
    }

    let h = &cfg.hotkeys;
    let g = |n: u32| ID_GLOBAL_ACTION_BASE + n;
    add(g(0), h.hotkey_minimize_all, HotkeyAction::MinimizeAll, "minimize all clients".into());
    add(g(1), h.hotkey_close_all, HotkeyAction::CloseAll, "close all clients".into());
    add(g(2), h.hotkey_toggle_visibility, HotkeyAction::ToggleVisibility, "toggle thumbnails visibility".into());
    add(g(3), gs.hotkey_next_profile, HotkeyAction::NextProfile, "cycle to next profile".into());
    add(g(4), gs.hotkey_previous_profile, HotkeyAction::PreviousProfile, "cycle to previous profile".into());
    add(g(5), h.hotkey_toggle_exclusion, HotkeyAction::ToggleExclusion, "toggle character exclusion from cycling".into());
    add(g(6), h.hotkey_next_excluded, HotkeyAction::NextExcluded, "cycle to next excluded character".into());
    add(g(7), h.hotkey_previous_excluded, HotkeyAction::PreviousExcluded, "cycle to previous excluded character".into());
    add(g(8), h.hotkey_suspend, HotkeyAction::SuspendHotkeys, "suspend/resume all hotkeys".into());
    add(g(9), h.hotkey_toggle_auto_minimize, HotkeyAction::ToggleAutoMinimize, "toggle auto-minimize mode".into());
    add(g(10), h.hotkey_cycle_notified, HotkeyAction::CycleNotified, "cycle to most recently notified character".into());
    add(g(11), h.hotkey_previous_notified, HotkeyAction::PreviousNotified, "cycle backward through notified characters".into());
    add(g(12), gs.hotkey_cycle_all_clients_forward, HotkeyAction::NextAllClients, "cycle forward through all logged-in clients".into());
    add(g(13), gs.hotkey_cycle_all_clients_backward, HotkeyAction::PreviousAllClients, "cycle backward through all logged-in clients".into());
    add(g(14), gs.hotkey_cycle_not_logged_in_forward, HotkeyAction::NextNotLoggedIn, "cycle forward through not-logged-in clients".into());
    add(g(15), gs.hotkey_cycle_not_logged_in_backward, HotkeyAction::PreviousNotLoggedIn, "cycle backward through not-logged-in clients".into());
    add(g(16), h.hotkey_move_to_saved_positions, HotkeyAction::MoveToSavedPositions, "move all clients to saved positions".into());
    add(g(17), gs.hotkey_return_to_last_app, HotkeyAction::ReturnToLastApp, "return focus to the last non-EVE app".into());
    out
}

/// Advances idx by one position within [0, num), wrapping at either end.
fn step(idx: usize, num: usize, forward: bool) -> usize {
    if forward { (idx + 1) % num } else if idx == 0 { num - 1 } else { idx - 1 }
}

/// The index to step *from* for a loop that steps before its first check.
fn index_before_start(current: Option<usize>, num: usize, forward: bool) -> usize {
    current.filter(|&c| c < num).unwrap_or(if forward { num - 1 } else { 0 })
}

/// First index to check for a loop that checks before stepping.
fn start_index(found: Option<usize>, num: usize, forward: bool) -> usize {
    match found {
        Some(i) => step(i, num, forward),
        None if forward => 0,
        None => num - 1,
    }
}

pub struct HotkeyManager<W: Copy + Eq> {
    pub bindings: Vec<Binding>,
    suspended: bool,
    excluded_cycle_index: Option<usize>,
    last_notified_cycle_name: Option<String>,
    last_all_clients_cycle_name: Option<String>,
    last_not_logged_in: Option<W>,
    /// Exclusions for characters in no hotkey group.
    manually_excluded: Vec<String>,
}

impl<W: Copy + Eq> Default for HotkeyManager<W> {
    fn default() -> Self {
        Self { bindings: Vec::new(), suspended: false, excluded_cycle_index: None, last_notified_cycle_name: None, last_all_clients_cycle_name: None, last_not_logged_in: None, manually_excluded: Vec::new() }
    }
}

impl<W: Copy + Eq> HotkeyManager<W> {
    /// Rebuilds the bindings from config (after a profile load/edit); cycle cursors survive.
    pub fn rebuild(&mut self, cfg: &Config, gs: &GlobalSettings) {
        self.bindings = build_bindings(cfg, gs);
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// The bindings that should be live right now: all of them, or only the suspend key while suspended.
    pub fn active_bindings(&self) -> impl Iterator<Item = &Binding> {
        self.bindings.iter().filter(move |b| !self.suspended || b.action == HotkeyAction::SuspendHotkeys)
    }

    /// Runs the action bound to `id`. `eve_focused` gates everything but suspend when requireEveFocus is set.
    pub fn handle_press(&mut self, id: u32, cfg: &mut Config, gs: &GlobalSettings, host: &mut impl HotkeyHost<W>) -> Option<Effect> {
        let Some(pos) = self.bindings.iter().position(|b| b.id == id) else {
            SLOG.warn(format_args!("Received unknown hotkey id {id}"));
            return None;
        };
        let action = self.bindings[pos].action.clone();
        if action == HotkeyAction::SuspendHotkeys {
            self.suspended = !self.suspended;
            SLOG.info(format_args!("Hotkeys {}", if self.suspended { "suspended" } else { "resumed" }));
            return Some(Effect::SuspendChanged(self.suspended));
        }
        if self.suspended {
            return None;
        }
        if cfg.hotkeys.require_eve_focus && host.focused().is_none() {
            SLOG.debug(format_args!("Hotkey {id} ignored - no EVE client focused"));
            return None;
        }
        self.run(action, Some(pos), cfg, gs, host)
    }

    /// Runs a global action requested from outside the keyboard (an evemajpreview://hotkey/<name> link, the tray); like
    /// the Windows protocol handler this bypasses requireEveFocus and suspension, and works without a bound key.
    pub fn run_global_action(&mut self, action: crate::protocol::HotkeyAction, cfg: &mut Config, gs: &GlobalSettings, host: &mut impl HotkeyHost<W>) -> Option<Effect> {
        use crate::protocol::HotkeyAction as P;
        let action = match action {
            P::MinimizeAll => HotkeyAction::MinimizeAll,
            P::CloseAll => HotkeyAction::CloseAll,
            P::ToggleVisibility => HotkeyAction::ToggleVisibility,
            P::NextProfile => HotkeyAction::NextProfile,
            P::PreviousProfile => HotkeyAction::PreviousProfile,
            P::ToggleExclusion => HotkeyAction::ToggleExclusion,
            P::NextExcluded => HotkeyAction::NextExcluded,
            P::PreviousExcluded => HotkeyAction::PreviousExcluded,
            P::SuspendHotkeys => {
                self.suspended = !self.suspended;
                return Some(Effect::SuspendChanged(self.suspended));
            }
            P::ToggleAutoMinimize => HotkeyAction::ToggleAutoMinimize,
            P::CycleNotified => HotkeyAction::CycleNotified,
            P::PreviousNotified => HotkeyAction::PreviousNotified,
            P::NextAllClients => HotkeyAction::NextAllClients,
            P::PreviousAllClients => HotkeyAction::PreviousAllClients,
            P::NextNotLoggedIn => HotkeyAction::NextNotLoggedIn,
            P::PreviousNotLoggedIn => HotkeyAction::PreviousNotLoggedIn,
            P::MoveToSavedPositions => HotkeyAction::MoveToSavedPositions,
            P::ReturnToLastApp => HotkeyAction::ReturnToLastApp,
        };
        self.run(action, None, cfg, gs, host)
    }

    fn run(&mut self, action: HotkeyAction, pos: Option<usize>, cfg: &mut Config, gs: &GlobalSettings, host: &mut impl HotkeyHost<W>) -> Option<Effect> {
        use HotkeyAction::*;
        match action {
            CycleGroup { group_index, forward } => {
                if group_index < cfg.hotkey_groups.len() {
                    self.cycle_group(cfg, group_index, forward, host);
                }
                None
            }
            ActivateCharacter { .. } => {
                if let Some(pos) = pos {
                    self.activate_per_character(pos, cfg, host);
                }
                None
            }
            ActivateAccount { binding_indices } => {
                self.activate_account(&binding_indices, cfg, host);
                None
            }
            AssignGroup { group_index } => self.assign_group(cfg, group_index, host),
            MinimizeAll => Some(Effect::MinimizeAll),
            CloseAll => Some(Effect::CloseAll),
            ToggleVisibility => Some(Effect::ToggleVisibility),
            ToggleAutoMinimize => Some(Effect::ToggleAutoMinimize),
            MoveToSavedPositions => Some(Effect::MoveToSavedPositions),
            ReturnToLastApp => Some(Effect::ReturnToLastApp),
            NextProfile => cycle_profile(&cfg.profile_name, true).map(Effect::SwitchProfile),
            PreviousProfile => cycle_profile(&cfg.profile_name, false).map(Effect::SwitchProfile),
            SwitchToProfile { profile_index } => gs.profile_switch_hotkeys.get(profile_index).map(|p| Effect::SwitchProfile(p.target_profile.clone())),
            ToggleExclusion => host.focused().map(|_| Effect::ToggleExclusionOfFocused),
            NextExcluded => {
                self.cycle_excluded(cfg, true, host);
                None
            }
            PreviousExcluded => {
                self.cycle_excluded(cfg, false, host);
                None
            }
            CycleNotified => {
                self.cycle_notified(cfg, true, host);
                None
            }
            PreviousNotified => {
                self.cycle_notified(cfg, false, host);
                None
            }
            NextAllClients => {
                self.cycle_all_clients(cfg, gs, true, host);
                None
            }
            PreviousAllClients => {
                self.cycle_all_clients(cfg, gs, false, host);
                None
            }
            NextNotLoggedIn => {
                self.cycle_not_logged_in(true, host);
                None
            }
            PreviousNotLoggedIn => {
                self.cycle_not_logged_in(false, host);
                None
            }
            ActivateApp { app_index } => gs.app_hotkeys.get(app_index).map(|a| Effect::ActivateApp(a.executable_name.clone())),
            OpenUrl { url_index } => {
                gs.url_hotkeys.get(url_index).filter(|u| !u.url.is_empty()).map(|u| Effect::OpenUrl { url: u.url.clone(), upload_clipboard: u.upload_clipboard })
            }
            SuspendHotkeys => {
                self.suspended = !self.suspended;
                Some(Effect::SuspendChanged(self.suspended))
            }
        }
    }

    fn activate_per_character(&mut self, pos: usize, cfg: &Config, host: &mut impl HotkeyHost<W>) {
        let HotkeyAction::ActivateCharacter { character_indices, current } = &mut self.bindings[pos].action else { return };
        let num = character_indices.len();
        if num == 0 {
            return;
        }
        let mut idx = index_before_start(*current, num, true);
        for _ in 0..num {
            idx = step(idx, num, true);
            let Some(c) = cfg.characters.get(character_indices[idx]) else { continue };
            if let Some(w) = host.window_by_name(&c.name) {
                *current = Some(idx);
                SLOG.info(format_args!("Activating character: {} ({}/{num})", c.name, idx + 1));
                host.activate(w);
                return;
            }
        }
        SLOG.warn(format_args!("No character sharing this hotkey is currently running"));
    }

    /// Characters-list order of the tracked clients (unconfigured ones after, in discovery order).
    fn ordered_windows(cfg: &Config, host: &impl HotkeyHost<W>) -> Vec<(W, String)> {
        let windows = host.windows();
        let order_map = build_character_order_map(&cfg.characters);
        let mut idx: Vec<usize> = (0..windows.len()).collect();
        idx.sort_by(|&a, &b| {
            if order_map_less_than(&order_map, &windows[a].1, &windows[b].1, a, b) {
                std::cmp::Ordering::Less
            } else if order_map_less_than(&order_map, &windows[b].1, &windows[a].1, b, a) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
        idx.into_iter().map(|i| windows[i].clone()).collect()
    }

    fn activate_account(&mut self, binding_indices: &[usize], cfg: &Config, host: &mut impl HotkeyHost<W>) {
        let membership = host.account_membership();
        let matches: Vec<W> = Self::ordered_windows(cfg, host)
            .into_iter()
            .filter(|(_, name)| !is_generic_character_name(name))
            .filter(|(_, name)| {
                let Some(account) = accounts_store::account_of(&membership, name) else { return false };
                binding_indices.iter().filter_map(|&i| cfg.account_hotkeys.get(i)).any(|b| b.account_id == account)
            })
            .map(|(w, _)| w)
            .collect();
        if matches.is_empty() {
            SLOG.warn(format_args!("No clients of the bound account(s) are currently running"));
            return;
        }
        let focused = host.focused();
        let next = matches.iter().position(|&w| Some(w) == focused).map_or(0, |i| (i + 1) % matches.len());
        SLOG.info(format_args!("Account key binding: activating client {}/{}", next + 1, matches.len()));
        host.activate(matches[next]);
    }

    fn cycle_group(&mut self, cfg: &mut Config, gi: usize, forward: bool, host: &mut impl HotkeyHost<W>) {
        let nli = if cfg.hotkey_groups[gi].include_not_logged_in { host.not_logged_in_windows() } else { Vec::new() };
        let group = &cfg.hotkey_groups[gi];
        let num_chars = group.characters.len();
        let total = num_chars + nli.len();
        if total == 0 {
            SLOG.warn(format_args!("Attempted to cycle empty hotkey group"));
            return;
        }
        let saved = group.current_index;
        let focused = host.focused();
        // The real focus wins for the not-logged-in tail, since logging out while focused fires no focus event.
        let mut found = focused.and_then(|f| nli.iter().position(|&w| w == f)).map(|i| num_chars + i);
        if found.is_none() {
            found = saved.filter(|&c| c < num_chars);
        }
        if found.is_none() {
            found = self.last_not_logged_in.and_then(|l| nli.iter().position(|&w| w == l)).map(|i| num_chars + i);
        }
        let still_nli: Vec<W> = host.not_logged_in_windows();
        let mut idx = index_before_start(found, total, forward);
        for _ in 0..total {
            idx = step(idx, total, forward);
            if idx < num_chars {
                let name = cfg.hotkey_groups[gi].characters[idx].clone();
                if cfg.hotkey_groups[gi].excluded_characters.contains(&name) {
                    continue;
                }
                if let Some(w) = host.window_by_name(&name) {
                    cfg.hotkey_groups[gi].current_index = Some(idx);
                    SLOG.info(format_args!("Cycling {} to: {name} ({}/{total})", if forward { "forward" } else { "backward" }, idx + 1));
                    host.activate(w);
                    return;
                }
                continue;
            }
            let w = nli[idx - num_chars];
            if !still_nli.contains(&w) {
                continue;
            }
            cfg.hotkey_groups[gi].current_index = Some(idx);
            host.activate(w);
            self.last_not_logged_in = Some(w);
            return;
        }
        cfg.hotkey_groups[gi].current_index = saved;
        SLOG.warn(format_args!("No characters from hotkey group are currently running (or all are excluded)"));
    }

    fn assign_group(&mut self, cfg: &mut Config, gi: usize, host: &mut impl HotkeyHost<W>) -> Option<Effect> {
        let group = cfg.hotkey_groups.get_mut(gi)?;
        let Some(name) = host.thumbnail_under_cursor() else {
            SLOG.debug(format_args!("Group {gi} assign pressed but no thumbnail is under the cursor"));
            return None;
        };
        if let Some(i) = group.characters.iter().position(|c| *c == name) {
            group.characters.remove(i);
            SLOG.info(format_args!("Removed {name} from group {gi} [{}]", group.name));
        } else {
            group.characters.push(name.clone());
            SLOG.info(format_args!("Added {name} to group {gi} [{}]", group.name));
        }
        group.current_index = None;
        Some(Effect::GroupMembershipChanged(name))
    }

    pub fn is_character_excluded(&self, cfg: &Config, name: &str) -> bool {
        self.manually_excluded.iter().any(|n| n == name)
            || cfg.hotkey_groups.iter().any(|g| g.characters.iter().any(|c| c == name) && g.excluded_characters.iter().any(|c| c == name))
    }

    /// Toggles exclusion in every group containing the character; characters in no group use the manual list. Returns the new state.
    pub fn toggle_character_exclusion(&mut self, cfg: &mut Config, name: &str) -> bool {
        let mut in_group = false;
        for g in cfg.hotkey_groups.iter_mut().filter(|g| g.characters.iter().any(|c| c == name)) {
            in_group = true;
            match g.excluded_characters.iter().position(|c| c == name) {
                Some(i) => {
                    g.excluded_characters.remove(i);
                }
                None => g.excluded_characters.push(name.to_owned()),
            }
        }
        if !in_group {
            match self.manually_excluded.iter().position(|c| c == name) {
                Some(i) => {
                    self.manually_excluded.remove(i);
                }
                None => self.manually_excluded.push(name.to_owned()),
            }
        }
        self.excluded_cycle_index = None;
        self.is_character_excluded(cfg, name)
    }

    /// Deduplicated union of every group's exclusions, then the manual list, in the order they were added.
    fn excluded_list(&self, cfg: &Config) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for name in cfg.hotkey_groups.iter().flat_map(|g| &g.excluded_characters).chain(&self.manually_excluded) {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }

    fn cycle_excluded(&mut self, cfg: &Config, forward: bool, host: &mut impl HotkeyHost<W>) {
        let list = self.excluded_list(cfg);
        let num = list.len();
        if num == 0 {
            SLOG.info(format_args!("No excluded characters to cycle through"));
            return;
        }
        let mut idx = index_before_start(self.excluded_cycle_index, num, forward);
        for _ in 0..num {
            idx = step(idx, num, forward);
            if let Some(w) = host.window_by_name(&list[idx]) {
                self.excluded_cycle_index = Some(idx);
                host.activate(w);
                return;
            }
        }
        SLOG.warn(format_args!("No excluded characters are currently running"));
    }

    fn cycle_notified(&mut self, cfg: &Config, forward: bool, host: &mut impl HotkeyHost<W>) {
        let retention_ms = cfg.thumbnail.notifications.notified_cycle_retention_seconds as u64 * 1000;
        let names = host.notified_character_names(retention_ms);
        let num = names.len();
        if num == 0 {
            SLOG.info(format_args!("No recently-notified characters to cycle through"));
            return;
        }
        let found = self.last_notified_cycle_name.as_ref().and_then(|last| names.iter().position(|n| n == last));
        let mut idx = start_index(found, num, forward);
        for _ in 0..num {
            if let Some(w) = host.window_by_name(&names[idx]) {
                host.activate(w);
                self.last_notified_cycle_name = Some(names[idx].clone());
                return;
            }
            idx = step(idx, num, forward);
        }
        SLOG.warn(format_args!("No recently-notified characters are currently running"));
    }

    fn cycle_all_clients(&mut self, cfg: &Config, gs: &GlobalSettings, forward: bool, host: &mut impl HotkeyHost<W>) {
        let ordered = Self::ordered_windows(cfg, host);
        let num = ordered.len();
        if num == 0 {
            return;
        }
        let found = self.last_all_clients_cycle_name.as_ref().and_then(|last| ordered.iter().position(|(_, n)| n == last));
        let mut idx = start_index(found, num, forward);
        for _ in 0..num {
            let (w, name) = &ordered[idx];
            let skip = is_generic_character_name(name) || (gs.cycle_all_clients_respect_exclusions && self.is_character_excluded(cfg, name));
            if !skip {
                host.activate(*w);
                self.last_all_clients_cycle_name = Some(name.clone());
                return;
            }
            idx = step(idx, num, forward);
        }
        SLOG.warn(format_args!("No logged-in clients are eligible to cycle to (all excluded)"));
    }

    fn cycle_not_logged_in(&mut self, forward: bool, host: &mut impl HotkeyHost<W>) {
        let list = host.not_logged_in_windows();
        let num = list.len();
        if num == 0 {
            return;
        }
        let focused = host.focused();
        let found = focused.and_then(|f| list.iter().position(|&w| w == f)).or_else(|| self.last_not_logged_in.and_then(|l| list.iter().position(|&w| w == l)));
        let idx = start_index(found, num, forward);
        host.activate(list[idx]);
        self.last_not_logged_in = Some(list[idx]);
    }

    /// Syncs every cycle cursor to a client the user focused some other way (click, alt-tab).
    pub fn update_focused_character(&mut self, cfg: &mut Config, name: &str, window: W) {
        if is_generic_character_name(name) {
            self.last_not_logged_in = Some(window);
            return;
        }
        if let Some(i) = self.excluded_list(cfg).iter().position(|n| n == name) {
            self.excluded_cycle_index = Some(i);
        }
        self.last_all_clients_cycle_name = Some(name.to_owned());
        for b in &mut self.bindings {
            if let HotkeyAction::ActivateCharacter { character_indices, current } = &mut b.action {
                if let Some(i) = character_indices.iter().position(|&ci| cfg.characters.get(ci).is_some_and(|c| c.name == name)) {
                    *current = Some(i);
                }
            }
        }
        let reset = cfg.hotkeys.reset_group_index_on_non_group_focus;
        for g in &mut cfg.hotkey_groups {
            if let Some(i) = g.characters.iter().position(|c| c == name) {
                g.current_index = Some(i);
            } else if reset {
                g.current_index = None;
            }
        }
    }
}

/// The profile after/before `current` in the profiles directory (wrapping), or None if there are none.
fn cycle_profile(current: &str, forward: bool) -> Option<String> {
    let profiles = GlobalSettings::enumerate_profiles().map_err(|e| SLOG.err(format_args!("Failed to enumerate profiles: {e}"))).ok()?;
    if profiles.is_empty() {
        return None;
    }
    let found = profiles.iter().position(|p| p == current);
    Some(profiles[start_index(found, profiles.len(), forward)].clone())
}

/// xkb key name for a profile hotkey's base key (what Linux compositors bind on), e.g. VK 'A' -> "A", VK_PRIOR -> "Prior".
/// Mouse buttons use the compositor-neutral evdev button names "mouse:275"/"mouse:276" and wheel "mouse_up"/"mouse_down".
pub fn xkb_key_name(vk: u32) -> Option<String> {
    if (VK_F1..=VK_F24).contains(&vk) {
        return Some(format!("F{}", vk - VK_F1 + 1));
    }
    if (b'A' as u32..=b'Z' as u32).contains(&vk) || (b'0' as u32..=b'9' as u32).contains(&vk) {
        return Some((vk as u8 as char).to_string());
    }
    if (VK_NUMPAD0..=VK_NUMPAD9).contains(&vk) {
        return Some(format!("KP_{}", vk - VK_NUMPAD0));
    }
    let name = match vk {
        0x08 => "BackSpace",
        0x0D => "Return",
        0x1B => "Escape",
        VK_TAB => "Tab",
        VK_PAUSE => "Pause",
        VK_CAPITAL => "Caps_Lock",
        VK_NUMLOCK => "Num_Lock",
        VK_SCROLL => "Scroll_Lock",
        VK_SPACE => "space",
        VK_PRIOR => "Prior",
        VK_NEXT => "Next",
        VK_END => "End",
        VK_HOME => "Home",
        VK_LEFT => "Left",
        VK_UP => "Up",
        VK_RIGHT => "Right",
        VK_DOWN => "Down",
        VK_INSERT => "Insert",
        VK_DELETE => "Delete",
        VK_MULTIPLY => "KP_Multiply",
        VK_ADD => "KP_Add",
        VK_SUBTRACT => "KP_Subtract",
        VK_DECIMAL => "KP_Decimal",
        VK_DIVIDE => "KP_Divide",
        VK_XBUTTON1 => "mouse:275",
        VK_XBUTTON2 => "mouse:276",
        VK_WHEELUP => "mouse_up",
        VK_WHEELDOWN => "mouse_down",
        VK_OEM_1 => "semicolon",
        VK_OEM_PLUS => "equal",
        VK_OEM_COMMA => "comma",
        VK_OEM_MINUS => "minus",
        VK_OEM_PERIOD => "period",
        VK_OEM_2 => "slash",
        VK_OEM_3 => "grave",
        VK_OEM_4 => "bracketleft",
        VK_OEM_5 => "backslash",
        VK_OEM_6 => "bracketright",
        VK_OEM_7 => "apostrophe",
        _ => return None,
    };
    Some(name.to_owned())
}

/// Whether a hotkey is Ctrl+Alt+F1..F12, which Linux reserves for switching to the text consoles (xkb turns it into
/// XF86Switch_VT_n before any app sees it). Such a key can't work as a hotkey there, and pressing it leaves the desktop
/// for a black console, so the Linux front end refuses to bind it.
pub fn is_vt_switch_combo(combined: u32) -> bool {
    let m = extract_modifiers(combined);
    let vk = extract_vk(combined);
    m & MOD_CONTROL != 0 && m & MOD_ALT != 0 && (VK_F1..VK_F1 + 12).contains(&vk)
}

/// Modifier names for a combined key, in Hyprland's spelling (SUPER/SHIFT/CTRL/ALT).
pub fn modifier_names(combined: u32) -> Vec<&'static str> {
    let m = extract_modifiers(combined);
    let mut out = Vec::new();
    if m & MOD_WIN != 0 {
        out.push("SUPER");
    }
    if m & MOD_CONTROL != 0 {
        out.push("CTRL");
    }
    if m & MOD_ALT != 0 {
        out.push("ALT");
    }
    if m & MOD_SHIFT != 0 {
        out.push("SHIFT");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HotkeyGroup;

    #[derive(Default)]
    struct FakeHost {
        windows: Vec<(u32, String)>,
        focused: Option<u32>,
        activated: Vec<u32>,
        under_cursor: Option<String>,
    }

    impl HotkeyHost<u32> for FakeHost {
        fn windows(&self) -> Vec<(u32, String)> {
            self.windows.clone()
        }
        fn not_logged_in_windows(&self) -> Vec<u32> {
            self.windows.iter().filter(|(_, n)| is_generic_character_name(n)).map(|(w, _)| *w).collect()
        }
        fn focused(&self) -> Option<u32> {
            self.focused
        }
        fn activate(&mut self, w: u32) {
            self.activated.push(w);
            self.focused = Some(w);
        }
        fn thumbnail_under_cursor(&self) -> Option<String> {
            self.under_cursor.clone()
        }
        fn notified_character_names(&self, _: u64) -> Vec<String> {
            vec!["B".into(), "A".into()]
        }
        fn account_membership(&mut self) -> Membership {
            Membership::new()
        }
    }

    fn setup() -> (Config, GlobalSettings, FakeHost) {
        let mut cfg = Config::defaults_with_profile("default.json").unwrap();
        let mut g = HotkeyGroup::default();
        g.characters = vec!["A".into(), "B".into(), "C".into()];
        g.forward_key = Some(combine_key(VK_F1, 0));
        g.backward_key = Some(combine_key(VK_F1, MOD_SHIFT));
        cfg.hotkey_groups = vec![g];
        cfg.hotkeys.require_eve_focus = false;
        let host = FakeHost { windows: vec![(1, "A".into()), (2, "B".into()), (3, "C".into()), (4, "EVE".into())], ..Default::default() };
        (cfg, GlobalSettings::default(), host)
    }

    #[test]
    fn group_cycles_forward_backward_and_skips_excluded() {
        let (mut cfg, gs, mut host) = setup();
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        let fwd = m.bindings.iter().find(|b| b.action == HotkeyAction::CycleGroup { group_index: 0, forward: true }).unwrap().id;
        let back = m.bindings.iter().find(|b| b.action == HotkeyAction::CycleGroup { group_index: 0, forward: false }).unwrap().id;
        m.handle_press(fwd, &mut cfg, &gs, &mut host);
        m.handle_press(fwd, &mut cfg, &gs, &mut host);
        assert_eq!(host.activated, vec![1, 2]);
        assert!(m.toggle_character_exclusion(&mut cfg, "C"));
        m.handle_press(fwd, &mut cfg, &gs, &mut host);
        assert_eq!(host.activated.last(), Some(&1), "C is excluded, wraps to A");
        m.handle_press(back, &mut cfg, &gs, &mut host);
        assert_eq!(host.activated.last(), Some(&2), "backward from A skips excluded C");
    }

    #[test]
    fn suspend_blocks_everything_but_itself() {
        let (mut cfg, gs, mut host) = setup();
        cfg.hotkeys.hotkey_suspend = Some(combine_key(VK_PAUSE, 0));
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        let suspend = m.bindings.iter().find(|b| b.action == HotkeyAction::SuspendHotkeys).unwrap().id;
        assert_eq!(m.handle_press(suspend, &mut cfg, &gs, &mut host), Some(Effect::SuspendChanged(true)));
        assert_eq!(m.active_bindings().count(), 1);
        let fwd = m.bindings[0].id;
        m.handle_press(fwd, &mut cfg, &gs, &mut host);
        assert!(host.activated.is_empty());
    }

    #[test]
    fn assign_toggles_membership_of_hovered_thumbnail() {
        let (mut cfg, gs, mut host) = setup();
        cfg.hotkey_groups[0].assign_key = Some(combine_key(b'G' as u32, MOD_CONTROL));
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        let assign = m.bindings.iter().find(|b| matches!(b.action, HotkeyAction::AssignGroup { .. })).unwrap().id;
        host.under_cursor = Some("D".into());
        assert_eq!(m.handle_press(assign, &mut cfg, &gs, &mut host), Some(Effect::GroupMembershipChanged("D".into())));
        assert!(cfg.hotkey_groups[0].characters.contains(&"D".to_owned()));
        m.handle_press(assign, &mut cfg, &gs, &mut host);
        assert!(!cfg.hotkey_groups[0].characters.contains(&"D".to_owned()));
    }

    #[test]
    fn shared_per_character_key_cycles_running_characters() {
        let (mut cfg, gs, mut host) = setup();
        let key = Some(combine_key(VK_F1 + 1, 0));
        cfg.get_or_create_character("B").hotkey = key;
        cfg.get_or_create_character("Z").hotkey = key;
        cfg.get_or_create_character("C").hotkey = key;
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        let b = m.bindings.iter().find(|b| matches!(b.action, HotkeyAction::ActivateCharacter { .. })).unwrap().id;
        for _ in 0..3 {
            m.handle_press(b, &mut cfg, &gs, &mut host);
        }
        assert_eq!(host.activated, vec![2, 3, 2], "Z isn't running and is skipped");
    }

    #[test]
    fn notified_and_all_clients_cycles() {
        let (mut cfg, gs, mut host) = setup();
        cfg.hotkeys.hotkey_cycle_notified = Some(combine_key(VK_F1 + 2, 0));
        let mut gs = gs;
        gs.hotkey_cycle_all_clients_forward = Some(combine_key(VK_F1 + 3, 0));
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        let notified = m.bindings.iter().find(|b| b.action == HotkeyAction::CycleNotified).unwrap().id;
        let all = m.bindings.iter().find(|b| b.action == HotkeyAction::NextAllClients).unwrap().id;
        m.handle_press(notified, &mut cfg, &gs, &mut host);
        m.handle_press(notified, &mut cfg, &gs, &mut host);
        assert_eq!(host.activated, vec![2, 1]);
        host.activated.clear();
        for _ in 0..4 {
            m.handle_press(all, &mut cfg, &gs, &mut host);
        }
        assert_eq!(host.activated, vec![1, 2, 3, 1], "the login-screen client is skipped");
    }

    #[test]
    fn require_eve_focus_gates_presses() {
        let (mut cfg, gs, mut host) = setup();
        cfg.hotkeys.require_eve_focus = true;
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        let fwd = m.bindings[0].id;
        m.handle_press(fwd, &mut cfg, &gs, &mut host);
        assert!(host.activated.is_empty());
        host.focused = Some(3);
        m.handle_press(fwd, &mut cfg, &gs, &mut host);
        assert_eq!(host.activated, vec![1]);
    }

    #[test]
    fn protocol_actions_run_without_a_bound_key() {
        let (mut cfg, gs, mut host) = setup();
        let mut m = HotkeyManager::<u32>::default();
        m.rebuild(&cfg, &gs);
        use crate::protocol::HotkeyAction as P;
        assert_eq!(m.run_global_action(P::MinimizeAll, &mut cfg, &gs, &mut host), Some(Effect::MinimizeAll));
        m.run_global_action(P::NextAllClients, &mut cfg, &gs, &mut host);
        assert_eq!(host.activated, vec![1]);
        assert_eq!(m.run_global_action(P::SuspendHotkeys, &mut cfg, &gs, &mut host), Some(Effect::SuspendChanged(true)));
    }

    #[test]
    fn key_names() {
        assert_eq!(xkb_key_name(VK_F1 + 11).as_deref(), Some("F12"));
        assert_eq!(xkb_key_name(VK_PRIOR).as_deref(), Some("Prior"));
        assert_eq!(xkb_key_name(VK_NUMPAD0 + 7).as_deref(), Some("KP_7"));
        assert_eq!(xkb_key_name(VK_XBUTTON2).as_deref(), Some("mouse:276"));
        assert_eq!(modifier_names(combine_key(VK_F1, MOD_CONTROL | MOD_SHIFT)), vec!["CTRL", "SHIFT"]);
    }

    #[test]
    fn vt_switch_combos_are_detected() {
        assert!(is_vt_switch_combo(combine_key(VK_F1 + 10, MOD_CONTROL | MOD_ALT)));
        assert!(is_vt_switch_combo(combine_key(VK_F1, MOD_CONTROL | MOD_ALT | MOD_SHIFT)));
        assert!(!is_vt_switch_combo(combine_key(VK_F1 + 12, MOD_CONTROL | MOD_ALT)), "F13 has no console");
        assert!(!is_vt_switch_combo(combine_key(VK_F1 + 10, MOD_CONTROL | MOD_SHIFT)));
        assert!(!is_vt_switch_combo(combine_key(b'A' as u32, MOD_CONTROL | MOD_ALT)));
    }
}
