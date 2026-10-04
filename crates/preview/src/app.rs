//! Process lifecycle: startup, the timer window's message dispatch, the per-tick pipeline, profile reloads and the
//! config dialog's live-preview IPC (main.zig).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use eve_maj_app::protocol as ipc;
use eve_maj_app::{sound, tts};
use eve_maj_core::config::{Config, ConfigError, GlobalSettings, NotificationTypeConfig, DEFAULT_PROFILE};
use eve_maj_core::log::{self, LogLevel, Scope};
use eve_maj_core::protocol::{self, Command, HotkeyAction, TIMER_CLASS_NAME, WM_PROTOCOL_HOTKEY};
use eve_maj_core::types::NotificationType;
use eve_maj_core::update::{UpdateChecker, CURRENT_VERSION};
use eve_maj_win::time::Ticks;
use eve_maj_win::{wide, window};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ};
use windows_sys::Win32::System::Console::{AllocConsole, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Shell::{NIIF_INFO, NIIF_WARNING};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::fonts;
use crate::gdi_overlay;
use crate::globals::{self, Global};
use crate::input;
use crate::painter::Painter;
use crate::scout::Scout;
use crate::tray::{self, TrayIcon, WM_HOTKEYS_STATE_CHANGED, WM_SWITCH_PROFILE, WM_TOGGLE_VISIBILITY, WM_TRAYICON};

const SLOG: Scope = Scope::new("main");

const TIMER_ID: usize = 1;

// Scan throttling: only run expensive EnumWindows every N ticks; 20 ticks at 50ms/tick is roughly 1 second between scans.
const SCAN_INTERVAL_TICKS: u32 = 20;
const TRAVEL_CHECK_INTERVAL_MS: u64 = 2000;

struct TickState {
    scan_tick_counter: u32,
    last_travel_check: Ticks,
}

static TICK_STATE: Global<TickState> = Global::new();
static TRAY_ICON: Global<TrayIcon> = Global::new();

unsafe extern "system" fn timer_window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            if let Some(icon) = TRAY_ICON.get() {
                icon.handle_tray_message(lparam);
            }
            0
        }
        WM_COMMAND => {
            tray::handle_menu_command(wparam as u16);
            0
        }
        WM_TIMER => {
            if wparam == TIMER_ID {
                on_timer_tick();
            }
            0
        }
        WM_HOTKEY => {
            // Registered hotkeys arrive here once the hotkey manager is ported.
            0
        }
        WM_HOTKEYS_STATE_CHANGED => {
            if let (Some(icon), Some(manager)) = (TRAY_ICON.get(), globals::hotkey_manager()) {
                if globals::config().hotkeys.suspend_hotkey_notification {
                    if manager.are_hotkeys_suspended() {
                        icon.show_balloon("EVE-Maj Preview", "Hotkeys suspended", NIIF_WARNING);
                    } else {
                        icon.show_balloon("EVE-Maj Preview", "Hotkeys resumed", NIIF_INFO);
                    }
                }
            }
            0
        }
        WM_TOGGLE_VISIBILITY => {
            if let Some(painter) = globals::painter() {
                painter.toggle_all_thumbnails_visibility();
            }
            0
        }
        WM_SWITCH_PROFILE => {
            if let Some(new_profile) = TrayIcon::take_pending_profile_name() {
                SLOG.info(format_args!("Switching to profile: {new_profile}"));
                if let Err(err) = reload_with_profile(&new_profile) {
                    SLOG.err(format_args!("Failed to switch profile to {new_profile}: {err}"));
                }
            }
            0
        }
        WM_COPYDATA => {
            let cds = &*(lparam as *const COPYDATASTRUCT);
            if let Some(cmd) = ipc::command_from_copy_data(cds) {
                handle_command(cmd);
            }
            0
        }
        WM_PROTOCOL_HOTKEY => {
            // wParam identifies which hotkey action the protocol handler requested
            match HotkeyAction::from_wparam(wparam) {
                Some(action) => {
                    SLOG.info(format_args!("Protocol handler: {}", action.name()));
                    if globals::hotkey_manager().is_none() {
                        SLOG.warn(format_args!("Hotkey actions are not available in this build yet"));
                    }
                }
                None => SLOG.warn(format_args!("Unknown protocol hotkey action: {wparam}")),
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Dispatch for a WM_COPYDATA command from config.exe or a second (protocol-handler) instance.
fn handle_command(cmd: Command) {
    match cmd {
        Command::Switch(name) => {
            let char_name = String::from_utf8_lossy(&name);
            SLOG.info(format_args!("Protocol handler: switch to character: {char_name}"));
            if let Some(scout) = globals::scout() {
                match scout.hwnd_by_name(&char_name) {
                    Some(target) => input::handle_thumbnail_click(target),
                    None => SLOG.warn(format_args!("Character '{char_name}' not found")),
                }
            }
        }
        Command::Profile(name) => {
            let profile_name = String::from_utf8_lossy(&name).into_owned();
            SLOG.info(format_args!("Protocol handler: switch to profile: {profile_name}"));
            if let Err(err) = reload_with_profile(&profile_name) {
                SLOG.err(format_args!("Failed to switch profile to {profile_name}: {err}"));
            }
        }
        Command::PreviewThumbnail(json) => {
            if let Err(err) = apply_thumbnail_preview(&json) {
                SLOG.err(format_args!("Failed to apply thumbnail preview: {err}"));
            }
        }
        Command::TestNotification(json) => {
            if let Err(err) = show_test_notification(&json) {
                SLOG.err(format_args!("Failed to show test notification: {err}"));
            }
        }
        Command::RevertPreview => revert_thumbnail_preview(),
        // Suspending hotkeys while the dialog captures a key binding only matters once hotkeys are ported.
        Command::DialogSuspendHotkeys | Command::DialogResumeHotkeys => {}
        Command::StartRegionSelect(request) => {
            if let Some(painter) = globals::painter() {
                painter.start_region_select(&request);
            }
        }
        Command::Hotkey(_) => {}
    }
}

/// Runs one WM_TIMER tick: scan for EVE windows, then push the results through the painter.
fn on_timer_tick() {
    let Some(state) = TICK_STATE.get() else { return };
    state.scan_tick_counter += 1;
    let force_scan = state.scan_tick_counter >= SCAN_INTERVAL_TICKS;
    if force_scan {
        state.scan_tick_counter = 0;
    }

    let Some(scout) = globals::scout() else { return };
    let result = match scout.update(force_scan) {
        Ok(r) => r,
        Err(err) => {
            SLOG.err(format_args!("Failed to update Scout: {err}"));
            return;
        }
    };
    // Snapshot, since painting can pump messages that let Scout's hooks change its list mid-update.
    let windows = scout.windows.clone();

    if let Some(painter) = globals::painter() {
        painter.update(&windows, &result.closed_windows, &result.name_changes);
        painter.update_notifications();
    }

    let now = Ticks::now();
    if let Some(painter) = globals::painter() {
        if now.elapsed_since(state.last_travel_check) >= TRAVEL_CHECK_INTERVAL_MS {
            state.last_travel_check = now;
            painter.check_travel_left_behind(now);
        }
    }
}

unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    if matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT) {
        log::flush();
    }
    // Never claim to have handled it: this only flushes, the OS's default behavior for the event (e.g. terminating the process) still applies.
    FALSE
}

