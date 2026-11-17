//! Answers the dialog page's `webui.call(...)` requests: the Zig build's config_dialog.zig bound functions, on top of
//! eve_maj_core. Replies are the same strings the Zig build returns (mostly JSON); see the call contract in the
//! comments of each arm. Live actions go to the running preview app over its Unix socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eve_maj_core::config::{self, clamp_profile_name, parse_hex_color, profile_path, Config, GlobalSettings, Position, DEFAULT_ORE_TABLE, PROFILES_DIR};
use eve_maj_core::display_info::{self, Display};
use eve_maj_core::log::Scope;
use eve_maj_core::protocol::{region_select_result_path, Command, RegionSelectRequest, RegionSelectResult, WireMessage};
use eve_maj_core::{esi_prices, eve_accounts, ultra_potato, update};
use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use super::server::{Asset, Handler};
use super::UiEvent;

const SLOG: Scope = Scope::new("config");
const DEFAULT_PROFILE: &str = "default.json";

/// Which native picker a browse call opens.
#[derive(Debug, Clone, Copy)]
pub enum Browse {
    ChatlogDir,
    GamelogDir,
    SoundFile,
}

pub struct Backend {
    proxy: EventLoopProxy<UiEvent>,
    /// The profile file the dialog edits (not necessarily the one the app runs).
    profile: String,
    page: Vec<u8>,
}

fn ok() -> String {
    json!({ "success": true }).to_string()
}

fn fail(error: &str) -> String {
    json!({ "success": false, "error": error }).to_string()
}

fn arg(args: &[Value], i: usize) -> String {
    match args.get(i) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

const LANGS: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../../ui/lang/en.json")),
    ("de", "Deutsch", include_str!("../../ui/lang/de.json")),
    ("ru", "Русский", include_str!("../../ui/lang/ru.json")),
    ("zh", "简体中文", include_str!("../../ui/lang/zh.json")),
    ("pt", "Português", include_str!("../../ui/lang/pt.json")),
    ("es", "Español", include_str!("../../ui/lang/es.json")),
    ("fr", "Français", include_str!("../../ui/lang/fr.json")),
    ("pl", "Polski", include_str!("../../ui/lang/pl.json")),
];

/// English hints that describe Windows behaviour, reworded for what the Linux build does. Other languages keep the
/// shared wording until they're translated.
const LINUX_EN_HINTS: &[(&str, &str)] = &[
    ("tab.notifications.section.text-to-speech.hint", "Speak alerts aloud through speech-dispatcher (spd-say) or espeak-ng. Enable individual event types below under each event's \"TTS\" setting."),
    ("tab.behavior.section.startup.hint", "Launch at login (an XDG autostart entry), and register the evemajpreview:// URL protocol for external control (Stream Deck, scripts, etc.)."),
    ("tab.displays.section.displays.hint", "Every display connected to this PC, arranged the way Hyprland lays them out, with the resolution, refresh rate and scaling each one is running. View only: EVE-Maj Preview never changes your display settings. Click a display to highlight its details, or use Identify to show each display's number on screen."),
    ("tab.resources.section.resource-usage-overlay.hint", "Displays each client's CPU% and RAM usage on its thumbnail, read from /proc rather than from EVE's logs. VRAM isn't available on Linux."),
    ("field.hotkeyToggleExclusion.hint", "Applies to whichever EVE window currently has focus, not the thumbnail under your cursor."),
    ("field.regionDisplayUseWorkArea.label", "Leave Room for Bars"),
    ("field.regionDisplayUseWorkArea.hint", "Fills the display's work area so thumbnails don't sit under Waybar or other bars that reserve space."),
    ("tab.accounts.section.characters.hint", "Every character that has logged in on this PC, found in EVE's settings folders (CCP/EVE under the client's Wine prefix, drive_c/users/*/AppData/Local). Accounts are suggested by matching each character's settings file to the account settings file EVE saved at the same moment, so check a suggestion before accepting it. Changes save automatically."),
];

/// A language's catalog JSON, with the Linux rewording applied to English.
fn catalog_json(code: &str, catalog: &str) -> String {
    if code != "en" {
        return catalog.to_owned();
    }
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(catalog) else { return catalog.to_owned() };
    for (key, text) in LINUX_EN_HINTS {
        if map.contains_key(*key) {
            map.insert((*key).to_owned(), Value::String((*text).to_owned()));
        } else {
            SLOG.warn(format_args!("Linux wording for a missing translation key: {key}"));
        }
    }
    Value::Object(map).to_string()
}

