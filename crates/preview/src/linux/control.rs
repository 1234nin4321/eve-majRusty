//! Hotkeys and client-window actions: the Linux counterpart of the Windows build's hotkeys.rs/manager.rs/input.rs
//! glue. The decisions (which client a cycle lands on, exclusions, which profile is next) live in
//! eve_maj_core::hotkeys; this module feeds it a snapshot of the running clients and carries out what it asks for
//! through the window manager backend.

use eve_maj_core::accounts_store::{self, Membership};
use eve_maj_core::config::{Config, Position};
use eve_maj_core::hotkeys::{self, Effect, HotkeyHost};
use eve_maj_core::layout::is_generic_character_name;
use eve_maj_core::log::Scope;
use eve_maj_core::render::{now_ms, ActiveNotification, MAX_STACKED_NOTIFICATIONS};
use eve_maj_core::state::{try_transition_visibility, VisibilityState};
use eve_maj_core::types::NotificationType;
use eve_maj_core::virtual_keys::extract_vk;

use super::backend::{process_image_path, KeyBind, WindowId};
use super::tray::{TrayCommand, TrayState};
use super::App;

const SLOG: Scope = Scope::new("control");

/// A client moved to its saved position, re-checked until it stops moving (EVE repositions its own window while loading).
pub(super) struct PendingMove {
    window: WindowId,
    target: (i32, i32),
    last_move: u64,
    checks_left: u8,
    last_seen: Option<(i32, i32)>,
}

/// What the hotkey logic sees of the running app at the moment of a press; activations are carried out afterwards.
struct Snapshot {
    windows: Vec<(WindowId, String)>,
    not_logged_in: Vec<WindowId>,
    focused: Option<WindowId>,
    under_cursor: Option<String>,
    notified: Vec<(String, u64)>,
    activated: Vec<WindowId>,
}

impl HotkeyHost<WindowId> for Snapshot {
    fn windows(&self) -> Vec<(WindowId, String)> {
        self.windows.clone()
    }
    fn not_logged_in_windows(&self) -> Vec<WindowId> {
        self.not_logged_in.clone()
    }
    fn focused(&self) -> Option<WindowId> {
        self.focused
    }
    fn activate(&mut self, window: WindowId) {
        self.activated.push(window);
    }
    fn thumbnail_under_cursor(&self) -> Option<String> {
        self.under_cursor.clone()
    }
    fn notified_character_names(&self, retention_ms: u64) -> Vec<String> {
        let now = now_ms();
        let mut out: Vec<String> = Vec::new();
        for (name, at) in self.notified.iter().rev() {
            if now.saturating_sub(*at) <= retention_ms && !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }
    fn account_membership(&mut self) -> Membership {
        accounts_store::load_membership()
    }
}

impl App {
    /// (Re)binds every live hotkey in the compositor: all of them, or just the suspend key while suspended.
    pub(super) fn register_hotkeys(&mut self) {
        if self.dialog_hotkeys_suspended {
            self.wm.set_hotkeys(&[]);
            SLOG.info(format_args!("Hotkeys paused while the settings dialog records a key"));
            return;
        }
        let repeating = self.config.hotkeys.allow_hotkey_auto_repeat;
        let binds: Vec<KeyBind> = self
            .hotkeys
            .active_bindings()
            .filter(|b| {
                let vt = hotkeys::is_vt_switch_combo(b.key);
                if vt {
                    SLOG.err(format_args!(
                        "Not binding {} ({}): Ctrl+Alt+F1-F12 switch Linux to a text console; pick another key",
                        eve_maj_core::virtual_keys::format_virtual_key(b.key),
                        b.description
                    ));
                }
                !vt
            })
            .filter_map(|b| match hotkeys::xkb_key_name(extract_vk(b.key)) {
                Some(key) => Some(KeyBind { id: b.id, modifiers: hotkeys::modifier_names(b.key), key, repeating, description: b.description.clone() }),
                None => {
                    SLOG.warn(format_args!("No Linux key for {} ({})", eve_maj_core::virtual_keys::format_virtual_key(b.key), b.description));
                    None
                }
            })
            .collect();
        let ok = self.wm.set_hotkeys(&binds);
        SLOG.info(format_args!("Registered {ok} of {} hotkey(s)", binds.len()));
    }

