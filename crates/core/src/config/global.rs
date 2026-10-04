//! App-wide settings (profiles/global.settings.json) shared by every profile, plus the ore reference table.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::serde_helpers::{string_map_lenient, vk_opt};
use super::{atomic_write_file, read_limited, ConfigError, DEFAULT_PROFILE, GLOBAL_SETTINGS_FILE, PROFILES_DIR, SLOG};
use crate::log::LogLevel;

/// A default ore/ice/gas reference row (name, category, m3/unit, fallback price). Not persisted itself - see OrePriceEntry for what GlobalSettings actually stores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OreEntry {
    pub name: &'static str,
    pub category: &'static str,
    pub volume_m3: f64,
    pub price: f64,
}

/// Fallback prices are a Jita snapshot and will drift - re-fetch via "Fetch Prices" for current numbers.
pub const DEFAULT_ORE_TABLE: &[OreEntry] = &[
    OreEntry { name: "Veldspar", category: "Ore", volume_m3: 0.10, price: 11.53 },
    OreEntry { name: "Mordunium", category: "Ore", volume_m3: 0.10, price: 12.83 },
    OreEntry { name: "Scordite", category: "Ore", volume_m3: 0.15, price: 22.05 },
    OreEntry { name: "Pyroxeres", category: "Ore", volume_m3: 0.30, price: 32.0 },
    OreEntry { name: "Plagioclase", category: "Ore", volume_m3: 0.35, price: 33.4 },
    OreEntry { name: "Omber", category: "Ore", volume_m3: 0.60, price: 112.5 },
    OreEntry { name: "Ytirium", category: "Ore", volume_m3: 0.60, price: 350.9 },
    OreEntry { name: "Griemeer", category: "Ore", volume_m3: 0.80, price: 111.1 },
    OreEntry { name: "Kernite", category: "Ore", volume_m3: 1.20, price: 207.0 },
    OreEntry { name: "Kylixium", category: "Ore", volume_m3: 1.20, price: 252.5 },
    OreEntry { name: "Jaspet", category: "Ore", volume_m3: 2.00, price: 374.5 },
    OreEntry { name: "Hedbergite", category: "Ore", volume_m3: 3.00, price: 627.6 },
    OreEntry { name: "Hemorphite", category: "Ore", volume_m3: 3.00, price: 808.9 },
    OreEntry { name: "Talassonite", category: "Ore", volume_m3: 3.00, price: 8421.0 },
    OreEntry { name: "Nocxite", category: "Ore", volume_m3: 4.00, price: 605.3 },
    OreEntry { name: "Gneiss", category: "Ore", volume_m3: 5.00, price: 2100.0 },
    OreEntry { name: "Hezorime", category: "Ore", volume_m3: 5.00, price: 957.1 },
    OreEntry { name: "Rakovene", category: "Ore", volume_m3: 5.00, price: 7520.0 },
    OreEntry { name: "Ueganite", category: "Ore", volume_m3: 5.00, price: 900.0 },
    OreEntry { name: "Bezdnacine", category: "Ore", volume_m3: 8.00, price: 9220.0 },
    OreEntry { name: "Dark Ochre", category: "Ore", volume_m3: 8.00, price: 4200.0 },

    OreEntry { name: "Bitumens", category: "Moons", volume_m3: 10.00, price: 1164.0 },
    OreEntry { name: "Coesite", category: "Moons", volume_m3: 10.00, price: 0.0 },
    OreEntry { name: "Evaporite Deposits", category: "Moons", volume_m3: 10.00, price: 0.0 },
    OreEntry { name: "Sylvite", category: "Moons", volume_m3: 10.00, price: 801.0 },
    OreEntry { name: "Cobaltite", category: "Moons", volume_m3: 10.00, price: 285.8 },
    OreEntry { name: "Euxenite", category: "Moons", volume_m3: 10.00, price: 900.6 },
    OreEntry { name: "Scheelite", category: "Moons", volume_m3: 10.00, price: 253.1 },
    OreEntry { name: "Titanite", category: "Moons", volume_m3: 10.00, price: 211.1 },
    OreEntry { name: "Chromite", category: "Moons", volume_m3: 10.00, price: 1611.0 },
    OreEntry { name: "Otavite", category: "Moons", volume_m3: 10.00, price: 1439.0 },
    OreEntry { name: "Sperrylite", category: "Moons", volume_m3: 10.00, price: 1548.0 },
    OreEntry { name: "Vanadinite", category: "Moons", volume_m3: 10.00, price: 634.6 },
    OreEntry { name: "Carnotite", category: "Moons", volume_m3: 10.00, price: 4706.0 },
    OreEntry { name: "Cinnabar", category: "Moons", volume_m3: 10.00, price: 600.3 },
    OreEntry { name: "Pollucite", category: "Moons", volume_m3: 10.00, price: 1644.0 },
    OreEntry { name: "Zircon", category: "Moons", volume_m3: 10.00, price: 439.7 },
    OreEntry { name: "Monazite", category: "Moons", volume_m3: 10.00, price: 9800.0 },
    OreEntry { name: "Loparite", category: "Moons", volume_m3: 10.00, price: 8060.0 },
    OreEntry { name: "Xenotime", category: "Moons", volume_m3: 10.00, price: 8166.0 },
    OreEntry { name: "Ytterbite", category: "Moons", volume_m3: 10.00, price: 4728.0 },
    OreEntry { name: "Zeolites", category: "Moons", volume_m3: 10.00, price: 1360.0 },
    OreEntry { name: "Arkonor", category: "Ore", volume_m3: 16.00, price: 4264.0 },
    OreEntry { name: "Bistot", category: "Ore", volume_m3: 16.00, price: 3613.0 },
    OreEntry { name: "Crokite", category: "Ore", volume_m3: 16.00, price: 5300.0 },
    OreEntry { name: "Ducinium", category: "Ore", volume_m3: 16.00, price: 3629.0 },
    OreEntry { name: "Eifyrium", category: "Ore", volume_m3: 16.00, price: 2889.0 },
    OreEntry { name: "Spodumain", category: "Ore", volume_m3: 16.00, price: 8507.0 },
    OreEntry { name: "Mercoxit", category: "Ore", volume_m3: 40.00, price: 18460.0 },
    OreEntry { name: "Prismaticite", category: "Ore", volume_m3: 40.00, price: 18740.0 },

    OreEntry { name: "Fullerite-C50", category: "Gas", volume_m3: 1.00, price: 4707.0 },
    OreEntry { name: "Fullerite-C60", category: "Gas", volume_m3: 1.00, price: 4683.0 },
    OreEntry { name: "Fullerite-C70", category: "Gas", volume_m3: 1.00, price: 8211.0 },
    OreEntry { name: "Fullerite-C28", category: "Gas", volume_m3: 2.00, price: 13100.0 },
    OreEntry { name: "Fullerite-C72", category: "Gas", volume_m3: 2.00, price: 6990.0 },
    OreEntry { name: "Fullerite-C84", category: "Gas", volume_m3: 2.00, price: 9896.0 },
    OreEntry { name: "Fullerite-C32", category: "Gas", volume_m3: 5.00, price: 20000.0 },
    OreEntry { name: "Fullerite-C320", category: "Gas", volume_m3: 5.00, price: 32400.0 },
    OreEntry { name: "Fullerite-C540", category: "Gas", volume_m3: 10.00, price: 47410.0 },
    OreEntry { name: "Amber Cytoserocin", category: "Gas", volume_m3: 10.00, price: 30600.0 },
    OreEntry { name: "Azure Cytoserocin", category: "Gas", volume_m3: 10.00, price: 19150.0 },
    OreEntry { name: "Celadon Cytoserocin", category: "Gas", volume_m3: 10.00, price: 26130.0 },
    OreEntry { name: "Golden Cytoserocin", category: "Gas", volume_m3: 10.00, price: 36220.0 },
    OreEntry { name: "Lime Cytoserocin", category: "Gas", volume_m3: 10.00, price: 31100.0 },
    OreEntry { name: "Malachite Cytoserocin", category: "Gas", volume_m3: 10.00, price: 133300.0 },
    OreEntry { name: "Vermillion Cytoserocin", category: "Gas", volume_m3: 10.00, price: 25120.0 },
    OreEntry { name: "Viridian Cytoserocin", category: "Gas", volume_m3: 10.00, price: 51080.0 },
    OreEntry { name: "Amber Mykoserocin", category: "Gas", volume_m3: 10.00, price: 95230.0 },
    OreEntry { name: "Azure Mykoserocin", category: "Gas", volume_m3: 10.00, price: 48370.0 },
    OreEntry { name: "Celadon Mykoserocin", category: "Gas", volume_m3: 10.00, price: 83500.0 },
    OreEntry { name: "Golden Mykoserocin", category: "Gas", volume_m3: 10.00, price: 97940.0 },
    OreEntry { name: "Lime Mykoserocin", category: "Gas", volume_m3: 10.00, price: 77980.0 },
    OreEntry { name: "Malachite Mykoserocin", category: "Gas", volume_m3: 10.00, price: 88850.0 },
    OreEntry { name: "Vermillion Mykoserocin", category: "Gas", volume_m3: 10.00, price: 77970.0 },
    OreEntry { name: "Viridian Mykoserocin", category: "Gas", volume_m3: 10.00, price: 91090.0 },

    OreEntry { name: "Clear Icicle", category: "Ice", volume_m3: 1000.00, price: 234300.0 },
    OreEntry { name: "Blue Ice", category: "Ice", volume_m3: 1000.00, price: 187200.0 },
    OreEntry { name: "Glacial Mass", category: "Ice", volume_m3: 1000.00, price: 170800.0 },
    OreEntry { name: "White Glaze", category: "Ice", volume_m3: 1000.00, price: 196600.0 },
    OreEntry { name: "Dark Glitter", category: "Ice", volume_m3: 1000.00, price: 332000.0 },
    OreEntry { name: "Gelidus", category: "Ice", volume_m3: 1000.00, price: 350100.0 },
    OreEntry { name: "Krystallos", category: "Ice", volume_m3: 1000.00, price: 592500.0 },
    OreEntry { name: "Glare Crust", category: "Ice", volume_m3: 1000.00, price: 220200.0 },
];

