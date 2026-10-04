//! Thumbnail appearance, notifications, and the display/layout section.

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::serde_helpers::{argb, argb_opt};
use super::{SLOG, DEFAULT_ACCENT_COLOR, DEFAULT_FONT_NAME};
use crate::display_grid::{DisplayGrid, DisplayLayout};
use crate::state::ThumbnailState;
use crate::types::{
    BorderStyle, ExclusionOverlayStyle, FontWeight, LayoutMode, ListViewOrder, NotificationType, RegionFitDirection,
    RegionFitOrder, TextPosition, ViewMode,
};

fn default_font() -> String {
    DEFAULT_FONT_NAME.to_owned()
}

fn clamp_in<T: PartialOrd + Copy>(v: &mut T, lo: T, hi: T) {
    if *v < lo {
        *v = lo;
    }
    if *v > hi {
        *v = hi;
    }
}

/// Per-state style overrides; None falls back to the thumbnail-wide setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StateVisualConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_width: Option<u8>,
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub border_color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_style: Option<BorderStyle>,
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub text_color: Option<u32>,
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub text_bg_color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_border: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_thumbnail: Option<bool>,
}

impl StateVisualConfig {
    const fn showing_thumbnail() -> Self {
        Self {
            border_width: None,
            border_color: None,
            border_style: None,
            text_color: None,
            text_bg_color: None,
            show_border: None,
            show_thumbnail: Some(true),
        }
    }

    pub fn border_width_or(&self, default: u8) -> u8 {
        self.border_width.unwrap_or(default)
    }
    pub fn border_color_or(&self, default: u32) -> u32 {
        self.border_color.unwrap_or(default)
    }
    pub fn border_style_or(&self, default: BorderStyle) -> BorderStyle {
        self.border_style.unwrap_or(default)
    }
    pub fn text_color_or(&self, default: u32) -> u32 {
        self.text_color.unwrap_or(default)
    }
    pub fn text_bg_color_or(&self, default: u32) -> u32 {
        self.text_bg_color.unwrap_or(default)
    }
    pub fn show_border_or(&self, default: bool) -> bool {
        self.show_border.unwrap_or(default)
    }
    pub fn show_thumbnail_or(&self, default: bool) -> bool {
        self.show_thumbnail.unwrap_or(default)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationTypeConfig {
    pub enabled: bool,
    pub duration_ms: u32,
    pub suppress_when_focused: bool,
    pub suppress_when_clicked: bool,
    /// 0 = no throttling; otherwise repeats of this type are dropped until this many ms have passed since the last one actually shown (per thumbnail).
    pub throttle_ms: u32,
    /// None = fall back to the Alert state's borderColor (or inactive border).
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub border_color: Option<u32>,
    /// None = fall back to the thumbnail's normal textColor.
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub text_color: Option<u32>,
    /// false suppresses the border entirely, overriding the Alert state's showBorder and any border_color above.
    pub show_border: bool,
    /// Flashes on/off a few times on start, then settles into an always-on border; no effect when show_border is false.
    pub flash_border: bool,
    /// Self-contained - there is no global TTS master switch.
    pub tts_enabled: bool,
    /// Self-contained too - there is no global sound master switch either.
    pub sound_enabled: bool,
    /// Absolute path to a .wav/.mp3 file; may be set while sound_enabled is false so the picked file isn't lost by unchecking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_path: Option<String>,
    pub sound_volume: u8,
}

impl Default for NotificationTypeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: 10000,
            suppress_when_focused: false,
            suppress_when_clicked: false,
            throttle_ms: 10000,
            border_color: None,
            text_color: None,
            show_border: false,
            flash_border: false,
            tts_enabled: false,
            sound_enabled: false,
            sound_path: None,
            sound_volume: 100,
        }
    }
}

/// One NotificationTypeConfig per NotificationType, serialized as an object keyed by type name in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeConfigs(Vec<NotificationTypeConfig>);