    pub(super) fn unregister_hotkeys(&mut self) {
        self.wm.set_hotkeys(&[]);
    }

    fn snapshot(&self) -> Snapshot {
        let mut not_logged_in: Vec<(u64, WindowId)> = self.thumbs.iter().filter(|t| is_generic_character_name(&t.name)).map(|t| (t.logged_out_at, t.window)).collect();
        not_logged_in.sort();
        Snapshot {
            windows: self.thumbs.iter().map(|t| (t.window, t.name.clone())).collect(),
            not_logged_in: not_logged_in.into_iter().map(|(_, w)| w).collect(),
            focused: self.active.filter(|a| self.thumbs.iter().any(|t| t.window == *a)),
            under_cursor: self.pointer_state.hover.and_then(|id| self.thumb_index(id)).map(|i| self.thumbs[i].name.clone()),
            notified: self.notified.clone(),
            activated: Vec::new(),
        }
    }

    pub(super) fn on_hotkey(&mut self, id: u32) {
        let mut snap = self.snapshot();
        let effect = self.hotkeys.handle_press(id, &mut self.config, &self.global_settings, &mut snap);
        if let Some(&window) = snap.activated.last() {
            self.activate_window(window);
        }
        if let Some(effect) = effect {
            self.apply_effect(effect);
        }
        self.render_dirty();
    }

    fn apply_effect(&mut self, effect: Effect) {
        SLOG.debug(format_args!("Hotkey effect: {effect:?}"));
        match effect {
            Effect::MinimizeAll => self.minimize_all_clients(),
            Effect::CloseAll => self.close_all_clients(),
            Effect::ToggleVisibility => self.toggle_all_thumbnails_visibility(),
            Effect::ToggleAutoMinimize => {
                self.config.auto_minimize.enabled = !self.config.auto_minimize.enabled;
                SLOG.info(format_args!("Auto-minimize {}", if self.config.auto_minimize.enabled { "enabled" } else { "disabled" }));
            }
            Effect::MoveToSavedPositions => self.move_all_clients_to_saved_positions(),
            Effect::SwitchProfile(name) => self.switch_profile(&name),
            Effect::ToggleExclusionOfFocused => {
                if let Some(i) = self.active.and_then(|a| self.thumbs.iter().position(|t| t.window == a)) {
                    self.toggle_exclusion(i);
                }
            }
            Effect::ReturnToLastApp => match self.last_non_eve {
                Some(w) if self.toplevels.iter().any(|t| t.id == w) => self.wm.focus(w),
                _ => SLOG.debug(format_args!("No previous non-EVE window to return to")),
            },
            Effect::ActivateApp(exe) => self.activate_app(&exe),
            Effect::OpenUrl { url, upload_clipboard } => open_url(url, upload_clipboard),
            Effect::SuspendChanged(_) => self.register_hotkeys(),
            Effect::GroupMembershipChanged(name) => {
                for i in 0..self.thumbs.len() {
                    if self.thumbs[i].name == name {
                        self.refresh_character_caches(i);
                    }
                }
                if self.config.display.region_fit_order == eve_maj_core::types::RegionFitOrder::HotkeyGroups {
                    self.reflow();
                }
            }
        }
    }

    /// Brings a client up the way clicking its thumbnail does: unminimize, focus, clear click-dismissed notifications.
    pub(super) fn activate_window(&mut self, window: WindowId) {
        let Some(i) = self.thumbs.iter().position(|t| t.window == window) else { return };
        if self.thumbs[i].minimized {
            self.wm.restore(window);
        } else {
            self.wm.focus(window);
        }
        let t = &mut self.thumbs[i];
        let before = t.notifications.len();
        t.notifications.retain(|n| !n.suppress_when_clicked);
        if t.notifications.len() != before {
            t.needs_render = true;
        }
        self.set_active(Some(window));
    }

