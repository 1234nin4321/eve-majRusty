//! The config dialog's Account Config tab: scanning EVE's settings folders for characters/accounts, resolving character names, and loading/saving accounts.json.
//!
//! Each `pub fn` returning a String is one webui binding; the caller passes the request string in and returns the result to the page.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::accounts_store;
use crate::config::{self, GlobalSettings};
use crate::http_client::{FetchOptions, HttpClient};
use crate::log::Scope;

pub use crate::accounts_store::{AccountsFile, ACCOUNTS_FILE};

const SLOG: Scope = Scope::new("eve_accounts");

const ESI_NAMES_URL: &str = "https://esi.evetech.net/latest/universe/names/?datasource=tranquility";
/// ESI's documented per-request cap for /universe/names/.
const ESI_NAMES_BATCH: usize = 1000;
/// ESI rejects a whole /universe/names/ batch if any one ID is unknown, so a failed batch is retried one ID at a time - capped so a long list of dead IDs can't stall the dialog.
const ESI_MAX_SINGLE_LOOKUPS: usize = 100;

const NS_PER_S: i128 = 1_000_000_000;
/// EVE writes core_user_<id>.dat and core_char_<id>.dat together when a character logs out, so a user file whose mtime lands this close to a character file's is taken as that character's account. Heuristic only - the UI presents it as a suggestion.
const MATCH_WINDOW_NS: u128 = 10 * NS_PER_S as u128;

const EMPTY_SCAN_RESPONSE: &str = "{\"characters\":[],\"users\":[]}";
const EMPTY_ACCOUNTS_RESPONSE: &str = "{\"version\":1,\"accounts\":[],\"characters\":[]}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedCharacter {
    pub id: String,
    pub name: Option<String>,
    /// Unix seconds of the newest core_char_<id>.dat across all settings folders.
    pub last_seen: i64,
    pub folders: Vec<String>,
    pub suggested_user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedUser {
    pub id: String,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResponse {
    pub eve_root: Option<String>,
    pub characters: Vec<ScannedCharacter>,
    pub users: Vec<ScannedUser>,
}

#[derive(Debug, Clone)]
struct SettingsFile {
    id: String,
    /// Nanoseconds since the Unix epoch.
    mtime: i128,
}

/// Per-character accumulator while walking settings folders; the newest sighting wins the suggestion.
struct CharAccum {
    mtime: i128,
    folders: Vec<String>,
    suggested_user: Option<String>,
}

/// Returns the numeric ID from `<prefix><digits>.dat`, or None for anything else (including EVE's `core_char__.dat` template file).
fn parse_settings_file_id<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let id = name.strip_prefix(prefix)?.strip_suffix(".dat")?;
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(id)
}

/// The user file written closest in time to `char_mtime`, if within MATCH_WINDOW_NS.
fn nearest_user(users: &[SettingsFile], char_mtime: i128) -> Option<&str> {
    let mut best = None;
    let mut best_delta = MATCH_WINDOW_NS + 1;
    for u in users {
        let delta = (u.mtime - char_mtime).unsigned_abs();
        if delta < best_delta {
            best_delta = delta;
            best = Some(u.id.as_str());
        }
    }
    best
}

fn ns_to_seconds(ns: i128) -> i64 {
    (ns / NS_PER_S) as i64
}