/// The page with styles, script, fonts, images and translations inlined, as the Zig build's injectResources does.
fn assemble_page(language: &str) -> Vec<u8> {
    let (lang, _, catalog) = LANGS.iter().find(|(code, _, _)| *code == language).unwrap_or(&LANGS[0]);
    let catalog = catalog_json(lang, catalog);
    let mut html = include_str!("../../ui/config_dialog.html").to_owned();
    let replace = |html: &mut String, needle: &str, with: &str| {
        if html.contains(needle) {
            *html = html.replacen(needle, with, 1);
        } else {
            SLOG.warn(format_args!("Dialog page template is missing {needle:?}"));
        }
    };
    replace(&mut html, "/* Styles will be injected here by WebUI */", include_str!("../../ui/config_dialog.css"));
    replace(&mut html, "LAYOUT_PREVIEW_IMAGE_PLACEHOLDER", &base64(include_bytes!("../../ui/layout_preview.jpg")));
    replace(&mut html, "CASCADIA_CODE_WOFF2_PLACEHOLDER", &base64(include_bytes!("../../ui/CascadiaCode.woff2")));
    // The page stays hidden (html.pre-init) until the backend reveals it; here that happens as soon as it's set up.
    let script = format!(
        "{}\nwindow.addEventListener('DOMContentLoaded', () => setTimeout(() => document.documentElement.classList.remove('pre-init'), 0));",
        include_str!("../../ui/config_dialog.js")
    );
    replace(&mut html, "// Script will be injected here by WebUI", &script);
    replace(
        &mut html,
        "<!-- Favicon will be injected here by WebUI -->",
        &format!("<link rel=\"icon\" type=\"image/x-icon\" href=\"data:image/x-icon;base64,{}\">", base64(include_bytes!("../../../../assets/icon.ico"))),
    );
    replace(&mut html, "window.__I18N__ = {}; /* Translations will be injected here by WebUI */", &format!("window.__I18N__ = {catalog};"));
    let names: serde_json::Map<String, Value> = LANGS.iter().map(|(c, n, _)| (c.to_string(), Value::String(n.to_string()))).collect();
    replace(
        &mut html,
        "window.__I18N_LANGS__ = {}; /* Language list will be injected here by WebUI */",
        &format!("window.__I18N_LANGS__ = {};", Value::Object(names)),
    );
    let all = LANGS.iter().map(|(c, _, cat)| format!("\"{c}\":{}", catalog_json(c, cat))).collect::<Vec<_>>().join(",");
    replace(&mut html, "window.__I18N_ALL__ = {}; /* All translations will be injected here by WebUI */", &format!("window.__I18N_ALL__ = {{{all}}};"));
    replace(&mut html, "<html lang=\"en\" class=\"pre-init\">", &format!("<html lang=\"{lang}\" class=\"pre-init\">"));
    html.into_bytes()
}

// ------------------------------------------------------------------------------------------------ the preview app --

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("eve-maj-preview.sock")
}

fn main_app_running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

/// Sends a dialog command to the running preview app: "evemaj-cmd:<id>\n" then the payload, the same id/payload pair
/// the Windows build sends with WM_COPYDATA.
pub fn send_command(cmd: &Command) -> bool {
    let WireMessage::CopyData { dw_data, payload } = cmd.to_wire() else { return false };
    let Ok(mut s) = UnixStream::connect(socket_path()) else { return false };
    let mut msg = format!("evemaj-cmd:{dw_data}\n").into_bytes();
    if let Some(p) = payload {
        msg.extend_from_slice(&p);
    }
    s.write_all(&msg).is_ok()
}

// ---------------------------------------------------------------------------------------------- Hyprland queries --