    /// Shift+click / toggle-exclusion hotkey: in or out of hotkey cycling, with the exclusion overlay and a short notice.
    pub(super) fn toggle_exclusion(&mut self, i: usize) {
        let name = self.thumbs[i].name.clone();
        let excluded = self.hotkeys.toggle_character_exclusion(&mut self.config, &name);
        self.thumbs[i].excluded = excluded;
        if excluded && self.config.exclusion.auto_minimize_excluded {
            self.wm.minimize(self.thumbs[i].window);
        }
        let text = if excluded { "Excluded" } else { "Included" };
        let t = &mut self.thumbs[i];
        t.notifications.insert(
            0,
            ActiveNotification {
                text: text.to_owned(),
                notification_type: NotificationType::Generic,
                start_ms: now_ms(),
                duration_ms: 3000,
                suppress_when_focused: false,
                suppress_when_clicked: false,
                border_color_override: None,
                text_color_override: None,
                show_border: false,
                flash_border: false,
            },
        );
        t.notifications.truncate(MAX_STACKED_NOTIFICATIONS);
        t.needs_render = true;
        SLOG.info(format_args!("Toggled cycle exclusion for {name}: {text}"));
    }

    /// Focus moved: sync hotkey cycle cursors and remember per-monitor/last-non-EVE focus.
    pub(super) fn on_focus_changed(&mut self, new_active: Option<WindowId>) {
        let Some(window) = new_active else { return };
        match self.thumbs.iter().find(|t| t.window == window) {
            Some(t) => {
                let name = t.name.clone();
                self.hotkeys.update_focused_character(&mut self.config, &name, window);
                if let Some(monitor) = self.window_monitor(window) {
                    self.last_focused_by_monitor.retain(|_, w| *w != window);
                    self.last_focused_by_monitor.insert(monitor, window);
                }
            }
            None => self.last_non_eve = Some(window),
        }
    }

    fn window_monitor(&self, window: WindowId) -> Option<String> {
        self.toplevels.iter().find(|t| t.id == window).map(|t| t.monitor.clone())
    }

    pub(super) fn minimize_all_clients(&mut self) {
        for t in &self.thumbs {
            if !t.minimized {
                self.wm.minimize(t.window);
            }
        }
        self.scan_needed = true;
    }

    pub(super) fn close_all_clients(&mut self) {
        let (mut closed, mut skipped) = (0, 0);
        for t in &self.thumbs {
            let login_screen = self.config.close_all.exclude_login_screen_clients && is_generic_character_name(&t.name);
            if self.config.is_excluded_from_close_all(&t.name) || login_screen {
                skipped += 1;
                continue;
            }
            self.wm.close(t.window);
            closed += 1;
        }
        SLOG.info(format_args!("Close all: closed {closed}, skipped {skipped}"));
    }

    pub(super) fn toggle_all_thumbnails_visibility(&mut self) {
        let Some(first) = self.thumbs.first() else { return };
        let target = if first.visibility == VisibilityState::Visible { VisibilityState::HiddenManual } else { VisibilityState::Visible };
        let dragging = self.pointer_state.drag.as_ref().map(|d| d.thumb);
        for t in &mut self.thumbs {
            // Alerting or dragged thumbnails can't be hidden, like the Windows set_visibility guard.
            if target != VisibilityState::Visible && (!t.notifications.is_empty() || dragging == Some(t.id)) {
                continue;
            }
            t.visibility = try_transition_visibility(t.visibility, target, &t.name);
            t.needs_render = true;
        }
    }