impl Default for TypeConfigs {
    fn default() -> Self {
        Self(vec![NotificationTypeConfig::default(); NotificationType::COUNT])
    }
}

impl std::ops::Index<NotificationType> for TypeConfigs {
    type Output = NotificationTypeConfig;
    fn index(&self, t: NotificationType) -> &Self::Output {
        &self.0[t.index()]
    }
}

impl std::ops::IndexMut<NotificationType> for TypeConfigs {
    fn index_mut(&mut self, t: NotificationType) -> &mut Self::Output {
        &mut self.0[t.index()]
    }
}

impl TypeConfigs {
    pub fn iter(&self) -> impl Iterator<Item = (NotificationType, &NotificationTypeConfig)> {
        NotificationType::ALL.iter().copied().zip(self.0.iter())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (NotificationType, &mut NotificationTypeConfig)> {
        NotificationType::ALL.iter().copied().zip(self.0.iter_mut())
    }
}

impl Serialize for TypeConfigs {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(NotificationType::COUNT))?;
        for (t, cfg) in self.iter() {
            map.serialize_entry(t.as_str(), cfg)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TypeConfigs {
    /// Unknown type names are skipped and missing ones keep their defaults; a non-object reads as all defaults, as in the Zig build.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut result = TypeConfigs::default();
        let serde_json::Value::Object(obj) = serde_json::Value::deserialize(d)? else {
            return Ok(result);
        };
        for (key, value) in obj {
            let Some(t) = NotificationType::from_name(&key) else { continue };
            result[t] = NotificationTypeConfig::deserialize(value).map_err(D::Error::custom)?;
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub position: TextPosition,
    pub offset_x: i32,
    pub offset_y: i32,
    pub font_name: String,
    pub font_size: i32,
    pub font_weight: FontWeight,
    #[serde(with = "argb")]
    pub bg_color: u32,
    pub suppress_click_duration_ms: u32,
    pub tts_volume: u8,
    pub tts_rate: i8,
    pub tts_speak_character_name: bool,
    /// Only consulted when tts_speak_character_name is true; falls back to the character name if no Custom Display Name is set (see Config::display_name).
    pub tts_use_display_name: bool,
    /// Seconds a character stays eligible in the "cycle to recently notified character" queue after their last notification before aging out; re-notifying resets this window.
    pub notified_cycle_retention_seconds: u32,
    pub type_configs: TypeConfigs,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            position: TextPosition::Center,
            offset_x: 0,
            offset_y: 0,
            font_name: default_font(),
            font_size: 12,
            font_weight: FontWeight::Regular,
            bg_color: 0xE6000000,
            suppress_click_duration_ms: 2000,
            tts_volume: 100,
            tts_rate: 0,
            tts_speak_character_name: true,
            tts_use_display_name: false,
            notified_cycle_retention_seconds: 30,
            type_configs: TypeConfigs::default(),
        }
    }
}

impl NotificationConfig {
    pub fn type_config(&self, t: NotificationType) -> &NotificationTypeConfig {
        &self.type_configs[t]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThumbnailConfig {
    pub width: i32,
    pub height: i32,
    pub show_border_when_focused: bool,
    pub border_width: u8,
    #[serde(with = "argb")]
    pub border_color: u32,
    pub border_style: BorderStyle,
    pub show_border_when_inactive: bool,
    pub inactive_border_width: u8,
    #[serde(with = "argb")]
    pub inactive_border_color: u32,
    pub inactive_border_style: BorderStyle,
    pub show_text: bool,
    pub show_character_name: bool,
    pub show_system_name: bool,
    #[serde(with = "argb")]
    pub character_name_color: u32,
    #[serde(with = "argb")]
    pub character_name_bg_color: u32,
    pub use_unique_character_name_colors: bool,
    pub use_unique_character_border_colors: bool,
    pub character_name_font_name: String,
    pub character_name_font_size: i32,
    pub character_name_font_weight: FontWeight,
    pub use_unique_system_colors: bool,
    #[serde(with = "argb")]
    pub system_name_color: u32,
    #[serde(with = "argb")]
    pub system_name_bg_color: u32,
    pub character_name_position: TextPosition,
    pub character_name_offset_x: i32,
    pub character_name_offset_y: i32,
    pub system_name_position: TextPosition,
    pub system_name_offset_x: i32,
    pub system_name_offset_y: i32,
    pub system_name_font_name: String,
    pub system_name_font_size: i32,
    pub system_name_font_weight: FontWeight,
    pub show_quick_group_badge: bool,
    #[serde(with = "argb")]
    pub quick_group_badge_color: u32,
    #[serde(with = "argb")]
    pub quick_group_badge_bg_color: u32,
    pub quick_group_badge_position: TextPosition,
    pub quick_group_badge_offset_x: i32,
    pub quick_group_badge_offset_y: i32,
    pub quick_group_badge_font_name: String,
    pub quick_group_badge_font_size: i32,
    pub quick_group_badge_font_weight: FontWeight,
    pub exclusion_overlay_style: ExclusionOverlayStyle,
    #[serde(with = "argb")]
    pub exclusion_overlay_color: u32,
    pub notifications: NotificationConfig,
    pub thumbnail_opacity: u8,
    pub apply_opacity_to_overlay_texts: bool,
    pub active_thumbnail_hidden: bool,
    pub hide_when_no_eve_focus: bool,
    pub hide_debounce_ms: u32,
    /// show_thumbnail defaults to None on `active` so it means "use active_thumbnail_hidden" instead of a fixed true/false.
    pub active: StateVisualConfig,
    pub inactive: StateVisualConfig,
    pub alert: StateVisualConfig,
    pub minimized: StateVisualConfig,
    pub dragging: StateVisualConfig,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            width: 200,
            height: 112,
            show_border_when_focused: true,
            border_width: 2,
            border_color: DEFAULT_ACCENT_COLOR,
            border_style: BorderStyle::Solid,
            show_border_when_inactive: false,
            inactive_border_width: 2,
            inactive_border_color: 0xFF606060,
            inactive_border_style: BorderStyle::Solid,
            show_text: true,
            show_character_name: true,
            show_system_name: false,
            character_name_color: 0xFFFFFF,
            character_name_bg_color: 0xE6000000,
            use_unique_character_name_colors: false,
            use_unique_character_border_colors: false,
            character_name_font_name: default_font(),
            character_name_font_size: 12,
            character_name_font_weight: FontWeight::Regular,
            use_unique_system_colors: false,
            system_name_color: 0xFFFFFF,
            system_name_bg_color: 0xE6000000,
            character_name_position: TextPosition::TopLeft,
            character_name_offset_x: 0,
            character_name_offset_y: 0,
            system_name_position: TextPosition::BottomLeft,
            system_name_offset_x: 0,
            system_name_offset_y: 0,
            system_name_font_name: default_font(),
            system_name_font_size: 12,
            system_name_font_weight: FontWeight::Regular,
            show_quick_group_badge: false,
            quick_group_badge_color: 0xFF44FF44,
            quick_group_badge_bg_color: 0xE6000000,
            quick_group_badge_position: TextPosition::RightCenter,
            quick_group_badge_offset_x: 0,
            quick_group_badge_offset_y: 0,
            quick_group_badge_font_name: default_font(),
            quick_group_badge_font_size: 12,
            quick_group_badge_font_weight: FontWeight::Regular,
            exclusion_overlay_style: ExclusionOverlayStyle::X,
            exclusion_overlay_color: 0x33A62222,
            notifications: NotificationConfig::default(),
            thumbnail_opacity: 255,
            apply_opacity_to_overlay_texts: false,
            active_thumbnail_hidden: false,
            hide_when_no_eve_focus: false,
            hide_debounce_ms: 500,
            active: StateVisualConfig::default(),
            inactive: StateVisualConfig::showing_thumbnail(),
            alert: StateVisualConfig::showing_thumbnail(),
            minimized: StateVisualConfig::showing_thumbnail(),
            dragging: StateVisualConfig::showing_thumbnail(),
        }
    }
}

impl ThumbnailConfig {
    // Also read by Config::build_validation_ranges_json() so the config dialog's inputs share these exact limits instead of a copy that can drift.
    pub const WIDTH_MIN: i32 = 50;
    pub const WIDTH_MAX: i32 = 3840;
    pub const HEIGHT_MIN: i32 = 50;
    pub const HEIGHT_MAX: i32 = 2160;
    pub const BORDER_WIDTH_MIN: u8 = 1;
    pub const BORDER_WIDTH_MAX: u8 = 50;
    pub const FONT_SIZE_MIN: i32 = 6;
    pub const FONT_SIZE_MAX: i32 = 72;
    /// 20% of 255, the floor below which thumbnails/overlays effectively vanish.
    pub const OPACITY_MIN: u8 = 51;
    pub const OFFSET_MIN: i32 = -500;
    pub const OFFSET_MAX: i32 = 500;
    pub const TTS_VOLUME_MAX: u8 = 100;
    pub const SOUND_VOLUME_MAX: u8 = 100;
    /// SAPI native range.
    pub const TTS_RATE_MIN: i8 = -10;
    pub const TTS_RATE_MAX: i8 = 10;
    pub const CYCLE_RETENTION_MIN: u32 = 5;
    pub const CYCLE_RETENTION_MAX: u32 = 600;
    pub const HIDE_DEBOUNCE_MS_MAX: u32 = 5000;
    pub const SUPPRESS_CLICK_DURATION_MS_MAX: u32 = 60000;
    pub const NOTIFICATION_DURATION_MS_MAX: u32 = 60000;
    pub const NOTIFICATION_THROTTLE_MS_MAX: u32 = 300000;

