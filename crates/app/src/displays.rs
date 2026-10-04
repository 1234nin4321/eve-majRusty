//! The config dialog's Display Config tab: a read-only snapshot of every connected display (resolution, refresh rate, scaling, arrangement, GPU, monitor model) plus an "Identify" overlay. Never changes display settings. Bound into config_dialog only.
//!
//! The record, JSON shape and mode maths are in eve_maj_core::display_info; this file is the Win32 side.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use eve_maj_core::display_info::{
    displays_json, gdi_number, orientation_degrees, scale_percent, wide_to_utf8, Display, ModeScan, Rect,
};
use eve_maj_core::log::Scope;
use eve_maj_win::geometry::monitor_dpi;
use eve_maj_win::wide;
use windows_sys::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows_sys::Win32::Foundation::{BOOL, FALSE, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, EnumDisplayDevicesW, EnumDisplayMonitors,
    EnumDisplaySettingsW, FillRect, GetMonitorInfoW, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, DEFAULT_CHARSET,
    DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DT_CENTER, DT_SINGLELINE, DT_VCENTER, ENUM_CURRENT_SETTINGS, FW_BOLD, HDC,
    HMONITOR, MONITORINFOEXW, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW, GetWindowLongPtrW, KillTimer,
    PostQuitMessage, RegisterClassExW, SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage, GWLP_USERDATA,
    MONITORINFOF_PRIMARY, MSG, SW_SHOWNOACTIVATE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

const SLOG: Scope = Scope::new("displays");

fn rect_from(r: RECT) -> Rect {
    Rect::from_edges(r.left, r.top, r.right, r.bottom)
}

unsafe extern "system" fn enum_monitor_proc(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let monitors = &mut *(data as *mut Vec<HMONITOR>);
    monitors.push(monitor);
    TRUE
}

struct TargetNames {
    friendly: String,
    device_path: String,
}

/// GDI device name (\\.\DISPLAYn) -> monitor model and device path, via the DisplayConfig API. Empty on failure; callers fall back to EnumDisplayDevices.
fn query_target_names() -> HashMap<String, TargetNames> {
    let mut map = HashMap::new();

    let (mut path_count, mut mode_count) = (0u32, 0u32);
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count) } != 0 {
        return map;
    }
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![unsafe { std::mem::zeroed() }; path_count as usize];
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { std::mem::zeroed() }; mode_count as usize];
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        SLOG.warn(format_args!("QueryDisplayConfig failed; monitor model names unavailable"));
        return map;
    }
    paths.truncate(path_count as usize);

    for path in &paths {
        let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
        source.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }

        let mut target: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { std::mem::zeroed() };
        target.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
            continue;
        }

        let gdi = wide_to_utf8(&source.viewGdiDeviceName);
        if gdi.is_empty() {
            continue;
        }
        // Mirrored/cloned displays share a source; the first target wins.
        map.entry(gdi).or_insert_with(|| TargetNames {
            friendly: wide_to_utf8(&target.monitorFriendlyDeviceName),
            device_path: wide_to_utf8(&target.monitorDevicePath),
        });
    }
    map
}

fn new_display_device() -> DISPLAY_DEVICEW {
    let mut dev: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
    dev.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
    dev
}

/// GDI device name -> graphics adapter description (e.g. "NVIDIA GeForce RTX 4070").
fn query_adapter_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for i in 0..64u32 {
        let mut dev = new_display_device();
        if unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut dev, 0) } == FALSE {
            break;
        }
        if dev.StateFlags & DISPLAY_DEVICE_ACTIVE == 0 {
            continue;
        }
        map.insert(wide_to_utf8(&dev.DeviceName), wide_to_utf8(&dev.DeviceString));
    }
    map
}

/// Windows' own description of the monitor on `gdi_name_w` (often "Generic PnP Monitor"), for when DisplayConfig has no EDID name.
fn generic_monitor_name(gdi_name_w: *const u16) -> Option<String> {
    let mut dev = new_display_device();
    if unsafe { EnumDisplayDevicesW(gdi_name_w, 0, &mut dev, 0) } == FALSE {
        return None;
    }
    let name = wide_to_utf8(&dev.DeviceString);
    (!name.is_empty()).then_some(name)
}

