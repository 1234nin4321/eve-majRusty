//! Win32 side of the evemajpreview:// protocol: registry registration, finding the running instance and
//! sending it commands over WM_COPYDATA / WM_PROTOCOL_HOTKEY, and the region-select result mapping.
//! URL parsing, the command set and the wire layouts are in eve_maj_core::protocol.

// windows-sys handles (HWND, HANDLE) are raw pointers that are only ever passed back to Win32, never dereferenced here.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;

use eve_maj_core::log::Scope;
pub use eve_maj_core::protocol::*;
use eve_maj_win::wide;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, FALSE, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, RECT};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageW, WM_COPYDATA};

const SLOG: Scope = Scope::new("protocol");

const REGISTRY_KEY: &str = "Software\\Classes\\evemajpreview";
const REGISTRY_COMMAND_KEY: &str = "Software\\Classes\\evemajpreview\\shell\\open\\command";

pub fn find_existing_instance(class_name: &str) -> Option<HWND> {
    let class = wide(class_name);
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    (!hwnd.is_null()).then_some(hwnd)
}

fn send_copy_data(hwnd: HWND, dw_data: usize, payload: Option<&[u8]>) {
    let cds = COPYDATASTRUCT {
        dwData: dw_data,
        cbData: payload.map_or(0, |p| p.len() as u32),
        lpData: payload.map_or(std::ptr::null_mut(), |p| p.as_ptr() as *mut _),
    };
    unsafe { SendMessageW(hwnd, WM_COPYDATA, 0, &cds as *const COPYDATASTRUCT as LPARAM) };
}

pub fn send_command_to_instance(hwnd: HWND, cmd: &Command) {
    match cmd.to_wire() {
        WireMessage::Hotkey { wparam } => {
            unsafe { SendMessageW(hwnd, WM_PROTOCOL_HOTKEY, wparam, 0) };
        }
        WireMessage::CopyData { dw_data, payload } => send_copy_data(hwnd, dw_data, payload.as_deref()),
    }
    match cmd {
        Command::Switch(char_name) => {
            SLOG.info(format_args!("Sent switch to '{}'", String::from_utf8_lossy(char_name)))
        }
        Command::Profile(profile_name) => {
            SLOG.info(format_args!("Sent load profile '{}'", String::from_utf8_lossy(profile_name)))
        }
        Command::Hotkey(hotkey_action) => SLOG.info(format_args!("Sent hotkey action '{}'", hotkey_action.name())),
        Command::PreviewThumbnail(json) => {
            SLOG.debug(format_args!("Sent thumbnail preview patch ({} bytes)", json.len()))
        }
        Command::RevertPreview => SLOG.info(format_args!("Sent revert preview")),
        Command::DialogSuspendHotkeys => SLOG.debug(format_args!("Sent dialog suspend hotkeys")),
        Command::DialogResumeHotkeys => SLOG.debug(format_args!("Sent dialog resume hotkeys")),
        Command::StartRegionSelect(_) => SLOG.info(format_args!("Sent start region select")),
        Command::TestNotification(json) => SLOG.debug(format_args!("Sent test notification ({} bytes)", json.len())),
    }
}

/// The payload of a received WM_COPYDATA, or None when lpData is NULL.
///
/// # Safety
/// `cds` must be the COPYDATASTRUCT of a WM_COPYDATA message currently being handled.
pub unsafe fn copy_data_payload(cds: &COPYDATASTRUCT) -> Option<&[u8]> {
    if cds.lpData.is_null() {
        return None;
    }
    Some(std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize))
}

/// Receiving side of a WM_COPYDATA: the command it carries (see `Command::from_copy_data`).
///
/// # Safety
/// `cds` must be the COPYDATASTRUCT of a WM_COPYDATA message currently being handled.
pub unsafe fn command_from_copy_data(cds: &COPYDATASTRUCT) -> Option<Command> {
    Command::from_copy_data(cds.dwData, copy_data_payload(cds))
}

/// Receiving side of `Command::StartRegionSelect`; a malformed payload falls back to a plain fresh drag.
///
/// # Safety
/// `cds` must be the COPYDATASTRUCT of a WM_COPYDATA message currently being handled.
pub unsafe fn region_select_request_from_copy_data(cds: &COPYDATASTRUCT) -> RegionSelectRequest {
    RegionSelectRequest::from_wire_bytes(copy_data_payload(cds))
}

pub fn to_wire_rect(r: RECT) -> Rect {
    Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

pub fn from_wire_rect(r: Rect) -> RECT {
    RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

// A named file mapping only lives while a handle to it stays open somewhere, so this is kept open for the process's lifetime rather than per-write.
// Stored as an integer since HANDLE is a raw pointer (not Send); 0 means not created yet.
static REGION_SELECT_MAPPING: Mutex<usize> = Mutex::new(0);

fn ensure_region_select_mapping() -> Option<HANDLE> {
    let mut cached = REGION_SELECT_MAPPING.lock().unwrap_or_else(|e| e.into_inner());
    if *cached != 0 {
        return Some(*cached as HANDLE);
    }
    let name = wide(REGION_SELECT_MAPPING_NAME);
    let mapping = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            REGION_SELECT_RESULT_SIZE as u32,
            name.as_ptr(),
        )
    };
    if mapping.is_null() {
        SLOG.warn(format_args!("Failed to create region-select file mapping"));
        return None;
    }
    *cached = mapping as usize;
    Some(mapping)
}