fn hyprctl_json(what: &str) -> Option<Value> {
    let out = std::process::Command::new("hyprctl").args(["-j", what]).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// Mapped EVE client windows ("EVE - Name"), oldest process first: (name, x, y, minimized).
fn eve_clients() -> Vec<(String, i64, i64, bool)> {
    let Some(Value::Array(list)) = hyprctl_json("clients") else { return Vec::new() };
    let mut out: Vec<(i64, String, i64, i64, bool)> = list
        .iter()
        .filter(|c| c["mapped"].as_bool().unwrap_or(false))
        .filter_map(|c| {
            let name = c["title"].as_str()?.strip_prefix("EVE - ")?.trim().to_owned();
            let at = c["at"].as_array()?;
            let minimized = c["workspace"]["name"].as_str().is_some_and(|w| w == "special:evemaj");
            Some((c["pid"].as_i64().unwrap_or(0), name, at.first()?.as_i64()?, at.get(1)?.as_i64()?, minimized))
        })
        .collect();
    out.sort_by_key(|c| c.0);
    out.into_iter().map(|(_, n, x, y, m)| (n, x, y, m)).collect()
}

fn process_image(pid: i64) -> String {
    // Wine/Proton processes report the Windows exe as argv[0]; native ones are better named by /proc/pid/exe.
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let argv0 = String::from_utf8_lossy(cmdline.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
    let base = |s: &str| s.rsplit(['/', '\\']).next().unwrap_or("").to_owned();
    if argv0.to_ascii_lowercase().ends_with(".exe") {
        return base(&argv0);
    }
    std::fs::read_link(format!("/proc/{pid}/exe")).map(|p| base(&p.to_string_lossy())).unwrap_or_else(|_| base(&argv0))
}

fn running_windows() -> String {
    let Some(Value::Array(list)) = hyprctl_json("clients") else { return "[]".into() };
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for c in &list {
        let (Some(class), Some(title)) = (c["class"].as_str(), c["title"].as_str()) else { continue };
        if title.is_empty() || !c["mapped"].as_bool().unwrap_or(false) {
            continue;
        }
        let exe = process_image(c["pid"].as_i64().unwrap_or(0));
        let key = (class.to_owned(), exe.to_ascii_lowercase());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(json!({ "class": class, "exe": exe, "title": title }));
    }
    out.sort_by_key(|v| v["exe"].as_str().unwrap_or("").to_ascii_lowercase());
    Value::Array(out).to_string()
}

fn displays() -> String {
    let Some(Value::Array(list)) = hyprctl_json("monitors") else { return display_info::EMPTY_DISPLAYS_JSON.into() };
    let mut displays: Vec<Display> = list
        .iter()
        .map(|m| {
            let i = |k: &str| m[k].as_i64().unwrap_or(0) as i32;
            let scale = m["scale"].as_f64().unwrap_or(1.0);
            let transform = m["transform"].as_i64().unwrap_or(0);
            let (w, h) = (i("width"), i("height"));
            // Layout-space size: rotated and scaled like Hyprland places it.
            let (lw, lh) = if transform % 2 == 1 { (h, w) } else { (w, h) };
            let (lw, lh) = ((lw as f64 / scale).round() as i32, (lh as f64 / scale).round() as i32);
            let reserved: Vec<i32> = m["reserved"].as_array().map(|r| r.iter().map(|v| v.as_i64().unwrap_or(0) as i32).collect()).unwrap_or_default();
            let res = |k: usize| reserved.get(k).copied().unwrap_or(0);
            let bounds = display_info::Rect { x: i("x"), y: i("y"), width: lw, height: lh };
            let name = format!("{} {}", m["make"].as_str().unwrap_or(""), m["model"].as_str().unwrap_or("")).trim().to_owned();
            Display {
                number: i("id") as u32 + 1,
                id: m["name"].as_str().unwrap_or("").to_owned(),
                gdi_name: m["name"].as_str().unwrap_or("").to_owned(),
                name: if name.is_empty() { m["name"].as_str().unwrap_or("").to_owned() } else { name },
                gpu: None,
                primary: i("id") == 0,
                bounds,
                work_area: display_info::Rect { x: bounds.x + res(0), y: bounds.y + res(1), width: lw - res(0) - res(2), height: lh - res(1) - res(3) },
                mode_width: w as u32,
                mode_height: h as u32,
                refresh_hz: m["refreshRate"].as_f64().unwrap_or(0.0).round() as u32,
                bits_per_pixel: 32,
                orientation: (transform % 4) as u32 * 90,
                scale_percent: (scale * 100.0).round() as u32,
                max_width: w as u32,
                max_height: h as u32,
                max_refresh_hz: m["refreshRate"].as_f64().unwrap_or(0.0).round() as u32,
            }
        })
        .collect();
    displays.sort_by_key(|d| d.number);
    display_info::displays_json(&displays)
}

/// The startup notice's status. The dialog only asks when update checks are enabled, so the first ask starts the
/// background check (it asks again a few seconds later if nothing was known yet).
fn update_status() -> String {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("update_check".into()).spawn(update::UpdateChecker::check_for_updates_background) {
            SLOG.warn(format_args!("Failed to start update check thread: {e}"));
        }
    });
    let s = &update::UPDATE_STATUS;
    json!({ "available": s.is_available(), "version": s.version(), "url": s.url(), "notes": s.notes() }).to_string()
}

