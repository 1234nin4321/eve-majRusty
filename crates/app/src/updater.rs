//! The config dialog's in-place updater: check GitHub for a newer release, download and stage its portable zip, then hand off to a PowerShell script that swaps the files once both processes have exited.
//! Each handler returns the JSON string the config dialog's webui binding hands back to the page.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use eve_maj_core::log::Scope;
use eve_maj_core::update::{self, render_install_script, Asset, ScriptParams, UpdateChecker, UpdateInfo, CURRENT_VERSION, UPDATE_STATUS};
use eve_maj_win::sys::Win32::Foundation::HWND;
use eve_maj_win::sys::Win32::System::Threading::GetCurrentProcessId;
use eve_maj_win::sys::Win32::UI::Shell::ShellExecuteW;
use eve_maj_win::sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, PostMessageW, SW_HIDE, WM_COMMAND};
use eve_maj_win::{shell, wide, window};
use serde::Serialize;

use crate::update_stage;

const SLOG: Scope = Scope::new("updater");
const SLOG_UPDATE: Scope = Scope::new("update");

/// The tray menu's Exit command id (win32.zig's IDM_EXIT); posting it shuts the main app down through its normal path.
const IDM_EXIT: usize = 1001;

const INTERNAL_ERROR_JSON: &str = r#"{"success":false,"error":"internal"}"#;

/// Returns the main app's window, if it is running.
pub type FindMainApp = fn() -> Option<HWND>;

static FIND_MAIN_APP: OnceLock<FindMainApp> = OnceLock::new();

struct State {
    /// Newest release found by the last successful check.
    pending: Option<UpdateInfo>,
    /// Set once that release's zip is downloaded, verified and unpacked.
    staged_dir: Option<PathBuf>,
    staged_version: Option<String>,
}

/// webui may run handlers on its own threads, so the check/download/install state is shared under this lock.
static STATE: Mutex<State> = Mutex::new(State { pending: None, staged_dir: None, staged_version: None });

fn lock_state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn init(find_main_app: FindMainApp) {
    let _ = FIND_MAIN_APP.set(find_main_app);
}

/// %TEMP%\EVE-Maj-Update - holds the downloaded zip, the staged files, the install script and its log.
fn work_dir() -> Option<PathBuf> {
    let temp = std::env::var_os("TEMP").or_else(|| std::env::var_os("TMP"))?;
    Some(PathBuf::from(temp).join("EVE-Maj-Update"))
}

impl State {
    fn clear_staged(&mut self) {
        self.staged_dir = None;
        self.staged_version = None;
    }
}

fn return_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| INTERNAL_ERROR_JSON.to_string())
}

fn return_error(code: &str) -> String {
    format!(r#"{{"success":false,"error":"{code}"}}"#)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckResponse<'a> {
    success: bool,
    available: bool,
    current_version: &'a str,
    version: Option<&'a str>,
    url: Option<&'a str>,
    notes: Option<&'a str>,
    asset_name: Option<&'a str>,
    asset_size: u64,
    /// Whether GitHub published a SHA-256 the download will be checked against.
    has_digest: bool,
    /// True when this release is already downloaded and ready to install.
    staged: bool,
}

#[derive(Serialize)]
struct DownloadResponse<'a> {
    success: bool,
    version: &'a str,
}

/// Queries GitHub releases on demand (not just at startup). Response: CheckResponse JSON, or {success:false,error}.
pub fn check_for_update_now() -> String {
    let checker = UpdateChecker::new();
    let maybe_info = match checker.check_for_updates() {
        Ok(info) => info,
        Err(err) => {
            SLOG.warn(format_args!("Manual update check failed: {err}"));
            return return_error("check_failed");
        }
    };

    let Some(info) = maybe_info else {
        lock_state().pending = None;
        return return_json(&CheckResponse {
            success: true,
            available: false,
            current_version: CURRENT_VERSION,
            version: None,
            url: None,
            notes: None,
            asset_name: None,
            asset_size: 0,
            has_digest: false,
            staged: false,
        });
    };

    UPDATE_STATUS.set(&info.version, &info.url, info.notes.as_deref());

    let mut state = lock_state();
    // A previously staged download only counts if it's this same release.
    if state.staged_version.as_deref().is_some_and(|v| v != info.version) {
        state.clear_staged();
    }

    let json = return_json(&CheckResponse {
        success: true,
        available: true,
        current_version: CURRENT_VERSION,
        version: Some(&info.version),
        url: Some(&info.url),
        notes: info.notes.as_deref(),
        asset_name: info.asset_name.as_deref(),
        asset_size: info.asset_size,
        has_digest: info.asset_digest.is_some(),
        staged: state.staged_dir.is_some(),
    });
    state.pending = Some(info);
    json
}

