//! eve-maj-preview on Linux (Hyprland).
//!
//! The Windows build binds each EVE client's live DWM thumbnail into a layered window. Here each thumbnail is a
//! wlr-layer-shell overlay surface; Hyprland renders the client window into a GPU buffer we own
//! (hyprland-toplevel-export-v1 + dmabuf), which is sampled straight into that surface, with the same text/border
//! overlay as Windows drawn on top. Window discovery and focus come from Hyprland's IPC sockets.
//!
//! The compositor-specific parts sit behind two seams (see `backend`): a `WindowManager` (wm_hyprland) and a capture
//! protocol (capture). Everything else here (layer-shell display, input, layout, styling) is compositor-independent.
//!
//! Layout, styling and snapping decisions live in eve-maj-core (`layout`, `render`) and match the Windows painter.

mod activity;
mod alerts;
mod backend;
mod capture;
mod control;
mod dialog;
mod gpu;
mod ipc;
mod keymap;
mod notify;
mod overlay;
mod panels;
mod protocols;
mod region_select;
mod shm;
mod tray;
mod wm_hyprland;

use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use eve_maj_core::accounts_store::{self, Membership};
use eve_maj_core::config::{CharacterBorderColors, Config, GlobalSettings, Position, DEFAULT_PROFILE};
use eve_maj_core::display_grid::Rect;
use eve_maj_core::layout::{self, is_generic_character_name, GhostGroup, Layout, Placement, Screens};
use eve_maj_core::log::{self, LogLevel, Scope};
use eve_maj_core::render::{self, now_ms, ActiveNotification, Metrics, RenderSettings, ThumbnailView};
use eve_maj_core::state::{try_transition_visibility, ThumbnailState, VisibilityState};
use eve_maj_core::types::{ClickTrigger, HoverCursor};
use khronos_egl as egl;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor, wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer,
    wl_region::WlRegion, wl_registry::WlRegistry, wl_seat::WlSeat, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1}, wp_cursor_shape_manager_v1::WpCursorShapeManagerV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1::{self, ZwlrLayerShellV1}, zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1}};

use backend::{Monitor, Toplevel, TrackedWindow, WindowId, WindowManager, WmEvent};
use eve_maj_core::hotkeys::HotkeyManager;
use capture::{Capture, CaptureProtocol};
use protocols::hyprland_toplevel_export::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;

const SLOG: Scope = Scope::new("main");
const LAYER_NAMESPACE: &str = "eve-maj-preview";
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

pub fn main() {
    if let Err(e) = run() {
        SLOG.err(format_args!("Fatal: {e}"));
        eprintln!("eve-maj-preview: {e}");
        log::flush();
        std::process::exit(1);
    }
    log::flush();
}

/// $XDG_CONFIG_HOME/eve-maj (profiles/, accounts.json and eve-maj.log live here, as they do next to the .exe on Windows).
fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("eve-maj"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/eve-maj"),
    }
}

struct Options {
    profile: Option<String>,
    fps: u32,
    no_dmabuf: bool,
    linear: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut o = Options { profile: None, fps: 30, no_dmabuf: false, linear: false };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" | "-p" => {
                o.profile = Some(args.get(i + 1).ok_or("--profile requires a profile name")?.clone());
                i += 1;
            }
            "--fps" => {
                o.fps = args.get(i + 1).and_then(|v| v.parse().ok()).filter(|&v| v > 0).ok_or("--fps requires a positive number")?;
                i += 1;
            }
            "--protocol" => i += 1, // handled by ipc::url_from_args
            a if a.starts_with("evemajpreview://") => {}
            "--no-dmabuf" => o.no_dmabuf = true,
            "--linear" => o.linear = true,
            "--help" | "-h" => {
                println!(
                    "eve-maj-preview [--profile NAME.json] [--fps N] [--no-dmabuf] [--linear]\n\
                     Profiles live in {}/profiles. Left-click a thumbnail to focus that client, right-drag to move it\n\
                     (Ctrl+right-drag moves them all). Edits to the profile JSON are picked up live.",
                    config_dir().display()
                );
                std::process::exit(0);
            }
            other => return Err(format!("Unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(o)
}

/// SIGINT and SIGTERM.
fn termination_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        set
    }
}