// dbghelp.dll (MiniDumpWriteDump) isn't thread-safe; this flag serializes writes and resets after each attempt so a later crash can still dump.
static DUMP_WRITE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

unsafe fn write_minidump(info: *mut EXCEPTION_POINTERS) {
    if DUMP_WRITE_IN_PROGRESS.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    // Overwritten on every crash - only the latest is kept, so a crash loop can't fill the disk.
    let name = wide("eve-maj-crash.dmp");
    let file = CreateFileW(name.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ, std::ptr::null(), CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
    if file != INVALID_HANDLE_VALUE {
        let exc_info = MINIDUMP_EXCEPTION_INFORMATION { ThreadId: GetCurrentThreadId(), ExceptionPointers: info, ClientPointers: FALSE };
        if MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), file, MiniDumpNormal, &exc_info, std::ptr::null(), std::ptr::null()) == FALSE {
            log::write_crash_line(format_args!("MiniDumpWriteDump failed, GetLastError=0x{:x}", GetLastError()));
        }
        CloseHandle(file);
    }
    DUMP_WRITE_IN_PROGRESS.store(false, Ordering::Release);
}

fn module_base() -> usize {
    unsafe { GetModuleHandleW(std::ptr::null()) as usize }
}

// Logs only hard faults (access violations and the like), passing everything else through silently, since the
// handler sees every first-chance exception including ones the code handles on purpose.
unsafe extern "system" fn first_chance_exception_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    let Some(rec) = (*info).ExceptionRecord.as_ref() else { return EXCEPTION_CONTINUE_SEARCH };
    if !matches!(rec.ExceptionCode, EXCEPTION_ACCESS_VIOLATION | EXCEPTION_ILLEGAL_INSTRUCTION | EXCEPTION_DATATYPE_MISALIGNMENT | EXCEPTION_STACK_OVERFLOW) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let base = module_base();
    let addr = rec.ExceptionAddress as usize;
    if rec.ExceptionCode == EXCEPTION_ACCESS_VIOLATION && rec.NumberParameters >= 2 {
        let kind = if rec.ExceptionInformation[0] == 1 { "write" } else { "read" };
        log::write_crash_line(format_args!(
            "First-chance access violation ({kind}) at address 0x{:x}, code address 0x{addr:x} (module base 0x{base:x}, RVA 0x{:x})",
            rec.ExceptionInformation[1],
            addr.wrapping_sub(base)
        ));
    } else {
        log::write_crash_line(format_args!(
            "First-chance exception 0x{:x} at address 0x{addr:x} (module base 0x{base:x}, RVA 0x{:x})",
            rec.ExceptionCode as u32,
            addr.wrapping_sub(base)
        ));
    }
    EXCEPTION_CONTINUE_SEARCH
}