fn new_dev_mode() -> DEVMODEW {
    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    dm
}

/// Every connected, active display, ordered by display number.
pub fn enumerate() -> Vec<Display> {
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(enum_monitor_proc), &mut monitors as *mut _ as LPARAM)
    };

    let targets = query_target_names();
    let adapters = query_adapter_names();

    let mut out: Vec<Display> = Vec::new();
    for monitor in monitors {
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut _) } == FALSE {
            continue;
        }

        let gdi_len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len() - 1);
        info.szDevice[gdi_len] = 0;
        let gdi_w = info.szDevice.as_ptr();
        let gdi_name = wide_to_utf8(&info.szDevice);

        let mut current = new_dev_mode();
        let have_mode = unsafe { EnumDisplaySettingsW(gdi_w, ENUM_CURRENT_SETTINGS, &mut current) } != FALSE;
        let bounds = rect_from(info.monitorInfo.rcMonitor);
        let mode_w = if have_mode { current.dmPelsWidth } else { bounds.width.max(0) as u32 };
        let mode_h = if have_mode { current.dmPelsHeight } else { bounds.height.max(0) as u32 };

        let mut scan = ModeScan::new(mode_w, mode_h, if have_mode { current.dmDisplayFrequency } else { 0 });
        for mode_index in 0..4096u32 {
            let mut dm = new_dev_mode();
            if unsafe { EnumDisplaySettingsW(gdi_w, mode_index, &mut dm) } == FALSE {
                break;
            }
            scan.consider(dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency);
        }

        let target = targets.get(&gdi_name);
        let friendly = target.filter(|t| !t.friendly.is_empty()).map(|t| t.friendly.clone());
        let number = gdi_number(&gdi_name);
        let id = target.filter(|t| !t.device_path.is_empty()).map_or_else(|| gdi_name.clone(), |t| t.device_path.clone());

        out.push(Display {
            number,
            id,
            name: friendly.or_else(|| generic_monitor_name(gdi_w)).unwrap_or_else(|| format!("Display {number}")),
            gpu: adapters.get(&gdi_name).cloned(),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            bounds,
            work_area: rect_from(info.monitorInfo.rcWork),
            mode_width: mode_w,
            mode_height: mode_h,
            refresh_hz: if have_mode { current.dmDisplayFrequency } else { 0 },
            bits_per_pixel: if have_mode { current.dmBitsPerPel } else { 0 },
            orientation: if have_mode { orientation_degrees(unsafe { current.Anonymous1.Anonymous2.dmDisplayOrientation }) } else { 0 },
            scale_percent: scale_percent(monitor_dpi(monitor)),
            max_width: scan.max_width,
            max_height: scan.max_height,
            max_refresh_hz: scan.max_refresh_hz,
            gdi_name,
        });
    }

    out.sort_by_key(|d| d.number);
    out
}

/// Response: DisplaysResponse JSON.
pub fn get_displays() -> String {
    displays_json(&enumerate())
}

// ---- Identify overlay: a big number in the corner of each display for a few seconds ----

const IDENTIFY_CLASS: &str = "EVEMajIdentifyDisplay";
const IDENTIFY_SIZE: i32 = 200;
const IDENTIFY_MARGIN: i32 = 48;
const IDENTIFY_DURATION_MS: u32 = 3000;
const IDENTIFY_TIMER_ID: usize = 1;
/// The dialog's accent (#d9a441) and background (#0b0c0d) as COLORREF (0x00BBGGRR).
const IDENTIFY_TEXT_COLOR: u32 = 0x0041A4D9;
const IDENTIFY_BG_COLOR: u32 = 0x000D0C0B;

struct IdentifyTarget {
    number: u32,
    x: i32,
    y: i32,
}

/// Only one overlay set at a time; a second click while it's up is ignored.
static IDENTIFY_RUNNING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Windows still open on the identify thread; owned by that thread only.
    static IDENTIFY_OPEN: Cell<u32> = const { Cell::new(0) };
}