fn system_time_ns(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn file_name_string(name: OsString) -> String {
    name.to_string_lossy().into_owned()
}

/// Insertion-ordered string map, like Zig's array_hash_map, so the scan lists entries in discovery order.
struct OrderedMap<V> {
    index: HashMap<String, usize>,
    entries: Vec<(String, V)>,
}

impl<V> OrderedMap<V> {
    fn new() -> Self {
        Self { index: HashMap::new(), entries: Vec::new() }
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        let i = *self.index.get(key)?;
        Some(&mut self.entries[i].1)
    }

    fn insert_new(&mut self, key: String, value: V) -> &mut V {
        let i = self.entries.len();
        self.index.insert(key.clone(), i);
        self.entries.push((key, value));
        &mut self.entries[i].1
    }
}

/// Walks `<local_app_data>\CCP\EVE\*\settings*\` for core_char_/core_user_ files.
fn scan_settings(local_app_data: Option<&Path>) -> ScanResponse {
    let Some(local_app_data) = local_app_data else {
        SLOG.warn(format_args!("LOCALAPPDATA environment variable not found"));
        return ScanResponse::default();
    };

    let eve_root: PathBuf = local_app_data.join("CCP").join("EVE");
    let eve_root_str = eve_root.to_string_lossy().into_owned();

    let install_iter = match std::fs::read_dir(&eve_root) {
        Ok(it) => it,
        Err(err) => {
            SLOG.info(format_args!("No EVE settings directory found at '{eve_root_str}': {err}"));
            return ScanResponse { eve_root: Some(eve_root_str), ..Default::default() };
        }
    };

    let mut chars: OrderedMap<CharAccum> = OrderedMap::new();
    let mut users: OrderedMap<i128> = OrderedMap::new();

    for install_entry in install_iter {
        let install_entry = match install_entry {
            Ok(e) => e,
            Err(err) => {
                SLOG.warn(format_args!("Failed to enumerate EVE install directory: {err}"));
                break;
            }
        };
        if !install_entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }

        let install_name = file_name_string(install_entry.file_name());
        let settings_iter = match std::fs::read_dir(install_entry.path()) {
            Ok(it) => it,
            Err(err) => {
                SLOG.warn(format_args!("Failed to open EVE install directory '{install_name}': {err}"));
                continue;
            }
        };

        for settings_entry in settings_iter {
            let settings_entry = match settings_entry {
                Ok(e) => e,
                Err(err) => {
                    SLOG.warn(format_args!("Failed to enumerate EVE settings directory: {err}"));
                    break;
                }
            };
            if !settings_entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let settings_name = file_name_string(settings_entry.file_name());
            if !settings_name.starts_with("settings") {
                continue;
            }

            let file_iter = match std::fs::read_dir(settings_entry.path()) {
                Ok(it) => it,
                Err(err) => {
                    SLOG.warn(format_args!("Failed to open EVE settings folder '{settings_name}': {err}"));
                    continue;
                }
            };

            let folder_label = format!("{install_name} / {settings_name}");

            let mut folder_chars: Vec<SettingsFile> = Vec::new();
            let mut folder_users: Vec<SettingsFile> = Vec::new();

            for file_entry in file_iter {
                let file_entry = match file_entry {
                    Ok(e) => e,
                    Err(err) => {
                        SLOG.warn(format_args!("Failed to enumerate '{folder_label}': {err}"));
                        break;
                    }
                };
                if !file_entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }

                let file_os_name = file_entry.file_name();
                let Some(file_name) = file_os_name.to_str() else { continue };
                let is_char = parse_settings_file_id(file_name, "core_char_");
                let is_user = parse_settings_file_id(file_name, "core_user_");
                let Some(id) = is_char.or(is_user) else { continue };

                let mtime = match std::fs::metadata(file_entry.path()).and_then(|m| m.modified()) {
                    Ok(t) => system_time_ns(t),
                    Err(err) => {
                        SLOG.warn(format_args!("Failed to stat '{file_name}' in '{folder_label}': {err}"));
                        continue;
                    }
                };
                let file = SettingsFile { id: id.to_owned(), mtime };
                if is_char.is_some() {
                    folder_chars.push(file);
                } else {
                    folder_users.push(file);
                }
            }

            for u in &folder_users {
                match users.get_mut(&u.id) {
                    Some(existing) => {
                        if u.mtime > *existing {
                            *existing = u.mtime;
                        }
                    }
                    None => {
                        users.insert_new(u.id.clone(), u.mtime);
                    }
                }
            }

            for c in &folder_chars {
                let suggestion = nearest_user(&folder_users, c.mtime).map(str::to_owned);
                let accum = match chars.get_mut(&c.id) {
                    None => chars.insert_new(
                        c.id.clone(),
                        CharAccum { mtime: c.mtime, folders: Vec::new(), suggested_user: suggestion },
                    ),
                    Some(accum) => {
                        if c.mtime > accum.mtime {
                            accum.mtime = c.mtime;
                            // Newest sighting wins, but don't discard an older folder's match just because the newest one had none.
                            if suggestion.is_some() {
                                accum.suggested_user = suggestion;
                            }
                        } else if accum.suggested_user.is_none() {
                            accum.suggested_user = suggestion;
                        }
                        accum
                    }
                };
                accum.folders.push(folder_label.clone());
            }
        }
    }

    let out_chars = chars
        .entries
        .into_iter()
        .map(|(id, accum)| ScannedCharacter {
            id,
            name: None,
            last_seen: ns_to_seconds(accum.mtime),
            folders: accum.folders,
            suggested_user_id: accum.suggested_user,
        })
        .collect();

    let out_users =
        users.entries.into_iter().map(|(id, mtime)| ScannedUser { id, last_seen: ns_to_seconds(mtime) }).collect();

    ScanResponse { eve_root: Some(eve_root_str), characters: out_chars, users: out_users }
}