// Last handler in the chain; returns EXCEPTION_CONTINUE_SEARCH so Windows' normal handling still runs after.
unsafe extern "system" fn unhandled_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let base = module_base();
    match (*info).ExceptionRecord.as_ref() {
        Some(rec) => {
            let addr = rec.ExceptionAddress as usize;
            // Wrapping sub: a wild jump could fault below the module base and this handler must not itself panic on overflow.
            log::write_crash_line(format_args!(
                "Unhandled exception 0x{:x} at address 0x{addr:x} (module base 0x{base:x}, RVA 0x{:x})",
                rec.ExceptionCode as u32,
                addr.wrapping_sub(base)
            ));
        }
        None => log::write_crash_line(format_args!("Unhandled exception (no exception record), module base 0x{base:x}")),
    }
    write_minidump(info as *mut _);
    EXCEPTION_CONTINUE_SEARCH
}

pub fn main() {
    unsafe {
        // Must precede any window/monitor API call, or Windows bitmap-stretches our windows on scaled monitors.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        AddVectoredExceptionHandler(1, Some(first_chance_exception_handler));
        SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
    }
    // Gets the panic message into eve-maj.log - the default hook only writes to stderr, which is invisible in this Windows-subsystem build outside logLevel=debug.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::write_crash_line(format_args!("PANIC: {info}"));
        default_hook(info);
    }));

    let code = match main_impl() {
        Ok(()) => 0,
        Err(err) => {
            SLOG.err(format_args!("Fatal error: {err}"));
            1
        }
    };
    log::deinit_file();
    std::process::exit(code);
}

