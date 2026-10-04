//! The smaller profile sections: overlays (combat/mining/bounty/resources), chatlog, travel, timing, window behaviour, characters, system colors and hotkey bindings.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use super::serde_helpers::{argb, argb_opt, vk_opt};
use super::{SLOG, DEFAULT_FONT_NAME};
use crate::types::{FontWeight, TextPosition};

fn default_font() -> String {
    DEFAULT_FONT_NAME.to_owned()
}

/// Clamps `v` into `lo..=hi` in place.
fn clamp_in<T: PartialOrd + Copy>(v: &mut T, lo: T, hi: T) {
    if *v < lo {
        *v = lo;
    }
    if *v > hi {
        *v = hi;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChatlogConfig {
    pub enabled: bool,
    // Defaults to "" in the JSON since the real Documents/EVE/logs/... default is resolved from the OS Documents known folder at runtime; Config::from_wire substitutes it when empty.
    pub chatlog_dir: String,
    pub gamelog_dir: String,
    pub poll_interval_ms: u32,
    pub idle_poll_threshold: u32,
    pub max_poll_multiplier: u8,
    pub use_threading: bool,
}

impl Default for ChatlogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chatlog_dir: String::new(),
            gamelog_dir: String::new(),
            poll_interval_ms: 500,
            idle_poll_threshold: 600,
            max_poll_multiplier: 2,
            use_threading: true,
        }
    }
}

impl ChatlogConfig {
    pub const POLL_INTERVAL_MS_MIN: u32 = 100;
    pub const POLL_INTERVAL_MS_MAX: u32 = 5000;
    pub const IDLE_POLL_THRESHOLD_MIN: u32 = 1;
    pub const IDLE_POLL_THRESHOLD_MAX: u32 = 1000;
    pub const MAX_POLL_MULTIPLIER_MIN: u8 = 1;
    /// Must be a power of 2 for exponential backoff.
    pub const MAX_POLL_MULTIPLIER_MAX: u8 = 32;

