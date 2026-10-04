//! Profile JSON model shared by eve-maj-preview and config: loading, validation, saving, and the lookups the rest of the app makes against a profile.
//!
//! Paths are relative to the working directory, as in the Zig build (the exe runs from its own folder).

mod global;
mod patch;
mod sections;
pub mod serde_helpers;
mod thumbnail;

use std::fmt;
use std::io::Read;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use global::*;
pub use sections::*;
pub use serde_helpers::parse_hex_color;
pub use thumbnail::*;

use crate::color::{AutoColorEntry, AutoColors};
use crate::log::Scope;
use crate::profile_name;
use serde_helpers::{argb, vk_opt};

pub(crate) const SLOG: Scope = Scope::new("config");

pub const PROFILES_DIR: &str = "profiles";
pub const DEFAULT_PROFILE: &str = "default.json";
pub const GLOBAL_SETTINGS_FILE: &str = "profiles/global.settings.json";
pub const MAX_PROFILE_NAME_LEN: usize = 16;
pub const DEFAULT_ACCENT_COLOR: u32 = 0xFFD9A441;
pub const DEFAULT_FONT_NAME: &str = "Segoe UI";
const MAX_CONFIG_FILE_SIZE: u64 = 300 * 1024;
const AUTO_COLORS_FILE: &str = "colors.json";

/// Identifies a profile JSON as this app's own format, distinct from its release version; bump PROFILE_FORMAT_VERSION only when the schema change matters for parsing/migration.
pub const PROFILE_FORMAT_IDENTIFIER: &str = "eve-maj-preview";
/// v2: character positions saved while this app was DPI-unaware are migrated to physical pixels on load - see Config::from_wire.
pub const PROFILE_FORMAT_VERSION: u32 = 2;

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidProfileName,
    ConfigFileTooLarge,
    MissingEnvironmentVariable,
    InvalidColorFormat,
    MissingSystemName,
    MissingSystemColor,
    InvalidSystemColor,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "{e}"),
            Self::InvalidProfileName => f.write_str("invalid profile name"),
            Self::ConfigFileTooLarge => write!(f, "config file larger than {MAX_CONFIG_FILE_SIZE} bytes"),
            Self::MissingEnvironmentVariable => f.write_str("missing environment variable"),
            Self::InvalidColorFormat => f.write_str("invalid color format"),
            Self::MissingSystemName => f.write_str("system color is missing systemName"),
            Self::MissingSystemColor => f.write_str("system color is missing color"),
            Self::InvalidSystemColor => f.write_str("system color entry is malformed"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Whether `name` is a plain "<name>.json" profile file name (see profile_name); everything that turns a profile name into a path goes through profile_path, which enforces it.
pub fn is_safe_profile_name(name: &str) -> bool {
    profile_name::is_safe(name)
}

/// PROFILES_DIR/<name>, refusing names that could address any other file.
pub fn profile_path(name: &str) -> Result<PathBuf, ConfigError> {
    if !is_safe_profile_name(name) {
        SLOG.warn(format_args!("Rejected unsafe profile name '{name}'"));
        return Err(ConfigError::InvalidProfileName);
    }
    Ok(PathBuf::from(PROFILES_DIR).join(name))
}

/// Truncates a user-supplied profile name to MAX_PROFILE_NAME_LEN bytes; names are ASCII (enforced by the config dialog's input sanitization), and a stray multi-byte character is cut before rather than split.
pub fn clamp_profile_name(name: &str) -> &str {
    if name.len() <= MAX_PROFILE_NAME_LEN {
        return name;
    }
    let mut end = MAX_PROFILE_NAME_LEN;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Reads a whole settings file, refusing anything over MAX_CONFIG_FILE_SIZE.
pub(crate) fn read_limited(path: impl AsRef<std::path::Path>) -> Result<Vec<u8>, ConfigError> {
    let file = std::fs::File::open(path)?;
    let mut content = Vec::new();
    file.take(MAX_CONFIG_FILE_SIZE + 1).read_to_end(&mut content)?;
    if content.len() as u64 > MAX_CONFIG_FILE_SIZE {
        return Err(ConfigError::ConfigFileTooLarge);
    }
    Ok(content)
}

/// Writes via a temp file + rename so a failed write can't corrupt the destination file.
pub fn atomic_write_file(path: impl AsRef<std::path::Path>, content: &[u8]) -> Result<(), ConfigError> {
    use std::hash::{BuildHasher, Hasher};

    let path = path.as_ref();
    // Unique per call so overlapping saves of the same path can't share a temp file.
    let unique = std::collections::hash_map::RandomState::new().build_hasher().finish();
    let mut temp_path = path.as_os_str().to_owned();
    temp_path.push(format!(".{unique:x}.tmp"));
    let temp_path = PathBuf::from(temp_path);

    if let Err(err) = std::fs::write(&temp_path, content) {
        SLOG.err(format_args!("Failed to write {} bytes to temp file '{}': {err}", content.len(), temp_path.display()));
        if let Err(cleanup_err) = std::fs::remove_file(&temp_path) {
            if cleanup_err.kind() != std::io::ErrorKind::NotFound {
                SLOG.err(format_args!("Failed to cleanup temp file '{}' after write failure (original error: {err}): {cleanup_err}", temp_path.display()));
            }
        }
        return Err(err.into());
    }

    if let Err(err) = std::fs::rename(&temp_path, path) {
        SLOG.err(format_args!("Failed to rename temp file '{}' to '{}' ({} bytes): {err}", temp_path.display(), path.display(), content.len()));
        if let Err(cleanup_err) = std::fs::remove_file(&temp_path) {
            SLOG.err(format_args!("Failed to cleanup temp file '{}' after rename failure (original error: {err}): {cleanup_err}", temp_path.display()));
        }
        return Err(err.into());
    }
    Ok(())
}

/// Nested under "hotkeys" in the JSON because config_dialog.js keeps these under a "hotkeys" sub-object throughout, not just in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HotkeysConfig {
    pub require_eve_focus: bool,
    pub reset_group_index_on_non_group_focus: bool,
    pub allow_hotkey_auto_repeat: bool,
    pub suspend_hotkey_notification: bool,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_minimize_all: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_close_all: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_toggle_visibility: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_toggle_auto_minimize: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_toggle_exclusion: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_next_excluded: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_previous_excluded: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_suspend: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_cycle_notified: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_previous_notified: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_move_to_saved_positions: Option<u32>,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self {
            require_eve_focus: false,
            reset_group_index_on_non_group_focus: false,
            allow_hotkey_auto_repeat: false,
            suspend_hotkey_notification: true,
            hotkey_minimize_all: None,
            hotkey_close_all: None,
            hotkey_toggle_visibility: None,
            hotkey_toggle_auto_minimize: None,
            hotkey_toggle_exclusion: None,
            hotkey_next_excluded: None,
            hotkey_previous_excluded: None,
            hotkey_suspend: None,
            hotkey_cycle_notified: None,
            hotkey_previous_notified: None,
            hotkey_move_to_saved_positions: None,
        }
    }
}