/// The Updates tab's check: whether GitHub has a newer release, with `manual` set so the tab links to it instead of
/// offering the Windows zip.
fn check_for_update_now() -> String {
    match update::UpdateChecker::new().check_for_updates() {
        Ok(None) => json!({ "success": true, "available": false, "currentVersion": update::CURRENT_VERSION }).to_string(),
        Ok(Some(info)) => {
            update::UPDATE_STATUS.set(&info.version, &info.url, info.notes.as_deref());
            json!({
                "success": true, "available": true, "manual": true, "currentVersion": update::CURRENT_VERSION,
                "version": info.version, "url": info.url, "notes": info.notes,
                "assetName": null, "assetSize": 0, "hasDigest": false, "staged": false,
            })
            .to_string()
        }
        Err(e) => {
            SLOG.warn(format_args!("Manual update check failed: {e}"));
            json!({ "success": false, "error": "check_failed" }).to_string()
        }
    }
}

/// Shows each monitor's number (the ones getDisplays lists) for a few seconds, through the preview binary's
/// `--identify-displays` helper; a click while it's still up is ignored, like the Windows build.
fn identify_displays() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return true;
    }
    let targets: Vec<String> = match hyprctl_json("monitors") {
        Some(Value::Array(list)) => list.iter().filter_map(|m| Some(format!("{}={}", m["name"].as_str()?, m["id"].as_i64()? + 1))).collect(),
        _ => Vec::new(),
    };
    // Next to this binary, else on PATH (as the preview app finds this one).
    let sibling = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("eve-maj-preview"))).filter(|p| p.is_file());
    let exe = sibling.unwrap_or_else(|| PathBuf::from("eve-maj-preview"));
    let child = if targets.is_empty() {
        Err(std::io::Error::other("hyprctl listed no monitors"))
    } else {
        std::process::Command::new(&exe).arg("--identify-displays").args(&targets).stdin(std::process::Stdio::null()).spawn()
    };
    match child {
        // Waited on from its own thread, so it's reaped as soon as it exits and the next click can show it again.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
                RUNNING.store(false, Ordering::Release);
            });
            true
        }
        Err(e) => {
            SLOG.warn(format_args!("Couldn't start {}: {e}", exe.display()));
            RUNNING.store(false, Ordering::Release);
            false
        }
    }
}

/// Points LOCALAPPDATA at the running EVE client's Wine prefix (WINEPREFIX or Proton's STEAM_COMPAT_DATA_PATH/pfx),
/// so the EVE settings-folder scans (accounts, Ultra Potato) find the client's files.
pub fn set_local_app_data_from_wine() {
    if std::env::var_os("LOCALAPPDATA").is_some() {
        return;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else { return };
    for p in procs.filter_map(|e| e.ok()) {
        let dir = p.path();
        let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default().to_ascii_lowercase();
        if !cmdline.windows(11).any(|w| w == b"exefile.exe") && !cmdline.windows(15).any(|w| w == b"eve-online.exe") {
            continue;
        }
        let Ok(environ) = std::fs::read(dir.join("environ")) else { continue };
        let var = |name: &str| environ.split(|&b| b == 0).find_map(|kv| kv.strip_prefix(format!("{name}=").as_bytes())).map(|v| PathBuf::from(String::from_utf8_lossy(v).into_owned()));
        for prefix in [var("WINEPREFIX"), var("STEAM_COMPAT_DATA_PATH").map(|p| p.join("pfx"))].into_iter().flatten() {
            let Ok(users) = std::fs::read_dir(prefix.join("drive_c/users")) else { continue };
            for u in users.filter_map(|e| e.ok()) {
                let local = u.path().join("AppData/Local");
                if local.join("CCP/EVE").is_dir() {
                    SLOG.info(format_args!("Using EVE's Wine prefix for settings scans: {}", local.display()));
                    std::env::set_var("LOCALAPPDATA", &local);
                    return;
                }
            }
        }
    }
}

/// Run on login (runOnStartup): an XDG autostart entry for the preview app next to this binary.
fn apply_run_on_startup(enabled: bool) {
    let dir = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("autostart"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/autostart"),
    };
    let file = dir.join("eve-maj-preview.desktop");
    if !enabled {
        let _ = std::fs::remove_file(&file);
        return;
    }
    let Some(exe) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("eve-maj-preview"))) else { return };
    let contents = format!("[Desktop Entry]\nType=Application\nName=EVE-Maj Preview\nExec={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n", exe.display());
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, contents)) {
        SLOG.warn(format_args!("Couldn't write {}: {e}", file.display()));
    }
}

