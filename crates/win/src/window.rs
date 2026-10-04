//! Window, class and process lookups.

use windows_sys::Win32::Foundation::{CloseHandle, HWND, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::ProcessStatus::GetModuleFileNameExW;
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW, IsIconic, IsWindow, IsWindowVisible};

/// The window's title, or None if it has none (or the window is gone).
pub fn window_title(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 256];
    let len = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

pub fn class_name(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// Full executable path of `process_id`'s process, or None if it can't be opened or queried.
pub fn process_exe_path(process_id: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if handle.is_null() {
        return None;
    }
    let mut buf = [0u16; MAX_PATH as usize];
    let len = unsafe { GetModuleFileNameExW(handle, std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    unsafe { CloseHandle(handle) };
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// Full path of the current process's own executable.
pub fn self_exe_path() -> Option<String> {
    let mut buf = vec![0u16; 32 * 1024];
    let len = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (len > 0 && len < buf.len()).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// Directory containing the current process's own executable.
pub fn self_exe_dir() -> Option<std::path::PathBuf> {
    self_exe_path().and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_path_buf()))
}

pub fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd) != 0 }
}

pub fn is_window_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd) != 0 }
}

pub fn is_window_iconic(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd) != 0 }
}

/// HWND <-> GWLP_USERDATA / LPARAM round trips.
pub fn hwnd_to_isize(hwnd: HWND) -> isize {
    hwnd as isize
}

pub fn isize_to_hwnd(value: isize) -> Option<HWND> {
    (value != 0).then_some(value as HWND)
}