/// One loaded profile. `profile_name` comes from the file name; the auto-color stores are runtime state persisted to their own file.
#[derive(Debug)]
pub struct Config {
    pub profile_name: String,
    pub thumbnail: ThumbnailConfig,
    pub timer: TimerConfig,
    pub display: DisplayConfig,
    pub snapping: SnappingConfig,
    pub interaction: InteractionConfig,
    pub auto_minimize: AutoMinimizeConfig,
    pub auto_move_position: AutoMovePositionConfig,
    pub exclusion: ExclusionConfig,
    pub close_all: CloseAllConfig,
    pub chatlog: ChatlogConfig,
    pub combat: CombatConfig,
    pub mining: MiningConfig,
    pub bounty: BountyConfig,
    pub resources: ResourcesConfig,
    pub travel: TravelConfig,
    pub accent_color: u32,
    pub window_filters: Vec<WindowFilter>,
    pub characters: Vec<CharacterConfig>,
    pub system_colors: Vec<SystemColor>,
    pub hotkey_groups: Vec<HotkeyGroup>,
    pub account_hotkeys: Vec<AccountHotkey>,
    pub hotkeys: HotkeysConfig,

    // Persisted to AUTO_COLORS_FILE rather than the profile so saving the profile mid live-preview can't leak unsaved edits.
    pub auto_system_colors: AutoColors,
    pub auto_character_colors: AutoColors,
    auto_colors_loaded: bool,
}

/// The profile file as read: every section optional, plus the legacy quickGroups list.
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ConfigFileIn {
    #[allow(dead_code)]
    app: String,
    format_version: u32,
    thumbnail: ThumbnailConfig,
    timer: TimerConfig,
    display: DisplayConfig,
    snapping: SnappingConfig,
    interaction: InteractionConfig,
    auto_minimize: AutoMinimizeConfig,
    auto_move_position: AutoMovePositionConfig,
    exclusion: ExclusionConfig,
    close_all: CloseAllConfig,
    chatlog: ChatlogConfig,
    combat: CombatConfig,
    mining: MiningConfig,
    bounty: BountyConfig,
    resources: ResourcesConfig,
    travel: TravelConfig,
    #[serde(with = "argb")]
    accent_color: u32,
    window_filters: Vec<WindowFilter>,
    characters: Vec<CharacterConfig>,
    system_colors: Vec<SystemColor>,
    hotkey_groups: Vec<HotkeyGroup>,
    account_hotkeys: Vec<AccountHotkey>,
    // Read-only legacy: merged into hotkey_groups on load and always written back empty.
    quick_groups: Vec<LegacyQuickGroup>,
    hotkeys: HotkeysConfig,
}

impl Default for ConfigFileIn {
    fn default() -> Self {
        Self {
            app: PROFILE_FORMAT_IDENTIFIER.into(),
            format_version: PROFILE_FORMAT_VERSION,
            thumbnail: Default::default(),
            timer: Default::default(),
            display: Default::default(),
            snapping: Default::default(),
            interaction: Default::default(),
            auto_minimize: Default::default(),
            auto_move_position: Default::default(),
            exclusion: Default::default(),
            close_all: Default::default(),
            chatlog: Default::default(),
            combat: Default::default(),
            mining: Default::default(),
            bounty: Default::default(),
            resources: Default::default(),
            travel: Default::default(),
            accent_color: DEFAULT_ACCENT_COLOR,
            window_filters: vec![WindowFilter::eve_default()],
            characters: Vec::new(),
            system_colors: Vec::new(),
            hotkey_groups: Vec::new(),
            account_hotkeys: Vec::new(),
            quick_groups: Vec::new(),
            hotkeys: Default::default(),
        }
    }
}

