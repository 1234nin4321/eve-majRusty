//! Shell integration: opening files/URLs, the clipboard, known folders and the common file dialogs.

use std::ffi::c_void;

use windows_sys::core::{GUID, HRESULT, PWSTR};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE,
};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW;

use crate::{from_wide_ptr, wide};

/// Opens `target` (a file path or URL) with its default handler. Returns false on failure.
pub fn shell_open(target: &str, workdir: Option<&str>) -> bool {
    let target_w = wide(target);
    let open_w = wide("open");
    let workdir_w = workdir.map(wide);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            open_w.as_ptr(),
            target_w.as_ptr(),
            std::ptr::null(),
            workdir_w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
            SW_SHOW,
        )
    };
    result as usize > 32
}

/// The clipboard's text content, or None if it's unavailable or not text.
// The Zig build read CF_TEXT (ANSI); CF_UNICODETEXT keeps non-ASCII text intact.
pub fn clipboard_text() -> Option<String> {
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return None;
    }
    let text = (|| {
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT as u32) };
        if handle.is_null() {
            return None;
        }
        let ptr = unsafe { GlobalLock(handle) } as *const u16;
        let text = unsafe { from_wide_ptr(ptr) };
        if !ptr.is_null() {
            unsafe { GlobalUnlock(handle) };
        }
        text
    })();
    unsafe { CloseClipboard() };
    text
}

/// Replaces the clipboard contents with `text`. Ownership of the allocated handle passes to the clipboard on success, per SetClipboardData's contract.
pub fn set_clipboard_text(text: &str) -> bool {
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return false;
    }
    let ok = (|| {
        let w = wide(text);
        let bytes = w.len() * 2;
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle.is_null() {
            return false;
        }
        let ptr = unsafe { GlobalLock(handle) } as *mut u16;
        if ptr.is_null() {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(w.as_ptr(), ptr, w.len());
            GlobalUnlock(handle);
            EmptyClipboard();
            !SetClipboardData(CF_UNICODETEXT as u32, handle).is_null()
        }
    })();
    unsafe { CloseClipboard() };
    ok
}

/// Resolves a shell known folder (e.g. FOLDERID_Documents) to its current real path, backslash-separated as returned by Windows.
pub fn known_folder_path(folder_id: &GUID) -> Option<String> {
    let mut raw: PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(folder_id, 0, std::ptr::null_mut(), &mut raw) };
    let path = if hr >= 0 { unsafe { from_wide_ptr(raw) } } else { None };
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

const CLSID_FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xDC1C5A9C_E88A_4DDE_A5A1_60F82A20AEF7);
const IID_IFILE_OPEN_DIALOG: GUID = GUID::from_u128(0xD57C7288_D4AD_4768_BE02_9D969532D960);
const FOS_PICKFOLDERS: u32 = 0x0000_0020;
const SIGDN_FILESYSPATH: u32 = 0x8005_8000;

#[repr(C)]
struct FilterSpec {
    name: *const u16,
    spec: *const u16,
}

/// Vtable prefix of IFileOpenDialog up to GetResult — the only slots used here.
#[repr(C)]
struct FileOpenDialogVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    show: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    set_file_types: unsafe extern "system" fn(*mut c_void, u32, *const FilterSpec) -> HRESULT,
    set_file_type_index: usize,
    get_file_type_index: usize,
    advise: usize,
    unadvise: usize,
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_options: usize,
    set_default_folder: usize,
    set_folder: usize,
    get_folder: usize,
    get_current_selection: usize,
    set_file_name: usize,
    get_file_name: usize,
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    set_ok_button_label: usize,
    set_file_name_label: usize,
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

/// Vtable prefix of IShellItem up to GetDisplayName.
#[repr(C)]
struct ShellItemVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    bind_to_handler: usize,
    get_parent: usize,
    get_display_name: unsafe extern "system" fn(*mut c_void, u32, *mut PWSTR) -> HRESULT,
}

/// Calls through a COM object's vtable pointer.
unsafe fn vtbl<'a, T>(obj: *mut c_void) -> &'a T {
    &**(obj as *const *const T)
}

#[derive(Debug)]
pub enum PickerError {
    ComInitFailed,
    CreateDialogFailed,
}

impl std::fmt::Display for PickerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ComInitFailed => "COM initialization failed",
            Self::CreateDialogFailed => "could not create the file dialog",
        })
    }
}

impl std::error::Error for PickerError {}

/// Shows the "Select Folder" dialog with `title`, returning the chosen path or None if cancelled.
/// `owner`, when given, keeps the picker above a topmost owner window (an owned window always draws above its owner, even a HWND_TOPMOST one).
pub fn show_folder_picker(title: &str, owner: Option<HWND>) -> Result<Option<String>, PickerError> {
    show_open_dialog(title, None, owner)
}

/// Shows the "Open File" dialog restricted to `filter_spec` (e.g. "*.wav;*.mp3"); see show_folder_picker for the rest.
pub fn show_file_picker(title: &str, filter_name: &str, filter_spec: &str, owner: Option<HWND>) -> Result<Option<String>, PickerError> {
    show_open_dialog(title, Some((filter_name, filter_spec)), owner)
}

fn show_open_dialog(title: &str, filter: Option<(&str, &str)>, owner: Option<HWND>) -> Result<Option<String>, PickerError> {
    let hr = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    // S_FALSE (already initialized on this thread) is fine; it still needs a matching CoUninitialize.
    if hr < 0 {
        return Err(PickerError::ComInitFailed);
    }
    let result = unsafe { run_open_dialog(title, filter, owner) };
    unsafe { CoUninitialize() };
    result
}

unsafe fn run_open_dialog(title: &str, filter: Option<(&str, &str)>, owner: Option<HWND>) -> Result<Option<String>, PickerError> {
    let mut dialog: *mut c_void = std::ptr::null_mut();
    let hr = CoCreateInstance(&CLSID_FILE_OPEN_DIALOG, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_IFILE_OPEN_DIALOG, &mut dialog);
    if hr < 0 || dialog.is_null() {
        return Err(PickerError::CreateDialogFailed);
    }
    let d: &FileOpenDialogVtbl = vtbl(dialog);

    let title_w = wide(title);
    (d.set_title)(dialog, title_w.as_ptr());

    // Kept alive until Show returns, since the dialog borrows the filter strings.
    let filter_w = filter.map(|(name, spec)| (wide(name), wide(spec)));
    match &filter_w {
        Some((name, spec)) => {
            let specs = [FilterSpec { name: name.as_ptr(), spec: spec.as_ptr() }];
            (d.set_file_types)(dialog, 1, specs.as_ptr());
        }
        None => {
            (d.set_options)(dialog, FOS_PICKFOLDERS);
        }
    }

    let path = if (d.show)(dialog, owner.unwrap_or(std::ptr::null_mut())) < 0 {
        None
    } else {
        let mut item: *mut c_void = std::ptr::null_mut();
        if (d.get_result)(dialog, &mut item) < 0 || item.is_null() {
            None
        } else {
            let i: &ShellItemVtbl = vtbl(item);
            let mut name: PWSTR = std::ptr::null_mut();
            let path = if (i.get_display_name)(item, SIGDN_FILESYSPATH, &mut name) >= 0 { from_wide_ptr(name) } else { None };
            CoTaskMemFree(name.cast());
            (i.release)(item);
            path
        }
    };
    (d.release)(dialog);
    Ok(path)
}
