//! Live-preview patches from the config dialog. These aren't used by the load/save path (which parses a whole profile); they merge partial JSON *fragments* into the running config, field by field, leaving anything absent or mistyped untouched.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::serde_helpers::{enum_from_str, parse_hex_color};
use super::*;
use crate::display_grid::{self, DisplayGrid};
use crate::types::{BorderStyle, ExclusionOverlayStyle, FontWeight, NotificationType, TextPosition};

type Obj = Map<String, Value>;

fn set_bool(obj: &Obj, key: &str, field: &mut bool) {
    if let Some(Value::Bool(b)) = obj.get(key) {
        *field = *b;
    }
}

/// Integers that don't fit the field's type leave it unchanged.
fn set_int<T: TryFrom<i64>>(obj: &Obj, key: &str, field: &mut T) {
    if let Some(v) = obj.get(key).and_then(Value::as_i64) {
        if let Ok(v) = T::try_from(v) {
            *field = v;
        }
    }
}

/// Like set_int, but JSON null clears the field.
fn set_opt_int<T: TryFrom<i64>>(obj: &Obj, key: &str, field: &mut Option<T>) {
    match obj.get(key) {
        Some(Value::Null) => *field = None,
        Some(v) => {
            if let Some(v) = v.as_i64().and_then(|v| T::try_from(v).ok()) {
                *field = Some(v);
            }
        }
        None => {}
    }
}

fn set_color(obj: &Obj, key: &str, field: &mut u32) -> Result<(), ConfigError> {
    if let Some(Value::String(s)) = obj.get(key) {
        *field = parse_hex_color(s)?;
    }
    Ok(())
}

/// A string naming an unknown variant resets the field to `fallback`.
fn set_enum_or<T: DeserializeOwned>(obj: &Obj, key: &str, field: &mut T, fallback: T) {
    if let Some(Value::String(s)) = obj.get(key) {
        *field = enum_from_str(s).unwrap_or(fallback);
    }
}

/// A string naming an unknown variant leaves the field unchanged.
fn set_enum_keep<T: DeserializeOwned>(obj: &Obj, key: &str, field: &mut T) {
    if let Some(Value::String(s)) = obj.get(key) {
        if let Some(v) = enum_from_str(s) {
            *field = v;
        }
    }
}

fn set_string(obj: &Obj, key: &str, field: &mut String) {
    if let Some(Value::String(s)) = obj.get(key) {
        if field != s {
            *field = s.clone();
        }
    }
}

fn set_opt_string(obj: &Obj, key: &str, field: &mut Option<String>) {
    match obj.get(key) {
        Some(Value::String(s)) => *field = Some(s.clone()),
        Some(Value::Null) => *field = None,
        _ => {}
    }
}

fn set_opt_color(obj: &Obj, key: &str, field: &mut Option<u32>) -> Result<(), ConfigError> {
    match obj.get(key) {
        Some(Value::String(s)) => *field = Some(parse_hex_color(s)?),
        Some(Value::Null) => *field = None,
        _ => {}
    }
    Ok(())
}

impl NotificationTypeConfig {
    /// Merges only the fields present in `obj`.
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_bool(obj, "enabled", &mut self.enabled);
        set_int(obj, "duration_ms", &mut self.duration_ms);
        set_bool(obj, "suppress_when_focused", &mut self.suppress_when_focused);
        set_bool(obj, "suppress_when_clicked", &mut self.suppress_when_clicked);
        set_int(obj, "throttle_ms", &mut self.throttle_ms);
        set_bool(obj, "tts_enabled", &mut self.tts_enabled);
        set_bool(obj, "sound_enabled", &mut self.sound_enabled);
        set_int(obj, "sound_volume", &mut self.sound_volume);
        set_opt_string(obj, "sound_path", &mut self.sound_path);
        set_bool(obj, "show_border", &mut self.show_border);
        set_bool(obj, "flash_border", &mut self.flash_border);
        set_opt_color(obj, "border_color", &mut self.border_color)?;
        set_opt_color(obj, "text_color", &mut self.text_color)?;
        Ok(())
    }
}