/// The profile file as written, borrowing from Config so saving never clones.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFileOut<'a> {
    app: &'a str,
    format_version: u32,
    thumbnail: &'a ThumbnailConfig,
    timer: &'a TimerConfig,
    display: &'a DisplayConfig,
    snapping: &'a SnappingConfig,
    interaction: &'a InteractionConfig,
    auto_minimize: &'a AutoMinimizeConfig,
    auto_move_position: &'a AutoMovePositionConfig,
    exclusion: &'a ExclusionConfig,
    close_all: &'a CloseAllConfig,
    chatlog: &'a ChatlogConfig,
    combat: &'a CombatConfig,
    mining: &'a MiningConfig,
    bounty: &'a BountyConfig,
    resources: &'a ResourcesConfig,
    travel: &'a TravelConfig,
    #[serde(with = "argb")]
    accent_color: u32,
    window_filters: &'a [WindowFilter],
    characters: &'a [CharacterConfig],
    system_colors: &'a [SystemColor],
    hotkey_groups: &'a [HotkeyGroup],
    account_hotkeys: &'a [AccountHotkey],
    quick_groups: [(); 0],
    hotkeys: &'a HotkeysConfig,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AutoColorsFile {
    system_colors: Vec<AutoColorsFileEntry>,
    character_colors: Vec<AutoColorsFileEntry>,
}

#[derive(Serialize, Deserialize)]
struct AutoColorsFileEntry {
    name: String,
    #[serde(with = "argb")]
    color: u32,
}

impl AutoColorsFileEntry {
    fn from_store(store: &AutoColors) -> Vec<Self> {
        store.entries.iter().map(|e: &AutoColorEntry| Self { name: e.name.clone(), color: e.color }).collect()
    }
}

/// Expand %VAR% patterns in a path string; backslashes in expanded values become forward slashes.
fn expand_environment_variables(path: &str) -> String {
    if !path.contains('%') {
        return path.to_owned();
    }

    let mut result = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        result.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            result.push('%');
            rest = after;
            continue;
        };
        let var_name = &after[..end];
        if var_name.is_empty() {
            result.push('%');
            rest = after;
            continue;
        }
        match std::env::var(var_name) {
            Ok(value) => result.push_str(&value.replace('\\', "/")),
            Err(_) => {
                SLOG.warn(format_args!("Environment variable '{var_name}' not found"));
                result.push_str(&rest[start..start + end + 2]);
            }
        }
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    result
}

/// Resolves the real Documents folder via the shell known-folder API rather than assuming `%USERPROFILE%/Documents`, since OneDrive's Known Folder Move can silently redirect it and EVE itself writes logs to wherever this API resolves.
#[cfg(windows)]
fn documents_dir() -> Option<String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut raw) };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) })
    });
    unsafe { CoTaskMemFree(raw.cast()) };
    path.map(|p| p.replace('\\', "/"))
}

#[cfg(not(windows))]
fn documents_dir() -> Option<String> {
    None
}

/// Not a constant since it depends on runtime shell/env state; used by both defaults_with_profile and from_wire's chatlog-section-missing fallback.
fn default_log_dirs() -> Result<(String, String), ConfigError> {
    let documents = match documents_dir() {
        Some(dir) => dir,
        None => {
            if cfg!(windows) {
                SLOG.warn(format_args!("Failed to resolve Documents known folder, falling back to USERPROFILE/Documents"));
            }
            // HOME stands in for USERPROFILE when running the unit tests off Windows.
            let home = std::env::var("USERPROFILE").or_else(|_| if cfg!(windows) { Err(std::env::VarError::NotPresent) } else { std::env::var("HOME") });
            let Ok(home) = home else {
                SLOG.warn(format_args!("USERPROFILE environment variable not found"));
                return Err(ConfigError::MissingEnvironmentVariable);
            };
            format!("{}/Documents", home.replace('\\', "/"))
        }
    };
    Ok((format!("{documents}/EVE/logs/Chatlogs"), format!("{documents}/EVE/logs/Gamelogs")))
}

impl Config {
    pub fn defaults_with_profile(profile_name: &str) -> Result<Config, ConfigError> {
        let (chatlog_dir, gamelog_dir) = default_log_dirs()?;
        Ok(Config {
            profile_name: profile_name.to_owned(),
            thumbnail: ThumbnailConfig::default(),
            timer: TimerConfig::default(),
            display: DisplayConfig::default(),
            snapping: SnappingConfig::default(),
            interaction: InteractionConfig::default(),
            auto_minimize: AutoMinimizeConfig::default(),
            auto_move_position: AutoMovePositionConfig::default(),
            exclusion: ExclusionConfig::default(),
            close_all: CloseAllConfig::default(),
            chatlog: ChatlogConfig { chatlog_dir, gamelog_dir, ..Default::default() },
            combat: CombatConfig::default(),
            mining: MiningConfig::default(),
            bounty: BountyConfig::default(),
            resources: ResourcesConfig::default(),
            travel: TravelConfig::default(),
            accent_color: DEFAULT_ACCENT_COLOR,
            window_filters: vec![WindowFilter::eve_default()],
            characters: Vec::new(),
            system_colors: Vec::new(),
            hotkey_groups: Vec::new(),
            account_hotkeys: Vec::new(),
            hotkeys: HotkeysConfig::default(),
            auto_system_colors: AutoColors::default(),
            auto_character_colors: AutoColors::default(),
            auto_colors_loaded: false,
        })
    }