#[derive(Deserialize)]
struct NamesEntry {
    id: u64,
    name: String,
    category: String,
}

/// Character names out of an ESI /universe/names/ response; None if it doesn't parse.
fn parse_names_response(body: &[u8]) -> Option<Vec<(u64, String)>> {
    let parsed: Vec<NamesEntry> = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(err) => {
            SLOG.warn(format_args!("Failed to parse ESI universe/names response: {err}"));
            return None;
        }
    };
    Some(parsed.into_iter().filter(|e| e.category == "character").map(|e| (e.id, e.name)).collect())
}

/// One POST to ESI /universe/names/. Returns false if the request failed, so the caller can retry IDs individually.
fn fetch_names<F>(fetch: &F, ids: &[u64], out: &mut HashMap<u64, String>) -> bool
where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>>,
{
    let Ok(body) = serde_json::to_vec(ids) else { return false };
    let options = FetchOptions { content_type: Some("application/json"), payload: Some(&body), ..Default::default() };
    let Some(response) = fetch(ESI_NAMES_URL, &options) else { return false };

    let Some(entries) = parse_names_response(&response) else { return false };
    out.extend(entries);
    true
}

/// Fills in `name` for each character: names cached in accounts.json first, then the chatlog-built characterIdMap (name -> id), then public ESI for whatever's left.
fn resolve_names<F>(
    characters: &mut [ScannedCharacter],
    accounts: Option<&AccountsFile>,
    character_id_map: &BTreeMap<String, String>,
    fetch: &F,
) where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>>,
{
    let mut known: HashMap<&str, &str> = HashMap::new();

    if let Some(file) = accounts {
        for c in &file.characters {
            if let Some(n) = &c.name {
                known.insert(&c.id, n);
            }
        }
    }

    for (name, id) in character_id_map {
        known.insert(id, name);
    }

    let mut missing: Vec<u64> = Vec::new();
    for c in characters.iter_mut() {
        if let Some(n) = known.get(c.id.as_str()) {
            c.name = Some((*n).to_owned());
        } else if let Ok(numeric) = c.id.parse::<u64>() {
            missing.push(numeric);
        }
    }
    if missing.is_empty() {
        return;
    }

    let mut resolved: HashMap<u64, String> = HashMap::new();
    let mut single_lookups = 0;
    for batch in missing.chunks(ESI_NAMES_BATCH) {
        if fetch_names(fetch, batch, &mut resolved) {
            continue;
        }
        for &id in batch {
            if single_lookups >= ESI_MAX_SINGLE_LOOKUPS {
                break;
            }
            single_lookups += 1;
            fetch_names(fetch, &[id], &mut resolved);
        }
    }

    for c in characters.iter_mut() {
        if c.name.is_some() {
            continue;
        }
        let Ok(numeric) = c.id.parse::<u64>() else { continue };
        if let Some(n) = resolved.get(&numeric) {
            c.name = Some(n.clone());
        }
    }
}

