use serde_json::json;

use super::*;
use crate::types::{BorderStyle, NotificationType, TextPosition};

fn parse(json: serde_json::Value) -> Config {
    Config::build_config_from_json(json.to_string().as_bytes(), "test.json").expect("profile should parse")
}

#[test]
fn defaults_round_trip_through_json() {
    let cfg = Config::defaults_with_profile("default.json").unwrap();
    let json = cfg.to_json_string().unwrap();
    let again = Config::build_config_from_json(json.as_bytes(), "default.json").unwrap();
    assert_eq!(again.to_json_string().unwrap(), json);

    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["app"], "eve-maj-preview");
    assert_eq!(v["formatVersion"], 2);
    assert_eq!(v["thumbnail"]["borderColor"], "0xFFD9A441");
    assert_eq!(v["thumbnail"]["characterNameColor"], "0x00FFFFFF");
    assert_eq!(v["thumbnail"]["active"], json!({}));
    assert_eq!(v["thumbnail"]["inactive"], json!({ "showThumbnail": true }));
    assert_eq!(v["thumbnail"]["notifications"]["type_configs"].as_object().unwrap().len(), NotificationType::COUNT);
    assert_eq!(v["windowFilters"][0]["class_names"][0], "trinityWindow");
    assert_eq!(v["quickGroups"], json!([]));
    assert!(v["display"].get("regionX").is_none(), "null optionals are omitted");
    assert!(v["hotkeys"].get("hotkeyMinimizeAll").is_none());
}

#[test]
fn top_level_key_order_matches_zig_output() {
    let json = Config::defaults_with_profile("x.json").unwrap().to_json_string().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "app", "formatVersion", "thumbnail", "timer", "display", "snapping", "interaction", "autoMinimize",
            "autoMovePosition", "exclusion", "closeAll", "chatlog", "combat", "mining", "bounty", "resources", "travel",
            "accentColor", "windowFilters", "characters", "systemColors", "hotkeyGroups", "accountHotkeys",
            "quickGroups", "hotkeys"
        ]
    );
}

#[test]
fn parses_a_hand_edited_profile() {
    std::env::set_var("EVE_MAJ_TEST_LOGS", "C:\\Users\\pilot\\Logs");
    let cfg = parse(json!({
        "thumbnail": {
            "width": 320,
            "borderColor": "#00FF00",
            "inactiveBorderStyle": "Dashed",
            "notifications": { "type_configs": { "TakingDamage": { "tts_enabled": true, "border_color": "0xFFFF0000" }, "NotAType": {} } }
        },
        "chatlog": { "enabled": true, "chatlogDir": "%EVE_MAJ_TEST_LOGS%/Chatlogs" },
        "characters": [
            { "name": "Pilot One", "hotkey": "Ctrl+F1", "nameColor": "0xFF112233", "opacity": 10, "thumbnailSize": { "width": 5 } },
            { "name": "Pilot Two", "displayName": "Two", "borderColors": { "activeBorderColor": "0xFF00FF00" } }
        ],
        "systemColors": [ { "systemName": "Jita", "color": "0xFFFF0000" }, { "systemName": "J*", "color": "0xFF0000FF" } ],
        "hotkeyGroups": [ { "name": "Fleet", "characters": ["Pilot One"], "forwardKey": "0x70" } ],
        "quickGroups": [ { "name": "Quick", "assignKey": "F2" } ],
        "hotkeys": { "hotkeyMinimizeAll": "0x278" },
        "someFutureField": 1
    }));

    assert_eq!(cfg.thumbnail.width, 320);
    assert_eq!(cfg.thumbnail.border_color, 0x00FF00);
    assert_eq!(cfg.thumbnail.inactive_border_style, BorderStyle::Dashed);
    let damage = cfg.thumbnail.notifications.type_config(NotificationType::TakingDamage);
    assert!(damage.tts_enabled);
    assert_eq!(damage.border_color, Some(0xFFFF0000));
    assert_eq!(damage.duration_ms, 10000, "unset fields keep defaults");

    assert_eq!(cfg.chatlog.chatlog_dir, "C:/Users/pilot/Logs/Chatlogs");
    assert!(cfg.chatlog.gamelog_dir.ends_with("/EVE/logs/Gamelogs"), "empty dir falls back to the default");

    let one = cfg.find_character("Pilot One").unwrap();
    assert_eq!(one.hotkey, Some(crate::virtual_keys::combine_key(0x70, crate::virtual_keys::MOD_CONTROL)));
    assert_eq!(one.opacity, Some(ThumbnailConfig::OPACITY_MIN), "validate clamps per-character opacity");
    assert_eq!(one.thumbnail_size.unwrap().width, Some(ThumbnailConfig::WIDTH_MIN));
    assert_eq!(cfg.display_name("Pilot Two"), "Two");
    assert_eq!(cfg.display_name("Nobody"), "Nobody");

    assert_eq!(cfg.find_system_color("jita"), Some(0xFFFF0000));
    assert_eq!(cfg.find_system_color("J123"), Some(0xFF0000FF));

    assert_eq!(cfg.hotkey_groups.len(), 2, "quick groups merge into hotkey groups");
    let quick = &cfg.hotkey_groups[1];
    assert!(quick.temporary_membership && quick.show_badge);
    assert_eq!(quick.assign_key, Some(0x71));
    assert_eq!(cfg.hotkeys.hotkey_minimize_all, Some(0x278));

    let out: serde_json::Value = serde_json::from_str(&cfg.to_json_string().unwrap()).unwrap();
    assert_eq!(out["quickGroups"], json!([]));
    assert_eq!(out["hotkeyGroups"][1]["characters"], json!([]));
    assert_eq!(out["characters"][0]["hotkey"], "0x270");
    assert_eq!(out["hotkeys"]["hotkeyMinimizeAll"], "0x278");
}