/// True if `base` appears as a whole word in `name` - i.e. `name` is `base` with a quality adjective added before and/or after (e.g. "Nocxite II-Grade" or "Shining Loparite" are both variants of their base ore, priced/measured the same).
fn is_grade_variant(name: &str, base: &str) -> bool {
    if base.is_empty() || base.len() >= name.len() {
        return false;
    }
    let bytes = name.as_bytes();
    name.match_indices(base).any(|(pos, _)| {
        let before_ok = pos == 0 || bytes[pos - 1] == b' ';
        let after = pos + base.len();
        let after_ok = after == bytes.len() || bytes[after] == b' ';
        before_ok && after_ok
    })
}

/// The only per-ore state GlobalSettings actually persists - name/category/volume come from DEFAULT_ORE_TABLE instead, since those never vary by user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrePriceEntry {
    pub name: String,
    #[serde(default)]
    pub price: f64,
}

/// Binding of a hotkey to a specific target profile ("quick switch")
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSwitchHotkey {
    #[serde(default, with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<u32>,
    pub target_profile: String,
}

/// Binding of a hotkey to an external application, matched by executable name at press time (mirrors AutoHotkey's ahk_exe).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppHotkey {
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<u32>,
    pub executable_name: String,
}

/// Binding of a hotkey to a URL, opened via ShellExecute (or, with upload_clipboard, POSTed as a paste upload first - see paste_upload).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UrlHotkey {
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<u32>,
    pub url: String,
    pub upload_clipboard: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GlobalSettings {
    pub last_used_profile: String,
    pub log_level: LogLevel,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_next_profile: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_previous_profile: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_cycle_all_clients_forward: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_cycle_all_clients_backward: Option<u32>,
    pub cycle_all_clients_respect_exclusions: bool,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_cycle_not_logged_in_forward: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_cycle_not_logged_in_backward: Option<u32>,
    #[serde(with = "vk_opt", skip_serializing_if = "Option::is_none")]
    pub hotkey_return_to_last_app: Option<u32>,
    pub profile_switch_hotkeys: Vec<ProfileSwitchHotkey>,
    pub app_hotkeys: Vec<AppHotkey>,
    pub url_hotkeys: Vec<UrlHotkey>,
    /// Written from the ESI lookup thread as well as the main thread, hence the lock.
    #[serde(deserialize_with = "deserialize_id_map", serialize_with = "serialize_id_map")]
    character_id_map: Mutex<BTreeMap<String, String>>,
    pub disable_update_checks: bool,
    pub run_on_startup: bool,
    pub auto_register_protocol: bool,
    pub always_on_top: bool,
    pub advanced_mode: bool,
    pub language: String,
    pub ore_table: Vec<OrePriceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog_y: Option<i32>,
}