impl NotificationConfig {
    fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_bool(obj, "enabled", &mut self.enabled);
        set_enum_or(obj, "position", &mut self.position, TextPosition::Center);
        set_int(obj, "offset_x", &mut self.offset_x);
        set_int(obj, "offset_y", &mut self.offset_y);
        set_string(obj, "font_name", &mut self.font_name);
        set_int(obj, "font_size", &mut self.font_size);
        set_enum_or(obj, "font_weight", &mut self.font_weight, FontWeight::Regular);
        set_color(obj, "bg_color", &mut self.bg_color)?;
        set_int(obj, "suppress_click_duration_ms", &mut self.suppress_click_duration_ms);
        set_int(obj, "tts_volume", &mut self.tts_volume);
        set_int(obj, "tts_rate", &mut self.tts_rate);
        set_bool(obj, "tts_speak_character_name", &mut self.tts_speak_character_name);
        set_bool(obj, "tts_use_display_name", &mut self.tts_use_display_name);
        set_int(obj, "notified_cycle_retention_seconds", &mut self.notified_cycle_retention_seconds);
        if let Some(Value::Object(types)) = obj.get("type_configs") {
            for &t in NotificationType::ALL {
                if let Some(Value::Object(type_obj)) = types.get(t.as_str()) {
                    self.type_configs[t].apply_json(type_obj)?;
                }
            }
        }
        Ok(())
    }
}

impl ThumbnailConfig {
    /// Merge only the fields present in `obj`, leaving the rest untouched; used to apply an in-memory, unsaved thumbnail appearance patch from the config dialog's live preview (see PROTOCOL_PREVIEW_THUMBNAIL).
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_int(obj, "width", &mut self.width);
        set_int(obj, "height", &mut self.height);
        set_bool(obj, "showBorderWhenFocused", &mut self.show_border_when_focused);
        set_int(obj, "borderWidth", &mut self.border_width);
        set_color(obj, "borderColor", &mut self.border_color)?;
        set_enum_or(obj, "borderStyle", &mut self.border_style, BorderStyle::Solid);
        set_bool(obj, "showBorderWhenInactive", &mut self.show_border_when_inactive);
        set_int(obj, "inactiveBorderWidth", &mut self.inactive_border_width);
        set_color(obj, "inactiveBorderColor", &mut self.inactive_border_color)?;
        set_enum_or(obj, "inactiveBorderStyle", &mut self.inactive_border_style, BorderStyle::Solid);
        set_bool(obj, "showText", &mut self.show_text);
        set_bool(obj, "showCharacterName", &mut self.show_character_name);
        set_bool(obj, "showSystemName", &mut self.show_system_name);
        set_color(obj, "characterNameColor", &mut self.character_name_color)?;
        set_color(obj, "characterNameBgColor", &mut self.character_name_bg_color)?;
        set_bool(obj, "useUniqueCharacterNameColors", &mut self.use_unique_character_name_colors);
        set_bool(obj, "useUniqueCharacterBorderColors", &mut self.use_unique_character_border_colors);
        set_string(obj, "characterNameFontName", &mut self.character_name_font_name);
        set_int(obj, "characterNameFontSize", &mut self.character_name_font_size);
        set_enum_or(obj, "characterNameFontWeight", &mut self.character_name_font_weight, FontWeight::Regular);
        set_bool(obj, "useUniqueSystemColors", &mut self.use_unique_system_colors);
        set_color(obj, "systemNameColor", &mut self.system_name_color)?;
        set_color(obj, "systemNameBgColor", &mut self.system_name_bg_color)?;
        set_string(obj, "systemNameFontName", &mut self.system_name_font_name);
        set_int(obj, "systemNameFontSize", &mut self.system_name_font_size);
        set_enum_or(obj, "systemNameFontWeight", &mut self.system_name_font_weight, FontWeight::Regular);
        set_int(obj, "thumbnailOpacity", &mut self.thumbnail_opacity);
        set_bool(obj, "applyOpacityToOverlayTexts", &mut self.apply_opacity_to_overlay_texts);
        set_bool(obj, "activeThumbnailHidden", &mut self.active_thumbnail_hidden);
        set_bool(obj, "hideWhenNoEveFocus", &mut self.hide_when_no_eve_focus);
        set_int(obj, "hideDebounceMs", &mut self.hide_debounce_ms);
        set_enum_or(obj, "characterNamePosition", &mut self.character_name_position, TextPosition::TopLeft);
        set_enum_or(obj, "systemNamePosition", &mut self.system_name_position, TextPosition::BottomLeft);
        set_int(obj, "characterNameOffsetX", &mut self.character_name_offset_x);
        set_int(obj, "characterNameOffsetY", &mut self.character_name_offset_y);
        set_int(obj, "systemNameOffsetX", &mut self.system_name_offset_x);
        set_int(obj, "systemNameOffsetY", &mut self.system_name_offset_y);
        set_bool(obj, "showQuickGroupBadge", &mut self.show_quick_group_badge);
        set_color(obj, "quickGroupBadgeColor", &mut self.quick_group_badge_color)?;
        set_color(obj, "quickGroupBadgeBgColor", &mut self.quick_group_badge_bg_color)?;
        set_enum_or(obj, "quickGroupBadgePosition", &mut self.quick_group_badge_position, TextPosition::RightCenter);
        set_int(obj, "quickGroupBadgeOffsetX", &mut self.quick_group_badge_offset_x);
        set_int(obj, "quickGroupBadgeOffsetY", &mut self.quick_group_badge_offset_y);
        set_string(obj, "quickGroupBadgeFontName", &mut self.quick_group_badge_font_name);
        set_int(obj, "quickGroupBadgeFontSize", &mut self.quick_group_badge_font_size);
        set_enum_or(obj, "quickGroupBadgeFontWeight", &mut self.quick_group_badge_font_weight, FontWeight::Regular);
        set_enum_or(obj, "exclusionOverlayStyle", &mut self.exclusion_overlay_style, ExclusionOverlayStyle::X);
        set_color(obj, "exclusionOverlayColor", &mut self.exclusion_overlay_color)?;
        if let Some(Value::Object(notif)) = obj.get("notifications") {
            self.notifications.apply_json(notif)?;
        }
        Ok(())
    }
}