/// Run-key startup entries launch with an arbitrary working directory, not the exe's folder.
fn set_cwd_to_exe_dir() {
    match window::self_exe_dir() {
        Some(dir) => {
            if let Err(err) = std::env::set_current_dir(&dir) {
                SLOG.warn(format_args!("Failed to change to exe directory: {err}"));
            }
        }
        None => SLOG.warn(format_args!("Failed to resolve exe directory")),
    }
}

#[derive(Debug)]
enum FatalError {
    ParseProtocolUrl(protocol::ParseError),
    NoExistingInstance,
    MutexCreationFailed,
    AlreadyRunning,
    InvalidArguments,
    Config(ConfigError),
    CreateWindowFailed,
    SetTimerFailed,
    Painter(crate::painter::CreateError),
    Tray(tray::TrayError),
    Scan(crate::scout::EnumWindowsFailed),
    InvalidProfileName,
}

impl std::fmt::Display for FatalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseProtocolUrl(e) => write!(f, "{e:?}"),
            Self::NoExistingInstance => f.write_str("NoExistingInstance"),
            Self::MutexCreationFailed => f.write_str("MutexCreationFailed"),
            Self::AlreadyRunning => f.write_str("AlreadyRunning"),
            Self::InvalidArguments => f.write_str("InvalidArguments"),
            Self::Config(e) => write!(f, "{e}"),
            Self::CreateWindowFailed => f.write_str("CreateWindowFailed"),
            Self::SetTimerFailed => f.write_str("SetTimerFailed"),
            Self::Painter(e) => write!(f, "{e}"),
            Self::Tray(e) => write!(f, "{e}"),
            Self::Scan(e) => write!(f, "{e}"),
            Self::InvalidProfileName => f.write_str("InvalidProfileName"),
        }
    }
}

impl From<ConfigError> for FatalError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

/// Logs that a profile setting is on but its feature isn't in this build yet.
fn warn_unported_features(config: &Config) {
    let unported = [
        (config.chatlog.enabled, "Chatlog monitoring"),
        (config.combat.enabled, "The combat DPS overlay"),
        (config.mining.enabled, "The mining rate overlay"),
        (config.bounty.enabled, "The bounty overlay"),
        (config.resources.enabled, "The resource usage overlay"),
    ];
    for (enabled, feature) in unported {
        if enabled {
            SLOG.warn(format_args!("{feature} is enabled in this profile but not available in this build yet"));
        }
    }
}

fn main_impl() -> Result<(), FatalError> {
    set_cwd_to_exe_dir();

    // Handle protocol invocation before the mutex check, so commands work even when another instance is already running.
    if let Some(url) = ipc::check_command_line() {
        SLOG.info(format_args!("Protocol handler invoked: {url}"));
        let Some(existing) = ipc::find_existing_instance(TIMER_CLASS_NAME) else {
            SLOG.warn(format_args!("No existing instance found, protocol command ignored"));
            return Err(FatalError::NoExistingInstance);
        };
        let cmd = protocol::parse_url(&url).map_err(|err| {
            SLOG.err(format_args!("Failed to parse protocol URL: {err:?}"));
            FatalError::ParseProtocolUrl(err)
        })?;
        ipc::send_command_to_instance(existing, &cmd);
        SLOG.info(format_args!("Protocol command sent successfully"));
        return Ok(());
    }

    let mutex_name = wide("Local\\EVE-Maj-Preview-SingleInstance");
    let instance_mutex = unsafe { CreateMutexW(std::ptr::null(), TRUE, mutex_name.as_ptr()) };
    if instance_mutex.is_null() {
        SLOG.err(format_args!("Failed to create instance mutex"));
        return Err(FatalError::MutexCreationFailed);
    }
    let result = (|| {
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            SLOG.info(format_args!("Another instance of EVE-Maj Preview is already running"));
            return Err(FatalError::AlreadyRunning);
        }
        run()
    })();
    unsafe { CloseHandle(instance_mutex) };
    result
}