    /// Build a runtime Config from a parsed profile file (`profile_name` comes from the filename, not the JSON body).
    fn from_wire(w: ConfigFileIn, profile_name: &str) -> Result<Config, ConfigError> {
        let mut cfg = Config::defaults_with_profile(profile_name)?;

        cfg.thumbnail = w.thumbnail;
        cfg.timer = w.timer;
        cfg.display = w.display;
        cfg.display.display_grid.truncate_slots();
        crate::display_grid::truncate_layouts(&mut cfg.display.display_layouts);
        cfg.snapping = w.snapping;
        cfg.interaction = w.interaction;
        cfg.auto_minimize = w.auto_minimize;
        cfg.auto_move_position = w.auto_move_position;
        cfg.exclusion = w.exclusion;
        cfg.close_all = w.close_all;
        cfg.combat = w.combat;
        cfg.mining = w.mining;
        cfg.bounty = w.bounty;
        cfg.resources = w.resources;
        cfg.travel = w.travel;
        cfg.accent_color = w.accent_color;
        cfg.hotkeys = w.hotkeys;

        let default_dirs = std::mem::take(&mut cfg.chatlog);
        cfg.chatlog = w.chatlog;
        cfg.chatlog.chatlog_dir = expand_environment_variables(&cfg.chatlog.chatlog_dir);
        cfg.chatlog.gamelog_dir = expand_environment_variables(&cfg.chatlog.gamelog_dir);
        if cfg.chatlog.chatlog_dir.is_empty() {
            cfg.chatlog.chatlog_dir = default_dirs.chatlog_dir;
        }
        if cfg.chatlog.gamelog_dir.is_empty() {
            cfg.chatlog.gamelog_dir = default_dirs.gamelog_dir;
        }

        cfg.window_filters = w.window_filters;
        cfg.characters = w.characters;
        if w.format_version < 2 {
            for c in &mut cfg.characters {
                if let Some(pos) = c.position {
                    c.position = Some(pos.scale_from_legacy_dpi_unaware());
                }
            }
        }
        cfg.system_colors = w.system_colors;
        cfg.hotkey_groups = w.hotkey_groups;
        cfg.hotkey_groups.extend(w.quick_groups.into_iter().map(HotkeyGroup::from));
        cfg.account_hotkeys = w.account_hotkeys;

        Ok(cfg)
    }

    pub fn to_json_string(&self) -> Result<String, ConfigError> {
        let out = ConfigFileOut {
            app: PROFILE_FORMAT_IDENTIFIER,
            format_version: PROFILE_FORMAT_VERSION,
            thumbnail: &self.thumbnail,
            timer: &self.timer,
            display: &self.display,
            snapping: &self.snapping,
            interaction: &self.interaction,
            auto_minimize: &self.auto_minimize,
            auto_move_position: &self.auto_move_position,
            exclusion: &self.exclusion,
            close_all: &self.close_all,
            chatlog: &self.chatlog,
            combat: &self.combat,
            mining: &self.mining,
            bounty: &self.bounty,
            resources: &self.resources,
            travel: &self.travel,
            accent_color: self.accent_color,
            window_filters: &self.window_filters,
            characters: &self.characters,
            system_colors: &self.system_colors,
            hotkey_groups: &self.hotkey_groups,
            account_hotkeys: &self.account_hotkeys,
            quick_groups: [],
            hotkeys: &self.hotkeys,
        };
        Ok(serde_json::to_string_pretty(&out)?)
    }

    pub fn save_to_json_file(&self, path: impl AsRef<std::path::Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        atomic_write_file(path, self.to_json_string()?.as_bytes())?;
        SLOG.info(format_args!("Saved JSON config to: {}", path.display()));
        Ok(())
    }

    /// Parses and validates a complete profile; the error is surfaced (rather than falling back to defaults) so the config dialog can reject a malformed save instead of overwriting the profile.
    pub fn build_config_from_json(json_text: &[u8], profile_name: &str) -> Result<Config, ConfigError> {
        let wire: ConfigFileIn = serde_json::from_slice(json_text)?;
        let mut config = Config::from_wire(wire, profile_name)?;
        config.validate();
        Ok(config)
    }

    /// A parse failure anywhere in the tree (syntax error or a hard-required field failure, e.g. a character entry missing "name") logs and falls back to defaults for this profile rather than propagating the error.
    fn load_profile_from_json(path: &std::path::Path, profile_name: &str) -> Result<Config, ConfigError> {
        let content = match read_limited(path) {
            Err(ConfigError::ConfigFileTooLarge) => {
                SLOG.err(format_args!("Config file '{}' too large (max: {MAX_CONFIG_FILE_SIZE} bytes)", path.display()));
                return Err(ConfigError::ConfigFileTooLarge);
            }
            other => other?,
        };
        Config::build_config_from_json(&content, profile_name).or_else(|err| {
            SLOG.err(format_args!("Failed to parse config file '{}' ({err}), falling back to defaults", path.display()));
            Config::defaults_with_profile(profile_name)
        })
    }

    fn ensure_profiles_dir() -> Result<(), ConfigError> {
        std::fs::create_dir_all(PROFILES_DIR)?;
        let path = PathBuf::from(PROFILES_DIR).join(DEFAULT_PROFILE);
        if !path.try_exists()? {
            SLOG.debug(format_args!("Default profile not found, creating: {}", path.display()));
            Config::defaults_with_profile(DEFAULT_PROFILE)?.save_to_json_file(&path)?;
            SLOG.info(format_args!("Created default profile: {}", path.display()));
        }
        Ok(())
    }

    pub fn load_profile(profile_name: &str) -> Result<Config, ConfigError> {
        Config::ensure_profiles_dir()?;

        // DEFAULT_PROFILE is itself a safe name, so this can't recurse forever.
        if !is_safe_profile_name(profile_name) {
            SLOG.warn(format_args!("Refusing to load unsafe profile name '{profile_name}', using default"));
            return Config::load_profile(DEFAULT_PROFILE);
        }
        let path = profile_path(profile_name)?;

        SLOG.info(format_args!("Loading JSON config from: {}", path.display()));
        match Config::load_profile_from_json(&path, profile_name) {
            // ensure_profiles_dir() above guarantees DEFAULT_PROFILE exists, so this can't recurse forever.
            Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound && profile_name != DEFAULT_PROFILE => {
                SLOG.warn(format_args!("Profile '{profile_name}' not found, falling back to default profile"));
                Config::load_profile(DEFAULT_PROFILE)
            }
            other => other,
        }
    }

