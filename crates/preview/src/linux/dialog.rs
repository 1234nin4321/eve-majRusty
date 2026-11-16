//! Commands from the settings dialog (eve-maj-config), the Linux side of the Windows build's WM_COPYDATA handling in
//! app.rs: profile switches, live thumbnail previews and their revert, test notifications, and suspending hotkeys while
//! the dialog records a key.

use eve_maj_core::config::NotificationTypeConfig;
use eve_maj_core::log::Scope;
use eve_maj_core::protocol::{safe_profile_name, Command};
use eve_maj_core::render::{now_ms, ActiveNotification, MAX_STACKED_NOTIFICATIONS};
use eve_maj_core::types::NotificationType;
use serde_json::Value;

use super::{alerts, App};

const SLOG: Scope = Scope::new("dialog");

impl App {
    pub(super) fn on_dialog_command(&mut self, cmd: Command) {
        match cmd {
            Command::Profile(name) => match safe_profile_name(&name) {
                Some(n) => {
                    let n = n.to_owned();
                    self.switch_profile(&n);
                }
                None => SLOG.warn(format_args!("Rejected unsafe profile name from the dialog")),
            },
            Command::PreviewThumbnail(json) => {
                if let Err(e) = self.apply_thumbnail_preview(&json) {
                    SLOG.err(format_args!("Failed to apply thumbnail preview: {e}"));
                }
            }
            Command::RevertPreview => {
                if let Err(e) = self.config.reload_thumbnail_config_from_disk() {
                    SLOG.err(format_args!("Failed to revert thumbnail preview: {e}"));
                    return;
                }
                self.refresh_all_visuals(true);
            }
            Command::TestNotification(json) => {
                if let Err(e) = self.show_test_notification(&json) {
                    SLOG.err(format_args!("Failed to show test notification: {e}"));
                }
            }
            Command::DialogSuspendHotkeys => {
                self.dialog_hotkeys_suspended = true;
                self.register_hotkeys();
            }
            Command::DialogResumeHotkeys => {
                if self.dialog_hotkeys_suspended {
                    self.dialog_hotkeys_suspended = false;
                    self.register_hotkeys();
                }
            }
            Command::Switch(name) => self.on_protocol_url(&format!("evemajpreview://switch/{}", String::from_utf8_lossy(&name))),
            Command::Hotkey(_) | Command::StartRegionSelect(_) => SLOG.debug(format_args!("Dialog command not supported on Linux yet")),
        }
        self.render_dirty();
        self.refresh_tray();
    }

    /// Re-derives every thumbnail's cached look from the config, and its placement when the layout may have changed.
    fn refresh_all_visuals(&mut self, layout_changed: bool) {
        for i in 0..self.thumbs.len() {
            self.refresh_character_caches(i);
            self.set_click_through(i);
        }
        if layout_changed {
            self.reflow();
        }
    }

    /// Merges a live-preview patch (changed fields only) into the running config without saving it; the Windows
    /// build's apply_thumbnail_preview.
    fn apply_thumbnail_preview(&mut self, json: &str) -> Result<(), String> {
        let value: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let obj = value.as_object().ok_or("preview patch isn't a JSON object")?;
        let config = &mut self.config;
        config.thumbnail.apply_json(obj).map_err(|e| e.to_string())?;
        config.thumbnail.validate();
        if let Some(Value::Array(colors)) = obj.get("systemColors") {
            if let Err(e) = config.replace_system_colors_from_json(colors) {
                SLOG.err(format_args!("Failed to apply system color overrides preview: {e}"));
            }
        }
        let mut layout_changed = false;
        if let Some(Value::Object(display)) = obj.get("display") {
            if let Err(e) = config.display.apply_json(display) {
                SLOG.err(format_args!("Failed to apply display preview: {e}"));
            }
            config.display.validate();
            layout_changed = true;
        }
        if let Some(Value::Array(badges)) = obj.get("hotkeyGroupBadges") {
            config.apply_group_badge_preview_from_json(badges);
        }
        if let Some(Value::Array(overrides)) = obj.get("characterOverrides") {
            if let Err(e) = config.apply_character_overrides_from_json(overrides) {
                SLOG.err(format_args!("Failed to apply character overrides preview: {e}"));
            }
            layout_changed = true;
        }
        macro_rules! section {
            ($key:literal, $field:ident) => {
                if let Some(Value::Object(o)) = obj.get($key) {
                    if let Err(e) = config.$field.apply_json(o) {
                        SLOG.err(format_args!("Failed to apply {} overlay preview: {e}", $key));
                    }
                    config.$field.validate();
                }
            };
        }
        section!("combat", combat);
        section!("mining", mining);
        section!("bounty", bounty);
        section!("resources", resources);
        self.refresh_all_visuals(layout_changed);
        Ok(())
    }

    /// One notification of the requested type on every thumbnail, from the dialog's unsaved per-type settings; it
    /// bypasses the usual suppression, and its sound/speech plays once.
    fn show_test_notification(&mut self, json: &str) -> Result<(), String> {
        let value: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let type_name = value["type"].as_str().ok_or("missing type")?;
        let text = value["text"].as_str().ok_or("missing text")?.to_owned();
        let config_obj = value["config"].as_object().ok_or("missing config")?;
        let ntype = NotificationType::from_name(type_name).ok_or_else(|| format!("unknown notification type {type_name}"))?;
        let mut tc = NotificationTypeConfig::default();
        tc.apply_json(config_obj).map_err(|e| e.to_string())?;
        // A permanent (0) duration would never clear a test.
        let duration_ms = if tc.duration_ms == 0 { 10_000 } else { tc.duration_ms };
        let now = now_ms();
        for t in &mut self.thumbs {
            t.notifications.retain(|n| n.notification_type != ntype);
            t.notifications.truncate(MAX_STACKED_NOTIFICATIONS - 1);
            t.notifications.insert(
                0,
                ActiveNotification {
                    text: text.clone(),
                    notification_type: ntype,
                    start_ms: now,
                    duration_ms,
                    suppress_when_focused: tc.suppress_when_focused,
                    suppress_when_clicked: tc.suppress_when_clicked,
                    border_color_override: tc.border_color,
                    text_color_override: tc.text_color,
                    show_border: tc.show_border,
                    flash_border: tc.flash_border,
                },
            );
            t.needs_render = true;
        }
        let n = &self.config.thumbnail.notifications;
        if tc.tts_enabled {
            alerts::speak(&text, n.tts_volume, n.tts_rate);
        }
        if tc.sound_enabled {
            if let Some(path) = &tc.sound_path {
                alerts::play_sound(path, tc.sound_volume);
            }
        }
        Ok(())
    }
}