    /// Minimizes clients that have been inactive for autoMinimize.delayMs, sparing each monitor's last-active client while focus is elsewhere.
    pub(super) fn check_auto_minimize(&mut self) {
        let now = now_ms();
        let dragging = self.pointer_state.drag.as_ref().map(|d| d.thumb);
        for t in &mut self.thumbs {
            if Some(t.window) == self.active || t.minimized || dragging == Some(t.id) {
                t.inactive_since = now;
            }
        }
        let auto = &self.config.auto_minimize;
        if !auto.enabled {
            return;
        }
        let eve_has_focus = self.active.is_some_and(|a| self.thumbs.iter().any(|t| t.window == a));
        let focused_monitor = if eve_has_focus { self.active.and_then(|a| self.window_monitor(a)) } else { None };
        let mut to_minimize = Vec::new();
        for t in &self.thumbs {
            if Some(t.window) == self.active || t.minimized || dragging == Some(t.id) {
                continue;
            }
            let monitor = self.window_monitor(t.window);
            let on_other_monitor = focused_monitor.is_some() && monitor != focused_monitor;
            let spared_by_focus_loss = auto.exempt_last_active_on_focus_loss && !eve_has_focus;
            let last_active_here = monitor.as_ref().and_then(|m| self.last_focused_by_monitor.get(m)).is_none_or(|&w| w == t.window || !self.thumbs.iter().any(|o| o.window == w));
            if (on_other_monitor || spared_by_focus_loss) && last_active_here {
                continue;
            }
            if now.saturating_sub(t.inactive_since) < auto.delay_ms as u64 || self.config.is_excluded_from_minimize(&t.name) {
                continue;
            }
            to_minimize.push((t.window, t.name.clone()));
        }
        for (w, name) in to_minimize {
            self.wm.minimize(w);
            SLOG.info(format_args!("Auto-minimized {name}"));
            self.scan_needed = true;
        }
    }

    pub(super) fn move_all_clients_to_saved_positions(&mut self) {
        let targets: Vec<(WindowId, String)> = self.thumbs.iter().map(|t| (t.window, t.name.clone())).collect();
        for (w, name) in targets {
            self.move_client_to_saved_position(w, &name);
        }
    }

    /// Moves a client to its saved window position (callers gate on their own setting) and queues verification.
    pub(super) fn move_client_to_saved_position(&mut self, window: WindowId, name: &str) {
        if is_generic_character_name(name) || self.config.is_excluded_from_auto_move(name) {
            return;
        }
        let Some(pos) = self.config.character_window_position(name) else { return };
        self.wm.move_to(window, pos.x, pos.y);
        SLOG.info(format_args!("Moved {name} client window to saved position ({}, {})", pos.x, pos.y));
        let verify_count = self.config.auto_move_position.verify_count;
        if verify_count > 0 {
            self.pending_moves.retain(|p| p.window != window);
            self.pending_moves.push(PendingMove { window, target: (pos.x, pos.y), last_move: now_ms(), checks_left: verify_count, last_seen: None });
        }
    }

    /// Waits for a moved client to stop moving, then corrects it once if it drifted.
    pub(super) fn verify_pending_moves(&mut self) {
        if self.pending_moves.is_empty() {
            return;
        }
        let now = now_ms();
        let interval = self.config.auto_move_position.verify_interval_ms as u64;
        let mut reapply = Vec::new();
        let toplevels = &self.toplevels;
        self.pending_moves.retain_mut(|p| {
            let Some(at) = toplevels.iter().find(|t| t.id == p.window).map(|t| t.at) else { return false };
            if now.saturating_sub(p.last_move) < interval {
                return true;
            }
            p.last_move = now;
            if at == p.target {
                return false;
            }
            p.checks_left = p.checks_left.saturating_sub(1);
            if p.last_seen == Some(at) || p.checks_left == 0 {
                reapply.push((p.window, p.target));
                return false;
            }
            p.last_seen = Some(at);
            true
        });
        for (w, (x, y)) in reapply {
            SLOG.info(format_args!("Client drifted after auto-move, re-applying ({x}, {y})"));
            self.wm.move_to(w, x, y);
        }
    }

    /// Saves every logged-in client's current window position as its auto-move target (tray action; the config dialog does this on Windows).
    pub(super) fn save_client_window_positions(&mut self) {
        let positions: Vec<(String, (i32, i32))> = self
            .thumbs
            .iter()
            .filter(|t| !is_generic_character_name(&t.name))
            .filter_map(|t| self.toplevels.iter().find(|tl| tl.id == t.window).map(|tl| (t.name.clone(), tl.at)))
            .collect();
        for (name, (x, y)) in &positions {
            if let Err(e) = self.config.save_character_window_position(name, Position { x: *x, y: *y }) {
                SLOG.err(format_args!("Failed to save window position for {name}: {e}"));
            }
        }
        self.profile_mtime = super::profile_mtime(&self.config.profile_name);
        SLOG.info(format_args!("Saved window positions for {} client(s)", positions.len()));
    }

