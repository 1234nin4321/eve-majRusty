//! Chat-log monitoring on Linux: runs eve_maj_core's ChatlogWorker, keeps its character list in step with the
//! logged-in clients, and applies what it finds (system names, notifications with their sounds/speech, Travel Mode's
//! left-behind alert) the way the Windows painter does.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eve_maj_core::chatlog::{ChatlogEvent, ChatlogMonitor, ChatlogWorker, WorkerCommand};
use eve_maj_core::config::{ChatlogConfig, TravelThresholdMode};
use eve_maj_core::layout::is_generic_character_name;
use eve_maj_core::log::Scope;
use eve_maj_core::render::{now_ms, ActiveNotification, MAX_STACKED_NOTIFICATIONS};
use eve_maj_core::types::NotificationType;

use super::activity::{TrackerKey, Trackers};
use super::{alerts, App};

const SLOG: Scope = Scope::new("chatlog");

pub(super) struct Chatlog {
    worker: Option<ChatlogWorker>,
    wake_read: RawFd,
    wake_write: RawFd,
    /// Characters the worker has been told about.
    characters: Vec<String>,
    /// The settings it was started with; a profile change to any of them restarts it.
    config: ChatlogConfig,
    trackers: TrackerKey,
}

impl Chatlog {
    pub fn fd(&self) -> RawFd {
        self.wake_read
    }
}

impl Drop for Chatlog {
    fn drop(&mut self) {
        // Join the worker before closing the pipe its wake callback writes to.
        if let Some(w) = self.worker.take() {
            w.shutdown();
        }
        unsafe {
            libc::close(self.wake_read);
            libc::close(self.wake_write);
        }
    }
}

/// Where EVE writes its logs: the profile's directories when they exist, else the running client's Wine prefix
/// (WINEPREFIX, or Proton's STEAM_COMPAT_DATA_PATH/pfx) under drive_c/users/*/Documents/EVE/logs.
fn resolve_log_dirs(cfg: &ChatlogConfig, eve_pids: &[i32]) -> Option<(PathBuf, PathBuf, &'static str)> {
    let (chat, game) = (PathBuf::from(&cfg.chatlog_dir), PathBuf::from(&cfg.gamelog_dir));
    if chat.is_dir() && game.is_dir() {
        return Some((chat, game, "profile"));
    }
    for &pid in eve_pids {
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        let var = |name: &str| {
            environ.split(|&b| b == 0).find_map(|kv| kv.strip_prefix(format!("{name}=").as_bytes())).map(|v| PathBuf::from(String::from_utf8_lossy(v).into_owned()))
        };
        let prefixes = [var("WINEPREFIX"), var("STEAM_COMPAT_DATA_PATH").map(|p| p.join("pfx"))];
        for prefix in prefixes.into_iter().flatten() {
            if let Some(logs) = find_logs_in_prefix(&prefix) {
                return Some((logs.join("Chatlogs"), logs.join("Gamelogs"), "Wine prefix"));
            }
        }
    }
    None
}

fn find_logs_in_prefix(prefix: &Path) -> Option<PathBuf> {
    let users = std::fs::read_dir(prefix.join("drive_c/users")).ok()?;
    users.filter_map(|e| e.ok()).map(|e| e.path().join("Documents/EVE/logs")).find(|logs| logs.join("Chatlogs").is_dir() && logs.join("Gamelogs").is_dir())
}

