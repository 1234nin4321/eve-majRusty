//! Update checks against GitHub releases, plus the platform-neutral half of the config dialog's in-place updater:
//! verifying a downloaded release zip, vetting its entry names and rendering the PowerShell script that swaps the
//! files in. The download/extract/launch side lives in the app crate (update_stage.rs, updater.rs).

use std::cmp::Ordering;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::http_client::{FetchOptions, HttpClient};
use crate::log::Scope;

const SLOG: Scope = Scope::new("update");
const SLOG_STAGE: Scope = Scope::new("update_stage");

macro_rules! repo {
    () => {
        "1234nin4321/eve-majRusty"
    };
}

/// GitHub "owner/name" that update checks and downloads come from.
pub const REPO: &str = repo!();

/// The installed version (the Zig build's `build_options.version`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const RELEASES_API_URL: &str = concat!("https://api.github.com/repos/", repo!(), "/releases?per_page=100");
const RELEASES_PAGE_URL: &str = concat!("https://github.com/", repo!(), "/releases");

// ---------------------------------------------------------------------------------------------------------------
// Semantic versions
// ---------------------------------------------------------------------------------------------------------------

/// A software version formatted according to the Semantic Versioning 2.0.0 specification, parsed and ordered
/// exactly like Zig's `std.SemanticVersion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
    pub build: Option<String>,
}