    fn activate_app(&mut self, exe: &str) {
        let target = self.toplevels.iter().find(|t| {
            let image = process_image_path(t.pid).unwrap_or_default();
            let base = image.rsplit(['/', '\\']).next().unwrap_or("");
            base.eq_ignore_ascii_case(exe) || t.class.eq_ignore_ascii_case(exe) || t.class.eq_ignore_ascii_case(exe.trim_end_matches(".exe"))
        });
        match target {
            Some(t) if t.minimized => self.wm.restore(t.id),
            Some(t) => self.wm.focus(t.id),
            None => SLOG.debug(format_args!("No running window for {exe}")),
        }
    }

    /// Loads another profile and re-applies everything that depends on it (the Windows reload_with_profile).
    pub(super) fn switch_profile(&mut self, name: &str) {
        let config = match Config::load_profile(name) {
            Ok(c) => c,
            Err(e) => {
                SLOG.err(format_args!("Failed to load profile {name}: {e}"));
                return;
            }
        };
        SLOG.info(format_args!("Switching profile {} -> {}", self.config.profile_name, config.profile_name));
        self.apply_new_config(config);
        if let Err(e) = self.global_settings.update_last_used(&self.config.profile_name) {
            SLOG.warn(format_args!("Couldn't record last used profile: {e}"));
        }
    }

    pub(super) fn tray_state(&self) -> TrayState {
        TrayState {
            profiles: eve_maj_core::config::GlobalSettings::enumerate_profiles().unwrap_or_default(),
            current_profile: self.config.profile_name.clone(),
            dragging: self.config.interaction.enable_dragging,
            auto_minimize: self.config.auto_minimize.enabled,
            travel_mode: self.config.travel.enabled,
            thumbnails_visible: self.thumbs.first().is_none_or(|t| t.visibility.is_visible()),
            has_suspend_hotkey: self.config.hotkeys.hotkey_suspend.is_some(),
            hotkeys_suspended: self.hotkeys.is_suspended(),
        }
    }

    pub(super) fn refresh_tray(&mut self) {
        let state = self.tray_state();
        if let Some(t) = self.tray.as_mut() {
            t.update(state);
        }
    }

    fn save_profile(&mut self, what: &str) {
        if let Err(e) = self.config.save_current_profile() {
            SLOG.err(format_args!("Failed to save profile after toggling {what}: {e}"));
        }
        self.profile_mtime = super::profile_mtime(&self.config.profile_name);
    }

    pub(super) fn on_tray_command(&mut self, cmd: TrayCommand) {
        SLOG.info(format_args!("Tray: {cmd:?}"));
        match cmd {
            TrayCommand::OpenSettings => open_settings(&self.config.profile_name),
            TrayCommand::LoadProfile(name) => self.switch_profile(&name),
            TrayCommand::ToggleDragging => {
                self.config.interaction.enable_dragging = !self.config.interaction.enable_dragging;
                self.save_profile("dragging");
            }
            // Temporary, like on Windows: not saved.
            TrayCommand::ToggleAutoMinimize => self.config.auto_minimize.enabled = !self.config.auto_minimize.enabled,
            TrayCommand::ToggleTravelMode => {
                self.config.travel.enabled = !self.config.travel.enabled;
                self.save_profile("Travel Mode");
            }
            TrayCommand::ToggleVisibility => self.toggle_all_thumbnails_visibility(),
            TrayCommand::RestoreSavedPositions => self.move_all_clients_to_saved_positions(),
            TrayCommand::SaveClientPositions => self.save_client_window_positions(),
            TrayCommand::ClearNotificationHistory => self.notified.clear(),
            TrayCommand::SuspendHotkeys => self.run_link_action(eve_maj_core::protocol::HotkeyAction::SuspendHotkeys),
            TrayCommand::CloseAll => self.close_all_clients(),
            TrayCommand::Exit => self.exit_requested = true,
        }
        self.render_dirty();
        self.refresh_tray();
    }