/// Shows the overlay without blocking the caller. Response: {success}.
pub fn identify_displays() -> &'static str {
    if IDENTIFY_RUNNING.swap(true, Ordering::AcqRel) {
        return "{\"success\":true}";
    }

    let targets: Vec<IdentifyTarget> = enumerate()
        .iter()
        .map(|d| IdentifyTarget { number: d.number, x: d.bounds.x + IDENTIFY_MARGIN, y: d.bounds.y + IDENTIFY_MARGIN })
        .collect();

    match std::thread::Builder::new().name("identify-displays".into()).spawn(move || identify_thread(targets)) {
        Ok(_) => "{\"success\":true}",
        Err(err) => {
            SLOG.warn(format_args!("Failed to start identify thread: {err}"));
            IDENTIFY_RUNNING.store(false, Ordering::Release);
            "{\"success\":false}"
        }
    }
}

/// Only touched by the identify thread, and IDENTIFY_RUNNING keeps that to one at a time.
static IDENTIFY_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Clears the running flag however the identify thread exits.
struct RunningGuard;

impl Drop for RunningGuard {
    fn drop(&mut self) {
        IDENTIFY_RUNNING.store(false, Ordering::Release);
    }
}

/// Owns its windows and message loop, so it never touches webui's thread.
fn identify_thread(targets: Vec<IdentifyTarget>) {
    let _running = RunningGuard;

    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        return;
    }
    let class_name = wide(IDENTIFY_CLASS);
    if !IDENTIFY_CLASS_REGISTERED.load(Ordering::Acquire) {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(identify_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        if unsafe { RegisterClassExW(&wc) } == 0 {
            SLOG.warn(format_args!("Failed to register identify window class"));
            return;
        }
        IDENTIFY_CLASS_REGISTERED.store(true, Ordering::Release);
    }

    let title = wide("");
    IDENTIFY_OPEN.with(|open| open.set(0));
    for t in &targets {
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                t.x,
                t.y,
                IDENTIFY_SIZE,
                IDENTIFY_SIZE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            continue;
        }
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, t.number as isize);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(hwnd, IDENTIFY_TIMER_ID, IDENTIFY_DURATION_MS, None);
        }
        IDENTIFY_OPEN.with(|open| open.set(open.get() + 1));
    }
    if IDENTIFY_OPEN.with(Cell::get) == 0 {
        return;
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn identify_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            if hdc.is_null() {
                return 0;
            }

            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);
            let bg = CreateSolidBrush(IDENTIFY_BG_COLOR);
            if !bg.is_null() {
                FillRect(hdc, &rect, bg);
                DeleteObject(bg);
            }
            let accent = CreateSolidBrush(IDENTIFY_TEXT_COLOR);
            if !accent.is_null() {
                // 4px accent frame, drawn as four strips.
                let t = 4;
                let strips = [
                    RECT { left: 0, top: 0, right: rect.right, bottom: t },
                    RECT { left: 0, top: rect.bottom - t, right: rect.right, bottom: rect.bottom },
                    RECT { left: 0, top: 0, right: t, bottom: rect.bottom },
                    RECT { left: rect.right - t, top: 0, right: rect.right, bottom: rect.bottom },
                ];
                for s in &strips {
                    FillRect(hdc, s, accent);
                }
                DeleteObject(accent);
            }

            let number = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32;
            let text: Vec<u16> = number.to_string().encode_utf16().collect();

            let face = wide("Segoe UI");
            let font = CreateFontW(
                150,
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                ANTIALIASED_QUALITY as u32,
                0,
                face.as_ptr(),
            );
            let old_font = if font.is_null() { std::ptr::null_mut() } else { SelectObject(hdc, font) };
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, IDENTIFY_TEXT_COLOR);
            DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            if !old_font.is_null() {
                SelectObject(hdc, old_font);
            }
            if !font.is_null() {
                DeleteObject(font);
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_TIMER => {
            KillTimer(hwnd, IDENTIFY_TIMER_ID);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            IDENTIFY_OPEN.with(|open| {
                if open.get() > 0 {
                    open.set(open.get() - 1);
                }
                if open.get() == 0 {
                    PostQuitMessage(0);
                }
            });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