    pub fn load() -> Result<Config, ConfigError> {
        Config::load_profile(DEFAULT_PROFILE)
    }

    pub fn find_character(&self, name: &str) -> Option<&CharacterConfig> {
        self.characters.iter().find(|c| c.name == name)
    }

    pub fn find_character_mut(&mut self, name: &str) -> Option<&mut CharacterConfig> {
        self.characters.iter_mut().find(|c| c.name == name)
    }

    pub fn get_or_create_character(&mut self, name: &str) -> &mut CharacterConfig {
        let index = match self.characters.iter().position(|c| c.name == name) {
            Some(i) => i,
            None => {
                self.characters.push(CharacterConfig::new(name));
                self.characters.len() - 1
            }
        };
        &mut self.characters[index]
    }

    /// Exact names beat patterns regardless of row order; ties within each group go to the first row.
    pub fn find_system_color(&self, name: &str) -> Option<u32> {
        [false, true]
            .into_iter()
            .find_map(|wildcards| self.system_colors.iter().find(|sc| sc.matches(name, wildcards)).map(|sc| sc.color))
    }

    pub fn character_position(&self, name: &str) -> Option<Position> {
        self.find_character(name).and_then(|c| c.position)
    }

    pub fn character_window_position(&self, name: &str) -> Option<Position> {
        self.find_character(name).and_then(|c| c.window_position)
    }

    /// A character's own active border color wins; otherwise Unique Character Border Colors (if enabled) fills it in, leaving any inactive color as configured.
    pub fn character_border_colors(&mut self, name: &str) -> Option<CharacterBorderColors> {
        let configured = self.find_character(name).and_then(|c| c.border_colors);
        if !self.thumbnail.use_unique_character_border_colors {
            return configured;
        }
        let mut colors = configured.unwrap_or_default();
        if colors.active_border_color.is_none() {
            colors.active_border_color = Some(self.auto_character_color_for(name));
        }
        Some(colors)
    }

    pub fn character_size(&self, name: &str) -> Option<CharacterThumbnailSize> {
        self.find_character(name).and_then(|c| c.thumbnail_size)
    }

    pub fn is_excluded_from_minimize(&self, name: &str) -> bool {
        self.find_character(name).is_some_and(|c| c.exclude_from_minimize)
    }

    pub fn is_excluded_from_close_all(&self, name: &str) -> bool {
        self.find_character(name).is_some_and(|c| c.exclude_from_close_all)
    }

    pub fn is_thumbnail_hidden(&self, name: &str) -> bool {
        self.find_character(name).is_some_and(|c| c.hide_thumbnail)
    }

    pub fn is_excluded_from_auto_move(&self, name: &str) -> bool {
        self.find_character(name).is_some_and(|c| c.exclude_from_auto_move)
    }

    pub fn is_notification_muted(&self, name: &str) -> bool {
        self.find_character(name).is_some_and(|c| c.notifications_muted)
    }

    pub fn character_opacity(&self, name: &str) -> u8 {
        self.find_character(name).and_then(|c| c.opacity).unwrap_or(self.thumbnail.thumbnail_opacity)
    }