/// None when the file is missing or unparseable - callers fall back to an empty AccountsFile, never delete it, so a hand-edit typo can't wipe the user's accounts.
fn read_accounts_file() -> Option<AccountsFile> {
    let content = match accounts_store::read_accounts_bytes() {
        Ok(c) => c,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                SLOG.warn(format_args!("Failed to read {ACCOUNTS_FILE}: {err}"));
            }
            return None;
        }
    };
    match accounts_store::parse_accounts_file(&content) {
        Ok(file) => Some(file),
        Err(err) => {
            SLOG.warn(format_args!("Failed to parse {ACCOUNTS_FILE}: {err}"));
            None
        }
    }
}

fn return_json<T: Serialize>(value: &T, fallback: &str) -> String {
    match serde_json::to_string(value) {
        Ok(json) => json,
        Err(err) => {
            SLOG.warn(format_args!("Failed to serialize response: {err}"));
            fallback.to_owned()
        }
    }
}

/// `scan_eve_accounts` with its inputs injected, so it runs against a fake settings tree and canned ESI responses.
pub fn scan_eve_accounts_with<F>(
    local_app_data: Option<&Path>,
    accounts: Option<&AccountsFile>,
    character_id_map: &BTreeMap<String, String>,
    fetch: &F,
) -> String
where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>>,
{
    let mut result = scan_settings(local_app_data);
    resolve_names(&mut result.characters, accounts, character_id_map, fetch);

    SLOG.info(format_args!("Account scan found {} characters, {} accounts", result.characters.len(), result.users.len()));
    return_json(&result, EMPTY_SCAN_RESPONSE)
}

/// Scans EVE's settings folders for every character/account that has logged in on this machine. Response: ScanResponse as JSON.
pub fn scan_eve_accounts() -> String {
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let accounts = read_accounts_file();
    let character_id_map = GlobalSettings::load().character_id_map_snapshot();
    let client = HttpClient::new();
    scan_eve_accounts_with(local_app_data.as_deref(), accounts.as_ref(), &character_id_map, &|url: &str, options: &FetchOptions<'_>| {
        client.fetch(url, options)
    })
}

/// Returns accounts.json, normalized through AccountsFile, or an empty one if it's missing or unreadable.
pub fn load_accounts() -> String {
    let file = read_accounts_file().unwrap_or_default();
    return_json(&file, EMPTY_ACCOUNTS_RESPONSE)
}

/// Round-trips a saveAccounts request through AccountsFile into the pretty-printed text written to disk, or the error response to return.
fn normalize_accounts_request(request: &str) -> Result<String, &'static str> {
    let parsed: AccountsFile = match serde_json::from_str(request) {
        Ok(p) => p,
        Err(err) => {
            SLOG.warn(format_args!("Failed to parse saveAccounts request: {err}"));
            return Err("{\"success\":false,\"error\":\"Invalid request\"}");
        }
    };
    serde_json::to_string_pretty(&parsed).map_err(|_| "{\"success\":false,\"error\":\"Failed to serialize\"}")
}