/// Downloads, verifies and unpacks the release found by the last check. Response: {success, version} or {success:false,error}.
pub fn download_update() -> String {
    // Copied out so the lock isn't held across the network download.
    let (asset, version) = {
        let state = lock_state();
        let Some(pending) = &state.pending else {
            return return_error("no_update");
        };
        let Some(url) = &pending.asset_url else {
            return return_error("no_asset");
        };
        let asset = Asset {
            url: url.clone(),
            name: pending.asset_name.clone().unwrap_or_else(|| "update.zip".to_string()),
            size: pending.asset_size,
            digest: pending.asset_digest.clone(),
        };
        (asset, pending.version.clone())
    };

    let Some(work_dir) = work_dir() else {
        return return_error("no_temp");
    };

    SLOG.info(format_args!("Downloading update {version} from {}", asset.url));
    let staged = match update_stage::download_and_stage(&asset, &work_dir) {
        Ok(staged) => staged,
        Err(err) => {
            SLOG.warn(format_args!("Update download/staging failed: {err}"));
            return return_error(err.code());
        }
    };

    let mut state = lock_state();
    SLOG.info(format_args!("Update {version} staged at {}", staged.display()));
    state.staged_dir = Some(staged);
    state.staged_version = Some(version.clone());
    return_json(&DownloadResponse { success: true, version: &version })
}

/// Launches the install script for the staged release and asks the main app to exit; the dialog closes itself once this returns success. Response: {success} or {success:false,error}.
pub fn install_update() -> String {
    let Some(staged_dir) = lock_state().staged_dir.clone() else {
        return return_error("not_downloaded");
    };

    let Some(install_dir) = window::self_exe_dir() else {
        SLOG.err(format_args!("Failed to resolve install directory"));
        return return_error("internal");
    };

    // Fail here, while the app is still running, rather than leaving the script to discover it (e.g. an install under Program Files).
    if !is_writable(&install_dir) {
        SLOG.warn(format_args!("Install directory is not writable: {}", install_dir.display()));
        return return_error("not_writable");
    }

    let Some(work_dir) = work_dir() else {
        return return_error("no_temp");
    };
    let main_hwnd = FIND_MAIN_APP.get().and_then(|find| find());
    let mut main_pid: u32 = 0;
    if let Some(hwnd) = main_hwnd {
        unsafe { GetWindowThreadProcessId(hwnd, &mut main_pid) };
    }

    let script = render_install_script(&ScriptParams {
        staged_dir: &staged_dir.to_string_lossy(),
        install_dir: &install_dir.to_string_lossy(),
        log_path: &work_dir.join("install.log").to_string_lossy(),
        config_pid: unsafe { GetCurrentProcessId() },
        main_pid,
    });

    let script_path = work_dir.join("install.ps1");
    if let Err(err) = fs::write(&script_path, script) {
        SLOG.err(format_args!("Failed to write install script: {err}"));
        return return_error("script_failed");
    }

    if !launch_script(&script_path) {
        return return_error("launch_failed");
    }

    SLOG.info(format_args!("Install script launched (main app pid {main_pid}); closing for update"));
    // Same command as the tray's Exit item, so the main app shuts down through its normal path.
    if let Some(hwnd) = main_hwnd {
        unsafe { PostMessageW(hwnd, WM_COMMAND, IDM_EXIT, 0) };
    }
    r#"{"success":true}"#.to_string()
}

fn is_writable(dir_path: &Path) -> bool {
    let probe = dir_path.join(".update-write-test");
    if fs::write(&probe, b"").is_err() {
        return false;
    }
    let _ = fs::remove_file(&probe);
    true
}

fn launch_script(script_path: &Path) -> bool {
    let params = format!("-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"", script_path.to_string_lossy());
    let params_w = wide(&params);
    let open_w = wide("open");
    let file_w = wide("powershell.exe");
    let result = unsafe {
        ShellExecuteW(std::ptr::null_mut(), open_w.as_ptr(), file_w.as_ptr(), params_w.as_ptr(), std::ptr::null(), SW_HIDE)
    };
    if result as usize <= 32 {
        SLOG.err(format_args!("Failed to launch install script (ShellExecuteW returned {})", result as usize));
        return false;
    }
    true
}

/// Opens the stored release's page (or the repo's releases list) in the default browser.
pub fn open_releases_page() {
    let url = update::releases_page_url();

    SLOG_UPDATE.info(format_args!("Opening releases page: {url}"));

    if !shell::shell_open(&url, None) {
        SLOG_UPDATE.err(format_args!("Failed to open URL in browser"));
    }
}