    pub fn state_config(&self, state: ThumbnailState) -> StateVisualConfig {
        match state {
            ThumbnailState::Active => self.active,
            ThumbnailState::Inactive => self.inactive,
            ThumbnailState::Alert => self.alert,
            ThumbnailState::Minimized => self.minimized,
            ThumbnailState::Dragging => self.dragging,
        }
    }

    pub fn validate(&mut self) {
        if self.width < Self::WIDTH_MIN {
            SLOG.warn(format_args!("Thumbnail width {} too small, clamping to {}", self.width, Self::WIDTH_MIN));
            self.width = Self::WIDTH_MIN;
        } else if self.width > Self::WIDTH_MAX {
            SLOG.warn(format_args!("Thumbnail width {} too large, clamping to {}", self.width, Self::WIDTH_MAX));
            self.width = Self::WIDTH_MAX;
        }

        if self.height < Self::HEIGHT_MIN {
            SLOG.warn(format_args!("Thumbnail height {} too small, clamping to {}", self.height, Self::HEIGHT_MIN));
            self.height = Self::HEIGHT_MIN;
        } else if self.height > Self::HEIGHT_MAX {
            SLOG.warn(format_args!("Thumbnail height {} too large, clamping to {}", self.height, Self::HEIGHT_MAX));
            self.height = Self::HEIGHT_MAX;
        }

        if self.border_width < Self::BORDER_WIDTH_MIN {
            self.border_width = Self::BORDER_WIDTH_MIN;
        }
        if self.border_width > Self::BORDER_WIDTH_MAX {
            SLOG.warn(format_args!("Border width {} too large, clamping to {}", self.border_width, Self::BORDER_WIDTH_MAX));
            self.border_width = Self::BORDER_WIDTH_MAX;
        }
        if self.inactive_border_width < Self::BORDER_WIDTH_MIN {
            self.inactive_border_width = Self::BORDER_WIDTH_MIN;
        }
        if self.inactive_border_width > Self::BORDER_WIDTH_MAX {
            SLOG.warn(format_args!("Inactive border width {} too large, clamping to {}", self.inactive_border_width, Self::BORDER_WIDTH_MAX));
            self.inactive_border_width = Self::BORDER_WIDTH_MAX;
        }

        if self.character_name_font_size < Self::FONT_SIZE_MIN {
            SLOG.warn(format_args!("Font size {} too small, clamping to {}", self.character_name_font_size, Self::FONT_SIZE_MIN));
            self.character_name_font_size = Self::FONT_SIZE_MIN;
        } else if self.character_name_font_size > Self::FONT_SIZE_MAX {
            SLOG.warn(format_args!("Font size {} too large, clamping to {}", self.character_name_font_size, Self::FONT_SIZE_MAX));
            self.character_name_font_size = Self::FONT_SIZE_MAX;
        }

        clamp_in(&mut self.system_name_font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        clamp_in(&mut self.quick_group_badge_font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);

        if self.thumbnail_opacity < Self::OPACITY_MIN {
            SLOG.warn(format_args!("Thumbnail opacity {} too low, clamping to {}", self.thumbnail_opacity, Self::OPACITY_MIN));
            self.thumbnail_opacity = Self::OPACITY_MIN;
        }

        for offset in [
            &mut self.character_name_offset_x,
            &mut self.character_name_offset_y,
            &mut self.system_name_offset_x,
            &mut self.system_name_offset_y,
            &mut self.quick_group_badge_offset_x,
            &mut self.quick_group_badge_offset_y,
            &mut self.notifications.offset_x,
            &mut self.notifications.offset_y,
        ] {
            clamp_in(offset, Self::OFFSET_MIN, Self::OFFSET_MAX);
        }

        let n = &mut self.notifications;
        clamp_in(&mut n.font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        n.tts_volume = n.tts_volume.min(Self::TTS_VOLUME_MAX);
        clamp_in(&mut n.tts_rate, Self::TTS_RATE_MIN, Self::TTS_RATE_MAX);
        clamp_in(&mut n.notified_cycle_retention_seconds, Self::CYCLE_RETENTION_MIN, Self::CYCLE_RETENTION_MAX);
        n.suppress_click_duration_ms = n.suppress_click_duration_ms.min(Self::SUPPRESS_CLICK_DURATION_MS_MAX);

        for (_, tc) in n.type_configs.iter_mut() {
            tc.duration_ms = tc.duration_ms.min(Self::NOTIFICATION_DURATION_MS_MAX);
            tc.throttle_ms = tc.throttle_ms.min(Self::NOTIFICATION_THROTTLE_MS_MAX);
            tc.sound_volume = tc.sound_volume.min(Self::SOUND_VOLUME_MAX);
        }

        if self.hide_debounce_ms > Self::HIDE_DEBOUNCE_MS_MAX {
            SLOG.warn(format_args!("Hide debounce {} ms too long, clamping to {}", self.hide_debounce_ms, Self::HIDE_DEBOUNCE_MS_MAX));
            self.hide_debounce_ms = Self::HIDE_DEBOUNCE_MS_MAX;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisplayConfig {
    pub start_x: i32,
    pub start_y: i32,
    pub spacing: i32,

    /// Horizontal gap for thumbnails with no saved position, lined up left-to-right from start_x/start_y instead of stacking.
    pub new_thumbnail_spacing: i32,

    pub view_mode: ViewMode,
    pub list_view_order: ListViewOrder,
    pub remember_list_view_position: bool,
    pub list_view_opacity: u8,
    pub list_view_columns: u32,
    pub list_view_font_name: String,
    pub list_view_font_size: i32,
    pub list_view_font_weight: FontWeight,

    // History Panel: an always-available, resizable panel showing recent notification history.
    pub show_notif_info_panel: bool,
    pub notif_info_panel_x: i32,
    pub notif_info_panel_y: i32,
    pub notif_info_panel_width: i32,
    pub notif_info_panel_height: i32,
    pub remember_notif_info_panel_position: bool,
    pub hide_notif_info_panel_when_no_characters: bool,
    pub notif_info_panel_opacity: u8,
    pub notif_info_panel_font_name: String,
    pub notif_info_panel_font_size: i32,
    pub notif_info_panel_font_weight: FontWeight,
    pub notif_info_panel_max_rows: i32,
    pub notif_info_panel_show_timestamp: bool,
    pub notif_info_panel_show_category_filters: bool,
    pub notif_info_panel_show_fleet: bool,
    pub notif_info_panel_show_mining: bool,
    pub notif_info_panel_show_combat: bool,
    pub notif_info_panel_show_navigation: bool,
    pub notif_info_panel_show_general: bool,

    pub layout_mode: LayoutMode,
    pub region_fit_direction: RegionFitDirection,

    /// Physical pixels, absolute (like saved positions); None until captured via "Start Region Selection".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_height: Option<i32>,
    pub region_fit_order: RegionFitOrder,
    /// Whether a logout moves a thumbnail to the end of the grid, or leaves it in place until another reflow.
    pub region_fit_reorder_logged_out: bool,
    /// Whether selecting or editing the Thumbnail Space region temporarily hides visible thumbnails so they don't obscure the overlay; the dialog passes it along when it starts the selection.
    pub hide_thumbnails_during_region_select: bool,
    /// Caps RegionFit's cell size at the configured thumbnail size instead of always maximizing to fill the region.
    pub region_fit_limit_to_thumbnail_size: bool,

    /// A separate auto-fit holding area for not-yet-logged-in "EVE" placeholders; coexists with RegionFit by carving them out of that grid entirely.
    pub not_logged_in_space_enabled: bool,
    /// Physical pixels, absolute (like region_x/y); None until captured via "Start Region Selection".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_logged_in_space_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_logged_in_space_y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_logged_in_space_width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_logged_in_space_height: Option<i32>,
    /// Independent of RegionFit's `spacing`, so each Thumbnail Space can be tuned separately.
    pub not_logged_in_space_spacing: i32,
    /// Same cap as `region_fit_limit_to_thumbnail_size`, for the not-logged-in space's own cells.
    pub not_logged_in_space_limit_to_thumbnail_size: bool,
    /// Same as `hide_thumbnails_during_region_select`, for the not-logged-in space's region.
    pub not_logged_in_space_hide_thumbnails_during_region_select: bool,

    /// Display Regions: splits the RegionFit region into an NxN grid whose cells each claim specific thumbnails (see display_grid). size 0 = off.
    pub display_grid: DisplayGrid,
    /// Display Regions across several displays: one grid per display, each over its own captured rect. Takes over from `display_grid` (which only ever split the RegionFit region) once set.
    pub display_layouts: Vec<DisplayLayout>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_index: Option<u32>,
    pub use_monitor_work_area: bool,

    pub honor_saved_positions: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            start_x: 10,
            start_y: 10,
            spacing: 0,
            new_thumbnail_spacing: 10,
            view_mode: ViewMode::Thumbnails,
            list_view_order: ListViewOrder::Tracked,
            remember_list_view_position: true,
            list_view_opacity: 255,
            list_view_columns: 1,
            list_view_font_name: default_font(),
            list_view_font_size: 13,
            list_view_font_weight: FontWeight::Regular,
            show_notif_info_panel: false,
            notif_info_panel_x: 10,
            notif_info_panel_y: 250,
            notif_info_panel_width: 200,
            notif_info_panel_height: 224,
            remember_notif_info_panel_position: true,
            hide_notif_info_panel_when_no_characters: true,
            notif_info_panel_opacity: 255,
            notif_info_panel_font_name: default_font(),
            notif_info_panel_font_size: 13,
            notif_info_panel_font_weight: FontWeight::Regular,
            notif_info_panel_max_rows: 15,
            notif_info_panel_show_timestamp: false,
            notif_info_panel_show_category_filters: true,
            notif_info_panel_show_fleet: true,
            notif_info_panel_show_mining: true,
            notif_info_panel_show_combat: true,
            notif_info_panel_show_navigation: true,
            notif_info_panel_show_general: true,
            layout_mode: LayoutMode::Custom,
            region_fit_direction: RegionFitDirection::RowFirst_LTR_TTB,
            region_x: None,
            region_y: None,
            region_width: None,
            region_height: None,
            region_fit_order: RegionFitOrder::Characters,
            region_fit_reorder_logged_out: true,
            hide_thumbnails_during_region_select: true,
            region_fit_limit_to_thumbnail_size: false,
            not_logged_in_space_enabled: false,
            not_logged_in_space_x: None,
            not_logged_in_space_y: None,
            not_logged_in_space_width: None,
            not_logged_in_space_height: None,
            not_logged_in_space_spacing: 0,
            not_logged_in_space_limit_to_thumbnail_size: false,
            not_logged_in_space_hide_thumbnails_during_region_select: true,
            display_grid: DisplayGrid::default(),
            display_layouts: Vec::new(),
            monitor_index: None,
            use_monitor_work_area: true,
            honor_saved_positions: true,
        }
    }
}

impl DisplayConfig {
    pub const START_X_MIN: i32 = -3840;
    pub const START_X_MAX: i32 = 7680;
    pub const START_Y_MIN: i32 = -2160;
    pub const START_Y_MAX: i32 = 4320;
    pub const NOTIF_PANEL_WIDTH_MIN: i32 = 100;
    pub const NOTIF_PANEL_WIDTH_MAX: i32 = 1920;
    pub const NOTIF_PANEL_HEIGHT_MIN: i32 = 60;
    pub const NOTIF_PANEL_HEIGHT_MAX: i32 = 2160;
    pub const NOTIF_PANEL_FONT_SIZE_MIN: i32 = 6;
    pub const NOTIF_PANEL_FONT_SIZE_MAX: i32 = 72;
    // Mirrors the painter's NOTIF_HISTORY_CAPACITY, the ring buffer's actual size.
    pub const NOTIF_PANEL_MAX_ROWS_MIN: i32 = 1;
    pub const NOTIF_PANEL_MAX_ROWS_MAX: i32 = 30;
    pub const SPACING_MIN: i32 = 0;
    pub const SPACING_MAX: i32 = 500;
    pub const MONITOR_INDEX_MAX: u32 = 9;
    pub const LIST_VIEW_COLUMNS_MIN: u32 = 1;
    pub const LIST_VIEW_COLUMNS_MAX: u32 = 15;
    pub const LIST_VIEW_FONT_SIZE_MIN: i32 = 6;
    pub const LIST_VIEW_FONT_SIZE_MAX: i32 = 72;
    pub const OPACITY_MIN: u8 = 51;

    pub fn validate(&mut self) {
        clamp_in(&mut self.start_x, Self::START_X_MIN, Self::START_X_MAX);
        clamp_in(&mut self.start_y, Self::START_Y_MIN, Self::START_Y_MAX);
        clamp_in(&mut self.new_thumbnail_spacing, Self::SPACING_MIN, Self::SPACING_MAX);
        clamp_in(&mut self.notif_info_panel_x, Self::START_X_MIN, Self::START_X_MAX);
        clamp_in(&mut self.notif_info_panel_y, Self::START_Y_MIN, Self::START_Y_MAX);
        clamp_in(&mut self.notif_info_panel_width, Self::NOTIF_PANEL_WIDTH_MIN, Self::NOTIF_PANEL_WIDTH_MAX);
        clamp_in(&mut self.notif_info_panel_height, Self::NOTIF_PANEL_HEIGHT_MIN, Self::NOTIF_PANEL_HEIGHT_MAX);
        clamp_in(&mut self.notif_info_panel_max_rows, Self::NOTIF_PANEL_MAX_ROWS_MIN, Self::NOTIF_PANEL_MAX_ROWS_MAX);

        self.display_grid.validate();
        for layout in &mut self.display_layouts {
            layout.grid.validate();
        }

        if self.spacing < Self::SPACING_MIN {
            self.spacing = Self::SPACING_MIN;
        }
        if self.spacing > Self::SPACING_MAX {
            SLOG.warn(format_args!("Spacing {} too large, clamping to {}", self.spacing, Self::SPACING_MAX));
            self.spacing = Self::SPACING_MAX;
        }

        if self.not_logged_in_space_spacing < Self::SPACING_MIN {
            self.not_logged_in_space_spacing = Self::SPACING_MIN;
        }
        if self.not_logged_in_space_spacing > Self::SPACING_MAX {
            SLOG.warn(format_args!("Not-logged-in space spacing {} too large, clamping to {}", self.not_logged_in_space_spacing, Self::SPACING_MAX));
            self.not_logged_in_space_spacing = Self::SPACING_MAX;
        }

        for x in [&mut self.region_x, &mut self.not_logged_in_space_x].into_iter().flatten() {
            clamp_in(x, Self::START_X_MIN, Self::START_X_MAX);
        }
        for y in [&mut self.region_y, &mut self.not_logged_in_space_y].into_iter().flatten() {
            clamp_in(y, Self::START_Y_MIN, Self::START_Y_MAX);
        }
        if let Some(idx) = &mut self.monitor_index {
            if *idx > Self::MONITOR_INDEX_MAX {
                SLOG.warn(format_args!("Monitor index {} too high, clamping to {} (max {} monitors)", idx, Self::MONITOR_INDEX_MAX, Self::MONITOR_INDEX_MAX + 1));
                *idx = Self::MONITOR_INDEX_MAX;
            }
        }

        clamp_in(&mut self.list_view_columns, Self::LIST_VIEW_COLUMNS_MIN, Self::LIST_VIEW_COLUMNS_MAX);

        if self.list_view_opacity < Self::OPACITY_MIN {
            SLOG.warn(format_args!("Client List opacity {} too low, clamping to {}", self.list_view_opacity, Self::OPACITY_MIN));
            self.list_view_opacity = Self::OPACITY_MIN;
        }
        if self.notif_info_panel_opacity < Self::OPACITY_MIN {
            SLOG.warn(format_args!("History panel opacity {} too low, clamping to {}", self.notif_info_panel_opacity, Self::OPACITY_MIN));
            self.notif_info_panel_opacity = Self::OPACITY_MIN;
        }

        if self.list_view_font_size < Self::LIST_VIEW_FONT_SIZE_MIN {
            SLOG.warn(format_args!("List view font size {} too small, clamping to {}", self.list_view_font_size, Self::LIST_VIEW_FONT_SIZE_MIN));
            self.list_view_font_size = Self::LIST_VIEW_FONT_SIZE_MIN;
        } else if self.list_view_font_size > Self::LIST_VIEW_FONT_SIZE_MAX {
            SLOG.warn(format_args!("List view font size {} too large, clamping to {}", self.list_view_font_size, Self::LIST_VIEW_FONT_SIZE_MAX));
            self.list_view_font_size = Self::LIST_VIEW_FONT_SIZE_MAX;
        }

        if self.notif_info_panel_font_size < Self::NOTIF_PANEL_FONT_SIZE_MIN {
            SLOG.warn(format_args!("Notification history panel font size {} too small, clamping to {}", self.notif_info_panel_font_size, Self::NOTIF_PANEL_FONT_SIZE_MIN));
            self.notif_info_panel_font_size = Self::NOTIF_PANEL_FONT_SIZE_MIN;
        } else if self.notif_info_panel_font_size > Self::NOTIF_PANEL_FONT_SIZE_MAX {
            SLOG.warn(format_args!("Notification history panel font size {} too large, clamping to {}", self.notif_info_panel_font_size, Self::NOTIF_PANEL_FONT_SIZE_MAX));
            self.notif_info_panel_font_size = Self::NOTIF_PANEL_FONT_SIZE_MAX;
        }
    }
}