/// Request body: AccountsFile JSON. Round-trips it through the struct so only the known shape ever reaches disk.
pub fn save_accounts(request: &str) -> String {
    let json = match normalize_accounts_request(request) {
        Ok(json) => json,
        Err(response) => return response.to_owned(),
    };

    if let Err(err) = std::fs::create_dir(config::PROFILES_DIR) {
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            SLOG.warn(format_args!("Failed to create profiles directory: {err}"));
        }
    }

    if let Err(err) = config::atomic_write_file(ACCOUNTS_FILE, json.as_bytes()) {
        SLOG.err(format_args!("Failed to write {ACCOUNTS_FILE}: {err}"));
        return "{\"success\":false,\"error\":\"Failed to write accounts.json\"}".to_owned();
    }

    "{\"success\":true}".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn parse_settings_file_id_accepts_only_numeric_ids() {
        assert_eq!(parse_settings_file_id("core_char_12345.dat", "core_char_"), Some("12345"));
        assert_eq!(parse_settings_file_id("core_char__.dat", "core_char_"), None);
        assert_eq!(parse_settings_file_id("core_char_abc.dat", "core_char_"), None);
        assert_eq!(parse_settings_file_id("core_user_1.dat", "core_char_"), None);
        assert_eq!(parse_settings_file_id("core_char_1.dat.bak", "core_char_"), None);
        assert_eq!(parse_settings_file_id("core_char_.dat", "core_char_"), None);
    }

    #[test]
    fn nearest_user_picks_closest_within_window() {
        let s = NS_PER_S;
        let users = [SettingsFile { id: "a".into(), mtime: 100 * s }, SettingsFile { id: "b".into(), mtime: 203 * s }];
        assert_eq!(nearest_user(&users, 200 * s), Some("b"));
        assert_eq!(nearest_user(&users, 95 * s), Some("a"));
        assert_eq!(nearest_user(&users, 150 * s), None);
        assert_eq!(nearest_user(&users, 110 * s), Some("a"));
    }

    #[test]
    fn ns_to_seconds_truncates_toward_zero() {
        assert_eq!(ns_to_seconds(1_999_999_999), 1);
        assert_eq!(ns_to_seconds(-1_500_000_000), -1);
    }

    /// A scratch directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("eve_accounts_{tag}_{}_{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn touch(dir: &Path, name: &str, unix_secs: u64) {
        std::fs::create_dir_all(dir).unwrap();
        let file = std::fs::File::create(dir.join(name)).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(unix_secs)).unwrap();
    }

    fn no_fetch(_: &str, _: &FetchOptions<'_>) -> Option<Vec<u8>> {
        panic!("no request expected")
    }

    #[test]
    fn missing_local_app_data_scans_nothing() {
        assert_eq!(scan_settings(None), ScanResponse::default());
        let out = scan_eve_accounts_with(None, None, &BTreeMap::new(), &no_fetch);
        assert_eq!(out, r#"{"eveRoot":null,"characters":[],"users":[]}"#);
    }

    #[test]
    fn missing_eve_directory_reports_only_the_root() {
        let tmp = TempDir::new("noeve");
        let result = scan_settings(Some(&tmp.0));
        assert_eq!(result.eve_root.as_deref(), Some(tmp.0.join("CCP").join("EVE").to_string_lossy().as_ref()));
        assert!(result.characters.is_empty() && result.users.is_empty());
    }

    #[test]
    fn scan_merges_folders_and_suggests_accounts() {
        let tmp = TempDir::new("scan");
        let eve = tmp.0.join("CCP").join("EVE");
        let tq = eve.join("c_eve_sharedcache_tq_tranquility");
        // Older folder: char 111 logged out next to user 9; char 222 has no user nearby.
        touch(&tq.join("settings_Default"), "core_char_111.dat", 1000);
        touch(&tq.join("settings_Default"), "core_user_9.dat", 1003);
        touch(&tq.join("settings_Default"), "core_char_222.dat", 5000);
        touch(&tq.join("settings_Default"), "core_char__.dat", 1000);
        touch(&tq.join("settings_Default"), "prefs.ini", 1000);
        // Newer folder: char 111 seen again with no user match, char 222 now matches user 8.
        touch(&tq.join("settings_Fleet"), "core_char_111.dat", 2000);
        touch(&tq.join("settings_Fleet"), "core_char_222.dat", 4000);
        touch(&tq.join("settings_Fleet"), "core_user_8.dat", 4005);
        touch(&tq.join("settings_Fleet"), "core_user_9.dat", 900);
        // Not a settings folder, and a stray file at the install level.
        touch(&tq.join("cache"), "core_char_333.dat", 1000);
        touch(&eve, "core_char_444.dat", 1000);

        let mut result = scan_settings(Some(&tmp.0));
        result.characters.sort_by(|a, b| a.id.cmp(&b.id));
        result.users.sort_by(|a, b| a.id.cmp(&b.id));
        for c in &mut result.characters {
            c.folders.sort();
        }

        let label = |s: &str| format!("c_eve_sharedcache_tq_tranquility / {s}");
        assert_eq!(
            result.characters,
            vec![
                ScannedCharacter {
                    id: "111".into(),
                    name: None,
                    last_seen: 2000,
                    folders: vec![label("settings_Default"), label("settings_Fleet")],
                    suggested_user_id: Some("9".into()),
                },
                ScannedCharacter {
                    id: "222".into(),
                    name: None,
                    last_seen: 5000,
                    folders: vec![label("settings_Default"), label("settings_Fleet")],
                    suggested_user_id: Some("8".into()),
                },
            ]
        );
        assert_eq!(
            result.users,
            vec![ScannedUser { id: "8".into(), last_seen: 4005 }, ScannedUser { id: "9".into(), last_seen: 1003 }]
        );
    }

    fn character(id: &str) -> ScannedCharacter {
        ScannedCharacter { id: id.into(), name: None, last_seen: 0, folders: Vec::new(), suggested_user_id: None }
    }

    #[test]
    fn resolve_names_prefers_cached_names_then_esi() {
        let accounts = AccountsFile {
            characters: vec![accounts_store::CharacterLink {
                id: "1".into(),
                name: Some("Cached".into()),
                account_id: None,
                last_seen: None,
            }],
            ..Default::default()
        };
        let id_map = BTreeMap::from([("From Chatlog".to_owned(), "2".to_owned())]);
        let fetch = |url: &str, options: &FetchOptions<'_>| -> Option<Vec<u8>> {
            assert_eq!(url, ESI_NAMES_URL);
            assert_eq!(options.content_type, Some("application/json"));
            assert_eq!(options.payload, Some(&b"[3,4]"[..]));
            Some(
                br#"[{"id":3,"name":"From ESI","category":"character"},{"id":4,"name":"Some Corp","category":"corporation"}]"#
                    .to_vec(),
            )
        };
        let mut chars = vec![character("1"), character("2"), character("3"), character("4")];
        resolve_names(&mut chars, Some(&accounts), &id_map, &fetch);
        let names: Vec<Option<&str>> = chars.iter().map(|c| c.name.as_deref()).collect();
        assert_eq!(names, vec![Some("Cached"), Some("From Chatlog"), Some("From ESI"), None]);
    }

    #[test]
    fn character_id_map_overrides_accounts_file_names() {
        let accounts = AccountsFile {
            characters: vec![accounts_store::CharacterLink {
                id: "1".into(),
                name: Some("Old Name".into()),
                account_id: None,
                last_seen: None,
            }],
            ..Default::default()
        };
        let id_map = BTreeMap::from([("New Name".to_owned(), "1".to_owned())]);
        let mut chars = vec![character("1")];
        resolve_names(&mut chars, Some(&accounts), &id_map, &no_fetch);
        assert_eq!(chars[0].name.as_deref(), Some("New Name"));
    }

    #[test]
    fn failed_batch_retries_ids_one_at_a_time_up_to_the_cap() {
        let calls = AtomicUsize::new(0);
        let fetch = |_: &str, options: &FetchOptions<'_>| -> Option<Vec<u8>> {
            calls.fetch_add(1, Ordering::Relaxed);
            let ids: Vec<u64> = serde_json::from_slice(options.payload.unwrap()).unwrap();
            if ids.len() > 1 || ids[0] == 1 {
                return None;
            }
            Some(format!(r#"[{{"id":{},"name":"Pilot {}","category":"character"}}]"#, ids[0], ids[0]).into_bytes())
        };
        let mut chars: Vec<ScannedCharacter> = (1..=150).map(|i| character(&i.to_string())).collect();
        resolve_names(&mut chars, None, &BTreeMap::new(), &fetch);
        assert_eq!(calls.load(Ordering::Relaxed), 1 + ESI_MAX_SINGLE_LOOKUPS);
        assert_eq!(chars[0].name, None);
        assert_eq!(chars[1].name.as_deref(), Some("Pilot 2"));
        assert_eq!(chars[99].name.as_deref(), Some("Pilot 100"));
        assert_eq!(chars[100].name, None);
    }

    #[test]
    fn large_scans_are_split_into_esi_sized_batches() {
        let sizes = std::sync::Mutex::new(Vec::new());
        let fetch = |_: &str, options: &FetchOptions<'_>| -> Option<Vec<u8>> {
            let ids: Vec<u64> = serde_json::from_slice(options.payload.unwrap()).unwrap();
            sizes.lock().unwrap().push(ids.len());
            Some(b"[]".to_vec())
        };
        let mut chars: Vec<ScannedCharacter> = (1..=2500).map(|i| character(&i.to_string())).collect();
        resolve_names(&mut chars, None, &BTreeMap::new(), &fetch);
        assert_eq!(*sizes.lock().unwrap(), vec![1000, 1000, 500]);
    }

    #[test]
    fn unparseable_names_response_counts_as_a_failed_request() {
        assert!(parse_names_response(b"{}").is_none());
        assert!(parse_names_response(br#"[{"id":1,"name":"x"}]"#).is_none());
        assert_eq!(
            parse_names_response(br#"[{"id":1,"name":"x","category":"character","extra":0}]"#),
            Some(vec![(1, "x".to_owned())])
        );
    }

    #[test]
    fn scan_response_serializes_in_the_zig_shape() {
        let response = ScanResponse {
            eve_root: Some("C:\\Users\\x\\AppData\\Local\\CCP\\EVE".into()),
            characters: vec![ScannedCharacter {
                id: "1".into(),
                name: None,
                last_seen: 5,
                folders: vec!["tq / settings_Default".into()],
                suggested_user_id: Some("9".into()),
            }],
            users: vec![ScannedUser { id: "9".into(), last_seen: 4 }],
        };
        assert_eq!(
            return_json(&response, EMPTY_SCAN_RESPONSE),
            r#"{"eveRoot":"C:\\Users\\x\\AppData\\Local\\CCP\\EVE","characters":[{"id":"1","name":null,"lastSeen":5,"folders":["tq / settings_Default"],"suggestedUserId":"9"}],"users":[{"id":"9","lastSeen":4}]}"#
        );
    }

    #[test]
    fn empty_accounts_file_matches_the_fallback_response() {
        assert_eq!(return_json(&AccountsFile::default(), ""), EMPTY_ACCOUNTS_RESPONSE);
    }

    #[test]
    fn save_request_is_normalized_to_indented_known_fields() {
        let request = r#"{"version":1,"junk":[1,2],"accounts":[{"id":"a","name":"Main","userIds":["9"],"x":1}],"characters":[{"id":"1","name":"Pilot","accountId":"a","lastSeen":1700000000}]}"#;
        let expected = "{\n  \"version\": 1,\n  \"accounts\": [\n    {\n      \"id\": \"a\",\n      \"name\": \"Main\",\n      \"userIds\": [\n        \"9\"\n      ]\n    }\n  ],\n  \"characters\": [\n    {\n      \"id\": \"1\",\n      \"name\": \"Pilot\",\n      \"accountId\": \"a\",\n      \"lastSeen\": 1700000000\n    }\n  ]\n}";
        assert_eq!(normalize_accounts_request(request).as_deref(), Ok(expected));
        assert_eq!(
            normalize_accounts_request("{}").as_deref(),
            Ok("{\n  \"version\": 1,\n  \"accounts\": [],\n  \"characters\": []\n}")
        );
        assert_eq!(normalize_accounts_request("nope"), Err("{\"success\":false,\"error\":\"Invalid request\"}"));
    }
}