fn run() -> Result<(), String> {
    // Blocked before any thread exists (GL, fontconfig and the tray's D-Bus threads inherit the mask); otherwise the
    // kernel may deliver the signal to one of those threads and kill the process without our cleanup.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &termination_signals(), std::ptr::null_mut()) };
    let opts = parse_args()?;
    // A link click starts a second process; hand the link to the running one and exit, like the Windows build.
    if let Some(url) = ipc::url_from_args() {
        return if ipc::forward(&url) { Ok(()) } else { Err(format!("no running instance to handle {url}")) };
    }
    let dir = config_dir();
    std::fs::create_dir_all(dir.join("profiles")).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    std::env::set_current_dir(&dir).map_err(|e| format!("can't enter {}: {e}", dir.display()))?;

    SLOG.info(format_args!("EVE-Maj Preview v{} (Linux/Hyprland)", env!("CARGO_PKG_VERSION")));
    let mut global_settings = GlobalSettings::load();
    log::set_level(global_settings.log_level);
    if global_settings.log_level == LogLevel::Debug || std::env::var_os("EVE_MAJ_CONSOLE").is_some() {
        log::set_console_ready(true);
    }
    let profile_name = opts.profile.clone().unwrap_or_else(|| {
        if global_settings.last_used_profile.is_empty() { DEFAULT_PROFILE.to_owned() } else { global_settings.last_used_profile.clone() }
    });
    let config = Config::load_profile(&profile_name).map_err(|e| format!("loading profile {profile_name}: {e}"))?;
    if let Err(e) = global_settings.update_last_used(&config.profile_name) {
        SLOG.warn(format_args!("Couldn't record last used profile: {e}"));
    }
    config.log_settings();

    overlay::register_bundled_fonts(&dir.join("fonts"));
    let listener = match ipc::Listener::bind() {
        Ok(l) => l,
        Err(ipc::BindError::AlreadyRunning) => return Err("EVE-Maj Preview is already running".into()),
        Err(ipc::BindError::Io(e)) => return Err(format!("can't create the instance socket: {e}")),
    };
    if global_settings.auto_register_protocol {
        ipc::register_protocol_handler();
    }

    // Hyprland is the only window manager/capture backend so far; see backend.rs for what another one needs.
    let wm: Box<dyn WindowManager> = Box::new(wm_hyprland::HyprlandWm::connect().ok_or("this build supports Hyprland only (HYPRLAND_INSTANCE_SIGNATURE is unset)")?);
    let conn = Connection::connect_to_env().map_err(|e| format!("can't connect to the Wayland display: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).map_err(|_| "compositor lacks wl_compositor")?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).map_err(|_| "compositor lacks wlr-layer-shell")?;
    let capture = CaptureProtocol::HyprlandExport(globals.bind(&qh, 1..=2, ()).map_err(|_| "compositor lacks hyprland-toplevel-export")?);
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| "compositor lacks wl_shm")?;
    let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=3, ()).ok();
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
    let cursor_shape: Option<WpCursorShapeManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let outputs: Vec<OutputInfo> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == WlOutput::interface().name && g.version >= 4)
            .map(|g| OutputInfo { wl: globals.registry().bind(g.name, 4, &qh, ()), name: String::new() })
            .collect()
    });

    let display_ptr = conn.backend().display_ptr() as *mut c_void;
    let gpu = unsafe { gpu::Gpu::new(display_ptr) }?;
    SLOG.info(format_args!("GL: {}", gpu.renderer));
    let use_dmabuf = !opts.no_dmabuf && dmabuf.is_some() && gpu.dmabuf_capable();
    if !use_dmabuf {
        SLOG.warn(format_args!("GPU capture path unavailable, falling back to CPU (wl_shm) copies"));
    }

    let membership = accounts_store::load_membership();
    let profile_mtime = profile_mtime(&config.profile_name);
    let mut app = App {
        qh: qh.clone(),
        compositor,
        layer_shell,
        capture,
        shm,
        dmabuf,
        seat,
        pointer: None,
        keyboard: None,
        cursor_shape,
        cursor_device: None,
        outputs,
        gpu,
        use_dmabuf,
        linear: opts.linear,
        wm,
        config,
        membership,
        monitors: Vec::new(),
        thumbs: Vec::new(),
        next_id: 1,
        active: None,
        eve_focus_lost_at: None,
        pointer_state: PointerState::default(),
        ctrl: false,
        shift: false,
        scan_needed: true,
        tick: 0,
        profile_mtime,
        global_settings,
        hotkeys: HotkeyManager::default(),
        last_non_eve: None,
        notified: Vec::new(),
        pending_moves: Vec::new(),
        last_focused_by_monitor: std::collections::HashMap::new(),
        toplevels: Vec::new(),
        ipc: Some(listener),
        tray: None,
        exit_requested: false,
        chatlog: None,
        trackers: activity::Trackers::default(),
        dialog_hotkeys_suspended: false,
        chatlog_warned: false,
        region: None,
        confirm_keys: None,
        panels: panels::Panels::default(),
    };
    app.tray = tray::Tray::spawn(app.tray_state());
    queue.roundtrip(&mut app).map_err(|e| format!("roundtrip: {e}"))?;

    app.run_loop(&mut queue, &conn, opts.fps)
}

fn profile_mtime(name: &str) -> Option<std::time::SystemTime> {
    let path = eve_maj_core::config::profile_path(name).ok()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

struct OutputInfo {
    wl: WlOutput,
    name: String,
}

// ------------------------------------------------------------------------------------------- per-thumbnail state --

struct Surf {
    wl: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    egl_window: Option<wayland_egl::WlEglSurface>,
    egl: Option<egl::Surface>,
    /// Monitor (by name) the layer surface lives on, and that monitor's origin in layout space.
    output: String,
    origin: (i32, i32),
    configured: bool,
    frame_pending: bool,
    size: (i32, i32),
}

struct Thumb {
    id: u64,
    window: WindowId,
    title: String,
    name: String,
    system_name: String,
    minimized: bool,
    visibility: VisibilityState,
    /// Newest first, at most render::MAX_STACKED_NOTIFICATIONS.
    notifications: Vec<ActiveNotification>,
    excluded: bool,
    metrics: Metrics,
    character_color: Option<u32>,
    border_colors: Option<CharacterBorderColors>,
    hide_thumbnail: bool,
    opacity: u8,
    badge: String,
    system_color: u32,
    rect: Placement,
    surf: Option<Surf>,
    cap: Capture,
    settings: Option<RenderSettings>,
    overlay_tex: Option<glow::Texture>,
    needs_render: bool,
    needs_draw: bool,
    /// When this client last stopped being focused or minimized (auto-minimize's delay).
    inactive_since: u64,
    /// When this client went back to the login screen (not-logged-in cycling order).
    logged_out_at: u64,
    /// In-game time (YYYYMMDDhhmmss) of the line system_name came from; 0 for live updates.
    system_name_event_ts: u64,
    /// When this character last jumped (Travel Mode), and whether its left-behind alert has fired since.
    last_jump_ms: u64,
    travel_alert_fired: bool,
    /// Last thumbnail click (suppressWhenClicked).
    last_click_ms: u64,
    /// Last notification of each type (throttleMs), indexed by NotificationType::index().
    last_notified_by_type: Vec<u64>,
}

#[derive(Default)]
struct PointerState {
    hover: Option<u64>,
    /// Surface-local pointer position.
    local: (f64, f64),
    left_pending: Option<u64>,
    drag: Option<Drag>,
    enter_serial: u32,
}

struct Drag {
    thumb: u64,
    /// Where on the thumbnail it was grabbed.
    offset: (f64, f64),
    group: bool,
    ghosts: Vec<GhostGroup>,
    /// Client focused when the drag started; the overlay takes keyboard focus on click, so focus is handed back here.
    refocus: Option<u64>,
}

struct App {
    qh: QueueHandle<App>,
    compositor: WlCompositor,
    layer_shell: ZwlrLayerShellV1,
    capture: CaptureProtocol,
    shm: WlShm,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    outputs: Vec<OutputInfo>,
    gpu: gpu::Gpu,
    use_dmabuf: bool,
    linear: bool,
    wm: Box<dyn WindowManager>,
    config: Config,
    membership: Membership,
    monitors: Vec<Monitor>,
    thumbs: Vec<Thumb>,
    next_id: u64,
    /// The focused Hyprland window (any app), like the Windows build's foreground window.
    active: Option<u64>,
    /// When focus last left every EVE client, for hideWhenNoEveFocus's debounce.
    eve_focus_lost_at: Option<u64>,
    pointer_state: PointerState,
    ctrl: bool,
    shift: bool,
    scan_needed: bool,
    tick: u64,
    profile_mtime: Option<std::time::SystemTime>,
    global_settings: GlobalSettings,
    hotkeys: HotkeyManager<WindowId>,
    /// Last focused window that isn't an EVE client (return-to-last-app hotkey).
    last_non_eve: Option<WindowId>,
    /// Characters notified recently, oldest first (notified-character cycling).
    notified: Vec<(String, u64)>,
    pending_moves: Vec<control::PendingMove>,
    /// Last EVE client focused on each monitor (auto-minimize's exemption).
    last_focused_by_monitor: std::collections::HashMap<String, WindowId>,
    /// Every toplevel from the last scan (positions, monitors, processes).
    toplevels: Vec<Toplevel>,
    ipc: Option<ipc::Listener>,
    tray: Option<tray::Tray>,
    exit_requested: bool,
    chatlog: Option<notify::Chatlog>,
    trackers: activity::Trackers,
    /// The settings dialog is recording a key: no hotkeys bound until it's done.
    dialog_hotkeys_suspended: bool,
    /// The "logs not found" warning was logged (once per run).
    chatlog_warned: bool,
    /// The settings dialog's region selection overlay, while it's up.
    region: Option<region_select::RegionSelect>,
    /// Which keys are Enter and Escape in the seat's current keymap (None until it arrives or if it can't be read).
    confirm_keys: Option<keymap::ConfirmKeys>,
    /// The Client List and History Panel.
    panels: panels::Panels,
}

fn timerfd(interval_ms: u64) -> i32 {
    unsafe {
        let fd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK);
        let ts = libc::timespec { tv_sec: (interval_ms / 1000) as i64, tv_nsec: ((interval_ms % 1000) * 1_000_000) as i64 };
        let spec = libc::itimerspec { it_interval: ts, it_value: libc::timespec { tv_sec: 0, tv_nsec: 1 } };
        libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut());
        fd
    }
}