/// A profile file, or a backup to restore ("backup\<file>" / "backup/<file>"), as a path; None if the name is unsafe.
fn source_profile_path(source: &str) -> Option<PathBuf> {
    match source.strip_prefix("backup\\").or_else(|| source.strip_prefix("backup/")) {
        Some(file) => config::is_safe_profile_name(file).then(|| Path::new(PROFILES_DIR).join("backup").join(file)),
        None => profile_path(source).ok(),
    }
}

impl Backend {
    pub fn new(proxy: EventLoopProxy<UiEvent>, profile: String, language: &str) -> Self {
        Backend { proxy, profile, page: assemble_page(language) }
    }

    fn browse(&self, kind: Browse) -> String {
        let (tx, rx) = mpsc::channel();
        if self.proxy.send_event(UiEvent::Browse(kind, tx)).is_err() {
            return String::new();
        }
        rx.recv().unwrap_or_default()
    }

    fn load_config(&self) -> String {
        match Config::load_profile(&self.profile).and_then(|c| c.to_json_string()) {
            Ok(s) => s,
            Err(e) => {
                SLOG.err(format_args!("Failed to load {}: {e}", self.profile));
                json!({ "error": "Failed to read config file" }).to_string()
            }
        }
    }

    fn save_config(&self, text: &str) -> String {
        let cfg = match Config::build_config_from_json(text.as_bytes(), &self.profile) {
            Ok(c) => c,
            Err(e) => {
                SLOG.err(format_args!("Rejected configuration: {e}"));
                return fail("Invalid configuration");
            }
        };
        let Ok(path) = profile_path(&self.profile) else { return fail("Failed to write file") };
        if let Err(e) = cfg.save_to_json_file(path) {
            SLOG.err(format_args!("Failed to save {}: {e}", self.profile));
            return fail("Failed to write file");
        }
        // The running app reloads a changed profile file by itself; this makes the saved one the live one.
        send_command(&Command::Profile(self.profile.clone().into_bytes()));
        ok()
    }

    fn list_profiles(&self) -> String {
        let mut profiles = GlobalSettings::enumerate_profiles().unwrap_or_default();
        profiles.retain(|p| p != eve_maj_core::accounts_store::ACCOUNTS_FILE);
        profiles.sort();
        json!({ "profiles": profiles, "current": self.profile }).to_string()
    }