#[test]
fn malformed_profiles_are_rejected() {
    let bad = [
        json!({ "characters": [ { "displayName": "no name" } ] }),
        json!({ "thumbnail": { "borderColor": 123 } }),
        json!({ "thumbnail": { "borderColor": "not hex" } }),
        json!({ "thumbnail": { "borderWidth": 300 } }),
        json!({ "characters": [ { "name": "x", "hotkey": "Hyper+Q" } ] }),
        json!({ "systemColors": [ { "systemName": "Jita" } ] }),
    ];
    for profile in bad {
        assert!(Config::build_config_from_json(profile.to_string().as_bytes(), "x.json").is_err(), "accepted {profile}");
    }
    assert!(Config::build_config_from_json(b"{ not json", "x.json").is_err());
}

#[test]
fn validation_clamps_out_of_range_values() {
    let cfg = parse(json!({
        "thumbnail": { "width": 1, "height": 99999, "thumbnailOpacity": 0, "notifications": { "tts_rate": -100, "type_configs": { "Generic": { "throttle_ms": 999999999 } } } },
        "timer": { "scanIntervalMs": 1 },
        "display": { "monitorIndex": 42, "regionX": -99999, "displayGrid": { "size": 20 } },
        "combat": { "window_seconds": 0 },
        "mining": { "idle_alert_window_seconds": 0 },
        "travel": { "threshold_percent": 500.0 }
    }));
    assert_eq!(cfg.thumbnail.width, ThumbnailConfig::WIDTH_MIN);
    assert_eq!(cfg.thumbnail.height, ThumbnailConfig::HEIGHT_MAX);
    assert_eq!(cfg.thumbnail.thumbnail_opacity, ThumbnailConfig::OPACITY_MIN);
    assert_eq!(cfg.thumbnail.notifications.tts_rate, ThumbnailConfig::TTS_RATE_MIN);
    assert_eq!(cfg.thumbnail.notifications.type_config(NotificationType::Generic).throttle_ms, ThumbnailConfig::NOTIFICATION_THROTTLE_MS_MAX);
    assert_eq!(cfg.timer.scan_interval_ms, TimerConfig::SCAN_INTERVAL_MS_MIN);
    assert_eq!(cfg.display.monitor_index, Some(DisplayConfig::MONITOR_INDEX_MAX));
    assert_eq!(cfg.display.region_x, Some(DisplayConfig::START_X_MIN));
    assert_eq!(cfg.display.display_grid.size, crate::display_grid::MAX_DIM);
    assert_eq!(cfg.combat.window_seconds, 60);
    assert_eq!(cfg.mining.idle_alert_window_seconds, 15);
    assert_eq!(cfg.travel.threshold_percent, 100.0);
}