fn run() -> Result<(), FatalError> {
    SLOG.info(format_args!("EVE-Maj Preview v{CURRENT_VERSION}"));
    fonts::load_bundled();

    let global_settings = GlobalSettings::load();
    log::set_level(global_settings.log_level);
    global_settings.log_settings();

    let mut profile_name = if global_settings.last_used_profile.is_empty() { DEFAULT_PROFILE.to_owned() } else { global_settings.last_used_profile.clone() };
    globals::GLOBAL_SETTINGS.set(global_settings);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut j = 0;
    while j < args.len() {
        match args[j].as_str() {
            "--profile" | "-p" => {
                let Some(name) = args.get(j + 1) else {
                    SLOG.err(format_args!("--profile requires a profile name"));
                    return Err(FatalError::InvalidArguments);
                };
                profile_name = name.clone();
                j += 1;
            }
            // Skip protocol arg (already handled above)
            "--protocol" => j += 1,
            other => {
                SLOG.err(format_args!("Unknown argument: {other}"));
                return Err(FatalError::InvalidArguments);
            }
        }
        j += 1;
    }

    let config = Config::load_profile(&profile_name)?;
    let gs = globals::GLOBAL_SETTINGS.get().expect("set above");
    // Not profile_name: load_profile() may have fallen back to default, and this heals global settings to match.
    gs.update_last_used(&config.profile_name)?;
    config.log_settings();
    warn_unported_features(&config);
    globals::CONFIG.set(config);

    if gs.auto_register_protocol {
        if !ipc::is_registered() {
            SLOG.info(format_args!("Protocol handler not registered, attempting auto-registration..."));
            match ipc::register() {
                Ok(true) => SLOG.info(format_args!("Protocol handler successfully registered")),
                Ok(false) => SLOG.warn(format_args!("Protocol handler registration returned false")),
                Err(err) => {
                    SLOG.warn(format_args!("Failed to auto-register protocol handler: {err:?}"));
                    SLOG.warn(format_args!("You may need to run as administrator or manually register using register-protocol.reg"));
                }
            }
        } else {
            SLOG.debug(format_args!("Protocol handler already registered"));
        }
    }

    // Allocate console in debug mode (Windows GUI subsystem doesn't create one by default)
    if gs.log_level == LogLevel::Debug {
        unsafe { AllocConsole() };
        log::set_console_ready(true);
        // Closing the console window kills the process before any cleanup can run, so flush buffered log lines from here instead.
        unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), TRUE) };
    }

    let result = run_message_loop();

    // Teardown mirrors the Zig build's defer order: timer window and tray first, then painter, scout, audio.
    TRAY_ICON.take();
    globals::PAINTER.take();
    globals::SCOUT.take();
    tts::shutdown();
    sound::shutdown();
    globals::CONFIG.take();
    globals::GLOBAL_SETTINGS.take();
    result
}

fn run_message_loop() -> Result<(), FatalError> {
    globals::SCOUT.set(Box::new(Scout::new()));
    globals::PAINTER.set(Box::new(Painter::new().map_err(FatalError::Painter)?));
    TICK_STATE.set(TickState { scan_tick_counter: 0, last_travel_check: Ticks::default() });

    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // Never shown (0x0, no ShowWindow), so the class's cursor is never actually displayed.
    if !gdi_overlay::register_window_class(instance, Some(timer_window_proc), TIMER_CLASS_NAME, std::ptr::null_mut()) {
        return Err(FatalError::CreateWindowFailed);
    }
    let class_w = wide(TIMER_CLASS_NAME);
    let title_w = wide("EVE Timer Window");
    let timer_hwnd = unsafe {
        CreateWindowExW(0, class_w.as_ptr(), title_w.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null())
    };
    if timer_hwnd.is_null() {
        return Err(FatalError::CreateWindowFailed);
    }
    globals::TIMER_HWND.set(timer_hwnd);
    let result = run_with_timer_window(timer_hwnd);
    globals::TIMER_HWND.take();
    TRAY_ICON.take();
    unsafe { DestroyWindow(timer_hwnd) };
    result
}