    pub fn display_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.find_character(name).and_then(|c| c.display_name.as_deref()).unwrap_or(name)
    }

    /// Priority: custom color override, then unique generated color, then default.
    pub fn system_name_color(&mut self, system_name: &str) -> u32 {
        if let Some(custom) = self.find_system_color(system_name) {
            return custom;
        }
        if !self.thumbnail.use_unique_system_colors {
            return self.thumbnail.system_name_color;
        }
        self.load_auto_colors();
        let overrides: Vec<u32> = self.system_colors.iter().take(AutoColors::MAX_AVOIDED).map(|sc| sc.color).collect();
        self.auto_system_colors.color_for(system_name, &overrides)
    }

    /// Priority: manual per-character override, then auto-generated unique color (if enabled), else None; callers fall back to their own default.
    pub fn character_name_color(&mut self, name: &str) -> Option<u32> {
        if let Some(custom) = self.find_character(name).and_then(|c| c.name_color) {
            return Some(custom);
        }
        if !self.thumbnail.use_unique_character_name_colors {
            return None;
        }
        Some(self.auto_character_color_for(name))
    }

    /// One stored color per character, shared by its name and border; steers clear of every character's own name and active border overrides.
    fn auto_character_color_for(&mut self, name: &str) -> u32 {
        self.load_auto_colors();
        let overrides: Vec<u32> = self
            .characters
            .iter()
            .flat_map(|c| [c.name_color, c.border_colors.and_then(|b| b.active_border_color)])
            .flatten()
            .take(AutoColors::MAX_AVOIDED)
            .collect();
        self.auto_character_colors.color_for(name, &overrides)
    }

    /// Writes pending auto colors; deferred to when the last thumbnail closes (and to drop) so a session with EVE open never touches the disk for it.
    pub fn flush_auto_colors(&mut self) {
        if !self.auto_system_colors.dirty && !self.auto_character_colors.dirty {
            return;
        }
        self.auto_system_colors.dirty = false;
        self.auto_character_colors.dirty = false;
        self.save_auto_colors();
    }

    fn load_auto_colors(&mut self) {
        if self.auto_colors_loaded {
            return;
        }
        self.auto_colors_loaded = true;

        let content = match read_limited(AUTO_COLORS_FILE) {
            Ok(c) => c,
            Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                SLOG.warn(format_args!("Failed to read '{AUTO_COLORS_FILE}': {err}"));
                return;
            }
        };
        let file: AutoColorsFile = match serde_json::from_slice(&content) {
            Ok(f) => f,
            Err(err) => {
                SLOG.warn(format_args!("Failed to read auto colors from '{AUTO_COLORS_FILE}': {err}"));
                return;
            }
        };
        for e in file.system_colors {
            self.auto_system_colors.put(&e.name, e.color);
        }
        for e in file.character_colors {
            self.auto_character_colors.put(&e.name, e.color);
        }
    }

    fn save_auto_colors(&self) {
        let file = AutoColorsFile {
            system_colors: AutoColorsFileEntry::from_store(&self.auto_system_colors),
            character_colors: AutoColorsFileEntry::from_store(&self.auto_character_colors),
        };
        let result = serde_json::to_string_pretty(&file)
            .map_err(ConfigError::from)
            .and_then(|json| atomic_write_file(AUTO_COLORS_FILE, json.as_bytes()));
        if let Err(err) = result {
            SLOG.warn(format_args!("Failed to save '{AUTO_COLORS_FILE}': {err}"));
        }
    }

    pub fn validate(&mut self) {
        self.thumbnail.validate();
        self.timer.validate();
        self.display.validate();
        self.snapping.validate();
        self.auto_minimize.validate();
        self.auto_move_position.validate();
        self.chatlog.validate();
        self.combat.validate();
        self.mining.validate();
        self.bounty.validate();
        self.resources.validate();
        self.travel.validate();

        // Per-character thumbnail size/opacity overrides live on CharacterConfig, not ThumbnailConfig, so they bypass validate() above and need clamping here too.
        for c in &mut self.characters {
            if let Some(size) = &mut c.thumbnail_size {
                clamp_character_thumbnail_size(size);
            }
            if let Some(opacity) = &mut c.opacity {
                clamp_character_opacity(opacity);
            }
        }
    }

    /// Serializes every clamp bound the validate() functions above enforce, keyed by the same dotted config path config_dialog.js's CONFIG_SCHEMA uses, so the dialog can set matching HTML min/max without those bounds being hand-copied into JS.
    pub fn build_validation_ranges_json() -> String {
        type T = ThumbnailConfig;
        type D = DisplayConfig;
        let ranges: &[(&str, i64, i64)] = &[
            ("timer.scanIntervalMs", TimerConfig::SCAN_INTERVAL_MS_MIN.into(), TimerConfig::SCAN_INTERVAL_MS_MAX.into()),
            ("thumbnail.width", T::WIDTH_MIN.into(), T::WIDTH_MAX.into()),
            ("thumbnail.height", T::HEIGHT_MIN.into(), T::HEIGHT_MAX.into()),
            ("thumbnail.borderWidth", T::BORDER_WIDTH_MIN.into(), T::BORDER_WIDTH_MAX.into()),
            ("thumbnail.inactiveBorderWidth", T::BORDER_WIDTH_MIN.into(), T::BORDER_WIDTH_MAX.into()),
            ("thumbnail.characterNameFontSize", T::FONT_SIZE_MIN.into(), T::FONT_SIZE_MAX.into()),
            ("thumbnail.characterNameOffsetX", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.characterNameOffsetY", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.systemNameOffsetX", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.systemNameOffsetY", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.systemNameFontSize", T::FONT_SIZE_MIN.into(), T::FONT_SIZE_MAX.into()),
            ("thumbnail.quickGroupBadgeOffsetX", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.quickGroupBadgeOffsetY", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.quickGroupBadgeFontSize", T::FONT_SIZE_MIN.into(), T::FONT_SIZE_MAX.into()),
            ("thumbnail.hideDebounceMs", 0, T::HIDE_DEBOUNCE_MS_MAX.into()),
            ("thumbnail.thumbnailOpacity", T::OPACITY_MIN.into(), 255),
            ("thumbnail.notifications.offset_x", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.notifications.offset_y", T::OFFSET_MIN.into(), T::OFFSET_MAX.into()),
            ("thumbnail.notifications.font_size", T::FONT_SIZE_MIN.into(), T::FONT_SIZE_MAX.into()),
            ("thumbnail.notifications.tts_volume", 0, T::TTS_VOLUME_MAX.into()),
            ("thumbnail.notifications.tts_rate", T::TTS_RATE_MIN.into(), T::TTS_RATE_MAX.into()),
            ("thumbnail.notifications.notified_cycle_retention_seconds", T::CYCLE_RETENTION_MIN.into(), T::CYCLE_RETENTION_MAX.into()),
            ("thumbnail.notifications.suppress_click_duration_ms", 0, T::SUPPRESS_CLICK_DURATION_MS_MAX.into()),
            ("display.spacing", D::SPACING_MIN.into(), D::SPACING_MAX.into()),
            ("display.newThumbnailSpacing", D::SPACING_MIN.into(), D::SPACING_MAX.into()),
            ("display.notLoggedInSpaceSpacing", D::SPACING_MIN.into(), D::SPACING_MAX.into()),
            ("display.monitorIndex", 0, D::MONITOR_INDEX_MAX.into()),
            ("display.listViewColumns", D::LIST_VIEW_COLUMNS_MIN.into(), D::LIST_VIEW_COLUMNS_MAX.into()),
            ("display.listViewFontSize", D::LIST_VIEW_FONT_SIZE_MIN.into(), D::LIST_VIEW_FONT_SIZE_MAX.into()),
            ("display.notifInfoPanelWidth", D::NOTIF_PANEL_WIDTH_MIN.into(), D::NOTIF_PANEL_WIDTH_MAX.into()),
            ("display.notifInfoPanelHeight", D::NOTIF_PANEL_HEIGHT_MIN.into(), D::NOTIF_PANEL_HEIGHT_MAX.into()),
            ("display.notifInfoPanelFontSize", D::NOTIF_PANEL_FONT_SIZE_MIN.into(), D::NOTIF_PANEL_FONT_SIZE_MAX.into()),
            ("display.notifInfoPanelMaxRows", D::NOTIF_PANEL_MAX_ROWS_MIN.into(), D::NOTIF_PANEL_MAX_ROWS_MAX.into()),
            ("display.listViewOpacity", D::OPACITY_MIN.into(), 255),
            ("display.notifInfoPanelOpacity", D::OPACITY_MIN.into(), 255),
            ("snapping.threshold", SnappingConfig::THRESHOLD_MIN.into(), SnappingConfig::THRESHOLD_MAX.into()),
            ("autoMinimize.delayMs", 0, AutoMinimizeConfig::DELAY_MS_MAX.into()),
            ("autoMovePosition.verifyIntervalMs", AutoMovePositionConfig::VERIFY_INTERVAL_MS_MIN.into(), AutoMovePositionConfig::VERIFY_INTERVAL_MS_MAX.into()),
            ("autoMovePosition.verifyCount", 0, AutoMovePositionConfig::VERIFY_COUNT_MAX.into()),
            ("chatlog.pollIntervalMs", ChatlogConfig::POLL_INTERVAL_MS_MIN.into(), ChatlogConfig::POLL_INTERVAL_MS_MAX.into()),
            ("chatlog.idlePollThreshold", ChatlogConfig::IDLE_POLL_THRESHOLD_MIN.into(), ChatlogConfig::IDLE_POLL_THRESHOLD_MAX.into()),
            ("chatlog.maxPollMultiplier", ChatlogConfig::MAX_POLL_MULTIPLIER_MIN.into(), ChatlogConfig::MAX_POLL_MULTIPLIER_MAX.into()),
            ("combat.window_seconds", CombatConfig::WINDOW_SECONDS_MIN.into(), CombatConfig::WINDOW_SECONDS_MAX.into()),
            ("combat.update_interval_ms", CombatConfig::UPDATE_INTERVAL_MS_MIN.into(), CombatConfig::UPDATE_INTERVAL_MS_MAX.into()),
            ("combat.incoming_font_size", CombatConfig::FONT_SIZE_MIN.into(), CombatConfig::FONT_SIZE_MAX.into()),
            ("combat.outgoing_font_size", CombatConfig::FONT_SIZE_MIN.into(), CombatConfig::FONT_SIZE_MAX.into()),
            ("combat.incoming_offset_x", CombatConfig::OFFSET_MIN.into(), CombatConfig::OFFSET_MAX.into()),
            ("combat.incoming_offset_y", CombatConfig::OFFSET_MIN.into(), CombatConfig::OFFSET_MAX.into()),
            ("combat.outgoing_offset_x", CombatConfig::OFFSET_MIN.into(), CombatConfig::OFFSET_MAX.into()),
            ("combat.outgoing_offset_y", CombatConfig::OFFSET_MIN.into(), CombatConfig::OFFSET_MAX.into()),
            ("mining.window_seconds", MiningConfig::WINDOW_SECONDS_MIN.into(), MiningConfig::WINDOW_SECONDS_MAX.into()),
            ("mining.update_interval_ms", MiningConfig::UPDATE_INTERVAL_MS_MIN.into(), MiningConfig::UPDATE_INTERVAL_MS_MAX.into()),
            ("mining.font_size", MiningConfig::FONT_SIZE_MIN.into(), MiningConfig::FONT_SIZE_MAX.into()),
            ("mining.idle_alert_window_seconds", MiningConfig::ALERT_WINDOW_SECONDS_MIN.into(), MiningConfig::ALERT_WINDOW_SECONDS_MAX.into()),
            ("mining.stopped_alert_window_seconds", MiningConfig::ALERT_WINDOW_SECONDS_MIN.into(), MiningConfig::ALERT_WINDOW_SECONDS_MAX.into()),
            ("mining.offset_x", MiningConfig::OFFSET_MIN.into(), MiningConfig::OFFSET_MAX.into()),
            ("mining.offset_y", MiningConfig::OFFSET_MIN.into(), MiningConfig::OFFSET_MAX.into()),
            ("mining.idle_alert_threshold", MiningConfig::IDLE_ALERT_THRESHOLD_MIN.into(), MiningConfig::IDLE_ALERT_THRESHOLD_MAX.into()),
            ("travel.window_seconds", TravelConfig::WINDOW_SECONDS_MIN.into(), TravelConfig::WINDOW_SECONDS_MAX.into()),
            ("travel.threshold_count", TravelConfig::THRESHOLD_COUNT_MIN.into(), TravelConfig::THRESHOLD_COUNT_MAX.into()),
            ("bounty.window_seconds", BountyConfig::WINDOW_SECONDS_MIN.into(), BountyConfig::WINDOW_SECONDS_MAX.into()),
            ("bounty.update_interval_ms", BountyConfig::UPDATE_INTERVAL_MS_MIN.into(), BountyConfig::UPDATE_INTERVAL_MS_MAX.into()),
            ("bounty.font_size", BountyConfig::FONT_SIZE_MIN.into(), BountyConfig::FONT_SIZE_MAX.into()),
            ("bounty.offset_x", BountyConfig::OFFSET_MIN.into(), BountyConfig::OFFSET_MAX.into()),
            ("bounty.offset_y", BountyConfig::OFFSET_MIN.into(), BountyConfig::OFFSET_MAX.into()),
            ("resources.update_interval_ms", ResourcesConfig::UPDATE_INTERVAL_MS_MIN.into(), ResourcesConfig::UPDATE_INTERVAL_MS_MAX.into()),
            ("resources.font_size", ResourcesConfig::FONT_SIZE_MIN.into(), ResourcesConfig::FONT_SIZE_MAX.into()),
            ("resources.offset_x", ResourcesConfig::OFFSET_MIN.into(), ResourcesConfig::OFFSET_MAX.into()),
            ("resources.offset_y", ResourcesConfig::OFFSET_MIN.into(), ResourcesConfig::OFFSET_MAX.into()),
        ];
        let map: serde_json::Map<String, serde_json::Value> =
            ranges.iter().map(|&(key, min, max)| (key.to_owned(), serde_json::json!({ "min": min, "max": max }))).collect();
        serde_json::Value::Object(map).to_string()
    }

    /// Dumps the full config as pretty-printed JSON, one log line per JSON line.
    pub fn log_settings(&self) {
        SLOG.info(format_args!("Config loaded from profile: {}", self.profile_name));
        match self.to_json_string() {
            Ok(json) => json.lines().for_each(|line| SLOG.debug(format_args!("{line}"))),
            Err(err) => SLOG.warn(format_args!("Failed to serialize config for logging: {err}")),
        }
    }

    /// Persists the entire config as JSON to this profile's file.
    pub fn save_current_profile(&self) -> Result<(), ConfigError> {
        self.save_to_json_file(profile_path(&self.profile_name)?)
    }

    /// Persists the entire config as JSON, not just this one field.
    pub fn save_character_position(&mut self, name: &str, pos: Position) -> Result<(), ConfigError> {
        let c = self.get_or_create_character(name);
        let is_new = c.position.is_none();
        c.position = Some(pos);
        self.save_current_profile()?;
        let verb = if is_new { "Created new" } else { "Updated" };
        SLOG.debug(format_args!("{verb} position for '{name}' in profile '{}': ({}, {})", self.profile_name, pos.x, pos.y));
        Ok(())
    }

    pub fn save_character_window_position(&mut self, name: &str, pos: Position) -> Result<(), ConfigError> {
        self.get_or_create_character(name).window_position = Some(pos);
        self.save_current_profile()?;
        SLOG.debug(format_args!("Saved window position for '{name}' in profile '{}': ({}, {})", self.profile_name, pos.x, pos.y));
        Ok(())
    }

    /// No-op if `name` has no saved window position (or doesn't exist yet).
    pub fn clear_character_window_position(&mut self, name: &str) -> Result<(), ConfigError> {
        let Some(c) = self.find_character_mut(name) else { return Ok(()) };
        if c.window_position.take().is_none() {
            return Ok(());
        }
        self.save_current_profile()?;
        SLOG.debug(format_args!("Cleared window position for '{name}' in profile '{}'", self.profile_name));
        Ok(())
    }

    pub fn save_all_character_window_positions(&mut self, pos: Position) -> Result<(), ConfigError> {
        for c in &mut self.characters {
            c.window_position = Some(pos);
        }
        self.save_current_profile()?;
        SLOG.debug(format_args!("Saved window position for all {} character(s) in profile '{}': ({}, {})", self.characters.len(), self.profile_name, pos.x, pos.y));
        Ok(())
    }

    pub fn clear_all_character_window_positions(&mut self) -> Result<(), ConfigError> {
        for c in &mut self.characters {
            c.window_position = None;
        }
        self.save_current_profile()?;
        SLOG.debug(format_args!("Cleared window position for all {} character(s) in profile '{}'", self.characters.len(), self.profile_name));
        Ok(())
    }

    pub fn save_list_view_position(&mut self, pos: Position) -> Result<(), ConfigError> {
        self.display.start_x = pos.x;
        self.display.start_y = pos.y;
        self.save_current_profile()?;
        SLOG.debug(format_args!("Saved list view position for profile '{}': ({}, {})", self.profile_name, pos.x, pos.y));
        Ok(())
    }

    pub fn save_notif_info_panel_position(&mut self, pos: Position) -> Result<(), ConfigError> {
        self.display.notif_info_panel_x = pos.x;
        self.display.notif_info_panel_y = pos.y;
        self.save_current_profile()?;
        SLOG.debug(format_args!("Saved notification/info panel position for profile '{}': ({}, {})", self.profile_name, pos.x, pos.y));
        Ok(())
    }

    pub fn save_notif_info_panel_category_filter(&self) -> Result<(), ConfigError> {
        self.save_current_profile()?;
        SLOG.debug(format_args!("Saved notification history panel category filter for profile '{}'", self.profile_name));
        Ok(())
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        self.flush_auto_colors();
    }
}

/// CharacterConfig.thumbnail_size isn't covered by ThumbnailConfig::validate(), and a negative size would break layout maths downstream - this closes that gap.
fn clamp_character_thumbnail_size(size: &mut CharacterThumbnailSize) {
    if let Some(w) = &mut size.width {
        *w = (*w).clamp(ThumbnailConfig::WIDTH_MIN, ThumbnailConfig::WIDTH_MAX);
    }
    if let Some(h) = &mut size.height {
        *h = (*h).clamp(ThumbnailConfig::HEIGHT_MIN, ThumbnailConfig::HEIGHT_MAX);
    }
}

fn clamp_character_opacity(opacity: &mut u8) {
    *opacity = (*opacity).max(ThumbnailConfig::OPACITY_MIN);
}

#[cfg(test)]
mod tests;