impl CombatConfig {
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_bool(obj, "enabled", &mut self.enabled);
        set_int(obj, "window_seconds", &mut self.window_seconds);
        set_bool(obj, "show_incoming", &mut self.show_incoming);
        set_bool(obj, "show_outgoing", &mut self.show_outgoing);
        set_color(obj, "incoming_color", &mut self.incoming_color)?;
        set_color(obj, "outgoing_color", &mut self.outgoing_color)?;
        set_color(obj, "incoming_bg_color", &mut self.incoming_bg_color)?;
        set_color(obj, "outgoing_bg_color", &mut self.outgoing_bg_color)?;
        set_int(obj, "incoming_font_size", &mut self.incoming_font_size);
        set_string(obj, "incoming_font_name", &mut self.incoming_font_name);
        set_enum_or(obj, "incoming_font_weight", &mut self.incoming_font_weight, FontWeight::Regular);
        set_int(obj, "outgoing_font_size", &mut self.outgoing_font_size);
        set_string(obj, "outgoing_font_name", &mut self.outgoing_font_name);
        set_enum_or(obj, "outgoing_font_weight", &mut self.outgoing_font_weight, FontWeight::Regular);
        set_int(obj, "update_interval_ms", &mut self.update_interval_ms);
        set_enum_or(obj, "incoming_position", &mut self.incoming_position, TextPosition::TopCenter);
        set_enum_or(obj, "outgoing_position", &mut self.outgoing_position, TextPosition::BottomCenter);
        set_int(obj, "incoming_offset_x", &mut self.incoming_offset_x);
        set_int(obj, "incoming_offset_y", &mut self.incoming_offset_y);
        set_int(obj, "outgoing_offset_x", &mut self.outgoing_offset_x);
        set_int(obj, "outgoing_offset_y", &mut self.outgoing_offset_y);
        set_bool(obj, "incoming_show_prefix", &mut self.incoming_show_prefix);
        set_bool(obj, "outgoing_show_prefix", &mut self.outgoing_show_prefix);
        set_string(obj, "damage_alert_excluded_weapons", &mut self.damage_alert_excluded_weapons);
        Ok(())
    }
}