    fn run_link_action(&mut self, action: eve_maj_core::protocol::HotkeyAction) {
        let mut snap = self.snapshot();
        let effect = self.hotkeys.run_global_action(action, &mut self.config, &self.global_settings, &mut snap);
        if let Some(&window) = snap.activated.last() {
            self.activate_window(window);
        }
        if let Some(effect) = effect {
            self.apply_effect(effect);
        }
    }

    /// An evemajpreview:// link, from a second invocation.
    pub(super) fn on_protocol_url(&mut self, url: &str) {
        use eve_maj_core::protocol::{parse_url, safe_profile_name, Command};
        SLOG.info(format_args!("Link: {url}"));
        match parse_url(url) {
            Ok(Command::Switch(name)) => {
                let name = String::from_utf8_lossy(&name).into_owned();
                match self.thumbs.iter().find(|t| t.name == name).map(|t| t.window) {
                    Some(w) => self.activate_window(w),
                    None => SLOG.warn(format_args!("Link asked for {name}, who isn't running")),
                }
            }
            Ok(Command::Profile(name)) => match safe_profile_name(&name) {
                Some(n) => self.switch_profile(&n.to_owned()),
                None => SLOG.warn(format_args!("Rejected unsafe profile name in link")),
            },
            Ok(Command::Hotkey(action)) => self.run_link_action(action),
            Ok(other) => SLOG.warn(format_args!("Link command not supported on Linux yet: {other:?}")),
            Err(e) => SLOG.warn(format_args!("Bad link {url}: {e:?}")),
        }
        self.render_dirty();
        self.refresh_tray();
    }

    /// Swaps in a freshly loaded profile: caches, exclusion flags, layout and hotkeys.
    pub(super) fn apply_new_config(&mut self, config: Config) {
        self.config = config;
        self.profile_mtime = super::profile_mtime(&self.config.profile_name);
        self.membership = accounts_store::load_membership();
        self.hotkeys.rebuild(&self.config, &self.global_settings);
        for i in 0..self.thumbs.len() {
            self.thumbs[i].excluded = self.hotkeys.is_character_excluded(&self.config, &self.thumbs[i].name);
            self.refresh_character_caches(i);
            self.set_click_through(i);
        }
        self.reflow();
        self.register_hotkeys();
        // Restarts or stops chat-log monitoring if its settings changed.
        self.ensure_chatlog();
        self.scan_needed = true;
    }
}

/// Starts the settings window (eve-maj-config, next to this binary or on PATH) on the running profile.
fn open_settings(profile: &str) {
    let sibling = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("eve-maj-config"))).filter(|p| p.is_file());
    let program = sibling.map(|p| p.into_os_string()).unwrap_or_else(|| "eve-maj-config".into());
    match std::process::Command::new(&program).arg(profile).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => SLOG.err(format_args!("Couldn't start the settings window ({}): {e}", program.to_string_lossy())),
    }
}

/// Opens a URL hotkey's target, or uploads the clipboard to it first (uploadClipboard), off the UI thread.
fn open_url(url: String, upload_clipboard: bool) {
    std::thread::spawn(move || {
        let target = if upload_clipboard {
            if !eve_maj_core::paste_upload::is_allowed_upload_url(&url) {
                SLOG.warn(format_args!("Refusing clipboard upload to {url}: not on the allow-list"));
                return;
            }
            let Ok(out) = std::process::Command::new("wl-paste").arg("--no-newline").output() else {
                SLOG.err(format_args!("wl-paste failed; is wl-clipboard installed?"));
                return;
            };
            if out.stdout.is_empty() {
                SLOG.info(format_args!("Clipboard is empty, nothing to upload"));
                return;
            }
            let body = eve_maj_core::paste_upload::build_upload_body(&out.stdout);
            match eve_maj_core::paste_upload::post_and_follow_redirect(&url, &body) {
                Ok(page) => {
                    let _ = std::process::Command::new("wl-copy").arg(&page).status();
                    page
                }
                Err(e) => {
                    SLOG.err(format_args!("Clipboard upload to {url} failed: {e}"));
                    return;
                }
            }
        } else {
            url
        };
        if let Err(e) = std::process::Command::new("xdg-open").arg(&target).spawn() {
            SLOG.err(format_args!("xdg-open {target}: {e}"));
        }
    });
}