#[test]
fn legacy_profiles_keep_positions_off_windows() {
    let cfg = parse(json!({ "formatVersion": 1, "characters": [ { "name": "A", "position": { "x": 100, "y": 50 } } ] }));
    // scale_from_legacy_dpi_unaware is a no-op off Windows; on Windows it depends on the monitor's DPI.
    if !cfg!(windows) {
        assert_eq!(cfg.character_position("A"), Some(Position { x: 100, y: 50 }));
    }
}

#[test]
fn live_preview_patches_merge_fields() {
    let mut cfg = Config::defaults_with_profile("x.json").unwrap();
    let patch = json!({
        "width": 400,
        "borderWidth": 999,
        "borderStyle": "Nonsense",
        "characterNamePosition": "Center",
        "characterNameFontName": "Consolas",
        "notifications": { "font_size": 20, "type_configs": { "Decloak": { "sound_path": "C:/a.wav", "border_color": null } } }
    });
    cfg.thumbnail.border_style = BorderStyle::Dotted;
    cfg.thumbnail.apply_json(patch.as_object().unwrap()).unwrap();
    assert_eq!(cfg.thumbnail.width, 400);
    assert_eq!(cfg.thumbnail.border_width, 2, "out-of-range integers are ignored");
    assert_eq!(cfg.thumbnail.border_style, BorderStyle::Solid, "unknown enum names reset to the default");
    assert_eq!(cfg.thumbnail.character_name_position, TextPosition::Center);
    assert_eq!(cfg.thumbnail.character_name_font_name, "Consolas");
    assert_eq!(cfg.thumbnail.notifications.font_size, 20);
    assert_eq!(cfg.thumbnail.notifications.type_config(NotificationType::Decloak).sound_path.as_deref(), Some("C:/a.wav"));
    assert!(cfg.thumbnail.apply_json(json!({ "borderColor": "zz" }).as_object().unwrap()).is_err());

    let display_patch = json!({ "layoutMode": "RegionFit", "viewMode": "Bogus", "regionX": 5, "monitorIndex": null, "displayGrid": { "columns": 2, "rows": 1 } });
    cfg.display.monitor_index = Some(1);
    cfg.display.apply_json(display_patch.as_object().unwrap()).unwrap();
    assert_eq!(cfg.display.layout_mode, crate::types::LayoutMode::RegionFit);
    assert_eq!(cfg.display.view_mode, crate::types::ViewMode::Thumbnails, "unknown names leave display enums alone");
    assert_eq!(cfg.display.region_x, Some(5));
    assert_eq!(cfg.display.monitor_index, None);
    assert_eq!(cfg.display.display_grid.cell_count(), 2);

    let overrides = json!([
        { "name": "New Pilot", "displayName": "NP", "opacity": 3, "nameColor": "0xFF0000FF", "position": { "x": 1, "y": 2 } },
        { "name": "New Pilot", "displayName": "", "thumbnailSize": { "width": 9999 } },
        { "noName": true }
    ]);
    cfg.apply_character_overrides_from_json(overrides.as_array().unwrap()).unwrap();
    let c = cfg.find_character("New Pilot").unwrap();
    assert_eq!(c.display_name, None);
    assert_eq!(c.opacity, Some(ThumbnailConfig::OPACITY_MIN));
    assert_eq!(c.name_color, Some(0xFF0000FF));
    assert_eq!(c.position, Some(Position { x: 1, y: 2 }));
    assert_eq!(c.thumbnail_size, Some(CharacterThumbnailSize { width: Some(ThumbnailConfig::WIDTH_MAX), height: None }));
    assert_eq!(cfg.characters.len(), 1);

    cfg.replace_system_colors_from_json(json!([{ "systemName": "Amarr", "color": "#FF0000" }]).as_array().unwrap()).unwrap();
    assert_eq!(cfg.find_system_color("amarr"), Some(0xFF0000));
    assert!(cfg.replace_system_colors_from_json(json!([{ "systemName": "Bad" }]).as_array().unwrap()).is_err());
    assert_eq!(cfg.system_colors.len(), 1, "a failed replace keeps the old list");

    cfg.hotkey_groups = vec![HotkeyGroup::default(), HotkeyGroup::default()];
    cfg.apply_group_badge_preview_from_json(json!([true]).as_array().unwrap());
    assert!(!cfg.hotkey_groups[0].show_badge, "length mismatch skips the preview");
    cfg.apply_group_badge_preview_from_json(json!([true, "x"]).as_array().unwrap());
    assert!(cfg.hotkey_groups[0].show_badge && !cfg.hotkey_groups[1].show_badge);
}