fn deserialize_id_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Mutex<BTreeMap<String, String>>, D::Error> {
    string_map_lenient::deserialize(d).map(Mutex::new)
}

fn serialize_id_map<S: serde::Serializer>(map: &Mutex<BTreeMap<String, String>>, s: S) -> Result<S::Ok, S::Error> {
    lock(map).serialize(s)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Default for GlobalSettings {
    /// What a missing or unreadable global.settings.json loads as.
    fn default() -> Self {
        Self {
            last_used_profile: DEFAULT_PROFILE.to_owned(),
            log_level: LogLevel::Err,
            hotkey_next_profile: None,
            hotkey_previous_profile: None,
            hotkey_cycle_all_clients_forward: None,
            hotkey_cycle_all_clients_backward: None,
            cycle_all_clients_respect_exclusions: false,
            hotkey_cycle_not_logged_in_forward: None,
            hotkey_cycle_not_logged_in_backward: None,
            hotkey_return_to_last_app: None,
            profile_switch_hotkeys: Vec::new(),
            app_hotkeys: Vec::new(),
            url_hotkeys: Vec::new(),
            character_id_map: Mutex::new(BTreeMap::new()),
            disable_update_checks: false,
            run_on_startup: false,
            auto_register_protocol: true,
            always_on_top: true,
            advanced_mode: false,
            language: "en".to_owned(),
            ore_table: Vec::new(),
            dialog_x: None,
            dialog_y: None,
        }
    }
}

impl GlobalSettings {
    /// Volume never varies by user, so this reads DEFAULT_ORE_TABLE directly rather than the persisted ore_table.
    pub fn ore_volume(&self, name: &str) -> Option<f64> {
        DEFAULT_ORE_TABLE
            .iter()
            .find(|e| e.name == name)
            .or_else(|| DEFAULT_ORE_TABLE.iter().find(|e| is_grade_variant(name, e.name)))
            .map(|e| e.volume_m3)
    }

    /// Checks the persisted price override first, then falls back to DEFAULT_ORE_TABLE's snapshot price.
    pub fn ore_price(&self, name: &str) -> Option<f64> {
        let exact_override = self.ore_table.iter().find(|e| e.name == name).map(|e| e.price);
        let exact_default = || DEFAULT_ORE_TABLE.iter().find(|e| e.name == name).map(|e| e.price);
        let variant_override = || self.ore_table.iter().find(|e| is_grade_variant(name, &e.name)).map(|e| e.price);
        let variant_default = || DEFAULT_ORE_TABLE.iter().find(|e| is_grade_variant(name, e.name)).map(|e| e.price);
        exact_override.or_else(exact_default).or_else(variant_override).or_else(variant_default)
    }

    /// Load global settings from file, falling back to defaults on any missing/unreadable/malformed input, matching Config's load policy.
    pub fn load() -> GlobalSettings {
        let content = match read_limited(GLOBAL_SETTINGS_FILE) {
            Ok(c) => c,
            Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                SLOG.debug(format_args!("No global settings file, using defaults"));
                return GlobalSettings::default();
            }
            Err(err) => {
                // Usually transient (a sharing violation while the other process swaps the file in, an AV or sync lock), so leave the file alone rather than destroying the user's hotkeys.
                SLOG.err(format_args!("Failed to read global settings file: {err}"));
                return GlobalSettings::default();
            }
        };

        match serde_json::from_slice::<GlobalSettings>(&content) {
            Ok(settings) => {
                SLOG.info(format_args!("Loaded global settings: last profile = {}", settings.last_used_profile));
                settings
            }
            Err(err) => {
                // Kept as .corrupt (not deleted) so hand-edits or a newer version's file can be recovered.
                SLOG.warn(format_args!("Failed to parse global settings file ({err}), using defaults and setting it aside as {GLOBAL_SETTINGS_FILE}.corrupt"));
                if let Err(rename_err) = std::fs::rename(GLOBAL_SETTINGS_FILE, format!("{GLOBAL_SETTINGS_FILE}.corrupt")) {
                    SLOG.warn(format_args!("Failed to set aside corrupted global settings file: {rename_err}"));
                }
                GlobalSettings::default()
            }
        }
    }

    pub fn to_json_string(&self) -> Result<String, ConfigError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        atomic_write_file(GLOBAL_SETTINGS_FILE, self.to_json_string()?.as_bytes())?;
        SLOG.debug(format_args!("Saved global settings"));
        Ok(())
    }

    /// Log all global settings for debugging, one log line per JSON line.
    pub fn log_settings(&self) {
        match self.to_json_string() {
            Ok(json) => json.lines().for_each(|line| SLOG.debug(format_args!("{line}"))),
            Err(err) => SLOG.warn(format_args!("Failed to serialize global settings for logging: {err}")),
        }
    }

    pub fn update_last_used(&mut self, profile_name: &str) -> Result<(), ConfigError> {
        if !self.last_used_profile.is_empty() && self.last_used_profile == profile_name {
            return Ok(());
        }
        self.last_used_profile = profile_name.to_owned();
        self.save()
    }

    pub fn save_dialog_position(&mut self, x: i32, y: i32) -> Result<(), ConfigError> {
        self.dialog_x = Some(x);
        self.dialog_y = Some(y);
        self.save()
    }

    pub fn update_character_id(&self, character_name: &str, character_id: &str) -> Result<(), ConfigError> {
        {
            let mut map = lock(&self.character_id_map);
            if map.get(character_name).is_some_and(|existing| existing == character_id) {
                return Ok(());
            }
            map.insert(character_name.to_owned(), character_id.to_owned());
        }
        SLOG.info(format_args!("Cached character ID: {character_name} -> {character_id}"));
        self.save()
    }

    pub fn has_character_id(&self, character_name: &str) -> bool {
        lock(&self.character_id_map).contains_key(character_name)
    }

    pub fn character_id(&self, character_name: &str) -> Option<String> {
        lock(&self.character_id_map).get(character_name).cloned()
    }

    /// A copy of the whole character name -> ID map.
    pub fn character_id_map_snapshot(&self) -> BTreeMap<String, String> {
        lock(&self.character_id_map).clone()
    }

    /// Profile file names in the profiles directory (global.settings.json excluded), in directory order.
    pub fn enumerate_profiles() -> Result<Vec<String>, ConfigError> {
        let dir = match std::fs::read_dir(PROFILES_DIR) {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                SLOG.debug(format_args!("Profiles directory not found"));
                return Ok(Vec::new());
            }
            Err(err) => return Err(err.into()),
        };

        let mut profiles = Vec::new();
        for entry in dir {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else { continue };
            if name.ends_with(".json") && name != "global.settings.json" {
                profiles.push(name);
            }
        }
        SLOG.debug(format_args!("Found {} profile(s)", profiles.len()));
        Ok(profiles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ore_lookup_prefers_exact_then_override_then_variant() {
        let mut gs = GlobalSettings::default();
        assert_eq!(DEFAULT_ORE_TABLE.len(), 83);
        assert_eq!(gs.ore_volume("Veldspar"), Some(0.10));
        assert_eq!(gs.ore_volume("Nocxite II-Grade"), Some(4.00));
        assert_eq!(gs.ore_volume("Shining Loparite"), Some(10.00));
        assert_eq!(gs.ore_volume("Veldsparite"), None);
        assert_eq!(gs.ore_price("Veldspar"), Some(11.53));
        gs.ore_table.push(OrePriceEntry { name: "Veldspar".into(), price: 20.0 });
        assert_eq!(gs.ore_price("Veldspar"), Some(20.0));
        assert_eq!(gs.ore_price("Concentrated Veldspar"), Some(20.0));
    }

    #[test]
    fn json_defaults_and_round_trip() {
        let gs: GlobalSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(gs.last_used_profile, "default.json");
        assert_eq!(gs.log_level, LogLevel::Err);
        assert!(gs.always_on_top && gs.auto_register_protocol);

        let json = r#"{"logLevel":"debug","hotkeyNextProfile":"Ctrl+F9","characterIdMap":{"Bob":"123"},"urlHotkeys":[{"url":"x"}],"dialogX":5,"unknownField":1}"#;
        let gs: GlobalSettings = serde_json::from_str(json).unwrap();
        assert_eq!(gs.log_level, LogLevel::Debug);
        assert_eq!(gs.hotkey_next_profile, Some(0x278));
        assert_eq!(gs.character_id("Bob").as_deref(), Some("123"));
        let out = gs.to_json_string().unwrap();
        assert!(out.contains(r#""hotkeyNextProfile": "0x278""#), "{out}");
        assert!(out.contains(r#""logLevel": "debug""#));
        assert!(!out.contains("hotkeyPreviousProfile") && !out.contains("dialogY"));
        assert!(serde_json::from_str::<GlobalSettings>(r#"{"characterIdMap":{"Bob":1}}"#).is_err());
        assert!(serde_json::from_str::<GlobalSettings>(r#"{"characterIdMap":[]}"#).is_ok());
    }
}