fn run_with_timer_window(timer_hwnd: HWND) -> Result<(), FatalError> {
    TRAY_ICON.set(TrayIcon::new(timer_hwnd).map_err(FatalError::Tray)?);

    if !globals::GLOBAL_SETTINGS.get().expect("loaded").disable_update_checks {
        if let Err(err) = std::thread::Builder::new().name("update_check".into()).spawn(UpdateChecker::check_for_updates_background) {
            SLOG.warn(format_args!("Failed to start update check thread: {err}"));
        }
    } else {
        SLOG.info(format_args!("Update checks are disabled"));
    }

    let scout = globals::scout().expect("set above");
    scout.scan_for_eve_windows().map_err(FatalError::Scan)?;

    // Create thumbnail windows for each EVE client (fast - no I/O blocking)
    let windows = scout.windows.clone();
    let painter = globals::painter().expect("set above");
    for w in &windows {
        if let Err(err) = painter.create_thumbnail(w, "") {
            return Err(FatalError::Painter(err));
        }
        if globals::config().auto_move_position.move_on_startup {
            painter.move_client_to_saved_position(w.hwnd, &w.character_name);
        }
    }
    painter.reflow_if_region_fit_active();

    let interval = globals::config().timer.scan_interval_ms;
    if unsafe { SetTimer(timer_hwnd, TIMER_ID, interval, None) } == 0 {
        SLOG.err(format_args!("Failed to create timer"));
        return Err(FatalError::SetTimerFailed);
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        KillTimer(timer_hwnd, TIMER_ID);
    }
    Ok(())
}