impl MiningConfig {
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_bool(obj, "enabled", &mut self.enabled);
        set_int(obj, "window_seconds", &mut self.window_seconds);
        set_color(obj, "color", &mut self.color)?;
        set_color(obj, "bg_color", &mut self.bg_color)?;
        set_int(obj, "font_size", &mut self.font_size);
        set_string(obj, "font_name", &mut self.font_name);
        set_enum_or(obj, "font_weight", &mut self.font_weight, FontWeight::Regular);
        set_int(obj, "update_interval_ms", &mut self.update_interval_ms);
        set_enum_or(obj, "position", &mut self.position, TextPosition::BottomRight);
        set_int(obj, "offset_x", &mut self.offset_x);
        set_int(obj, "offset_y", &mut self.offset_y);
        set_int(obj, "idle_alert_window_seconds", &mut self.idle_alert_window_seconds);
        set_int(obj, "idle_alert_threshold", &mut self.idle_alert_threshold);
        set_int(obj, "stopped_alert_window_seconds", &mut self.stopped_alert_window_seconds);
        set_bool(obj, "show_isk_rate", &mut self.show_isk_rate);
        set_enum_or(obj, "isk_rate_unit", &mut self.isk_rate_unit, IskRateUnit::Hour);
        set_bool(obj, "show_prefix", &mut self.show_prefix);
        Ok(())
    }
}

impl BountyConfig {
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_bool(obj, "enabled", &mut self.enabled);
        set_int(obj, "window_seconds", &mut self.window_seconds);
        set_color(obj, "color", &mut self.color)?;
        set_color(obj, "bg_color", &mut self.bg_color)?;
        set_int(obj, "font_size", &mut self.font_size);
        set_string(obj, "font_name", &mut self.font_name);
        set_enum_or(obj, "font_weight", &mut self.font_weight, FontWeight::Regular);
        set_int(obj, "update_interval_ms", &mut self.update_interval_ms);
        set_enum_or(obj, "position", &mut self.position, TextPosition::TopRight);
        set_int(obj, "offset_x", &mut self.offset_x);
        set_int(obj, "offset_y", &mut self.offset_y);
        set_enum_or(obj, "isk_rate_unit", &mut self.isk_rate_unit, IskRateUnit::Hour);
        set_bool(obj, "show_prefix", &mut self.show_prefix);
        Ok(())
    }
}

impl ResourcesConfig {
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        set_bool(obj, "enabled", &mut self.enabled);
        set_bool(obj, "show_cpu", &mut self.show_cpu);
        set_bool(obj, "show_ram", &mut self.show_ram);
        set_bool(obj, "show_vram", &mut self.show_vram);
        set_color(obj, "color", &mut self.color)?;
        set_color(obj, "bg_color", &mut self.bg_color)?;
        set_int(obj, "font_size", &mut self.font_size);
        set_string(obj, "font_name", &mut self.font_name);
        set_enum_or(obj, "font_weight", &mut self.font_weight, FontWeight::Regular);
        set_int(obj, "update_interval_ms", &mut self.update_interval_ms);
        set_enum_or(obj, "position", &mut self.position, TextPosition::LeftCenter);
        set_int(obj, "offset_x", &mut self.offset_x);
        set_int(obj, "offset_y", &mut self.offset_y);
        Ok(())
    }
}