#[test]
fn validation_ranges_json_lists_every_bound_in_order() {
    let v: serde_json::Value = serde_json::from_str(&Config::build_validation_ranges_json()).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.len(), 69);
    assert_eq!(obj.keys().next().unwrap(), "timer.scanIntervalMs");
    assert_eq!(obj["thumbnail.notifications.tts_rate"], json!({ "min": -10, "max": 10 }));
    assert_eq!(obj["resources.offset_y"], json!({ "min": -50, "max": 50 }));
}

#[test]
fn environment_expansion_edge_cases() {
    std::env::set_var("EVE_MAJ_TEST_A", "x\\y");
    assert_eq!(expand_environment_variables("plain"), "plain");
    assert_eq!(expand_environment_variables("%EVE_MAJ_TEST_A%/z"), "x/y/z");
    assert_eq!(expand_environment_variables("a%%b"), "a%%b");
    assert_eq!(expand_environment_variables("50%"), "50%");
    assert_eq!(expand_environment_variables("%EVE_MAJ_NOT_SET%/q"), "%EVE_MAJ_NOT_SET%/q");
}

#[test]
fn profile_names() {
    assert!(profile_path("../x.json").is_err());
    assert_eq!(profile_path("pvp.json").unwrap(), PathBuf::from("profiles").join("pvp.json"));
    assert_eq!(clamp_profile_name("a-very-long-profile-name"), "a-very-long-prof");
}

/// Everything that touches the working directory runs here, inside a scratch directory.
#[test]
fn file_round_trips_in_a_scratch_directory() {
    let dir = std::env::temp_dir().join(format!("eve-maj-config-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();

    let result = std::panic::catch_unwind(|| {
        // First load creates profiles/default.json.
        let cfg = Config::load().unwrap();
        assert!(std::path::Path::new("profiles/default.json").exists());
        assert_eq!(cfg.profile_name, "default.json");

        // A missing profile falls back to the default; an unsafe one is refused.
        assert_eq!(Config::load_profile("missing.json").unwrap().profile_name, "default.json");
        assert_eq!(Config::load_profile("../evil.json").unwrap().profile_name, "default.json");

        // Saving a position rewrites the file; reloading sees it.
        let mut pvp = Config::defaults_with_profile("pvp.json").unwrap();
        pvp.save_character_position("Scout", Position { x: 7, y: 8 }).unwrap();
        let reloaded = Config::load_profile("pvp.json").unwrap();
        assert_eq!(reloaded.character_position("Scout"), Some(Position { x: 7, y: 8 }));

        // A corrupt profile loads as defaults instead of failing.
        std::fs::write("profiles/broken.json", "{").unwrap();
        assert!(Config::load_profile("broken.json").unwrap().characters.is_empty());

        // Live-preview revert drops unsaved characters and restores the saved look.
        let mut live = Config::load_profile("pvp.json").unwrap();
        live.thumbnail.width = 999;
        live.get_or_create_character("Unsaved");
        live.display.start_x = 1234;
        live.reload_thumbnail_config_from_disk().unwrap();
        assert_eq!(live.thumbnail.width, 200);
        assert!(live.find_character("Unsaved").is_none());
        assert_eq!(live.display.start_x, 1234, "start position survives a revert");

        // Unique colors persist to colors.json when the config drops.
        {
            let mut colored = Config::load_profile("pvp.json").unwrap();
            colored.thumbnail.use_unique_system_colors = true;
            let first = colored.system_name_color("Jita");
            assert_eq!(colored.system_name_color("JITA"), first);
        }
        assert!(std::fs::read_to_string("colors.json").unwrap().contains("Jita"));

        // Global settings: defaults when absent, corrupt file set aside.
        let gs = GlobalSettings::load();
        assert_eq!(gs.last_used_profile, "default.json");
        std::fs::write(GLOBAL_SETTINGS_FILE, "[").unwrap();
        let _ = GlobalSettings::load();
        assert!(std::path::Path::new("profiles/global.settings.json.corrupt").exists());
        let mut gs = GlobalSettings::default();
        gs.update_last_used("pvp.json").unwrap();
        assert_eq!(GlobalSettings::load().last_used_profile, "pvp.json");
        let mut profiles = GlobalSettings::enumerate_profiles().unwrap();
        profiles.sort();
        assert_eq!(profiles, ["broken.json", "default.json", "pvp.json"]);
    });

    std::env::set_current_dir(previous).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