    fn list_backups(&self) -> String {
        let mut backups: Vec<String> = std::fs::read_dir(Path::new(PROFILES_DIR).join("backup"))
            .map(|d| d.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".json")).collect())
            .unwrap_or_default();
        backups.sort_by(|a, b| b.cmp(a));
        json!({ "backups": backups }).to_string()
    }

    fn switch_profile(&mut self, name: &str) -> String {
        if profile_path(name).is_err() {
            return fail("Failed to allocate path");
        }
        self.profile = name.to_owned();
        let mut gs = GlobalSettings::load();
        if let Err(e) = gs.update_last_used(name) {
            SLOG.warn(format_args!("Couldn't record last used profile: {e}"));
        }
        ok()
    }

    fn create_profile(&self, name: &str, accent: &str) -> String {
        let file = format!("{}.json", clamp_profile_name(name));
        let Ok(path) = profile_path(&file) else { return fail("Memory allocation failed") };
        if path.exists() {
            return fail("Profile already exists");
        }
        let Ok(mut cfg) = Config::defaults_with_profile(&file) else { return fail("Failed to write profile") };
        if let Ok(color) = parse_hex_color(accent) {
            cfg.accent_color = color;
        }
        match cfg.save_to_json_file(&path) {
            Ok(()) => ok(),
            Err(_) => fail("Failed to write profile"),
        }
    }

    fn copy_profile(&self, request: &str) -> String {
        let Ok(v) = serde_json::from_str::<Value>(request) else { return fail("Invalid JSON") };
        if !v.is_object() {
            return fail("Expected JSON object");
        }
        let Some(source) = v["source"].as_str() else { return fail("Missing source") };
        let Some(target) = v["target"].as_str() else { return fail("Missing target") };
        let target_file = format!("{}.json", clamp_profile_name(target));
        let (Some(src), Ok(dst)) = (source_profile_path(source), profile_path(&target_file)) else { return fail("Memory allocation failed") };
        if dst.exists() {
            return fail("Target profile already exists");
        }
        if std::fs::copy(&src, &dst).is_err() {
            return fail("Failed to copy file");
        }
        if let Some(Ok(color)) = v["accentColor"].as_str().map(parse_hex_color) {
            if let Ok(mut cfg) = Config::load_profile(&target_file) {
                cfg.accent_color = color;
                if let Err(e) = cfg.save_to_json_file(&dst) {
                    SLOG.warn(format_args!("Couldn't set the copy's accent color: {e}"));
                }
            }
        }
        ok()
    }

    fn delete_profile(&self, name: &str) -> String {
        if name == DEFAULT_PROFILE {
            return fail("Cannot delete default profile");
        }
        let Ok(path) = profile_path(name) else { return fail("Memory allocation failed") };
        let backup_dir = Path::new(PROFILES_DIR).join("backup");
        if std::fs::create_dir_all(&backup_dir).is_err() {
            return fail("Failed to create backup directory");
        }
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match std::fs::rename(&path, backup_dir.join(format!("{secs}_{name}"))) {
            Ok(()) => ok(),
            Err(e) => fail(&format!("Failed to delete: {:?}", e.kind())),
        }
    }

    fn reset_profile(&self, name: &str) -> String {
        let (Ok(path), Ok(cfg)) = (profile_path(name), Config::defaults_with_profile(name)) else { return fail("Memory allocation failed") };
        match cfg.save_to_json_file(path) {
            Ok(()) => ok(),
            Err(_) => fail("Failed to write profile"),
        }
    }

    fn load_global_settings(&self) -> String {
        let gs = GlobalSettings::load();
        let Some(mut v) = gs.to_json_string().ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return "{}".into() };
        let table: Vec<Value> = DEFAULT_ORE_TABLE
            .iter()
            .map(|e| json!({ "name": e.name, "category": e.category, "volumeM3": e.volume_m3, "price": gs.ore_price(e.name).unwrap_or(e.price) }))
            .collect();
        v["oreTable"] = Value::Array(table);
        v.to_string()
    }

    fn save_global_settings(&self, text: &str) -> String {
        let Ok(_) = serde_json::from_str::<Value>(text) else { return fail("Invalid JSON") };
        let mut gs: GlobalSettings = match serde_json::from_str(text) {
            Ok(g) => g,
            Err(e) => {
                SLOG.err(format_args!("Rejected global settings: {e}"));
                return fail("Invalid settings format");
            }
        };
        let on_disk = GlobalSettings::load();
        gs.dialog_x = on_disk.dialog_x;
        gs.dialog_y = on_disk.dialog_y;
        // IDs the preview app learned while the dialog was open aren't in the page's copy; keep them.
        for (name, id) in on_disk.character_id_map_snapshot() {
            if !gs.has_character_id(&name) {
                let _ = gs.update_character_id(&name, &id);
            }
        }
        if gs.save().is_err() {
            return fail("Failed to write file");
        }
        apply_run_on_startup(gs.run_on_startup);
        ok()
    }

    /// Loads the current profile from disk, changes it with `f` (which saves) and reports like the Zig build.
    fn edit_profile(&self, f: impl FnOnce(&mut Config) -> Result<(), config::ConfigError>, extra: Value) -> String {
        let Ok(mut cfg) = Config::load_profile(&self.profile) else { return fail("Failed to load profile") };
        if f(&mut cfg).is_err() {
            return fail("Failed to save");
        }
        let mut out = json!({ "success": true });
        if let (Value::Object(o), Value::Object(e)) = (&mut out, extra) {
            o.extend(e);
        }
        out.to_string()
    }

    fn set_window_position(&self, name: &str, all: bool) -> String {
        let Some((_, x, y, minimized)) = eve_clients().into_iter().find(|c| c.0 == name) else {
            return fail(if all { "Selected character is not currently open" } else { "Character is not currently open" });
        };
        if minimized {
            return fail(if all { "Selected character's window is minimized" } else { "Character's window is minimized" });
        }
        let pos = Position { x: x as i32, y: y as i32 };
        if all {
            self.edit_profile(|c| c.save_all_character_window_positions(pos), json!({ "x": x, "y": y }))
        } else {
            self.edit_profile(|c| c.save_character_window_position(name, pos), json!({ "x": x, "y": y }))
        }
    }
}