/// Completely reinitializes the painter with a newly loaded profile's configuration.
fn reload_with_profile(new_profile_name: &str) -> Result<(), FatalError> {
    // Names reach here from evemajpreview:// URLs and IPC; refuse anything that isn't a plain profile file name before it can be saved as the last-used profile.
    if !eve_maj_core::config::is_safe_profile_name(new_profile_name) {
        SLOG.warn(format_args!("Ignoring profile switch to unsafe name '{new_profile_name}'"));
        return Err(FatalError::InvalidProfileName);
    }
    SLOG.info(format_args!("=== Starting profile reload: {new_profile_name} ==="));
    let timer_hwnd = globals::timer_hwnd().ok_or(FatalError::CreateWindowFailed)?;

    let new_config = match Config::load_profile(new_profile_name) {
        Ok(c) => c,
        Err(err) => {
            SLOG.err(format_args!("Failed to load new profile, reverting to default"));
            // The original profile-load error, not the fallback's.
            Config::load().map_err(|_| FatalError::Config(err))?
        }
    };

    // Snapshot last-known system names before the painter tears down thumbnails, so new ones can be seeded instead of going blank; keyed by source_hwnd, stable across teardown/recreate.
    let last_known_systems: HashMap<HWND, String> = globals::painter()
        .map(|p| p.thumbnails.iter().filter(|t| !t.system_name.is_empty()).map(|t| (t.source_hwnd, t.system_name.clone())).collect())
        .unwrap_or_default();

    if globals::PAINTER.take().is_some() {
        SLOG.debug(format_args!("Cleaned up painter"));
    }

    // Dropping the old config flushes its pending auto colors.
    globals::CONFIG.set(new_config);
    SLOG.debug(format_args!("Cleaned up old config"));
    SLOG.info(format_args!("Loaded new config: {new_profile_name}"));
    let config = globals::config();
    config.log_settings();
    warn_unported_features(config);

    // Picks up hotkey/profile-switch/log-level edits made via the config dialog while running, since global settings are otherwise only loaded once at startup.
    let reloaded = GlobalSettings::load();
    log::set_level(reloaded.log_level);
    SLOG.debug(format_args!("Reloaded global settings from disk"));
    reloaded.log_settings();
    globals::GLOBAL_SETTINGS.set(reloaded);
    if let Err(err) = globals::GLOBAL_SETTINGS.get().expect("set above").update_last_used(new_profile_name) {
        SLOG.warn(format_args!("Failed to update global settings: {err}"));
    }

    let painter = match Painter::new() {
        Ok(p) => p,
        Err(err) => {
            SLOG.err(format_args!("Failed to initialize painter: {err}"));
            return Err(FatalError::Painter(err));
        }
    };
    globals::PAINTER.set(Box::new(painter));
    SLOG.debug(format_args!("Reinitialized painter"));

    if let Some(scout) = globals::scout() {
        match scout.scan_for_eve_windows() {
            Err(err) => SLOG.err(format_args!("Failed to scan for EVE windows: {err}")),
            Ok(()) => {
                scout.prune_non_matching_windows();
                let windows = scout.windows.clone();
                let painter = globals::painter().expect("set above");
                for w in &windows {
                    // Only if monitoring stays on to refresh it, or a stale name would freeze on screen forever.
                    let initial_system_name = if config.chatlog.enabled { last_known_systems.get(&w.hwnd).map_or("", String::as_str) } else { "" };
                    if let Err(err) = painter.create_thumbnail(w, initial_system_name) {
                        SLOG.err(format_args!("Failed to create thumbnail for {}: {err}", w.character_name));
                    }
                }
                painter.reflow_if_region_fit_active();
                SLOG.debug(format_args!("Recreated {} thumbnail(s)", windows.len()));
            }
        }
    }

    let new_interval = config.timer.scan_interval_ms;
    unsafe { SetTimer(timer_hwnd, TIMER_ID, new_interval, None) };
    SLOG.debug(format_args!("Updated timer interval to {new_interval} ms"));
    SLOG.info(format_args!("=== Profile reload complete: {new_profile_name} ==="));
    Ok(())
}

#[derive(Debug)]
enum PreviewError {
    Json(serde_json::Error),
    Config(ConfigError),
    InvalidJsonFormat,
    MissingNotificationType,
    InvalidNotificationType,
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "{e}"),
            Self::Config(e) => write!(f, "{e}"),
            Self::InvalidJsonFormat => f.write_str("InvalidJsonFormat"),
            Self::MissingNotificationType => f.write_str("MissingNotificationType"),
            Self::InvalidNotificationType => f.write_str("InvalidNotificationType"),
        }
    }
}

impl From<ConfigError> for PreviewError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