impl DisplayConfig {
    /// start_x/start_y are patchable here, but reload_thumbnail_config_from_disk never reverts them - they can be live-dragged in the running app.
    pub fn apply_json(&mut self, obj: &Obj) -> Result<(), ConfigError> {
        // Parsed fully before the old grid is replaced, so a bad patch leaves the current layout intact.
        if let Some(v @ Value::Object(_)) = obj.get("displayGrid") {
            self.display_grid = DisplayGrid::from_json_value(v)?;
        }
        if let Some(v @ Value::Array(_)) = obj.get("displayLayouts") {
            self.display_layouts = display_grid::layouts_from_json_value(v)?;
        }
        set_int(obj, "startX", &mut self.start_x);
        set_int(obj, "startY", &mut self.start_y);
        set_int(obj, "spacing", &mut self.spacing);
        set_int(obj, "newThumbnailSpacing", &mut self.new_thumbnail_spacing);
        set_enum_keep(obj, "layoutMode", &mut self.layout_mode);
        set_enum_keep(obj, "regionFitDirection", &mut self.region_fit_direction);
        set_opt_int(obj, "regionX", &mut self.region_x);
        set_opt_int(obj, "regionY", &mut self.region_y);
        set_opt_int(obj, "regionWidth", &mut self.region_width);
        set_opt_int(obj, "regionHeight", &mut self.region_height);
        set_enum_keep(obj, "regionFitOrder", &mut self.region_fit_order);
        set_bool(obj, "regionFitReorderLoggedOut", &mut self.region_fit_reorder_logged_out);
        set_bool(obj, "hideThumbnailsDuringRegionSelect", &mut self.hide_thumbnails_during_region_select);
        set_bool(obj, "regionFitLimitToThumbnailSize", &mut self.region_fit_limit_to_thumbnail_size);
        set_bool(obj, "notLoggedInSpaceEnabled", &mut self.not_logged_in_space_enabled);
        set_opt_int(obj, "notLoggedInSpaceX", &mut self.not_logged_in_space_x);
        set_opt_int(obj, "notLoggedInSpaceY", &mut self.not_logged_in_space_y);
        set_opt_int(obj, "notLoggedInSpaceWidth", &mut self.not_logged_in_space_width);
        set_opt_int(obj, "notLoggedInSpaceHeight", &mut self.not_logged_in_space_height);
        set_int(obj, "notLoggedInSpaceSpacing", &mut self.not_logged_in_space_spacing);
        set_bool(obj, "notLoggedInSpaceLimitToThumbnailSize", &mut self.not_logged_in_space_limit_to_thumbnail_size);
        set_bool(obj, "notLoggedInSpaceHideThumbnailsDuringRegionSelect", &mut self.not_logged_in_space_hide_thumbnails_during_region_select);
        set_opt_int(obj, "monitorIndex", &mut self.monitor_index);
        set_bool(obj, "useMonitorWorkArea", &mut self.use_monitor_work_area);
        set_bool(obj, "honorSavedPositions", &mut self.honor_saved_positions);
        set_enum_keep(obj, "viewMode", &mut self.view_mode);
        set_enum_keep(obj, "listViewOrder", &mut self.list_view_order);
        set_bool(obj, "rememberListViewPosition", &mut self.remember_list_view_position);
        set_int(obj, "listViewOpacity", &mut self.list_view_opacity);
        set_int(obj, "listViewColumns", &mut self.list_view_columns);
        set_string(obj, "listViewFontName", &mut self.list_view_font_name);
        set_int(obj, "listViewFontSize", &mut self.list_view_font_size);
        set_enum_or(obj, "listViewFontWeight", &mut self.list_view_font_weight, FontWeight::Regular);
        set_bool(obj, "showNotifInfoPanel", &mut self.show_notif_info_panel);
        set_bool(obj, "rememberNotifInfoPanelPosition", &mut self.remember_notif_info_panel_position);
        set_bool(obj, "hideNotifInfoPanelWhenNoCharacters", &mut self.hide_notif_info_panel_when_no_characters);
        set_int(obj, "notifInfoPanelWidth", &mut self.notif_info_panel_width);
        set_int(obj, "notifInfoPanelHeight", &mut self.notif_info_panel_height);
        set_int(obj, "notifInfoPanelOpacity", &mut self.notif_info_panel_opacity);
        set_string(obj, "notifInfoPanelFontName", &mut self.notif_info_panel_font_name);
        set_int(obj, "notifInfoPanelFontSize", &mut self.notif_info_panel_font_size);
        set_enum_or(obj, "notifInfoPanelFontWeight", &mut self.notif_info_panel_font_weight, FontWeight::Regular);
        set_int(obj, "notifInfoPanelMaxRows", &mut self.notif_info_panel_max_rows);
        set_bool(obj, "notifInfoPanelShowTimestamp", &mut self.notif_info_panel_show_timestamp);
        set_bool(obj, "notifInfoPanelShowCategoryFilters", &mut self.notif_info_panel_show_category_filters);
        set_bool(obj, "notifInfoPanelShowFleet", &mut self.notif_info_panel_show_fleet);
        set_bool(obj, "notifInfoPanelShowMining", &mut self.notif_info_panel_show_mining);
        set_bool(obj, "notifInfoPanelShowCombat", &mut self.notif_info_panel_show_combat);
        set_bool(obj, "notifInfoPanelShowNavigation", &mut self.notif_info_panel_show_navigation);
        set_bool(obj, "notifInfoPanelShowGeneral", &mut self.notif_info_panel_show_general);
        Ok(())
    }
}