    pub fn validate(&mut self) {
        if self.poll_interval_ms < Self::POLL_INTERVAL_MS_MIN {
            SLOG.warn(format_args!("Chatlog poll interval {} ms too fast, clamping to {}", self.poll_interval_ms, Self::POLL_INTERVAL_MS_MIN));
            self.poll_interval_ms = Self::POLL_INTERVAL_MS_MIN;
        } else if self.poll_interval_ms > Self::POLL_INTERVAL_MS_MAX {
            SLOG.warn(format_args!("Chatlog poll interval {} ms too slow, clamping to {}", self.poll_interval_ms, Self::POLL_INTERVAL_MS_MAX));
            self.poll_interval_ms = Self::POLL_INTERVAL_MS_MAX;
        }

        if self.idle_poll_threshold < Self::IDLE_POLL_THRESHOLD_MIN {
            SLOG.warn(format_args!("Idle poll threshold {} too low, clamping to {}", self.idle_poll_threshold, Self::IDLE_POLL_THRESHOLD_MIN));
            self.idle_poll_threshold = Self::IDLE_POLL_THRESHOLD_MIN;
        } else if self.idle_poll_threshold > Self::IDLE_POLL_THRESHOLD_MAX {
            SLOG.warn(format_args!("Idle poll threshold {} too high, clamping to {}", self.idle_poll_threshold, Self::IDLE_POLL_THRESHOLD_MAX));
            self.idle_poll_threshold = Self::IDLE_POLL_THRESHOLD_MAX;
        }

        if self.max_poll_multiplier < Self::MAX_POLL_MULTIPLIER_MIN {
            SLOG.warn(format_args!("Max poll multiplier {} too low, clamping to {}", self.max_poll_multiplier, Self::MAX_POLL_MULTIPLIER_MIN));
            self.max_poll_multiplier = Self::MAX_POLL_MULTIPLIER_MIN;
        } else if self.max_poll_multiplier > Self::MAX_POLL_MULTIPLIER_MAX {
            SLOG.warn(format_args!("Max poll multiplier {} too high, clamping to {}", self.max_poll_multiplier, Self::MAX_POLL_MULTIPLIER_MAX));
            self.max_poll_multiplier = Self::MAX_POLL_MULTIPLIER_MAX;
        }

        if self.enabled {
            for (label, dir) in [("Chatlog", &self.chatlog_dir), ("Gamelog", &self.gamelog_dir)] {
                if dir.is_empty() {
                    SLOG.warn(format_args!("Chatlog monitoring enabled but {}Dir is empty", label.to_ascii_lowercase()));
                } else if let Err(err) = std::fs::metadata(dir) {
                    SLOG.warn(format_args!("{label} directory '{dir}' does not exist or is not accessible: {err}"));
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CombatConfig {
    pub enabled: bool,
    pub window_seconds: u32,
    pub show_incoming: bool,
    pub show_outgoing: bool,
    #[serde(with = "argb")]
    pub incoming_color: u32,
    #[serde(with = "argb")]
    pub outgoing_color: u32,
    #[serde(with = "argb")]
    pub incoming_bg_color: u32,
    #[serde(with = "argb")]
    pub outgoing_bg_color: u32,
    pub incoming_font_size: i32,
    pub incoming_font_name: String,
    pub incoming_font_weight: FontWeight,
    pub outgoing_font_size: i32,
    pub outgoing_font_name: String,
    pub outgoing_font_weight: FontWeight,
    pub update_interval_ms: u32,
    pub incoming_position: TextPosition,
    pub outgoing_position: TextPosition,
    pub incoming_offset_x: i32,
    pub incoming_offset_y: i32,
    pub outgoing_offset_x: i32,
    pub outgoing_offset_y: i32,
    pub incoming_show_prefix: bool,
    pub outgoing_show_prefix: bool,
    /// Comma-separated, case-insensitive substring match against the parsed weapon name (see activity_tracker); matching hits still count toward DPS stats, just don't retrigger the Taking Damage alert.
    pub damage_alert_excluded_weapons: String,
}

impl Default for CombatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_seconds: 60,
            show_incoming: true,
            show_outgoing: true,
            incoming_color: 0xFFFF4444,
            outgoing_color: 0xFF44FF44,
            incoming_bg_color: 0xE6000000,
            outgoing_bg_color: 0xE6000000,
            incoming_font_size: 11,
            incoming_font_name: default_font(),
            incoming_font_weight: FontWeight::Regular,
            outgoing_font_size: 11,
            outgoing_font_name: default_font(),
            outgoing_font_weight: FontWeight::Regular,
            update_interval_ms: 1000,
            incoming_position: TextPosition::TopCenter,
            outgoing_position: TextPosition::BottomCenter,
            incoming_offset_x: 0,
            incoming_offset_y: 0,
            outgoing_offset_x: 0,
            outgoing_offset_y: 0,
            incoming_show_prefix: true,
            outgoing_show_prefix: true,
            damage_alert_excluded_weapons: String::new(),
        }
    }
}

impl CombatConfig {
    pub const WINDOW_SECONDS_MIN: u32 = 1;
    pub const WINDOW_SECONDS_MAX: u32 = 3600;
    pub const FONT_SIZE_MIN: i32 = 6;
    pub const FONT_SIZE_MAX: i32 = 72;
    pub const UPDATE_INTERVAL_MS_MIN: u32 = 100;
    pub const UPDATE_INTERVAL_MS_MAX: u32 = 60000;
    pub const OFFSET_MIN: i32 = -50;
    pub const OFFSET_MAX: i32 = 50;

    pub fn validate(&mut self) {
        if self.window_seconds == 0 {
            self.window_seconds = 60;
        }
        self.window_seconds = self.window_seconds.min(Self::WINDOW_SECONDS_MAX);
        clamp_in(&mut self.incoming_font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        clamp_in(&mut self.outgoing_font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        clamp_in(&mut self.update_interval_ms, Self::UPDATE_INTERVAL_MS_MIN, Self::UPDATE_INTERVAL_MS_MAX);
        for offset in [&mut self.incoming_offset_x, &mut self.incoming_offset_y, &mut self.outgoing_offset_x, &mut self.outgoing_offset_y] {
            clamp_in(offset, Self::OFFSET_MIN, Self::OFFSET_MAX);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IskRateUnit {
    Minute,
    Hour,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MiningConfig {
    pub enabled: bool,
    pub window_seconds: u32,
    #[serde(with = "argb")]
    pub color: u32,
    #[serde(with = "argb")]
    pub bg_color: u32,
    pub font_size: i32,
    pub font_name: String,
    pub font_weight: FontWeight,
    pub update_interval_ms: u32,
    pub position: TextPosition,
    pub offset_x: i32,
    pub offset_y: i32,
    pub idle_alert_window_seconds: u32,
    pub idle_alert_threshold: u32,
    pub stopped_alert_window_seconds: u32,
    pub show_isk_rate: bool,
    pub isk_rate_unit: IskRateUnit,
    pub show_prefix: bool,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_seconds: 60,
            color: 0xFF44AAFF,
            bg_color: 0xE6000000,
            font_size: 11,
            font_name: default_font(),
            font_weight: FontWeight::Regular,
            update_interval_ms: 1000,
            position: TextPosition::BottomRight,
            offset_x: 0,
            offset_y: 0,
            idle_alert_window_seconds: 30,
            idle_alert_threshold: 1,
            stopped_alert_window_seconds: 60,
            show_isk_rate: true,
            isk_rate_unit: IskRateUnit::Hour,
            show_prefix: true,
        }
    }
}

impl MiningConfig {
    pub const WINDOW_SECONDS_MIN: u32 = 1;
    pub const WINDOW_SECONDS_MAX: u32 = 3600;
    pub const FONT_SIZE_MIN: i32 = 6;
    pub const FONT_SIZE_MAX: i32 = 72;
    pub const UPDATE_INTERVAL_MS_MIN: u32 = 100;
    pub const UPDATE_INTERVAL_MS_MAX: u32 = 60000;
    pub const ALERT_WINDOW_SECONDS_MIN: u32 = 1;
    pub const ALERT_WINDOW_SECONDS_MAX: u32 = 3600;
    pub const OFFSET_MIN: i32 = -50;
    pub const OFFSET_MAX: i32 = 50;
    pub const IDLE_ALERT_THRESHOLD_MIN: u32 = 0;
    pub const IDLE_ALERT_THRESHOLD_MAX: u32 = 60;

    pub fn validate(&mut self) {
        if self.window_seconds == 0 {
            self.window_seconds = 60;
        }
        self.window_seconds = self.window_seconds.min(Self::WINDOW_SECONDS_MAX);
        clamp_in(&mut self.font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        clamp_in(&mut self.update_interval_ms, Self::UPDATE_INTERVAL_MS_MIN, Self::UPDATE_INTERVAL_MS_MAX);
        if self.idle_alert_window_seconds == 0 {
            self.idle_alert_window_seconds = 15;
        }
        self.idle_alert_window_seconds = self.idle_alert_window_seconds.min(Self::ALERT_WINDOW_SECONDS_MAX);
        if self.stopped_alert_window_seconds == 0 {
            self.stopped_alert_window_seconds = 30;
        }
        self.stopped_alert_window_seconds = self.stopped_alert_window_seconds.min(Self::ALERT_WINDOW_SECONDS_MAX);
        clamp_in(&mut self.offset_x, Self::OFFSET_MIN, Self::OFFSET_MAX);
        clamp_in(&mut self.offset_y, Self::OFFSET_MIN, Self::OFFSET_MAX);
        self.idle_alert_threshold = self.idle_alert_threshold.min(Self::IDLE_ALERT_THRESHOLD_MAX);
    }
}

/// ISK/min-style overlay for bounty payouts; mirrors MiningConfig's ISK-rate half, but bounty has no m3 twin or ore table since payouts already arrive in ISK.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BountyConfig {
    pub enabled: bool,
    pub window_seconds: u32,
    #[serde(with = "argb")]
    pub color: u32,
    #[serde(with = "argb")]
    pub bg_color: u32,
    pub font_size: i32,
    pub font_name: String,
    pub font_weight: FontWeight,
    pub update_interval_ms: u32,
    pub position: TextPosition,
    pub offset_x: i32,
    pub offset_y: i32,
    pub isk_rate_unit: IskRateUnit,
    pub show_prefix: bool,
}

impl Default for BountyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_seconds: 1200,
            color: 0xFFFFD700,
            bg_color: 0xE6000000,
            font_size: 11,
            font_name: default_font(),
            font_weight: FontWeight::Regular,
            update_interval_ms: 1000,
            position: TextPosition::TopRight,
            offset_x: 0,
            offset_y: 0,
            isk_rate_unit: IskRateUnit::Hour,
            show_prefix: true,
        }
    }
}

impl BountyConfig {
    pub const WINDOW_SECONDS_MIN: u32 = 1;
    pub const WINDOW_SECONDS_MAX: u32 = 3600;
    pub const FONT_SIZE_MIN: i32 = 6;
    pub const FONT_SIZE_MAX: i32 = 72;
    pub const UPDATE_INTERVAL_MS_MIN: u32 = 100;
    pub const UPDATE_INTERVAL_MS_MAX: u32 = 60000;
    pub const OFFSET_MIN: i32 = -50;
    pub const OFFSET_MAX: i32 = 50;

    pub fn validate(&mut self) {
        if self.window_seconds == 0 {
            self.window_seconds = 60;
        }
        self.window_seconds = self.window_seconds.min(Self::WINDOW_SECONDS_MAX);
        clamp_in(&mut self.font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        clamp_in(&mut self.update_interval_ms, Self::UPDATE_INTERVAL_MS_MIN, Self::UPDATE_INTERVAL_MS_MAX);
        clamp_in(&mut self.offset_x, Self::OFFSET_MIN, Self::OFFSET_MAX);
        clamp_in(&mut self.offset_y, Self::OFFSET_MIN, Self::OFFSET_MAX);
    }
}

/// Per-process CPU/RAM/VRAM overlay; single combined label like BountyConfig, no alert machinery - see resource_tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourcesConfig {
    pub enabled: bool,
    pub show_cpu: bool,
    pub show_ram: bool,
    pub show_vram: bool,
    #[serde(with = "argb")]
    pub color: u32,
    #[serde(with = "argb")]
    pub bg_color: u32,
    pub font_size: i32,
    pub font_name: String,
    pub font_weight: FontWeight,
    pub update_interval_ms: u32,
    pub position: TextPosition,
    pub offset_x: i32,
    pub offset_y: i32,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            show_cpu: true,
            show_ram: true,
            show_vram: true,
            color: 0xFFFFFFFF,
            bg_color: 0xE6000000,
            font_size: 11,
            font_name: default_font(),
            font_weight: FontWeight::Regular,
            update_interval_ms: 10000,
            position: TextPosition::LeftCenter,
            offset_x: 0,
            offset_y: 0,
        }
    }
}

impl ResourcesConfig {
    pub const FONT_SIZE_MIN: i32 = 6;
    pub const FONT_SIZE_MAX: i32 = 72;
    pub const UPDATE_INTERVAL_MS_MIN: u32 = 500;
    pub const UPDATE_INTERVAL_MS_MAX: u32 = 60000;
    pub const OFFSET_MIN: i32 = -50;
    pub const OFFSET_MAX: i32 = 50;