/// Merges a live-preview patch (changed fields only) into the running config, repaints, and repositions thumbnails; unlike reload_with_profile() it never touches hotkeys/chatlog/identity, so it's cheap enough to run on every keystroke/slider drag in the dialog.
fn apply_thumbnail_preview(json: &str) -> Result<(), PreviewError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(PreviewError::Json)?;
    let obj = value.as_object().ok_or(PreviewError::InvalidJsonFormat)?;
    let config = globals::config();

    config.thumbnail.apply_json(obj)?;
    config.thumbnail.validate();

    // System color overrides live outside ThumbnailConfig, so they ride along in the same patch object.
    if let Some(serde_json::Value::Array(colors)) = obj.get("systemColors") {
        if let Err(err) = config.replace_system_colors_from_json(colors) {
            SLOG.err(format_args!("Failed to apply system color overrides preview: {err}"));
        }
    }

    // List View's own opacity/font settings live in DisplayConfig, parsed from a nested "display" object in the same patch.
    // startX/startY are deliberately never sent here, since they can be live-dragged in the running app.
    let mut layout_changed = false;
    if let Some(serde_json::Value::Object(display)) = obj.get("display") {
        if let Err(err) = config.display.apply_json(display) {
            SLOG.err(format_args!("Failed to apply display preview: {err}"));
        }
        config.display.validate();
        layout_changed = true;
    }

    // Only the badge flags ride along; membership stays whatever the running app has, since a temporary group's members exist only here.
    if let Some(serde_json::Value::Array(badges)) = obj.get("hotkeyGroupBadges") {
        config.apply_group_badge_preview_from_json(badges);
    }

    // Matched by character name against the running character list.
    if let Some(serde_json::Value::Array(overrides)) = obj.get("characterOverrides") {
        if let Err(err) = config.apply_character_overrides_from_json(overrides) {
            SLOG.err(format_args!("Failed to apply character overrides preview: {err}"));
        }
    }

    // Combat/Mining/Bounty/Resources overlays each live outside ThumbnailConfig, so they ride along as their own top-level keys.
    if let Some(serde_json::Value::Object(o)) = obj.get("combat") {
        if let Err(err) = config.combat.apply_json(o) {
            SLOG.err(format_args!("Failed to apply combat overlay preview: {err}"));
        }
        config.combat.validate();
    }
    if let Some(serde_json::Value::Object(o)) = obj.get("mining") {
        if let Err(err) = config.mining.apply_json(o) {
            SLOG.err(format_args!("Failed to apply mining overlay preview: {err}"));
        }
        config.mining.validate();
    }
    if let Some(serde_json::Value::Object(o)) = obj.get("bounty") {
        if let Err(err) = config.bounty.apply_json(o) {
            SLOG.err(format_args!("Failed to apply bounty overlay preview: {err}"));
        }
        config.bounty.validate();
    }
    if let Some(serde_json::Value::Object(o)) = obj.get("resources") {
        if let Err(err) = config.resources.apply_json(o) {
            SLOG.err(format_args!("Failed to apply resources overlay preview: {err}"));
        }
        config.resources.validate();
    }

    if let Some(painter) = globals::painter() {
        painter.refresh_all_thumbnail_visuals();
        if layout_changed {
            painter.reposition_all_thumbnails();
        }
    }
    Ok(())
}

/// Fires one event type on every thumbnail from the config dialog's unsaved per-type values; payload is `{type, text, config}`.
fn show_test_notification(json: &str) -> Result<(), PreviewError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(PreviewError::Json)?;
    let obj = value.as_object().ok_or(PreviewError::InvalidJsonFormat)?;
    let type_name = match obj.get("type") {
        None => return Err(PreviewError::MissingNotificationType),
        Some(serde_json::Value::String(s)) => s,
        Some(_) => return Err(PreviewError::InvalidJsonFormat),
    };
    let Some(serde_json::Value::String(text)) = obj.get("text") else { return Err(PreviewError::InvalidJsonFormat) };
    let Some(serde_json::Value::Object(config_obj)) = obj.get("config") else { return Err(PreviewError::InvalidJsonFormat) };

    let Some(ntype) = NotificationType::from_name(type_name) else {
        SLOG.warn(format_args!("Unknown notification type in test request: {type_name}"));
        return Err(PreviewError::InvalidNotificationType);
    };
    let mut type_config = NotificationTypeConfig::default();
    type_config.apply_json(config_obj)?;

    if let Some(painter) = globals::painter() {
        painter.show_test_notification(ntype, text, &type_config);
    }
    Ok(())
}

/// Discards live-previewed appearance and layout changes by reloading that section from disk, repainting, and repositioning; sent when the config dialog closes, a no-op if Save was already clicked.
fn revert_thumbnail_preview() {
    if let Err(err) = globals::config().reload_thumbnail_config_from_disk() {
        SLOG.err(format_args!("Failed to revert thumbnail preview: {err}"));
        return;
    }
    if let Some(painter) = globals::painter() {
        painter.refresh_all_thumbnail_visuals();
        painter.reposition_all_thumbnails();
    }
}