pub fn parse_json_system_color(obj: &Obj) -> Result<SystemColor, ConfigError> {
    let name = obj.get("systemName").ok_or(ConfigError::MissingSystemName)?;
    let color = obj.get("color").ok_or(ConfigError::MissingSystemColor)?;
    let (Value::String(name), Value::String(color)) = (name, color) else {
        return Err(ConfigError::InvalidSystemColor);
    };
    Ok(SystemColor { name: name.clone(), color: parse_hex_color(color)? })
}

impl Config {
    /// Live-preview only: per-group badge flags in the config dialog's group order, matched to the running groups by index.
    /// A group added or removed in the dialog shifts every index after it, so a length mismatch skips the preview until Save reloads the profile.
    pub fn apply_group_badge_preview_from_json(&mut self, flags: &[Value]) {
        if flags.len() != self.hotkey_groups.len() {
            return;
        }
        for (group, flag) in self.hotkey_groups.iter_mut().zip(flags) {
            if let Value::Bool(b) = flag {
                group.show_badge = *b;
            }
        }
    }

    /// Replace the system color override list from a JSON array of {systemName, color} objects, keeping the old list if any entry fails to parse.
    pub fn replace_system_colors_from_json(&mut self, colors: &[Value]) -> Result<(), ConfigError> {
        let new_list = colors
            .iter()
            .filter_map(Value::as_object)
            .map(parse_json_system_color)
            .collect::<Result<Vec<_>, _>>()?;
        self.system_colors = new_list;
        Ok(())
    }