    pub fn validate(&mut self) {
        clamp_in(&mut self.font_size, Self::FONT_SIZE_MIN, Self::FONT_SIZE_MAX);
        clamp_in(&mut self.update_interval_ms, Self::UPDATE_INTERVAL_MS_MIN, Self::UPDATE_INTERVAL_MS_MAX);
        clamp_in(&mut self.offset_x, Self::OFFSET_MIN, Self::OFFSET_MAX);
        clamp_in(&mut self.offset_y, Self::OFFSET_MIN, Self::OFFSET_MAX);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TravelThresholdMode {
    Percent,
    Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TravelConfig {
    pub enabled: bool,
    pub window_seconds: u32,
    pub threshold_mode: TravelThresholdMode,
    pub threshold_percent: f32,
    pub threshold_count: u32,
}

impl Default for TravelConfig {
    fn default() -> Self {
        Self { enabled: false, window_seconds: 30, threshold_mode: TravelThresholdMode::Percent, threshold_percent: 50.0, threshold_count: 2 }
    }
}

impl TravelConfig {
    pub const WINDOW_SECONDS_MIN: u32 = 1;
    pub const WINDOW_SECONDS_MAX: u32 = 3600;
    pub const THRESHOLD_PERCENT_MIN: f32 = 1.0;
    pub const THRESHOLD_PERCENT_MAX: f32 = 100.0;
    pub const THRESHOLD_COUNT_MIN: u32 = 1;
    pub const THRESHOLD_COUNT_MAX: u32 = 50;

    pub fn validate(&mut self) {
        if self.window_seconds == 0 {
            self.window_seconds = 30;
        }
        self.window_seconds = self.window_seconds.min(Self::WINDOW_SECONDS_MAX);
        clamp_in(&mut self.threshold_percent, Self::THRESHOLD_PERCENT_MIN, Self::THRESHOLD_PERCENT_MAX);
        clamp_in(&mut self.threshold_count, Self::THRESHOLD_COUNT_MIN, Self::THRESHOLD_COUNT_MAX);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TimerConfig {
    pub scan_interval_ms: u32,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self { scan_interval_ms: 50 }
    }
}

impl TimerConfig {
    pub const SCAN_INTERVAL_MS_MIN: u32 = 50;
    pub const SCAN_INTERVAL_MS_MAX: u32 = 10000;

    pub fn validate(&mut self) {
        if self.scan_interval_ms < Self::SCAN_INTERVAL_MS_MIN {
            SLOG.warn(format_args!("Scan interval {} ms too fast, clamping to {}", self.scan_interval_ms, Self::SCAN_INTERVAL_MS_MIN));
            self.scan_interval_ms = Self::SCAN_INTERVAL_MS_MIN;
        } else if self.scan_interval_ms > Self::SCAN_INTERVAL_MS_MAX {
            SLOG.warn(format_args!("Scan interval {} ms too slow, clamping to {}", self.scan_interval_ms, Self::SCAN_INTERVAL_MS_MAX));
            self.scan_interval_ms = Self::SCAN_INTERVAL_MS_MAX;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SnappingConfig {
    pub enabled: bool,
    pub threshold: i32,
    pub screen_edges: bool,
    pub thumbnail_edges: bool,
    pub ghost_positions: bool,
    pub show_ghost_position_borders: bool,
}

impl Default for SnappingConfig {
    fn default() -> Self {
        Self { enabled: true, threshold: 10, screen_edges: true, thumbnail_edges: true, ghost_positions: true, show_ghost_position_borders: true }
    }
}

impl SnappingConfig {
    pub const THRESHOLD_MIN: i32 = 0;
    pub const THRESHOLD_MAX: i32 = 100;

    pub fn validate(&mut self) {
        if self.threshold < Self::THRESHOLD_MIN {
            self.threshold = Self::THRESHOLD_MIN;
        }
        if self.threshold > Self::THRESHOLD_MAX {
            SLOG.warn(format_args!("Snap threshold {} too large, clamping to {}", self.threshold, Self::THRESHOLD_MAX));
            self.threshold = Self::THRESHOLD_MAX;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InteractionConfig {
    pub enable_dragging: bool,
    pub animation_style: crate::types::AnimationStyle,
    pub click_trigger: crate::types::ClickTrigger,
    pub click_through: bool,
    pub hover_cursor: crate::types::HoverCursor,
}

impl Default for InteractionConfig {
    fn default() -> Self {
        Self {
            enable_dragging: true,
            animation_style: crate::types::AnimationStyle::NoAnimation,
            click_trigger: crate::types::ClickTrigger::MouseDown,
            click_through: false,
            hover_cursor: crate::types::HoverCursor::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutoMinimizeConfig {
    pub enabled: bool,
    pub delay_ms: u32,
    /// Keep the last-focused EVE client exempt from auto-minimize while EVE itself has no window focused.
    pub exempt_last_active_on_focus_loss: bool,
}

impl Default for AutoMinimizeConfig {
    fn default() -> Self {
        Self { enabled: false, delay_ms: 5000, exempt_last_active_on_focus_loss: true }
    }
}

impl AutoMinimizeConfig {
    pub const DELAY_MS_MAX: u32 = 10000;

    pub fn validate(&mut self) {
        if self.delay_ms > Self::DELAY_MS_MAX {
            SLOG.warn(format_args!("Auto-minimize delay {} ms too long, clamping to {}", self.delay_ms, Self::DELAY_MS_MAX));
            self.delay_ms = Self::DELAY_MS_MAX;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutoMovePositionConfig {
    pub enabled: bool,
    pub move_on_startup: bool,
    pub verify_interval_ms: u32,
    pub verify_count: u8,
}

impl Default for AutoMovePositionConfig {
    fn default() -> Self {
        Self { enabled: false, move_on_startup: false, verify_interval_ms: 2000, verify_count: 6 }
    }
}

impl AutoMovePositionConfig {
    pub const VERIFY_INTERVAL_MS_MIN: u32 = 250;
    pub const VERIFY_INTERVAL_MS_MAX: u32 = 10000;
    pub const VERIFY_COUNT_MAX: u8 = 30;

    pub fn validate(&mut self) {
        let clamped = self.verify_interval_ms.clamp(Self::VERIFY_INTERVAL_MS_MIN, Self::VERIFY_INTERVAL_MS_MAX);
        if clamped != self.verify_interval_ms {
            SLOG.warn(format_args!("Auto-move verify interval {} ms out of range, clamping to {}", self.verify_interval_ms, clamped));
            self.verify_interval_ms = clamped;
        }
        if self.verify_count > Self::VERIFY_COUNT_MAX {
            SLOG.warn(format_args!("Auto-move verify count {} too high, clamping to {}", self.verify_count, Self::VERIFY_COUNT_MAX));
            self.verify_count = Self::VERIFY_COUNT_MAX;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExclusionConfig {
    pub enable_shift_click_exclude: bool,
    pub auto_minimize_excluded: bool,
}

impl Default for ExclusionConfig {
    fn default() -> Self {
        Self { enable_shift_click_exclude: true, auto_minimize_excluded: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CloseAllConfig {
    pub exclude_login_screen_clients: bool,
}

/// Which windows count as tracked clients. Unlike most sections, name/class_names/executable_names are required in the JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFilter {
    pub name: String,
    pub class_names: Vec<String>,
    pub executable_names: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl WindowFilter {
    /// The out-of-the-box EVE Online filter.
    pub fn eve_default() -> Self {
        Self {
            name: "EVE Online".into(),
            class_names: vec!["trinityWindow".into()],
            executable_names: vec!["exefile.exe".into()],
            enabled: true,
        }
    }

    pub fn matches_class(&self, class_name: &str) -> bool {
        if !self.enabled {
            return false;
        }
        // Empty defers to the executable check; both empty means no criteria, so match nothing.
        if self.class_names.is_empty() {
            return !self.executable_names.is_empty();
        }
        self.class_names.iter().any(|c| c == class_name)
    }

    pub fn matches_executable(&self, exe_path: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if self.executable_names.is_empty() {
            return !self.class_names.is_empty();
        }
        let path = exe_path.as_bytes();
        self.executable_names.iter().any(|exe| {
            let exe = exe.as_bytes();
            path.len() >= exe.len() && path[path.len() - exe.len()..].eq_ignore_ascii_case(exe)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

impl Position {
    /// Converts a position captured by a DPI-unaware process (pre-DPI-awareness saves, EVE-O/EVE-X/EVE-APM imports) from its virtualized 96-DPI space into true physical pixels.
    #[cfg(windows)]
    pub fn scale_from_legacy_dpi_unaware(self) -> Position {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
        use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

        let monitor = unsafe { MonitorFromPoint(POINT { x: self.x, y: self.y }, MONITOR_DEFAULTTONEAREST) };
        if monitor.is_null() {
            return self;
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        if dpi_x == 96 {
            return self;
        }
        let scale = dpi_x as f32 / 96.0;
        Position { x: (self.x as f32 * scale).round() as i32, y: (self.y as f32 * scale).round() as i32 }
    }

    /// No monitors to ask off Windows; positions pass through unchanged.
    #[cfg(not(windows))]
    pub fn scale_from_legacy_dpi_unaware(self) -> Position {
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CharacterBorderColors {
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub active_border_color: Option<u32>,
    #[serde(with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub inactive_border_color: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CharacterThumbnailSize {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
}

/// Per-character overrides; `name` is required in the JSON, everything else optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_position: Option<Position>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_colors: Option<CharacterBorderColors>,
    #[serde(default, with = "argb_opt", skip_serializing_if = "Option::is_none")]
    pub name_color: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_size: Option<CharacterThumbnailSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<u32>,
    #[serde(default)]
    pub exclude_from_minimize: bool,
    #[serde(default)]
    pub exclude_from_close_all: bool,
    #[serde(default)]
    pub exclude_from_auto_move: bool,
    #[serde(default)]
    pub hide_thumbnail: bool,
    #[serde(default)]
    pub notifications_muted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<u8>,
}

impl CharacterConfig {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_owned(), ..Default::default() }
    }
}

/// Name -> first-occurrence index among `characters`; used to rank thumbnails against the profile's configured Characters list order.
pub fn build_character_order_map(characters: &[CharacterConfig]) -> std::collections::HashMap<&str, usize> {
    let mut map = std::collections::HashMap::new();
    for (i, c) in characters.iter().enumerate() {
        map.entry(c.name.as_str()).or_insert(i);
    }
    map
}

/// Ranks a_name/b_name by order_map, falling back to array position for ties or names absent from order_map (which always sort last).
pub fn order_map_less_than(
    order_map: &std::collections::HashMap<&str, usize>,
    a_name: &str,
    b_name: &str,
    a_index: usize,
    b_index: usize,
) -> bool {
    match (order_map.get(a_name), order_map.get(b_name)) {
        (Some(ao), Some(bo)) if ao != bo => ao < bo,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => a_index < b_index,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemColor {
    #[serde(rename = "systemName")]
    pub name: String,
    #[serde(with = "argb")]
    pub color: u32,
}

impl SystemColor {
    /// `name` is a comma-separated list of exact names and `*`/`?`/`#` patterns; `wildcards` selects which kind of token is tried.
    pub fn matches(&self, system_name: &str, wildcards: bool) -> bool {
        self.name
            .split(',')
            .map(|raw| raw.trim_matches([' ', '\t']))
            .filter(|token| !token.is_empty())
            .filter(|token| token.contains(['*', '?', '#']) == wildcards)
            .any(|token| if wildcards { glob_match(token, system_name) } else { token.eq_ignore_ascii_case(system_name) })
    }
}

/// Case-insensitive glob: `*` any run, `?` any character, `#` any digit.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    let mut star_p: Option<usize> = None;
    let mut star_t = 0;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if p < pattern.len() && glob_char_matches(pattern[p], text[t]) {
            p += 1;
            t += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn glob_char_matches(pattern_char: u8, text_char: u8) -> bool {
    match pattern_char {
        b'?' => true,
        b'#' => text_char.is_ascii_digit(),
        _ => pattern_char.eq_ignore_ascii_case(&text_char),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HotkeyGroup {
    pub name: String,
    pub characters: Vec<String>,
    #[serde(with = "vk_opt")]
    pub forward_key: Option<u32>,
    #[serde(with = "vk_opt")]
    pub backward_key: Option<u32>,
    /// Hover a thumbnail and press this to toggle that character in or out of the group.
    #[serde(with = "vk_opt")]
    pub assign_key: Option<u32>,
    /// When true, membership is runtime-only: assign-key edits are never written back to the profile.
    pub temporary_membership: bool,
    /// Draws the group's name on its members' thumbnails.
    pub show_badge: bool,
    /// When true, cycling appends still-queued not-logged-in clients (see HotkeyManager::cycle_not_logged_in) to the end of this group's cycle order.
    pub include_not_logged_in: bool,
    // excluded_characters/current_index are deliberately not persisted — runtime-only cycling state that resets every launch.
    #[serde(skip)]
    pub excluded_characters: Vec<String>,
    /// None = not yet cycled; see cycle_group.
    #[serde(skip)]
    pub current_index: Option<usize>,
}

impl Serialize for HotkeyGroup {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        struct Vk(Option<u32>);
        impl Serialize for Vk {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                vk_opt::serialize(&self.0, s)
            }
        }

        let field_count = 5 + [self.forward_key, self.backward_key, self.assign_key].iter().filter(|k| k.is_some()).count();
        let mut st = s.serialize_struct("HotkeyGroup", field_count)?;
        st.serialize_field("name", &self.name)?;
        // Temporary groups never write their membership back.
        let none: &[String] = &[];
        st.serialize_field("characters", if self.temporary_membership { none } else { &self.characters })?;
        for (key, value) in [("forwardKey", self.forward_key), ("backwardKey", self.backward_key), ("assignKey", self.assign_key)] {
            if value.is_some() {
                st.serialize_field(key, &Vk(value))?;
            } else {
                st.skip_field(key)?;
            }
        }
        st.serialize_field("temporaryMembership", &self.temporary_membership)?;
        st.serialize_field("showBadge", &self.show_badge)?;
        st.serialize_field("includeNotLoggedIn", &self.include_not_logged_in)?;
        st.end()
    }
}

/// Pre-merge quick groups, read from older profiles and folded into hotkeyGroups on load.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LegacyQuickGroup {
    pub name: String,
    #[serde(with = "vk_opt")]
    pub assign_key: Option<u32>,
    #[serde(with = "vk_opt")]
    pub forward_key: Option<u32>,
    #[serde(with = "vk_opt")]
    pub backward_key: Option<u32>,
}

impl From<LegacyQuickGroup> for HotkeyGroup {
    fn from(q: LegacyQuickGroup) -> Self {
        HotkeyGroup {
            name: q.name,
            forward_key: q.forward_key,
            backward_key: q.backward_key,
            assign_key: q.assign_key,
            temporary_membership: true,
            show_badge: true,
            ..Default::default()
        }
    }
}

/// Key Binding tab: a hotkey that brings up an Account Config account's running client, cycling through them when several of its characters are open. account_id refers to profiles/accounts.json (see accounts_store).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AccountHotkey {
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<u32>,
    pub account_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_color_exact_and_glob() {
        let sc = SystemColor { name: "Jita, J1##### , *-ABC".into(), color: 1 };
        assert!(sc.matches("jita", false));
        assert!(!sc.matches("jita", true));
        assert!(sc.matches("J123456", true));
        assert!(!sc.matches("J12345X", true));
        assert!(sc.matches("X-ABC", true));
        assert!(!sc.matches("Amarr", false));
    }

    #[test]
    fn glob_edge_cases() {
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b", "aXXc"));
        assert!(glob_match("A?C", "abc"));
    }

    #[test]
    fn window_filter_matching() {
        let f = WindowFilter::eve_default();
        assert!(f.matches_class("trinityWindow"));
        assert!(!f.matches_class("Notepad"));
        assert!(f.matches_executable("C:\\EVE\\bin\\EXEFILE.EXE"));
        let none = WindowFilter { name: "x".into(), class_names: vec![], executable_names: vec![], enabled: true };
        assert!(!none.matches_class("anything"));
    }

    #[test]
    fn order_map_ranks_configured_first() {
        let chars = vec![CharacterConfig::new("B"), CharacterConfig::new("A"), CharacterConfig::new("B")];
        let map = build_character_order_map(&chars);
        assert_eq!(map["B"], 0);
        assert!(order_map_less_than(&map, "B", "A", 5, 0));
        assert!(order_map_less_than(&map, "A", "Zed", 9, 0));
        assert!(!order_map_less_than(&map, "Zed", "A", 0, 9));
        assert!(order_map_less_than(&map, "Q", "R", 1, 2));
    }

    #[test]
    fn temporary_group_writes_no_members() {
        let mut g = HotkeyGroup { name: "Q".into(), characters: vec!["A".into()], forward_key: Some(0x70), ..Default::default() };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains(r#""characters":["A"]"#) && json.contains(r#""forwardKey":"0x70""#) && !json.contains("backwardKey"));
        g.temporary_membership = true;
        assert!(serde_json::to_string(&g).unwrap().contains(r#""characters":[]"#));
    }
}