/// Reads the current result; returns None if the mapping doesn't exist yet (main app hasn't published a result this run).
pub fn read_region_select_result() -> Option<RegionSelectResult> {
    let name = wide(REGION_SELECT_MAPPING_NAME);
    let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, name.as_ptr()) };
    if mapping.is_null() {
        return None;
    }

    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, REGION_SELECT_RESULT_SIZE) };
    let result = if view.Value.is_null() {
        SLOG.warn(format_args!("Failed to map view of region-select file mapping"));
        None
    } else {
        let wire = unsafe { std::ptr::read_volatile(view.Value as *const RegionSelectResultWire) };
        unsafe { UnmapViewOfFile(view) };
        Some(RegionSelectResult::from(wire))
    };
    unsafe { CloseHandle(mapping) };
    result
}

/// Increments the sequence and writes a fresh result; called by the main app once a drag finishes or is cancelled.
pub fn publish_region_select_result(status: RegionSelectStatus, rect: RECT) {
    let Some(mapping) = ensure_region_select_mapping() else {
        SLOG.err(format_args!("Failed to create region-select result mapping"));
        return;
    };

    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, REGION_SELECT_RESULT_SIZE) };
    if view.Value.is_null() {
        SLOG.err(format_args!("Failed to map region-select result view"));
        return;
    }

    let result_ptr = view.Value as *mut RegionSelectResultWire;
    unsafe {
        let current = std::ptr::read_volatile(result_ptr);
        std::ptr::write_volatile(result_ptr, current.next(status, to_wire_rect(rect)));
        UnmapViewOfFile(view);
    }
}

/// Returns the protocol URL if --protocol was passed, otherwise None.
pub fn check_command_line() -> Option<String> {
    protocol_url_from_args(std::env::args_os().map(|a| a.to_string_lossy().into_owned()))
}

pub fn is_registered() -> bool {
    let key = wide(REGISTRY_KEY);
    let mut h_key: HKEY = std::ptr::null_mut();
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_READ, &mut h_key) };

    if result == ERROR_SUCCESS {
        unsafe { RegCloseKey(h_key) };
        return true;
    }

    false
}

/// Closes a registry key on drop.
struct KeyGuard(HKEY);

impl Drop for KeyGuard {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn create_key(sub_key: &str) -> Result<KeyGuard, u32> {
    let sub_key = wide(sub_key);
    let mut h_key: HKEY = std::ptr::null_mut();
    let mut disposition = 0u32;
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub_key.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut h_key,
            &mut disposition,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(result);
    }
    Ok(KeyGuard(h_key))
}

/// Writes a REG_SZ value; `name` None sets the key's default value.
fn set_string_value(key: &KeyGuard, name: Option<&str>, data: &str) -> u32 {
    let name = name.map(wide);
    // wide() appends the terminator, which REG_SZ data must include.
    let data = wide(data);
    unsafe {
        RegSetValueExW(
            key.0,
            name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            0,
            REG_SZ,
            data.as_ptr().cast::<u8>(),
            (data.len() * 2) as u32,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// The process's own executable path couldn't be read.
    SelfExePath,
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for RegisterError {}

pub fn register() -> Result<bool, RegisterError> {
    let exe_path = eve_maj_win::window::self_exe_path().ok_or(RegisterError::SelfExePath)?;

    // HKCU rather than HKEY_CLASSES_ROOT: the latter falls back to HKLM for new keys, which requires admin rights.
    let h_key = match create_key(REGISTRY_KEY) {
        Ok(k) => k,
        Err(result) => {
            SLOG.err(format_args!(
                "Failed to create registry key HKCU\\Software\\Classes\\evemajpreview: error {result}"
            ));
            return Ok(false);
        }
    };

    let result = set_string_value(&h_key, None, "URL:EVE-Maj Preview Protocol");
    if result != ERROR_SUCCESS {
        SLOG.err(format_args!("Failed to set default value: error {result}"));
        return Ok(false);
    }

    let result = set_string_value(&h_key, Some("URL Protocol"), "");
    if result != ERROR_SUCCESS {
        SLOG.err(format_args!("Failed to set URL Protocol value: error {result}"));
        return Ok(false);
    }

    let h_command_key = match create_key(REGISTRY_COMMAND_KEY) {
        Ok(k) => k,
        Err(result) => {
            SLOG.err(format_args!("Failed to create command key: error {result}"));
            return Ok(false);
        }
    };

    let command = format!("\"{exe_path}\" --protocol \"%1\"");
    let result = set_string_value(&h_command_key, None, &command);
    if result != ERROR_SUCCESS {
        SLOG.err(format_args!("Failed to set command value: error {result}"));
        return Ok(false);
    }

    SLOG.info(format_args!("Protocol handler registered successfully: {exe_path}"));
    Ok(true)
}