impl Handler for Backend {
    fn call_without_state(name: &str) -> Option<String> {
        // A GitHub request can take a while when offline; it mustn't block saves and status polls meanwhile.
        (name == "checkForUpdateNow").then(check_for_update_now)
    }

    fn asset(&self, path: &str) -> Option<Asset> {
        match path {
            "/" | "/index.html" => Some(Asset { content_type: "text/html; charset=utf-8", body: self.page.clone() }),
            "/webui.js" => Some(Asset { content_type: "text/javascript; charset=utf-8", body: include_bytes!("../../ui/webui.js").to_vec() }),
            _ => None,
        }
    }

    fn call(&mut self, name: &str, args: Vec<Value>) -> String {
        let a0 = arg(&args, 0);
        match name {
            "closeDialog" => {
                let _ = self.proxy.send_event(UiEvent::Close);
                String::new()
            }
            "getConfigData" => json!({
                "configPath": format!("{PROFILES_DIR}/{}", self.profile),
                "version": env!("CARGO_PKG_VERSION"),
                "categories": [
                    {"id": "general", "name": "General"}, {"id": "thumbnails", "name": "Thumbnails"},
                    {"id": "display", "name": "Display"}, {"id": "hotkeys", "name": "Hotkeys"},
                    {"id": "colors", "name": "Colors"}, {"id": "states", "name": "Visual States"},
                    {"id": "notifications", "name": "Notifications"}, {"id": "chatlog", "name": "Chatlog"},
                    {"id": "characters", "name": "Characters"}
                ]
            })
            .to_string(),
            "loadConfig" => self.load_config(),
            "getDefaultConfig" => Config::defaults_with_profile(&self.profile)
                .and_then(|c| c.to_json_string())
                .unwrap_or_else(|_| json!({ "error": "Failed to build defaults" }).to_string()),
            "saveConfig" => self.save_config(&a0),
            "getValidationRanges" => Config::build_validation_ranges_json(),
            "listProfiles" => self.list_profiles(),
            "listProfileBackups" => self.list_backups(),
            "switchProfile" => self.switch_profile(&a0),
            "createProfile" => self.create_profile(&a0, &arg(&args, 1)),
            "copyProfile" => self.copy_profile(&a0),
            "deleteProfile" => self.delete_profile(&a0),
            "resetProfile" => self.reset_profile(&a0),
            "loadGlobalSettings" => self.load_global_settings(),
            "saveGlobalSettings" => self.save_global_settings(&a0),
            "logClientMessage" => {
                let msg = arg(&args, 1);
                if a0 == "warn" {
                    SLOG.warn(format_args!("[js] {msg}"));
                } else {
                    SLOG.err(format_args!("[js] {msg}"));
                }
                String::new()
            }
            "scaleLegacyPositions" => {
                let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&a0) else { return "[]".into() };
                let scaled: Vec<Value> = list
                    .iter()
                    .map(|p| {
                        let pos = Position { x: p["x"].as_i64().unwrap_or(0) as i32, y: p["y"].as_i64().unwrap_or(0) as i32 }.scale_from_legacy_dpi_unaware();
                        json!({ "x": pos.x, "y": pos.y })
                    })
                    .collect();
                Value::Array(scaled).to_string()
            }
            "setCharacterWindowPosition" => self.set_window_position(&a0, false),
            "setAllCharacterWindowPositions" => self.set_window_position(&a0, true),
            "clearCharacterWindowPosition" => self.edit_profile(|c| c.clear_character_window_position(&a0), json!({})),
            "clearAllCharacterWindowPositions" => self.edit_profile(|c| c.clear_all_character_window_positions(), json!({})),
            "getOpenClients" => Value::Array(eve_clients().into_iter().map(|c| Value::String(c.0)).collect()).to_string(),
            "getRunningWindows" => running_windows(),
            "getDisplays" => displays(),
            "identifyDisplays" => json!({ "success": identify_displays() }).to_string(),

            // Live: the running preview app.
            "getMainAppStatus" => json!({ "running": main_app_running() }).to_string(),
            "switchProfileLive" => {
                send_command(&Command::Profile(a0.into_bytes()));
                ok()
            }
            "previewThumbnailConfig" => {
                send_command(&Command::PreviewThumbnail(a0));
                ok()
            }
            "testNotification" => json!({ "success": send_command(&Command::TestNotification(a0)) }).to_string(),
            "suspendHotkeysForRecording" => {
                send_command(&Command::DialogSuspendHotkeys);
                ok()
            }
            "resumeHotkeysAfterRecording" => {
                send_command(&Command::DialogResumeHotkeys);
                ok()
            }
            "startRegionSelect" => {
                // The app draws the overlay and leaves the outcome in a file; clear any stale one first.
                let _ = std::fs::remove_file(region_select_result_path());
                let request = RegionSelectRequest::from_dialog_json(&a0);
                if send_command(&Command::StartRegionSelect(Box::new(request))) {
                    ok()
                } else {
                    fail("EVE-Maj Preview isn't running")
                }
            }
            "pollRegionSelectResult" => {
                let path = region_select_result_path();
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        let _ = std::fs::remove_file(&path);
                        RegionSelectResult::from_file_json(&text).unwrap_or_else(RegionSelectResult::cancelled).to_dialog_json()
                    }
                    Err(_) => json!({ "done": false }).to_string(),
                }
            }

            // Desktop integration.
            "browseChatlogDir" => self.browse(Browse::ChatlogDir),
            "browseGamelogDir" => self.browse(Browse::GamelogDir),
            "browseSoundFile" => self.browse(Browse::SoundFile),
            "setAlwaysOnTop" => {
                let _ = self.proxy.send_event(UiEvent::SetAlwaysOnTop(args.first().and_then(Value::as_bool).unwrap_or(false)));
                String::new()
            }
            "openUrlInBrowser" => {
                let lower = a0.to_ascii_lowercase();
                let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
                let valid = rest.is_some_and(|r| !r.is_empty() && !r.starts_with(['/', '\\'])) && a0.len() < 1024 && a0.bytes().all(|b| b > 0x20 && b != 0x7F);
                if valid {
                    let _ = std::process::Command::new("xdg-open").arg(&a0).spawn();
                } else {
                    SLOG.warn(format_args!("Refusing to open {a0:?}"));
                }
                String::new()
            }

            // Updates: GitHub is checked and the release linked, but releases ship a Windows zip, so the
            // download/install half stays Windows-only; a Linux build is updated by rebuilding or its package.
            "getUpdateStatus" => update_status(),
            "downloadUpdate" | "installUpdate" => fail("unsupported"),

            // EVE files and network.
            "loadAccounts" => eve_accounts::load_accounts(),
            "saveAccounts" => eve_accounts::save_accounts(&a0),
            "scanEveAccounts" => eve_accounts::scan_eve_accounts(),
            "fetchOrePrices" => esi_prices::fetch_ore_prices(&a0),
            "scanUltraPotatoProfiles" => serde_json::to_string(&ultra_potato::scan_profiles()).unwrap_or_else(|_| "[]".into()),
            "applyUltraPotatoMode" => {
                let Ok(paths) = serde_json::from_str::<Vec<String>>(&a0) else { return fail("Invalid request") };
                if paths.is_empty() {
                    return fail("No profiles selected");
                }
                let known: Vec<String> = ultra_potato::scan_profiles().into_iter().map(|p| p.path).collect();
                if !paths.iter().all(|p| known.contains(p)) {
                    return fail("Invalid request");
                }
                let results = ultra_potato::apply_to_files(&paths);
                serde_json::to_string(&json!({ "success": true, "results": results })).unwrap_or_else(|_| fail("Failed to build response"))
            }
            _ => {
                SLOG.warn(format_args!("Unhandled call {name} ({} args)", args.len()));
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_wording_replaces_existing_english_keys_only() {
        let en: Value = serde_json::from_str(LANGS[0].2).unwrap();
        for (key, _) in LINUX_EN_HINTS {
            assert!(en.get(*key).is_some(), "en.json no longer has {key}");
        }
        let patched: Value = serde_json::from_str(&catalog_json("en", LANGS[0].2)).unwrap();
        assert_eq!(patched["field.regionDisplayUseWorkArea.label"], "Leave Room for Bars");
        assert!(!patched["tab.notifications.section.text-to-speech.hint"].as_str().unwrap().contains("Windows"));
        assert_eq!(catalog_json("de", LANGS[1].2), LANGS[1].2);
    }
}