fn drain_timerfd(fd: i32) {
    let mut buf = [0u8; 8];
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, 8) };
}

impl App {
    fn run_loop(&mut self, queue: &mut EventQueue<App>, conn: &Connection, fps: u32) -> Result<(), String> {
        SLOG.info(format_args!("Window manager: {}, capture: {}", self.wm.name(), self.capture.name()));
        // SIGINT/SIGTERM arrive on a signalfd (blocked process-wide in run()), so shutdown runs the normal teardown: unbinding hotkeys, flushing the log.
        let signal_fd = unsafe { libc::signalfd(-1, &termination_signals(), libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        let tick_fd = timerfd(self.config.timer.scan_interval_ms.max(10) as u64);
        let capture_fd = timerfd((1000 / fps.max(1)).max(1) as u64);

        self.sync_windows();
        self.hotkeys.rebuild(&self.config, &self.global_settings);
        for i in 0..self.thumbs.len() {
            self.thumbs[i].excluded = self.hotkeys.is_character_excluded(&self.config, &self.thumbs[i].name);
        }
        self.register_hotkeys();
        if self.config.auto_move_position.move_on_startup {
            self.move_all_clients_to_saved_positions();
        }
        loop {
            queue.dispatch_pending(self).map_err(|e| format!("Wayland: {e}"))?;
            queue.flush().map_err(|e| format!("Wayland flush: {e}"))?;
            let guard = match queue.prepare_read() {
                Some(g) => g,
                None => continue,
            };
            let wl_fd = guard.connection_fd().as_raw_fd();
            // Fixed slots first, then whichever optional sources exist.
            let mut fds = vec![
                libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: tick_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: capture_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: signal_fd, events: libc::POLLIN, revents: 0 },
            ];
            let mut slot = |fd: Option<i32>| {
                fd.map(|fd| {
                    fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
                    fds.len() - 1
                })
            };
            let wm_slot = slot(self.wm.event_fd());
            let ipc_slot = slot(self.ipc.as_ref().map(|l| l.fd()));
            let tray_slot = slot(self.tray.as_ref().map(|t| t.fd()));
            let chatlog_slot = slot(self.chatlog.as_ref().map(|c| c.fd()));
            let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if r < 0 {
                drop(guard);
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err("poll failed".into());
            }
            if fds[0].revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    return Err(format!("Wayland read: {e}"));
                }
            } else {
                drop(guard);
            }
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err("Wayland connection closed".into());
            }
            queue.dispatch_pending(self).map_err(|e| format!("Wayland: {e}"))?;
            let ready = |slot: Option<usize>| slot.is_some_and(|i| fds[i].revents & (libc::POLLIN | libc::POLLHUP) != 0);