    /// Apply per-character border-color/name-color/thumbnail-size/display-name/hidden/position overrides from a JSON array, matched by name; creates missing characters so an unsaved "Populate from Open Clients" character still gets a live entry to preview against, which reload_thumbnail_config_from_disk() removes again on revert if it never got saved.
    pub fn apply_character_overrides_from_json(&mut self, overrides: &[Value]) -> Result<(), ConfigError> {
        for item in overrides {
            let Some(obj) = item.as_object() else { continue };
            let Some(Value::String(name)) = obj.get("name") else { continue };
            let c = self.get_or_create_character(name);

            if let Some(v) = obj.get("displayName") {
                c.display_name = match v {
                    Value::String(s) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                };
            }

            if let Some(v) = obj.get("thumbnailSize") {
                c.thumbnail_size = match v {
                    Value::Object(size_obj) => {
                        let as_i32 = |key| size_obj.get(key).and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok());
                        let mut size = CharacterThumbnailSize { width: as_i32("width"), height: as_i32("height") };
                        clamp_character_thumbnail_size(&mut size);
                        (size.width.is_some() || size.height.is_some()).then_some(size)
                    }
                    _ => None,
                };
            }

            if let Some(v) = obj.get("borderColors") {
                c.border_colors = match v {
                    Value::Object(colors_obj) => {
                        let mut colors = CharacterBorderColors::default();
                        if let Some(Value::String(s)) = colors_obj.get("activeBorderColor") {
                            colors.active_border_color = Some(parse_hex_color(s)?);
                        }
                        if let Some(Value::String(s)) = colors_obj.get("inactiveBorderColor") {
                            colors.inactive_border_color = Some(parse_hex_color(s)?);
                        }
                        (colors.active_border_color.is_some() || colors.inactive_border_color.is_some()).then_some(colors)
                    }
                    _ => None,
                };
            }

            if let Some(v) = obj.get("nameColor") {
                c.name_color = match v {
                    Value::String(s) => Some(parse_hex_color(s)?),
                    _ => None,
                };
            }

            set_bool(obj, "hideThumbnail", &mut c.hide_thumbnail);

            // An integer out of u8 range leaves the override alone; anything else (null, a float, a string) clears it.
            if let Some(v) = obj.get("opacity") {
                match v.as_i64() {
                    Some(o) => {
                        if let Ok(mut opacity) = u8::try_from(o) {
                            clamp_character_opacity(&mut opacity);
                            c.opacity = Some(opacity);
                        }
                    }
                    None => c.opacity = None,
                }
            }

            // Only sent by the post-import preview (see buildCharacterOverridesPreviewPatch in config_dialog.js), never the general per-edit debounce.
            if let Some(Value::Object(pos)) = obj.get("position") {
                let as_i32 = |key| pos.get(key).and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok());
                if let (Some(x), Some(y)) = (as_i32("x"), as_i32("y")) {
                    c.position = Some(Position { x, y });
                }
            }
        }
        Ok(())
    }

    /// Discard the in-memory thumbnail appearance, layout, and system color overrides, replacing them with a fresh read of this profile from disk, to revert an unsaved live-preview patch (see PROTOCOL_REVERT_PREVIEW); start_x/start_y are left untouched.
    pub fn reload_thumbnail_config_from_disk(&mut self) -> Result<(), ConfigError> {
        let mut fresh = Config::load_profile(&self.profile_name)?;

        // Index-matched, like apply_group_badge_preview_from_json.
        for (i, group) in self.hotkey_groups.iter_mut().enumerate() {
            group.show_badge = fresh.hotkey_groups.get(i).is_some_and(|g| g.show_badge);
        }

        // Restore per-character overrides by matching on name; characters with no match in `fresh` (created live but never saved) are removed entirely so reverting leaves no residue.
        self.characters.retain_mut(|c| match fresh.find_character(&c.name) {
            Some(f) => {
                c.thumbnail_size = f.thumbnail_size;
                c.border_colors = f.border_colors;
                c.name_color = f.name_color;
                c.hide_thumbnail = f.hide_thumbnail;
                c.position = f.position;
                c.display_name = f.display_name.clone();
                true
            }
            None => false,
        });

        // Combat/Mining/Bounty/Resources overlay previews ride along in the same live-preview patch (see PROTOCOL_PREVIEW_THUMBNAIL).
        self.thumbnail = std::mem::take(&mut fresh.thumbnail);
        self.combat = std::mem::take(&mut fresh.combat);
        self.mining = std::mem::take(&mut fresh.mining);
        self.bounty = std::mem::take(&mut fresh.bounty);
        self.resources = std::mem::take(&mut fresh.resources);
        self.system_colors = std::mem::take(&mut fresh.system_colors);

        let (d, f) = (&mut self.display, &mut fresh.display);
        // List View and History Panel appearance ride along too.
        d.list_view_opacity = f.list_view_opacity;
        d.list_view_font_name = std::mem::take(&mut f.list_view_font_name);
        d.list_view_font_size = f.list_view_font_size;
        d.list_view_font_weight = f.list_view_font_weight;
        d.notif_info_panel_opacity = f.notif_info_panel_opacity;
        d.notif_info_panel_font_name = std::mem::take(&mut f.notif_info_panel_font_name);
        d.notif_info_panel_font_size = f.notif_info_panel_font_size;
        d.notif_info_panel_font_weight = f.notif_info_panel_font_weight;
        d.notif_info_panel_max_rows = f.notif_info_panel_max_rows;
        d.notif_info_panel_show_timestamp = f.notif_info_panel_show_timestamp;
        d.notif_info_panel_show_category_filters = f.notif_info_panel_show_category_filters;
        // RegionFit fields ride along too, but never start_x/start_y - those can be live-dragged in the running app.
        d.spacing = f.spacing;
        d.layout_mode = f.layout_mode;
        d.region_fit_direction = f.region_fit_direction;
        d.region_fit_limit_to_thumbnail_size = f.region_fit_limit_to_thumbnail_size;
        d.new_thumbnail_spacing = f.new_thumbnail_spacing;
        d.monitor_index = f.monitor_index;
        d.use_monitor_work_area = f.use_monitor_work_area;
        d.honor_saved_positions = f.honor_saved_positions;
        d.region_x = f.region_x;
        d.region_y = f.region_y;
        d.region_width = f.region_width;
        d.region_height = f.region_height;
        d.not_logged_in_space_enabled = f.not_logged_in_space_enabled;
        d.not_logged_in_space_x = f.not_logged_in_space_x;
        d.not_logged_in_space_y = f.not_logged_in_space_y;
        d.not_logged_in_space_width = f.not_logged_in_space_width;
        d.not_logged_in_space_height = f.not_logged_in_space_height;
        d.not_logged_in_space_spacing = f.not_logged_in_space_spacing;
        d.not_logged_in_space_limit_to_thumbnail_size = f.not_logged_in_space_limit_to_thumbnail_size;
        d.display_grid = std::mem::take(&mut f.display_grid);
        d.display_layouts = std::mem::take(&mut f.display_layouts);
        Ok(())
    }
}