impl App {
    /// Starts monitoring once chat-log monitoring is enabled and the log directories can be found (which, without
    /// configured directories, needs a running client to read the Wine prefix from).
    pub(super) fn ensure_chatlog(&mut self) {
        let cfg = &self.config.chatlog;
        let key = TrackerKey::from(&self.config);
        if self.chatlog.as_ref().is_some_and(|c| !cfg.enabled || c.config != *cfg || c.trackers != key) {
            SLOG.info(format_args!("Chat-log or tracker settings changed, restarting monitoring"));
            self.chatlog = None;
            self.trackers = Trackers::default();
            for t in &mut self.thumbs {
                (t.metrics.has_dps_data, t.metrics.has_mining_data, t.metrics.has_bounty_data) = (false, false, false);
                t.needs_render = true;
            }
        }
        if !cfg.enabled || self.chatlog.is_some() || self.thumbs.is_empty() {
            return;
        }
        let pids: Vec<i32> = self.thumbs.iter().filter_map(|t| self.toplevels.iter().find(|tl| tl.id == t.window)).map(|tl| tl.pid).collect();
        let Some((chat, game, source)) = resolve_log_dirs(cfg, &pids) else {
            if !self.chatlog_warned {
                self.chatlog_warned = true;
                SLOG.warn(format_args!(
                    "Chat-log monitoring is on but EVE's logs weren't found ({} doesn't exist and no client's Wine prefix has them); set chatlog.chatlogDir/gamelogDir",
                    cfg.chatlog_dir
                ));
            }
            return;
        };
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            SLOG.err(format_args!("Couldn't create the chat-log wake pipe"));
            return;
        }
        let mut monitor = ChatlogMonitor::new(&chat, &game, cfg.idle_poll_threshold, cfg.max_poll_multiplier, self.global_settings.character_id_map_snapshot());
        let (trackers, feeds) = Trackers::new(&self.config, &self.global_settings);
        monitor.trackers = feeds;
        let wake_fd = fds[1];
        let wake = move || unsafe {
            libc::write(wake_fd, [1u8].as_ptr() as *const libc::c_void, 1);
        };
        match ChatlogWorker::spawn(monitor, Duration::from_millis(cfg.poll_interval_ms as u64), wake) {
            Ok(worker) => {
                SLOG.info(format_args!("Chat-log monitoring started ({source}): {} and {}", chat.display(), game.display()));
                self.chatlog = Some(Chatlog { worker: Some(worker), wake_read: fds[0], wake_write: fds[1], characters: Vec::new(), config: cfg.clone(), trackers: key });
                self.trackers = trackers;
                self.sync_chatlog_characters();
            }
            Err(e) => {
                SLOG.err(format_args!("Couldn't start the chat-log worker: {e}"));
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
            }
        }
    }

    /// Tells the worker about logins and logouts (any change in the set of logged-in character names).
    pub(super) fn sync_chatlog_characters(&mut self) {
        let Some(c) = self.chatlog.as_mut() else { return };
        let Some(worker) = c.worker.as_ref() else { return };
        let current: Vec<String> = self.thumbs.iter().map(|t| t.name.clone()).filter(|n| !is_generic_character_name(n)).collect();
        for gone in c.characters.iter().filter(|n| !current.contains(n)) {
            worker.send(WorkerCommand::RemoveCharacter(gone.clone()));
        }
        for new in current.iter().filter(|n| !c.characters.contains(n)) {
            worker.send(WorkerCommand::AddCharacter(new.clone()));
        }
        c.characters = current;
    }

    /// The worker has events waiting.
    pub(super) fn on_chatlog_ready(&mut self) {
        let Some(c) = self.chatlog.as_ref() else { return };
        let mut buf = [0u8; 64];
        while unsafe { libc::read(c.wake_read, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {}
        let events = c.worker.as_ref().map(|w| w.try_events()).unwrap_or_default();
        for event in events {
            match event {
                ChatlogEvent::System { character, system, event_ts, is_jump } => match self.thumbs.iter().position(|t| t.name == character) {
                    Some(i) => self.update_system_name(i, &system, event_ts, is_jump),
                    None => SLOG.debug(format_args!("No client for {character}, skipping system update")),
                },
                ChatlogEvent::Notification { character, text, ntype } => {
                    if let Some(i) = self.thumbs.iter().position(|t| t.name == character) {
                        self.show_notification(i, &text, ntype);
                    }
                }
                ChatlogEvent::CharacterId { character, id } => {
                    if let Err(e) = self.global_settings.update_character_id(&character, &id) {
                        SLOG.warn(format_args!("Couldn't cache character ID for {character}: {e}"));
                    }
                }
            }
        }
        self.render_dirty();
    }

    /// The Windows painter's update_system_name_by_hwnd.
    pub(super) fn update_system_name(&mut self, i: usize, system: &str, event_ts: u64, is_jump: bool) {
        let color = self.config.system_name_color(system);
        let t = &mut self.thumbs[i];
        if event_ts != 0 && event_ts < t.system_name_event_ts {
            SLOG.debug(format_args!("Ignoring stale system update for {}: {system}", t.name));
            return;
        }
        if t.system_name != system {
            t.system_name = system.to_owned();
            t.system_color = color;
            t.needs_render = true;
            SLOG.debug(format_args!("Updated system for {}: {system}", t.name));
        }
        t.system_name_event_ts = event_ts;
        if is_jump {
            t.last_jump_ms = now_ms();
            t.travel_alert_fired = false;
        }
    }

    /// The Windows painter's show_notification: per-type enable, focus/click suppression and throttling, then the
    /// thumbnail's stack, the notified-characters queue, sound and speech.
    pub(super) fn show_notification(&mut self, i: usize, text: &str, ntype: NotificationType) {
        let notifications = &self.config.thumbnail.notifications;
        if !notifications.enabled || self.config.is_notification_muted(&self.thumbs[i].name) {
            return;
        }
        let tc = notifications.type_config(ntype).clone();
        if !tc.enabled {
            return;
        }
        let focused = self.active == Some(self.thumbs[i].window);
        let now = now_ms();
        let t = &mut self.thumbs[i];
        if tc.suppress_when_focused && focused {
            return;
        }
        if tc.suppress_when_clicked && t.last_click_ms != 0 && now.saturating_sub(t.last_click_ms) < notifications.suppress_click_duration_ms as u64 {
            return;
        }
        let last = &mut t.last_notified_by_type[ntype.index()];
        if tc.throttle_ms > 0 && *last != 0 && now.saturating_sub(*last) < tc.throttle_ms as u64 {
            return;
        }
        *last = now;

        // Newest first; a repeat of the same type bumps to the top instead of stacking.
        t.notifications.retain(|n| n.notification_type != ntype);
        t.notifications.truncate(MAX_STACKED_NOTIFICATIONS - 1);
        t.notifications.insert(
            0,
            ActiveNotification {
                text: text.to_owned(),
                notification_type: ntype,
                start_ms: now,
                duration_ms: tc.duration_ms,
                suppress_when_focused: tc.suppress_when_focused,
                suppress_when_clicked: tc.suppress_when_clicked,
                border_color_override: tc.border_color,
                text_color_override: tc.text_color,
                show_border: tc.show_border,
                flash_border: tc.flash_border,
            },
        );
        t.needs_render = true;
        let name = t.name.clone();

        self.notified.retain(|(n, _)| *n != name);
        self.notified.push((name.clone(), now));
        // Disjoint from the config borrow above, so straight into the history rather than through a method.
        let (window, color) = (self.thumbs[i].window, self.thumbs[i].character_color);
        self.panels.history.push(window, &name, color, text, ntype, now);

        if tc.tts_enabled {
            let spoken = if notifications.tts_speak_character_name && !name.is_empty() {
                let shown = if notifications.tts_use_display_name { self.config.display_name(&name).to_owned() } else { name.clone() };
                format!("{shown}, {text}")
            } else {
                text.to_owned()
            };
            alerts::speak(&spoken, notifications.tts_volume, notifications.tts_rate);
        }
        if tc.sound_enabled {
            if let Some(path) = &tc.sound_path {
                alerts::play_sound(path, tc.sound_volume);
            }
        }
        SLOG.debug(format_args!("Notification for {name}: [{}] {text}", ntype.as_str()));
    }

    /// Travel Mode: once most of the group has sat in one system for travel.windowSeconds, flags whoever is elsewhere
    /// (the Windows painter's check_travel_left_behind).
    pub(super) fn check_travel_left_behind(&mut self) {
        let cfg = self.config.travel.clone();
        if !cfg.enabled {
            return;
        }
        let eligible: Vec<usize> = (0..self.thumbs.len()).filter(|&i| self.thumbs[i].last_jump_ms != 0 && !self.thumbs[i].excluded).collect();
        if eligible.len() < 2 {
            return;
        }
        let mut group: Option<(&str, usize, u64)> = None;
        for &c in &eligible {
            let system = self.thumbs[c].system_name.as_str();
            let members = eligible.iter().filter(|&&o| self.thumbs[o].system_name == system);
            let count = members.clone().count();
            let arrival = members.map(|&o| self.thumbs[o].last_jump_ms).max().unwrap_or(0);
            if group.is_none_or(|(_, n, _)| count > n) {
                group = Some((system, count, arrival));
            }
        }
        let Some((group_system, count, arrival)) = group else { return };
        let required = match cfg.threshold_mode {
            TravelThresholdMode::Percent => (cfg.threshold_percent / 100.0 * eligible.len() as f32).ceil() as usize,
            TravelThresholdMode::Count => cfg.threshold_count as usize,
        };
        if count < required || now_ms().saturating_sub(arrival) < cfg.window_seconds as u64 * 1000 {
            return;
        }
        let group_system = group_system.to_owned();
        let behind: Vec<usize> = eligible.into_iter().filter(|&i| self.thumbs[i].system_name != group_system && !self.thumbs[i].travel_alert_fired).collect();
        for i in behind {
            self.thumbs[i].travel_alert_fired = true;
            let text = format!("Left behind in {}", self.thumbs[i].system_name);
            self.show_notification(i, &text, NotificationType::TravelLeftBehind);
        }
    }
}