            if wm_slot.is_some() && ready(wm_slot) {
                for event in self.wm.read_events() {
                    match event {
                        WmEvent::Focus(window) => self.set_active(window),
                        WmEvent::Changed => self.scan_needed = true,
                        WmEvent::Hotkey(id) => self.on_hotkey(id),
                        WmEvent::ConfigReloaded => {
                            // A config reload drops runtime binds.
                            self.register_hotkeys();
                            self.scan_needed = true;
                        }
                    }
                }
            }
            if ready(ipc_slot) {
                let messages = self.ipc.as_ref().map(|l| l.accept_all()).unwrap_or_default();
                for message in messages {
                    match message {
                        ipc::Incoming::Url(url) => self.on_protocol_url(&url),
                        ipc::Incoming::Command(cmd) => self.on_dialog_command(cmd),
                    }
                }
            }
            if ready(chatlog_slot) {
                self.on_chatlog_ready();
            }
            if ready(tray_slot) {
                let commands = self.tray.as_ref().map(|t| t.drain()).unwrap_or_default();
                for cmd in commands {
                    self.on_tray_command(cmd);
                }
            }
            if fds[3].revents & libc::POLLIN != 0 || self.exit_requested {
                SLOG.info(format_args!("Shutting down"));
                self.cancel_region_select();
                self.unregister_hotkeys();
                self.chatlog = None;
                if let Some(t) = self.tray.take() {
                    t.shutdown();
                }
                let thumbs = std::mem::take(&mut self.thumbs);
                for t in thumbs {
                    self.destroy_thumb(t, false);
                }
                let _ = queue.flush();
                return Ok(());
            }
            if fds[1].revents & libc::POLLIN != 0 {
                drain_timerfd(tick_fd);
                self.on_tick();
            }
            if fds[2].revents & libc::POLLIN != 0 {
                drain_timerfd(capture_fd);
                for i in 0..self.thumbs.len() {
                    self.request_capture(i);
                }
            }
            let _ = conn.flush();
        }
    }

    fn on_tick(&mut self) {
        self.tick += 1;
        // Event-driven rescans, plus a periodic safety rescan like the Windows build's full EnumWindows every ~20 ticks.
        if self.scan_needed || self.tick % 20 == 0 {
            self.scan_needed = false;
            self.sync_windows();
        }
        if self.tick % 20 == 5 {
            self.check_profile_reload();
        }
        self.region_select_tick();
        self.expire_notifications();
        self.apply_hide_when_no_eve_focus();
        self.check_auto_minimize();
        self.verify_pending_moves();
        self.check_travel_left_behind();
        self.update_activity();
        self.render_dirty();
        if self.tick % 5 == 0 {
            self.refresh_tray();
        }
    }

    // ------------------------------------------------------------------------------------------ window tracking --

    fn sync_windows(&mut self) {
        let monitors = self.wm.monitors();
        let monitors_changed = monitors.iter().map(|m| (&m.name, m.bounds)).ne(self.monitors.iter().map(|m| (&m.name, m.bounds)));
        self.monitors = monitors;
        self.toplevels = self.wm.toplevels();
        let windows = backend::tracked_windows(&self.toplevels, &self.config.window_filters);
        if self.active.is_none() {
            self.active = self.wm.active();
        }

        let mut needs_reflow = monitors_changed;
        let managed = layout::is_layout_managed(&self.config.display) || layout::not_logged_in_space_rect_from_config(&self.config.display).is_some();

        // Closed windows.
        let before = self.thumbs.len();
        let mut i = 0;
        while i < self.thumbs.len() {
            if windows.iter().any(|w| w.id == self.thumbs[i].window) {
                i += 1;
                continue;
            }
            SLOG.info(format_args!("Client closed: {}", self.thumbs[i].name));
            let t = self.thumbs.remove(i);
            self.destroy_thumb(t, true);
        }
        needs_reflow |= managed && self.thumbs.len() != before;

        // Name changes and minimize state.
        for w in &windows {
            let Some(i) = self.thumbs.iter().position(|t| t.window == w.id) else { continue };
            if self.thumbs[i].minimized != w.minimized {
                self.thumbs[i].minimized = w.minimized;
                self.thumbs[i].needs_render = true;
            }
            if self.thumbs[i].title != w.title {
                self.thumbs[i].title = w.title.clone();
            }
            if self.thumbs[i].name != w.character_name {
                needs_reflow |= self.apply_name_change(i, &w.character_name);
            }
        }

        // New windows.
        for w in &windows {
            if self.thumbs.iter().any(|t| t.window == w.id) {
                continue;
            }
            self.create_thumb(w);
            needs_reflow |= managed;
            if self.config.auto_move_position.enabled {
                self.move_client_to_saved_position(w.id, &w.character_name);
            }
        }

        if needs_reflow {
            self.reflow();
        }
        self.reconcile_states();
        self.ensure_chatlog();
        self.sync_chatlog_characters();
    }

    fn refresh_character_caches(&mut self, i: usize) {
        let name = self.thumbs[i].name.clone();
        let character_color = self.config.character_name_color(&name);
        let border_colors = self.config.character_border_colors(&name);
        let system_name = self.thumbs[i].system_name.clone();
        let system_color = if system_name.is_empty() { self.config.thumbnail.system_name_color } else { self.config.system_name_color(&system_name) };
        let t = &mut self.thumbs[i];
        t.character_color = character_color;
        t.border_colors = border_colors;
        t.system_color = system_color;
        t.hide_thumbnail = self.config.is_thumbnail_hidden(&name);
        t.opacity = self.config.character_opacity(&name);
        t.badge = render::group_badge_label(&self.config, &name);
        t.settings = None;
        t.needs_render = true;
    }

    /// Returns whether a layout reflow is needed (same rules as the Windows painter's apply_name_changes).
    fn apply_name_change(&mut self, i: usize, new_name: &str) -> bool {
        let old_name = std::mem::replace(&mut self.thumbs[i].name, new_name.to_owned());
        SLOG.info(format_args!("Client renamed: {old_name} -> {new_name}"));
        let was_generic = is_generic_character_name(&old_name);
        let now_generic = is_generic_character_name(new_name);
        self.refresh_character_caches(i);

        if was_generic && !now_generic && !layout::is_layout_managed(&self.config.display) {
            if let Some(saved) = self.config.character_position(new_name) {
                let (w, h) = self.layout_size_for(new_name);
                self.thumbs[i].rect = Placement { x: saved.x, y: saved.y, width: w, height: h };
                self.apply_geometry(i);
            }
        }
        if now_generic {
            let t = &mut self.thumbs[i];
            t.system_name.clear();
            t.system_color = self.config.thumbnail.system_name_color;
            t.notifications.clear();
            t.logged_out_at = now_ms();
            t.system_name_event_ts = 0;
            t.last_jump_ms = 0;
            t.travel_alert_fired = false;
        }
        if was_generic && !now_generic {
            // Exclusion is per character, so it comes back with the login; same for the auto-move-on-login setting.
            self.thumbs[i].excluded = self.hotkeys.is_character_excluded(&self.config, new_name);
            if self.config.auto_move_position.enabled {
                let window = self.thumbs[i].window;
                self.move_client_to_saved_position(window, new_name);
            }
        }
        let login = was_generic && !now_generic;
        let logout = now_generic && !was_generic;
        let display = &self.config.display;
        if layout::not_logged_in_space_rect_from_config(display).is_some() && (login || logout) {
            return true;
        }
        layout::is_layout_managed(display) && (login || (logout && display.region_fit_reorder_logged_out))
    }

    fn screens_layout<'a>(&'a self, names: &'a [String], scale_at: &'a dyn Fn(i32, i32) -> f32) -> Layout<'a> {
        let cfg = &self.config.display;
        let monitor = cfg.monitor_index.and_then(|idx| self.monitors.get(idx as usize));
        if cfg.monitor_index.is_some() && monitor.is_none() {
            SLOG.warn(format_args!("display.monitorIndex {:?} not found ({} monitors)", cfg.monitor_index, self.monitors.len()));
        }
        // Windows measures startX/startY from the primary monitor at (0,0). Hyprland has no primary and its layout origin
        // may be empty space, so with no monitor configured the first monitor (lowest id) stands in for it.
        let monitor = monitor.or_else(|| self.monitors.first());
        let monitor_bounds = monitor.map(|m| if cfg.use_monitor_work_area { m.work_area } else { m.bounds });
        let virtual_screen = self.monitors.iter().fold(None::<Rect>, |acc, m| {
            Some(match acc {
                None => m.bounds,
                Some(a) => Rect { left: a.left.min(m.bounds.left), top: a.top.min(m.bounds.top), right: a.right.max(m.bounds.right), bottom: a.bottom.max(m.bounds.bottom) },
            })
        });
        Layout {
            config: &self.config,
            names,
            membership: &self.membership,
            // Layout space is already logical pixels on Wayland, so sizes need no DPI scaling.
            screens: Screens { monitor_bounds, virtual_screen: virtual_screen.unwrap_or(Rect { left: 0, top: 0, right: 1920, bottom: 1080 }), scale: 1.0, scale_at },
        }
    }

    fn names(&self) -> Vec<String> {
        self.thumbs.iter().map(|t| t.name.clone()).collect()
    }

    fn layout_size_for(&self, name: &str) -> (i32, i32) {
        let names = self.names();
        let scale_at = |_: i32, _: i32| 1.0;
        let layout = self.screens_layout(&names, &scale_at);
        layout.scaled_thumbnail_size(name, names.len(), 1.0)
    }

    fn create_thumb(&mut self, w: &TrackedWindow) {
        let names = self.names();
        let scale_at = |_: i32, _: i32| 1.0;
        let rect = {
            let layout = self.screens_layout(&names, &scale_at);
            let total = names.len() + 1;
            let (tw, th) = layout.scaled_thumbnail_size(&w.character_name, total, 1.0);
            let pos = layout.calculate_thumbnail_position(&w.character_name, tw, th, names.len(), total);
            Placement { x: pos.x, y: pos.y, width: tw, height: th }
        };
        let any_eve_focused = self.active.is_some_and(|a| a == w.id || self.thumbs.iter().any(|t| t.window == a));
        let visibility = if self.config.thumbnail.hide_when_no_eve_focus && !any_eve_focused { VisibilityState::HiddenAutomatic } else { VisibilityState::Visible };
        let id = self.next_id;
        self.next_id += 1;
        self.thumbs.push(Thumb {
            id,
            window: w.id,
            title: w.title.clone(),
            name: w.character_name.clone(),
            system_name: String::new(),
            minimized: w.minimized,
            visibility,
            notifications: Vec::new(),
            excluded: false,
            metrics: Metrics::default(),
            character_color: None,
            border_colors: None,
            hide_thumbnail: false,
            opacity: 255,
            badge: String::new(),
            system_color: 0,
            rect,
            surf: None,
            cap: Capture::default(),
            settings: None,
            overlay_tex: None,
            needs_render: true,
            needs_draw: false,
            inactive_since: now_ms(),
            logged_out_at: now_ms(),
            system_name_event_ts: 0,
            last_jump_ms: 0,
            travel_alert_fired: false,
            last_click_ms: 0,
            last_notified_by_type: vec![0; eve_maj_core::types::NotificationType::COUNT],
        });
        let i = self.thumbs.len() - 1;
        self.thumbs[i].excluded = self.hotkeys.is_character_excluded(&self.config, &w.character_name);
        self.refresh_character_caches(i);
        SLOG.info(format_args!("Created thumbnail for {} at ({}, {}) {}x{}", w.character_name, rect.x, rect.y, rect.width, rect.height));
    }

    /// `window_gone`: the client window closed or unmapped (as opposed to shutdown or a filter change).
    fn destroy_thumb(&mut self, mut t: Thumb, window_gone: bool) {
        if self.pointer_state.hover == Some(t.id) {
            self.pointer_state.hover = None;
        }
        if self.pointer_state.drag.as_ref().is_some_and(|d| d.thumb == t.id) {
            self.pointer_state.drag = None;
        }
        self.free_capture(&mut t.cap, window_gone);
        if let Some(tex) = t.overlay_tex.take() {
            self.gpu.make_current_surfaceless();
            self.gpu.delete_texture(tex);
        }
        if let Some(s) = t.surf.take() {
            self.destroy_surf(s);
        }
    }

    /// Re-applies every thumbnail's rectangle from the current config (the Windows reposition_all_thumbnails).
    fn reflow(&mut self) {
        if self.config.display.layout_mode == eve_maj_core::types::LayoutMode::RegionFit || self.config.display.not_logged_in_space_enabled {
            self.membership = accounts_store::load_membership();
        }
        let names = self.names();
        let scale_at = |_: i32, _: i32| 1.0;
        let placed = self.screens_layout(&names, &scale_at).place_all();
        for (i, p) in placed.into_iter().enumerate() {
            if self.thumbs[i].rect != p {
                self.thumbs[i].rect = p;
                self.apply_geometry(i);
            }
        }
    }

    fn effective_state(&self, t: &Thumb) -> ThumbnailState {
        if self.pointer_state.drag.as_ref().is_some_and(|d| d.thumb == t.id) {
            ThumbnailState::Dragging
        } else if !t.notifications.is_empty() {
            ThumbnailState::Alert
        } else if self.active == Some(t.window) {
            ThumbnailState::Active
        } else if t.minimized {
            ThumbnailState::Minimized
        } else {
            ThumbnailState::Inactive
        }
    }

    fn set_active(&mut self, new_active: Option<u64>) {
        if new_active == self.active {
            return;
        }
        let old = self.active;
        self.active = new_active;
        self.on_focus_changed(new_active);
        for t in &mut self.thumbs {
            if Some(t.window) == old || Some(t.window) == new_active {
                t.needs_render = true;
            }
        }
        self.reconcile_states();
        self.render_dirty();
    }

    /// Unhides auto-hidden thumbnails when an EVE client gains focus; starts the hide debounce when none has it.
    fn reconcile_states(&mut self) {
        let eve_focused = self.active.is_some_and(|a| self.thumbs.iter().any(|t| t.window == a));
        if eve_focused {
            self.eve_focus_lost_at = None;
            for t in &mut self.thumbs {
                if t.visibility == VisibilityState::HiddenAutomatic {
                    t.visibility = try_transition_visibility(t.visibility, VisibilityState::Visible, &t.name);
                    t.needs_render = true;
                }
            }
        } else if self.eve_focus_lost_at.is_none() {
            self.eve_focus_lost_at = Some(now_ms());
        }
    }

    fn apply_hide_when_no_eve_focus(&mut self) {
        if !self.config.thumbnail.hide_when_no_eve_focus || self.pointer_state.drag.is_some() {
            return;
        }
        let Some(since) = self.eve_focus_lost_at else { return };
        if now_ms().saturating_sub(since) < self.config.thumbnail.hide_debounce_ms as u64 {
            return;
        }
        for t in &mut self.thumbs {
            // Alerting thumbnails stay up, like the Windows set_visibility guard.
            if t.visibility == VisibilityState::Visible && t.notifications.is_empty() {
                t.visibility = try_transition_visibility(t.visibility, VisibilityState::HiddenAutomatic, &t.name);
                t.needs_render = true;
            }
        }
    }

    fn expire_notifications(&mut self) {
        let now = now_ms();
        for t in &mut self.thumbs {
            let before = t.notifications.len();
            t.notifications.retain(|n| n.duration_ms == 0 || now.saturating_sub(n.start_ms) < n.duration_ms as u64);
            // A flashing border needs a repaint each phase.
            let flashing = t.notifications.first().is_some_and(|n| n.flash_border && n.show_border && now.saturating_sub(n.start_ms) < 1300);
            if t.notifications.len() != before || flashing {
                t.needs_render = true;
            }
        }
    }

    fn check_profile_reload(&mut self) {
        let mtime = profile_mtime(&self.config.profile_name);
        if mtime.is_none() || mtime == self.profile_mtime {
            return;
        }
        self.profile_mtime = mtime;
        match Config::load_profile(&self.config.profile_name) {
            Ok(c) => {
                SLOG.info(format_args!("Profile {} changed on disk, reloading", c.profile_name));
                self.apply_new_config(c);
            }
            Err(e) => SLOG.warn(format_args!("Profile changed but failed to load: {e}")),
        }
    }

    // -------------------------------------------------------------------------------------------------- render --

    fn render_dirty(&mut self) {
        for i in 0..self.thumbs.len() {
            if self.thumbs[i].needs_render {
                self.thumbs[i].needs_render = false;
                self.render_thumb(i);
            }
        }
        self.update_panels();
    }

    /// The Windows render_thumbnail: resolve the look, show/hide, repaint the overlay only when it changed.
    fn render_thumb(&mut self, i: usize) {
        let state = self.effective_state(&self.thumbs[i]);
        // The Client List and Nothing view modes track clients without showing thumbnails.
        let hidden_for_region = self.region.as_ref().is_some_and(|r| r.hides_thumbnails()) || self.config.display.view_mode != eve_maj_core::types::ViewMode::Thumbnails;
        let t = &self.thumbs[i];
        let view = ThumbnailView {
            state,
            is_visible: t.visibility.is_visible() && !hidden_for_region,
            is_focused: self.active == Some(t.window),
            character_name: &t.name,
            system_name: &t.system_name,
            notifications: &t.notifications,
            hide_thumbnail: t.hide_thumbnail,
            border_colors: t.border_colors,
            character_color: t.character_color,
            system_color: t.system_color,
            opacity: t.opacity,
            group_badge_label: &t.badge,
            is_excluded_from_cycle: t.excluded,
            overlay_width: t.rect.width,
            overlay_height: t.rect.height,
            dpi_scale: 1.0,
            metrics: t.metrics,
        };
        let settings = render::create_render_settings(&self.config, &view);
        if t.settings.as_ref() == Some(&settings) {
            return;
        }
        if !settings.show_thumbnail {
            if let Some(s) = self.thumbs[i].surf.take() {
                self.destroy_surf(s);
            }
            self.thumbs[i].settings = Some(settings);
            return;
        }
        let overlay_unchanged = t.settings.as_ref().is_some_and(|c| c.only_visibility_changed(&settings)) && t.overlay_tex.is_some();
        if !overlay_unchanged {
            match overlay::paint(&self.config, &settings) {
                Some(mut img) => {
                    let (w, h, stride) = (img.width() as u32, img.height() as u32, img.stride() as u32);
                    self.gpu.make_current_surfaceless();
                    if self.thumbs[i].overlay_tex.is_none() {
                        self.thumbs[i].overlay_tex = self.gpu.new_texture();
                    }
                    if let (Some(tex), Ok(data)) = (self.thumbs[i].overlay_tex, img.data()) {
                        self.gpu.upload_rgba(tex, w, h, stride, &data);
                    }
                }
                None => SLOG.err(format_args!("Failed to paint overlay for {}", self.thumbs[i].name)),
            }
        }
        self.thumbs[i].settings = Some(settings);
        if self.thumbs[i].surf.is_none() {
            self.create_surf(i);
        }
        self.thumbs[i].needs_draw = true;
        self.draw(i);
    }

    /// Composites the live window image and the overlay into the thumbnail's surface, at most once per compositor frame.
    fn draw(&mut self, i: usize) {
        let t = &mut self.thumbs[i];
        let Some(s) = t.surf.as_mut() else { return };
        let Some(egl_surface) = s.egl else { return };
        if !s.configured || s.frame_pending || !t.needs_draw {
            return;
        }
        let Some(settings) = t.settings.as_ref() else { return };
        t.needs_draw = false;
        let (w, h) = s.size;
        if !self.gpu.begin_frame(egl_surface, w, h) {
            return;
        }
        let thumb_alpha = settings.thumbnail_alpha as f32 / 255.0;
        let front = t.cap.front();
        match (t.cap.have_image, front.tex) {
            (true, Some(tex)) => self.gpu.draw(tex, !front.dmabuf, t.cap.y_invert, false, thumb_alpha),
            _ => self.gpu.clear(0.0, 0.0, 0.0, thumb_alpha),
        }
        if let Some(tex) = t.overlay_tex {
            self.gpu.draw(tex, true, false, true, settings.overlay_alpha as f32 / 255.0);
        }
        s.wl.frame(&self.qh, t.id);
        s.frame_pending = true;
        self.gpu.end_frame(egl_surface);
    }

    // ------------------------------------------------------------------------------------------------- surfaces --

    fn monitor_for(&self, x: i32, y: i32) -> Option<&Monitor> {
        let inside = |m: &&Monitor| x >= m.bounds.left && x < m.bounds.right && y >= m.bounds.top && y < m.bounds.bottom;
        self.monitors.iter().find(inside).or_else(|| {
            self.monitors.iter().min_by_key(|m| {
                let dx = (m.bounds.left - x).max(0).max(x - m.bounds.right);
                let dy = (m.bounds.top - y).max(0).max(y - m.bounds.bottom);
                dx as i64 * dx as i64 + dy as i64 * dy as i64
            })
        })
    }

    fn create_surf(&mut self, i: usize) {
        let rect = self.thumbs[i].rect;
        let Some(monitor) = self.monitor_for(rect.x, rect.y).cloned() else {
            SLOG.warn(format_args!("No monitor to show {} on", self.thumbs[i].name));
            return;
        };
        let output = self.outputs.iter().find(|o| o.name == monitor.name).map(|o| o.wl.clone());
        let id = self.thumbs[i].id;
        let wl = self.compositor.create_surface(&self.qh, id);
        let layer = self.layer_shell.get_layer_surface(&wl, output.as_ref(), zwlr_layer_shell_v1::Layer::Overlay, LAYER_NAMESPACE.to_owned(), &self.qh, id);
        layer.set_size(rect.width.max(1) as u32, rect.height.max(1) as u32);
        layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        layer.set_margin(rect.y - monitor.bounds.top, 0, 0, rect.x - monitor.bounds.left);
        layer.set_exclusive_zone(-1);
        // On-demand so a click delivers the held modifiers (Ctrl+drag); focus is handed straight back to EVE.
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand);
        self.thumbs[i].surf = Some(Surf {
            wl,
            layer,
            egl_window: None,
            egl: None,
            output: monitor.name.clone(),
            origin: (monitor.bounds.left, monitor.bounds.top),
            configured: false,
            frame_pending: false,
            size: (rect.width.max(1), rect.height.max(1)),
        });
        self.set_click_through(i);
        if let Some(s) = &self.thumbs[i].surf {
            s.wl.commit();
        }
    }

    fn set_click_through(&mut self, i: usize) {
        let Some(s) = &self.thumbs[i].surf else { return };
        if self.config.interaction.click_through {
            let region: WlRegion = self.compositor.create_region(&self.qh, ());
            s.wl.set_input_region(Some(&region));
            region.destroy();
        } else {
            s.wl.set_input_region(None);
        }
        s.wl.commit();
    }

    fn destroy_surf(&mut self, s: Surf) {
        if let Some(e) = s.egl {
            self.gpu.destroy_surface(e);
        }
        drop(s.egl_window);
        s.layer.destroy();
        s.wl.destroy();
    }

    /// Moves/resizes a thumbnail's surface to its `rect`, re-creating it if the rect moved to another monitor.
    fn apply_geometry(&mut self, i: usize) {
        let rect = self.thumbs[i].rect;
        let target = self.monitor_for(rect.x, rect.y).map(|m| (m.name.clone(), (m.bounds.left, m.bounds.top)));
        let Some(s) = self.thumbs[i].surf.as_mut() else { return };
        if target.as_ref().is_some_and(|(name, _)| *name != s.output) {
            let s = self.thumbs[i].surf.take().expect("checked");
            self.destroy_surf(s);
            self.thumbs[i].settings = None;
            self.thumbs[i].needs_render = true;
            return;
        }
        let origin = target.map_or(s.origin, |(_, o)| o);
        s.origin = origin;
        s.layer.set_margin(rect.y - origin.1, 0, 0, rect.x - origin.0);
        let resized = s.size != (rect.width, rect.height);
        if resized {
            s.layer.set_size(rect.width.max(1) as u32, rect.height.max(1) as u32);
        }
        s.wl.commit();
        if resized {
            self.thumbs[i].needs_render = true;
        }
    }

    fn thumb_index(&self, id: u64) -> Option<usize> {
        self.thumbs.iter().position(|t| t.id == id)
    }

    // ---------------------------------------------------------------------------------------------------- input --

    fn on_click(&mut self, id: u64) {
        let Some(i) = self.thumb_index(id) else { return };
        if self.shift && self.config.exclusion.enable_shift_click_exclude {
            self.toggle_exclusion(i);
            self.render_dirty();
            return;
        }
        self.thumbs[i].last_click_ms = now_ms();
        let window = self.thumbs[i].window;
        self.activate_window(window);
    }

    fn start_drag(&mut self, id: u64) {
        if !self.config.interaction.enable_dragging {
            return;
        }
        let Some(i) = self.thumb_index(id) else { return };
        let ghosts = if self.config.snapping.enabled && self.config.snapping.ghost_positions {
            let names = self.names();
            let scale_at = |_: i32, _: i32| 1.0;
            self.screens_layout(&names, &scale_at).collect_ghost_groups(&self.thumbs[i].name)
        } else {
            Vec::new()
        };
        self.pointer_state.drag = Some(Drag { thumb: id, offset: self.pointer_state.local, group: self.ctrl, ghosts, refocus: self.active.filter(|a| self.thumbs.iter().any(|t| t.window == *a)) });
        self.thumbs[i].needs_render = true;
        self.render_dirty();
    }

    fn handle_drag_motion(&mut self) {
        let Some(drag) = &self.pointer_state.drag else { return };
        let Some(i) = self.thumb_index(drag.thumb) else { return };
        let rect = self.thumbs[i].rect;
        let (lx, ly) = self.pointer_state.local;
        let new_x = rect.x + (lx - drag.offset.0).round() as i32;
        let new_y = rect.y + (ly - drag.offset.1).round() as i32;
        if (new_x, new_y) == (rect.x, rect.y) {
            return;
        }
        let snapping = &self.config.snapping;
        let bounds = self.monitor_for(new_x + rect.width / 2, new_y + rect.height / 2).map(|m| m.bounds);
        if drag.group {
            // Group move: screen-edge snapping only, applied to the grabbed thumbnail's delta.
            let (mut sx, mut sy) = (new_x, new_y);
            if snapping.enabled && snapping.screen_edges {
                if let Some(b) = bounds {
                    (sx, sy) = layout::screen_edge_snap(new_x, new_y, rect.width, rect.height, snapping.threshold, b);
                }
            }
            let (dx, dy) = (sx - rect.x, sy - rect.y);
            for j in 0..self.thumbs.len() {
                self.thumbs[j].rect.x += dx;
                self.thumbs[j].rect.y += dy;
                self.move_surface_only(j);
            }
            return;
        }
        let (mut sx, mut sy) = (new_x, new_y);
        if snapping.enabled {
            if snapping.screen_edges {
                if let Some(b) = bounds {
                    (sx, sy) = layout::screen_edge_snap(sx, sy, rect.width, rect.height, snapping.threshold, b);
                }
            }
            if snapping.thumbnail_edges {
                let others: Vec<Rect> = self
                    .thumbs
                    .iter()
                    .filter(|t| t.id != drag.thumb && t.surf.is_some())
                    .map(|t| Rect { left: t.rect.x, top: t.rect.y, right: t.rect.x + t.rect.width, bottom: t.rect.y + t.rect.height })
                    .collect();
                (sx, sy) = layout::thumbnail_edge_snap(sx, sy, rect.width, rect.height, snapping.threshold, &others);
            }
            if snapping.ghost_positions {
                (sx, sy) = layout::ghost_snap(sx, sy, rect.width, rect.height, snapping.threshold, &drag.ghosts);
            }
        }
        self.thumbs[i].rect.x = sx;
        self.thumbs[i].rect.y = sy;
        self.move_surface_only(i);
    }

    /// Repositions within the surface's current monitor (a cross-monitor move re-homes the surface once the drag ends).
    fn move_surface_only(&mut self, i: usize) {
        let rect = self.thumbs[i].rect;
        if let Some(s) = &self.thumbs[i].surf {
            s.layer.set_margin(rect.y - s.origin.1, 0, 0, rect.x - s.origin.0);
            s.wl.commit();
        }
    }

    fn end_drag(&mut self) {
        let Some(drag) = self.pointer_state.drag.take() else { return };
        let targets: Vec<usize> = if drag.group { (0..self.thumbs.len()).collect() } else { self.thumb_index(drag.thumb).into_iter().collect() };
        for i in targets {
            let (name, rect) = (self.thumbs[i].name.clone(), self.thumbs[i].rect);
            if let Err(e) = self.config.save_character_position(&name, Position { x: rect.x, y: rect.y }) {
                SLOG.err(format_args!("Failed to save position for {name}: {e}"));
            }
            self.thumbs[i].needs_render = true;
            self.apply_geometry(i);
        }
        // Our own write; don't treat it as an external edit.
        self.profile_mtime = profile_mtime(&self.config.profile_name);
        if let Some(addr) = drag.refocus {
            self.wm.focus(addr);
        }
        self.render_dirty();
    }

    fn apply_hover_cursor(&mut self, serial: u32) {
        let (Some(mgr), Some(pointer)) = (&self.cursor_shape, &self.pointer) else { return };
        let shape = match self.config.interaction.hover_cursor {
            HoverCursor::Default => wp_cursor_shape_device_v1::Shape::Default,
            HoverCursor::Hand => wp_cursor_shape_device_v1::Shape::Pointer,
            HoverCursor::Crosshair => wp_cursor_shape_device_v1::Shape::Crosshair,
            HoverCursor::Move => wp_cursor_shape_device_v1::Shape::Move,
            HoverCursor::Help => wp_cursor_shape_device_v1::Shape::Help,
        };
        if self.cursor_device.is_none() {
            self.cursor_device = Some(mgr.get_pointer(pointer, &self.qh, ()));
        }
        if let Some(d) = &self.cursor_device {
            d.set_shape(serial, shape);
        }
    }
}