impl SemanticVersion {
    /// Parses `major.minor.patch[-pre][+build]`; None if `text` isn't valid semver.
    pub fn parse(text: &str) -> Option<Self> {
        // Parse the required major, minor, and patch numbers.
        let extra_index = text.find(['-', '+']);
        let required = &text[..extra_index.unwrap_or(text.len())];
        let mut it = required.split('.');
        let major = parse_num(it.next()?)?;
        let minor = parse_num(it.next()?)?;
        let patch = parse_num(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        let mut ver = Self { major, minor, patch, pre: None, build: None };
        let Some(extra_index) = extra_index else {
            return Some(ver);
        };

        // Slice optional pre-release or build metadata components.
        let extra = &text[extra_index..];
        if let Some(rest) = extra.strip_prefix('-') {
            match rest.find('+') {
                Some(b) => {
                    ver.pre = Some(rest[..b].to_string());
                    ver.build = Some(rest[b + 1..].to_string());
                }
                None => ver.pre = Some(rest.to_string()),
            }
        } else {
            ver.build = Some(extra[1..].to_string());
        }

        // Check validity of optional pre-release identifiers.
        // See: https://semver.org/#spec-item-9
        if let Some(pre) = &ver.pre {
            for id in pre.split('.') {
                if !is_valid_identifier(id) {
                    return None;
                }
                // Numeric identifiers MUST NOT include leading zeroes.
                if id.bytes().all(|c| c.is_ascii_digit()) {
                    parse_num(id)?;
                }
            }
        }

        // Check validity of optional build metadata identifiers.
        // See: https://semver.org/#spec-item-10
        if let Some(build) = &ver.build {
            if !build.split('.').all(is_valid_identifier) {
                return None;
            }
        }

        Some(ver)
    }

    /// Semver precedence; build metadata is ignored.
    pub fn order(&self, rhs: &Self) -> Ordering {
        let core = (self.major, self.minor, self.patch).cmp(&(rhs.major, rhs.minor, rhs.patch));
        if core != Ordering::Equal {
            return core;
        }
        let (lpre, rpre) = match (&self.pre, &rhs.pre) {
            (Some(_), None) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Greater,
            (Some(l), Some(r)) => (l, r),
        };

        // Iterate over pre-release identifiers until a difference is found.
        let mut lhs_it = lpre.split('.');
        let mut rhs_it = rpre.split('.');
        loop {
            // A larger set of pre-release fields has a higher precedence than a smaller set.
            let (lid, rid) = match (lhs_it.next(), rhs_it.next()) {
                (None, Some(_)) => return Ordering::Less,
                (None, None) => return Ordering::Equal,
                (Some(_), None) => return Ordering::Greater,
                (Some(l), Some(r)) => (l, r),
            };

            // Numeric identifiers always have lower precedence than non-numeric identifiers.
            // Identifiers consisting of only digits are compared numerically.
            // Identifiers with letters or hyphens are compared lexically in ASCII sort order.
            let ord = match (parse_unsigned(lid), parse_unsigned(rid)) {
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (Some(l), Some(r)) => l.cmp(&r),
                (None, None) => lid.as_bytes().cmp(rid.as_bytes()),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
    }
}

/// Identifiers MUST NOT be empty and MUST comprise only ASCII alphanumerics and hyphens [0-9A-Za-z-].
fn is_valid_identifier(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn parse_num(text: &str) -> Option<u64> {
    // Leading zeroes are not allowed.
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    parse_unsigned(text)
}

/// Base-10 unsigned parse with Zig's `std.fmt.parseUnsigned` rules: digits only, '_' allowed as a separator
/// except at either end; None on any other character or on overflow.
fn parse_unsigned(text: &str) -> Option<u64> {
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') {
        return None;
    }
    let mut acc: u64 = 0;
    for c in text.bytes() {
        if c == b'_' {
            continue;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(acc)
}

/// Strips a leading "v" (GitHub tag convention) and parses the rest as semver.
pub fn parse_tag_version(tag: &str) -> Option<SemanticVersion> {
    SemanticVersion::parse(tag.strip_prefix('v').unwrap_or(tag))
}

// ---------------------------------------------------------------------------------------------------------------
// Update status
// ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct StatusInner {
    version: Option<String>,
    url: Option<String>,
    /// Combined release notes for every release newer than the installed version (each prefixed with its tag name); None if none had notes.
    notes: Option<String>,
}

/// Mutex-guarded holder for the latest known update result, written by the background check thread and read by the tray menu on the main thread.
pub struct UpdateStatus {
    inner: Mutex<StatusInner>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateStatus {
    pub const fn new() -> Self {
        Self { inner: Mutex::new(StatusInner { version: None, url: None, notes: None }) }
    }

    fn lock(&self) -> MutexGuard<'_, StatusInner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stores a fresh update result, replacing any previous one; copies `version`/`url`/`notes`.
    pub fn set(&self, version: &str, url: &str, notes: Option<&str>) {
        let mut inner = self.lock();
        inner.version = Some(version.to_string());
        inner.url = Some(url.to_string());
        inner.notes = notes.map(str::to_string);
    }

    /// Forgets the stored result (the Zig `deinit`).
    pub fn clear(&self) {
        *self.lock() = StatusInner::default();
    }

    pub fn is_available(&self) -> bool {
        self.lock().version.is_some()
    }

    /// A copy of the stored release URL; None if no update is available.
    pub fn url(&self) -> Option<String> {
        self.lock().url.clone()
    }

    /// A copy of the stored latest version; None if no update is available.
    pub fn version(&self) -> Option<String> {
        self.lock().version.clone()
    }

    /// A copy of the stored release notes; None if unavailable.
    pub fn notes(&self) -> Option<String> {
        self.lock().notes.clone()
    }
}

/// Global update state (see UpdateStatus doc comment).
pub static UPDATE_STATUS: UpdateStatus = UpdateStatus::new();

/// The stored release's page, or the repo's releases list when no update is known.
pub fn releases_page_url() -> String {
    UPDATE_STATUS.url().unwrap_or_else(|| RELEASES_PAGE_URL.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// Update checker
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: Option<String>,
    /// The newest release's portable zip, for the config dialog's in-place updater; None if that release has no zip attached.
    pub asset_url: Option<String>,
    pub asset_name: Option<String>,
    pub asset_size: u64,
    /// GitHub's "sha256:<hex>" asset digest, when the release has one.
    pub asset_digest: Option<String>,
}

#[derive(Debug)]
pub enum UpdateError {
    FetchFailed,
    Json(serde_json::Error),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FetchFailed => f.write_str("FetchFailed"),
            Self::Json(err) => write!(f, "invalid JSON: {err}"),
        }
    }
}

impl std::error::Error for UpdateError {}

pub struct UpdateChecker {
    client: HttpClient,
    current_version: String,
}

impl Default for UpdateChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateChecker {
    pub fn new() -> Self {
        Self { client: HttpClient::new(), current_version: CURRENT_VERSION.to_string() }
    }

    pub fn check_for_updates(&self) -> Result<Option<UpdateInfo>, UpdateError> {
        SLOG.info(format_args!("Checking for updates (current: {})", self.current_version));

        // per_page=100 covers skipping many releases at once; unauthenticated requests only ever see published (non-draft) releases anyway.
        let options = FetchOptions { extra_headers: &[("Accept", "application/vnd.github+json")], ..Default::default() };
        let body = self.client.fetch(RELEASES_API_URL, &options).ok_or(UpdateError::FetchFailed)?;

        SLOG.debug(format_args!("GitHub API response: {}", String::from_utf8_lossy(&body)));

        parse_releases(&body, &self.current_version)
    }

    pub fn check_for_updates_background() {
        let checker = UpdateChecker::new();

        let update_info = match checker.check_for_updates() {
            Ok(info) => info,
            Err(err) => {
                SLOG.warn(format_args!("Update check failed: {err}"));
                return;
            }
        };

        if let Some(info) = update_info {
            UPDATE_STATUS.set(&info.version, &info.url, info.notes.as_deref());
            SLOG.info(format_args!("Update available stored: {}", info.version));
        }
    }
}

/// Picks the newest non-draft, non-prerelease release above `current_version` out of a GitHub `/releases` response,
/// collecting the notes of every release in between. Ok(None) when there is nothing newer or GitHub returned an error object.
pub fn parse_releases(body: &[u8], current_version: &str) -> Result<Option<UpdateInfo>, UpdateError> {
    let parsed: Value = serde_json::from_slice(body).map_err(UpdateError::Json)?;

    // Check for GitHub API errors (e.g., private repo, rate limit, 404) - the list endpoint returns an error object instead of an array in that case.
    if let Value::Object(obj) = &parsed {
        if let Some(Value::String(message)) = obj.get("message") {
            SLOG.debug(format_args!("GitHub API returned error: {message} (this is normal for private repos)"));
        }
        return Ok(None);
    }

    let Value::Array(releases) = &parsed else {
        SLOG.debug(format_args!("GitHub API response is not a JSON array"));
        return Ok(None);
    };

    let Some(current_semver) = parse_tag_version(current_version) else {
        SLOG.warn(format_args!("Failed to parse current version: {current_version}"));
        return Ok(None);
    };

    let mut latest: Option<(&str, &str, Option<ReleaseAsset>)> = None;
    let mut notes_buf = String::new();

    // GitHub lists releases newest-first, so this walks down from latest until it reaches (or passes) the installed version.
    for release_value in releases {
        let Value::Object(release) = release_value else { continue };

        if matches!(release.get("draft"), Some(Value::Bool(true))) {
            continue;
        }
        if matches!(release.get("prerelease"), Some(Value::Bool(true))) {
            continue;
        }

        let (Some(Value::String(tag_name)), Some(Value::String(html_url))) = (release.get("tag_name"), release.get("html_url")) else {
            continue;
        };

        let Some(release_semver) = parse_tag_version(tag_name) else {
            SLOG.warn(format_args!("Skipping release with unparsable tag: {tag_name}"));
            continue;
        };

        if release_semver.order(&current_semver) != Ordering::Greater {
            continue;
        }

        if latest.is_none() {
            latest = Some((tag_name, html_url, find_portable_zip(release)));
        }

        if let Some(Value::String(n)) = release.get("body") {
            if !notes_buf.is_empty() {
                notes_buf.push_str("\n\n");
            }
            notes_buf.push_str(tag_name);
            notes_buf.push_str("\n\n");
            notes_buf.push_str(n);
        }
    }

    let Some((latest_version, latest_url, latest_asset)) = latest else {
        SLOG.info(format_args!("Already on latest version: {current_version}"));
        return Ok(None);
    };

    SLOG.info(format_args!("Update available: {current_version} -> {latest_version}"));
    let mut info = UpdateInfo {
        version: latest_version.to_string(),
        url: latest_url.to_string(),
        notes: (!notes_buf.is_empty()).then_some(notes_buf),
        ..Default::default()
    };
    if let Some(a) = latest_asset {
        info.asset_url = Some(a.url.to_string());
        info.asset_name = Some(a.name.to_string());
        info.asset_size = a.size;
        info.asset_digest = a.digest.map(str::to_string);
    }
    Ok(Some(info))
}

struct ReleaseAsset<'a> {
    url: &'a str,
    name: &'a str,
    size: u64,
    digest: Option<&'a str>,
}

fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// The release's "-portable.zip" asset (what the in-dialog updater installs), falling back to any .zip; None if it has none.
fn find_portable_zip(release: &Map<String, Value>) -> Option<ReleaseAsset<'_>> {
    let Some(Value::Array(assets)) = release.get("assets") else {
        return None;
    };
    let mut fallback = None;
    for asset_value in assets {
        let Value::Object(asset) = asset_value else { continue };
        let (Some(Value::String(name)), Some(Value::String(url))) = (asset.get("name"), asset.get("browser_download_url")) else {
            continue;
        };
        if !ends_with_ignore_case(name, ".zip") {
            continue;
        }
        let size = match asset.get("size").and_then(Value::as_i64) {
            Some(sz) if sz > 0 => sz as u64,
            _ => 0,
        };
        let digest = match asset.get("digest") {
            Some(Value::String(d)) => Some(d.as_str()),
            _ => None,
        };
        let found = ReleaseAsset { url, name, size, digest };
        if ends_with_ignore_case(name, "-portable.zip") {
            return Some(found);
        }
        if fallback.is_none() {
            fallback = Some(found);
        }
    }
    fallback
}

// ---------------------------------------------------------------------------------------------------------------
// Staging: verification, zip vetting, install script
// ---------------------------------------------------------------------------------------------------------------

/// Release zips are ~5 MB; anything wildly larger is not one of ours.
pub const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Files a release zip must contain to be accepted - guards against installing an unrelated zip attached to a release.
pub const REQUIRED_FILES: [&str; 2] = ["eve-maj-preview.exe", "config.exe"];

/// Folders in the install directory that hold user data, which a release must never overwrite.
const PROTECTED_DIRS: [&str; 2] = ["profiles", "update-backup"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Asset {
    pub url: String,
    pub name: String,
    /// 0 when GitHub didn't report one.
    pub size: u64,
    /// GitHub's "sha256:<hex>" digest, if the release has one.
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageError {
    DownloadFailed,
    SizeMismatch,
    ChecksumMismatch,
    UnsupportedDigest,
    MissingRequiredFile,
    BadArchive,
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for StageError {}

/// Size and SHA-256 checks against what the GitHub release reported. A missing digest is allowed (older releases predate GitHub publishing them); a present one must match.
pub fn verify_download(body: &[u8], asset: &Asset) -> Result<(), StageError> {
    if asset.size != 0 && body.len() as u64 != asset.size {
        SLOG_STAGE.warn(format_args!("Update download size mismatch: got {}, expected {}", body.len(), asset.size));
        return Err(StageError::SizeMismatch);
    }
    let Some(digest) = asset.digest.as_deref() else {
        SLOG_STAGE.warn(format_args!("Release asset has no published digest; skipping checksum verification"));
        return Ok(());
    };
    const PREFIX: &str = "sha256:";
    if digest.len() < PREFIX.len() || !digest.as_bytes()[..PREFIX.len()].eq_ignore_ascii_case(PREFIX.as_bytes()) {
        return Err(StageError::UnsupportedDigest);
    }
    let expected_hex = &digest.as_bytes()[PREFIX.len()..];
    if expected_hex.len() != 64 {
        return Err(StageError::UnsupportedDigest);
    }

    let actual = Sha256::digest(body);
    let actual_hex: String = actual.iter().map(|b| format!("{b:02x}")).collect();
    if !actual_hex.as_bytes().eq_ignore_ascii_case(expected_hex) {
        SLOG_STAGE.warn(format_args!(
            "Update checksum mismatch: got {actual_hex}, expected {}",
            String::from_utf8_lossy(expected_hex)
        ));
        return Err(StageError::ChecksumMismatch);
    }
    Ok(())
}

fn read_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Walks the zip's central directory and rejects the archive if any entry could land outside the staging folder (drive letters, colons, absolute or UNC paths, ".." components) or inside a protected user-data folder.
pub fn validate_zip_entry_names(zip: &[u8]) -> Result<(), StageError> {
    const EOCD_SIG: [u8; 4] = [b'P', b'K', 5, 6];
    if zip.len() < 22 {
        return Err(StageError::BadArchive);
    }
    // The end-of-central-directory record sits in the last 22 + up to 65535 (comment) bytes.
    let search_start = zip.len().saturating_sub(22 + 0xFFFF);
    let eocd = zip[search_start..].windows(4).rposition(|w| w == EOCD_SIG).ok_or(StageError::BadArchive)?;
    let e = search_start + eocd;
    if e + 22 > zip.len() {
        return Err(StageError::BadArchive);
    }
    let entry_count = read_u16(zip, e + 10);
    let cd_offset = read_u32(zip, e + 16);
    // ZIP64 archives (0xFFFF/0xFFFFFFFF markers) are far beyond a release zip's size; refuse rather than half-parse them.
    if entry_count == 0xFFFF || cd_offset == 0xFFFF_FFFF {
        return Err(StageError::BadArchive);
    }

    let mut pos = cd_offset as usize;
    for _ in 0..entry_count {
        if pos + 46 > zip.len() {
            return Err(StageError::BadArchive);
        }
        if zip[pos..pos + 4] != [b'P', b'K', 1, 2] {
            return Err(StageError::BadArchive);
        }
        let name_len = read_u16(zip, pos + 28) as usize;
        let extra_len = read_u16(zip, pos + 30) as usize;
        let comment_len = read_u16(zip, pos + 32) as usize;
        if pos + 46 + name_len > zip.len() {
            return Err(StageError::BadArchive);
        }
        let name = &zip[pos + 46..pos + 46 + name_len];
        if !is_safe_entry_name(name) {
            SLOG_STAGE.warn(format_args!("Update zip has an unsafe entry name: {}", String::from_utf8_lossy(name)));
            return Err(StageError::BadArchive);
        }
        pos += 46 + name_len + extra_len + comment_len;
    }
    Ok(())
}

/// Whether a zip entry name stays inside the staging folder and out of the protected user-data folders.
pub fn is_safe_entry_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    if name[0] == b'/' || name[0] == b'\\' {
        return false;
    }
    if name.iter().any(|&c| c == b':' || c < 0x20) {
        return false;
    }
    let mut first = true;
    for part in name.split(|&c| c == b'/' || c == b'\\').filter(|p| !p.is_empty()) {
        if part == b".." || part == b"." {
            return false;
        }
        if first {
            if PROTECTED_DIRS.iter().any(|dir| part.eq_ignore_ascii_case(dir.as_bytes())) {
                return false;
            }
            first = false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy)]
pub struct ScriptParams<'a> {
    pub staged_dir: &'a str,
    pub install_dir: &'a str,
    pub log_path: &'a str,
    pub config_pid: u32,
    /// 0 when the main app isn't running (then it isn't restarted either).
    pub main_pid: u32,
}

/// PowerShell that waits for both processes to exit, backs up then overwrites the install folder's files from staging (restoring the backup on failure), and relaunches. Values are spliced in as single-quoted literals, so only `'` needs escaping.
pub fn render_install_script(p: &ScriptParams<'_>) -> String {
    let mut out = String::with_capacity(INSTALL_SCRIPT_BODY.len() + 512);
    // UTF-8 BOM: Windows PowerShell 5.1 reads a BOM-less script as the ANSI code page, which would mangle non-ASCII paths.
    out.push('\u{FEFF}');
    out.push_str("$Staged = ");
    write_ps_literal(&mut out, p.staged_dir);
    out.push_str("\r\n$InstallDir = ");
    write_ps_literal(&mut out, p.install_dir);
    out.push_str("\r\n$Log = ");
    write_ps_literal(&mut out, p.log_path);
    out.push_str(&format!("\r\n$ConfigPid = {}\r\n$MainPid = {}\r\n", p.config_pid, p.main_pid));
    out.push_str(INSTALL_SCRIPT_BODY);
    out
}

/// Single-quoted PowerShell literal. PowerShell also ends single-quoted strings on the typographic quotes U+2018-U+201B, so those are doubled too, same as the ASCII one.
fn write_ps_literal(out: &mut String, value: &str) {
    out.push('\'');
    for c in value.chars() {
        match c {
            '\'' | '\u{2018}'..='\u{201B}' => {
                out.push(c);
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('\'');
}

const INSTALL_SCRIPT_BODY: &str = r#"$ErrorActionPreference = 'Stop'
function Write-Log([string]$m) {
    try { Add-Content -LiteralPath $Log -Value ("{0:u} {1}" -f (Get-Date), $m) -Encoding UTF8 } catch {}
}
function Wait-Exit([int]$procId, [int]$seconds) {
    if ($procId -le 0) { return $true }
    $p = Get-Process -Id $procId -ErrorAction SilentlyContinue
    if (-not $p) { return $true }
    return $p.WaitForExit($seconds * 1000)
}
function Copy-WithRetry([string]$src, [string]$dest) {
    $parent = Split-Path -Parent $dest
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    for ($i = 0; ; $i++) {
        try { Copy-Item -LiteralPath $src -Destination $dest -Force; return }
        catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 500 }
    }
}

Write-Log "Update started: '$Staged' -> '$InstallDir'"
if (-not (Wait-Exit $ConfigPid 120)) {
    Write-Log 'Configuration window did not close; update aborted, nothing changed.'
    exit 1
}
if (-not (Wait-Exit $MainPid 30)) {
    Write-Log 'Main app did not exit in time; stopping it.'
    Stop-Process -Id $MainPid -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}

$StagedFull = (Get-Item -LiteralPath $Staged).FullName.TrimEnd('\')
$Backup = Join-Path $InstallDir 'update-backup'
# User data never comes from a release, even if one were to contain it.
$files = @(Get-ChildItem -LiteralPath $StagedFull -Recurse -File | Where-Object {
    $rel = $_.FullName.Substring($StagedFull.Length).TrimStart([char[]]'\/')
    -not ($rel -like 'profiles[\/]*' -or $rel -like 'update-backup[\/]*')
})
$ok = $false
try {
    if (Test-Path -LiteralPath $Backup) { Remove-Item -LiteralPath $Backup -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $Backup | Out-Null
    foreach ($f in $files) {
        $rel = $f.FullName.Substring($StagedFull.Length).TrimStart('\')
        $dest = Join-Path $InstallDir $rel
        if (Test-Path -LiteralPath $dest) { Copy-WithRetry $dest (Join-Path $Backup $rel) }
    }
    foreach ($f in $files) {
        $rel = $f.FullName.Substring($StagedFull.Length).TrimStart('\')
        Copy-WithRetry $f.FullName (Join-Path $InstallDir $rel)
    }
    $ok = $true
    Write-Log "Installed $($files.Count) files. Previous files kept in '$Backup'."
} catch {
    Write-Log "Install failed: $_ - restoring previous files."
    try {
        $BackupFull = (Get-Item -LiteralPath $Backup).FullName.TrimEnd('\')
        foreach ($b in @(Get-ChildItem -LiteralPath $BackupFull -Recurse -File)) {
            $rel = $b.FullName.Substring($BackupFull.Length).TrimStart('\')
            Copy-WithRetry $b.FullName (Join-Path $InstallDir $rel)
        }
        Write-Log 'Previous files restored.'
    } catch { Write-Log "Restore failed: $_" }
}

try {
    if ($MainPid -gt 0) {
        Start-Process -FilePath (Join-Path $InstallDir 'eve-maj-preview.exe') -WorkingDirectory $InstallDir
    }
    Start-Process -FilePath (Join-Path $InstallDir 'config.exe') -WorkingDirectory $InstallDir
} catch { Write-Log "Relaunch failed: $_" }
if ($ok) { exit 0 } else { exit 1 }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> SemanticVersion {
        SemanticVersion::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    #[test]
    fn semver_parse_accepts_and_rejects_like_zig() {
        for valid in [
            "0.0.4",
            "1.2.3",
            "10.20.30",
            "1.1.2-prerelease+meta",
            "1.1.2+meta-valid",
            "1.0.0-alpha.beta.1",
            "1.0.0-alpha0.valid",
            "1.0.0-alpha.0valid",
            "1.0.0-alpha-a.b-c-somethinglong+build.1-aef.1-its-okay",
            "1.2.3----RC-SNAPSHOT.12.9.1--.12+788",
            "1.0.0+0.build.1-rc.10000aaa-kk-0.1",
            "1_0.2.3",
        ] {
            assert!(SemanticVersion::parse(valid).is_some(), "{valid}");
        }
        for invalid in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.1.1",
            "1.01.1",
            "1.2.3-0123",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-alpha..1",
            "1.2.3-alpha_1",
            "1.2-SNAPSHOT",
            "+invalid",
            "-invalid",
            "1.2.3.DEV",
            "_1.2.3",
            "99999999999999999999999.0.0",
            "v1.2.3",
        ] {
            assert!(SemanticVersion::parse(invalid).is_none(), "{invalid}");
        }
        let full = v("1.2.3-rc.1+build.5");
        assert_eq!((full.major, full.minor, full.patch), (1, 2, 3));
        assert_eq!(full.pre.as_deref(), Some("rc.1"));
        assert_eq!(full.build.as_deref(), Some("build.5"));
    }

    #[test]
    fn semver_order_follows_precedence() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(v(pair[0]).order(&v(pair[1])), Ordering::Less, "{} < {}", pair[0], pair[1]);
            assert_eq!(v(pair[1]).order(&v(pair[0])), Ordering::Greater);
        }
        assert_eq!(v("1.6.1+a").order(&v("1.6.1+b")), Ordering::Equal);
        assert_eq!(parse_tag_version("v1.6.1"), Some(v("1.6.1")));
        assert_eq!(parse_tag_version("1.6.1"), Some(v("1.6.1")));
    }

    const RELEASES: &str = r#"[
        {"tag_name": "v2.0.0", "html_url": "https://x/2.0.0", "draft": true, "body": "draft"},
        {"tag_name": "v1.9.0-rc.1", "html_url": "https://x/rc", "prerelease": true, "body": "rc"},
        {"tag_name": "v1.8.0", "html_url": "https://x/1.8.0", "body": "Eighteen",
         "assets": [
            {"name": "eve-maj-1.8.0.ZIP", "browser_download_url": "https://dl/any.zip", "size": 10},
            {"name": "eve-maj-1.8.0-Portable.zip", "browser_download_url": "https://dl/portable.zip", "size": 5000, "digest": "sha256:abc"},
            {"name": "setup.exe", "browser_download_url": "https://dl/setup.exe", "size": 1}
         ]},
        {"tag_name": "garbage", "html_url": "https://x/g"},
        {"tag_name": "v1.7.0", "html_url": "https://x/1.7.0"},
        {"tag_name": "v1.6.5", "html_url": "https://x/1.6.5", "body": "Six five"},
        {"tag_name": "v1.6.1", "html_url": "https://x/1.6.1", "body": "current"},
        {"tag_name": "v1.5.0", "html_url": "https://x/1.5.0", "body": "old"}
    ]"#;

    #[test]
    fn parse_releases_picks_newest_and_collects_notes() {
        let info = parse_releases(RELEASES.as_bytes(), "1.6.1").unwrap().unwrap();
        assert_eq!(info.version, "v1.8.0");
        assert_eq!(info.url, "https://x/1.8.0");
        assert_eq!(info.notes.as_deref(), Some("v1.8.0\n\nEighteen\n\nv1.6.5\n\nSix five"));
        assert_eq!(info.asset_url.as_deref(), Some("https://dl/portable.zip"));
        assert_eq!(info.asset_name.as_deref(), Some("eve-maj-1.8.0-Portable.zip"));
        assert_eq!(info.asset_size, 5000);
        assert_eq!(info.asset_digest.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn parse_releases_falls_back_to_any_zip() {
        let body = r#"[{"tag_name": "1.7.0", "html_url": "u", "assets": [
            {"name": "a.txt", "browser_download_url": "t"},
            {"name": "first.zip", "browser_download_url": "f", "size": -3},
            {"name": "second.zip", "browser_download_url": "s", "size": 9}
        ]}]"#;
        let info = parse_releases(body.as_bytes(), "1.6.1").unwrap().unwrap();
        assert_eq!(info.version, "1.7.0");
        assert_eq!(info.notes, None);
        assert_eq!(info.asset_url.as_deref(), Some("f"));
        assert_eq!(info.asset_size, 0);
        assert_eq!(info.asset_digest, None);

        let no_assets = parse_releases(br#"[{"tag_name": "1.7.0", "html_url": "u"}]"#, "1.6.1").unwrap().unwrap();
        assert_eq!(no_assets.asset_url, None);
    }

    #[test]
    fn parse_releases_none_cases() {
        assert_eq!(parse_releases(RELEASES.as_bytes(), "1.8.0").unwrap(), None);
        assert_eq!(parse_releases(RELEASES.as_bytes(), "not-a-version").unwrap(), None);
        assert_eq!(parse_releases(br#"{"message": "Not Found"}"#, "1.0.0").unwrap(), None);
        assert_eq!(parse_releases(b"42", "1.0.0").unwrap(), None);
        assert!(matches!(parse_releases(b"[", "1.0.0"), Err(UpdateError::Json(_))));
    }

    #[test]
    fn update_status_roundtrip() {
        let status = UpdateStatus::new();
        assert!(!status.is_available());
        assert_eq!(status.url(), None);
        status.set("v1.2.3", "https://x", Some("notes"));
        assert!(status.is_available());
        assert_eq!(status.version().as_deref(), Some("v1.2.3"));
        assert_eq!(status.url().as_deref(), Some("https://x"));
        assert_eq!(status.notes().as_deref(), Some("notes"));
        status.set("v1.2.4", "https://y", None);
        assert_eq!(status.notes(), None);
        status.clear();
        assert!(!status.is_available());
    }

    fn asset(size: u64, digest: Option<&str>) -> Asset {
        Asset { size, digest: digest.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn verify_download_checks_size_and_sha256() {
        let body = b"hello";
        // sha256("hello")
        let good = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(verify_download(body, &asset(5, Some(good))), Ok(()));
        assert_eq!(verify_download(body, &asset(0, None)), Ok(()));
        assert_eq!(verify_download(body, &asset(5, Some(&good.to_uppercase()))), Ok(()));
        assert_eq!(verify_download(body, &asset(6, Some(good))), Err(StageError::SizeMismatch));
        let zeros = format!("sha256:{}", "0".repeat(64));
        assert_eq!(verify_download(body, &asset(5, Some(&zeros))), Err(StageError::ChecksumMismatch));
        assert_eq!(verify_download(body, &asset(5, Some("md5:abc"))), Err(StageError::UnsupportedDigest));
        assert_eq!(verify_download(body, &asset(5, Some("sha256:2cf24d"))), Err(StageError::UnsupportedDigest));
        assert_eq!(verify_download(body, &asset(5, Some("sha"))), Err(StageError::UnsupportedDigest));
        assert_eq!(verify_download(body, &asset(5, Some(&format!("{good}0")))), Err(StageError::UnsupportedDigest));
    }

    /// Minimal stored zip with one entry, just enough central directory for validate_zip_entry_names.
    fn build_test_zip(name: &str) -> Vec<u8> {
        let n = name.len() as u16;
        let mut out = Vec::new();
        // Local file header (contents irrelevant to the check).
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[0; 22]);
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        let cd_offset = out.len() as u32;
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&[0; 24]);
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(name.as_bytes());
        let cd_size = out.len() as u32 - cd_offset;
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn validate_zip_entry_names_rejects_drive_absolute_traversal_and_protected_entries() {
        let cases = [
            ("config.exe", true),
            ("sub/dir/file.txt", true),
            ("C:/Users/x/Startup/evil.exe", false),
            ("C:\\evil.exe", false),
            ("C:evil.exe", false),
            ("/etc/x", false),
            ("\\\\host\\share\\x", false),
            ("a/../../x", false),
            ("..\\x", false),
            ("profiles/global.settings.json", false),
            ("Update-Backup\\x", false),
            ("x.exe:stream", false),
        ];
        for (name, ok) in cases {
            let result = validate_zip_entry_names(&build_test_zip(name));
            if ok {
                assert_eq!(result, Ok(()), "{name}");
            } else {
                assert_eq!(result, Err(StageError::BadArchive), "{name}");
            }
        }
        assert_eq!(validate_zip_entry_names(b"not a zip"), Err(StageError::BadArchive));
    }

    #[test]
    fn write_ps_literal_doubles_typographic_single_quotes() {
        let script = render_install_script(&ScriptParams {
            staged_dir: "C:\\Users\\O\u{2019}Neil\\staged",
            install_dir: "C:\\EVE",
            log_path: "C:\\log.txt",
            config_pid: 1,
            main_pid: 0,
        });
        assert!(script.contains("O\u{2019}\u{2019}Neil"));
    }

    #[test]
    fn render_install_script_escapes_single_quotes() {
        let script = render_install_script(&ScriptParams {
            staged_dir: "C:\\Users\\O'Neil\\staged",
            install_dir: "C:\\EVE",
            log_path: "C:\\log.txt",
            config_pid: 12,
            main_pid: 0,
        });
        assert!(script.as_bytes().starts_with(b"\xEF\xBB\xBF$Staged = "));
        assert!(script.contains("$Staged = 'C:\\Users\\O''Neil\\staged'"));
        assert!(script.contains("$ConfigPid = 12\r\n$MainPid = 0\r\n$ErrorActionPreference = 'Stop'\n"));
        assert!(script.ends_with("if ($ok) { exit 0 } else { exit 1 }\n"));
    }
}