// ------------------------------------------------------------------------------------------- Wayland dispatching --

delegate_noop!(App: ignore WlCompositor);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlShmPool);
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: ignore WlRegion);
delegate_noop!(App: ignore ZwlrLayerShellV1);
delegate_noop!(App: ignore ZwpLinuxDmabufV1);
delegate_noop!(App: ignore HyprlandToplevelExportManagerV1);
delegate_noop!(App: ignore WpCursorShapeManagerV1);
delegate_noop!(App: ignore WpCursorShapeDeviceV1);

impl Dispatch<WlRegistry, GlobalListContents> for App {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlOutput, ()> for App {
    fn event(state: &mut Self, output: &WlOutput, event: <WlOutput as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wayland_client::protocol::wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.iter_mut().find(|o| &o.wl == output) {
                o.name = name;
            }
        }
    }
}

impl Dispatch<WlSurface, u64> for App {
    fn event(_: &mut Self, _: &WlSurface, _: <WlSurface as Proxy>::Event, _: &u64, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlCallback, u64> for App {
    fn event(state: &mut Self, _: &WlCallback, _: <WlCallback as Proxy>::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        if *id >= region_select::SURFACE_ID_BASE {
            return state.region_frame_done(*id);
        }
        if panels::is_panel(*id) {
            return state.panel_frame_done(*id);
        }
        let Some(i) = state.thumb_index(*id) else { return };
        if let Some(s) = state.thumbs[i].surf.as_mut() {
            s.frame_pending = false;
        }
        state.draw(i);
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u64> for App {
    fn event(state: &mut Self, layer: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        if *id >= region_select::SURFACE_ID_BASE {
            return state.region_layer_event(*id, event);
        }
        if panels::is_panel(*id) {
            return state.panel_layer_event(*id, event);
        }
        let Some(i) = state.thumb_index(*id) else { return };
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                let rect = state.thumbs[i].rect;
                let (w, h) = (if width > 0 { width as i32 } else { rect.width.max(1) }, if height > 0 { height as i32 } else { rect.height.max(1) });
                let Some(s) = state.thumbs[i].surf.as_mut() else { return };
                if s.egl_window.is_none() {
                    match wayland_egl::WlEglSurface::new(s.wl.id(), w, h) {
                        Ok(win) => {
                            s.egl = unsafe { state.gpu.create_window_surface(win.ptr() as *mut c_void) };
                            if let Some(e) = s.egl {
                                state.gpu.set_swap_interval_zero(e);
                            }
                            s.egl_window = Some(win);
                        }
                        Err(e) => SLOG.err(format_args!("wl_egl_window_create failed: {e}")),
                    }
                } else if s.size != (w, h) {
                    if let Some(win) = &s.egl_window {
                        win.resize(w, h, 0, 0);
                    }
                }
                s.size = (w, h);
                s.configured = true;
                state.thumbs[i].needs_draw = true;
                state.draw(i);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(s) = state.thumbs[i].surf.take() {
                    state.destroy_surf(s);
                }
                state.thumbs[i].settings = None;
                state.thumbs[i].needs_render = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(state: &mut Self, seat: &WlSeat, event: wayland_client::protocol::wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wayland_client::protocol::wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            use wayland_client::protocol::wl_seat::Capability;
            if caps.contains(Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(state: &mut Self, _: &WlKeyboard, event: wayland_client::protocol::wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wayland_client::protocol::wl_keyboard::{Event, KeyState};
        match event {
            // xkb's standard modifier bits: Shift = 0x1, Control = 0x4.
            Event::Modifiers { mods_depressed, .. } => {
                state.ctrl = mods_depressed & 0x4 != 0;
                state.shift = mods_depressed & 0x1 != 0;
            }
            Event::Keymap { format: WEnum::Value(wayland_client::protocol::wl_keyboard::KeymapFormat::XkbV1), fd, size } => {
                state.confirm_keys = keymap::read_keymap(&fd, size);
            }
            Event::Key { key, state: WEnum::Value(KeyState::Pressed), .. } if state.region.is_some() => state.region_key(key),
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(state: &mut Self, _: &WlPointer, event: wayland_client::protocol::wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wayland_client::protocol::wl_pointer::{ButtonState, Event};
        if state.region.is_some() {
            return state.region_pointer(event);
        }
        match event {
            Event::Enter { serial, surface, surface_x, surface_y } => {
                state.pointer_state.hover = surface.data::<u64>().copied();
                state.pointer_state.local = (surface_x, surface_y);
                state.pointer_state.enter_serial = serial;
                state.apply_hover_cursor(serial);
            }
            Event::Leave { .. } => {
                if state.pointer_state.drag.is_none() && !state.panels.dragging() {
                    state.pointer_state.hover = None;
                }
                state.pointer_state.left_pending = None;
            }
            Event::Motion { surface_x, surface_y, .. } => {
                state.pointer_state.local = (surface_x, surface_y);
                if state.panels.dragging() {
                    state.panel_drag_motion((surface_x, surface_y));
                } else if state.pointer_state.drag.is_some() {
                    state.handle_drag_motion();
                }
            }
            Event::Button { button, state: WEnum::Value(bstate), .. } => {
                let pressed = bstate == ButtonState::Pressed;
                if button == BTN_LEFT && !pressed && state.panels.dragging() {
                    return state.end_panel_drag();
                }
                if let Some(id) = state.pointer_state.hover.filter(|&id| panels::is_panel(id)) {
                    if button == BTN_LEFT && pressed {
                        state.panel_press(id, state.pointer_state.local);
                    }
                    return;
                }
                let Some(hover) = state.pointer_state.hover else {
                    if !pressed && button == BTN_RIGHT {
                        state.end_drag();
                    }
                    return;
                };
                match (button, pressed) {
                    (BTN_LEFT, true) => {
                        if state.config.interaction.click_trigger == ClickTrigger::MouseDown {
                            state.on_click(hover);
                        } else {
                            state.pointer_state.left_pending = Some(hover);
                        }
                    }
                    (BTN_LEFT, false) => {
                        if state.config.interaction.click_trigger == ClickTrigger::MouseUp && state.pointer_state.left_pending.take() == Some(hover) {
                            state.on_click(hover);
                        }
                    }
                    (BTN_RIGHT, true) => state.start_drag(hover),
                    (BTN_RIGHT, false) => state.end_drag(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
