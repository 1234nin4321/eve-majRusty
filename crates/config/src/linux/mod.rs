//! Linux front end of the settings dialog: a native window (tao + wry/WebKitGTK) showing the dialog page served by
//! [`server`], with [`backend`] answering the page's calls against the same files the preview app uses.

mod backend;
mod server;

use std::path::PathBuf;

use eve_maj_core::config::GlobalSettings;
use eve_maj_core::log::{self, Scope};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::{WebViewBuilder, WebViewBuilderExtUnix};

const SLOG: Scope = Scope::new("config");

/// Requests from the backend thread to the window.
#[derive(Debug)]
pub enum UiEvent {
    Close,
    SetAlwaysOnTop(bool),
    /// Open a native picker and send back the chosen path ("" when cancelled).
    Browse(backend::Browse, std::sync::mpsc::Sender<String>),
}

/// Native folder/file chooser (portal-aware), run modally on the GTK thread.
fn pick(kind: backend::Browse) -> String {
    use gtk::prelude::*;
    use gtk::{FileChooserAction, FileChooserNative, FileFilter, ResponseType};
    let (title, action) = match kind {
        backend::Browse::ChatlogDir => ("Select Chatlog Directory", FileChooserAction::SelectFolder),
        backend::Browse::GamelogDir => ("Select Gamelog Directory", FileChooserAction::SelectFolder),
        backend::Browse::SoundFile => ("Select Sound File", FileChooserAction::Open),
    };
    let dialog = FileChooserNative::new(Some(title), None::<&gtk::Window>, action, Some("Select"), Some("Cancel"));
    if let backend::Browse::SoundFile = kind {
        let filter = FileFilter::new();
        filter.set_name(Some("Audio Files (*.wav, *.mp3)"));
        for pattern in ["*.wav", "*.WAV", "*.mp3", "*.MP3"] {
            filter.add_pattern(pattern);
        }
        dialog.add_filter(filter);
    }
    let chosen = if dialog.run() == ResponseType::Accept { dialog.filename().map(|p| p.display().to_string()) } else { None };
    dialog.destroy();
    chosen.unwrap_or_default()
}

pub fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("eve-maj"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/eve-maj"),
    }
}

pub fn run() -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(dir.join("profiles")).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    std::env::set_current_dir(&dir).map_err(|e| format!("can't enter {}: {e}", dir.display()))?;
    let global = GlobalSettings::load();
    backend::set_local_app_data_from_wine();
    // A profile file name on the command line opens that one; otherwise the last used, as on Windows.
    let profile = std::env::args()
        .nth(1)
        .filter(|a| eve_maj_core::config::is_safe_profile_name(a))
        .unwrap_or_else(|| if global.last_used_profile.is_empty() { "default.json".to_owned() } else { global.last_used_profile.clone() });
    log::set_level(global.log_level);
    if std::env::var_os("EVE_MAJ_CONSOLE").is_some() {
        log::set_console_ready(true);
    }
    SLOG.info(format_args!("EVE-Maj Config v{} (Linux)", env!("CARGO_PKG_VERSION")));

    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let server = server::Server::start(backend::Backend::new(proxy, profile, &global.language)).map_err(|e| format!("can't start the dialog server: {e}"))?;
    let url = format!("http://127.0.0.1:{}/", server.port);

    let window = WindowBuilder::new()
        .with_title("EVE-Maj Preview Configuration")
        .with_inner_size(LogicalSize::new(1100.0, 800.0))
        .with_min_inner_size(LogicalSize::new(760.0, 520.0))
        .with_always_on_top(global.always_on_top)
        .build(&event_loop)
        .map_err(|e| format!("can't open the window: {e}"))?;
    let vbox = window.default_vbox().ok_or("no GTK container for the web view")?;
    let token = server.token.clone();
    let _webview = WebViewBuilder::new()
        .with_url(&url)
        // The shim reads the call token from here; it never appears in a URL or a page another origin can load.
        .with_initialization_script(&format!("window.__EVEMAJ_TOKEN__ = {};", serde_json::Value::String(token)))
        .with_devtools(std::env::var_os("EVE_MAJ_DEVTOOLS").is_some())
        .build_gtk(vbox)
        .map_err(|e| format!("can't create the web view: {e}"))?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } | Event::UserEvent(UiEvent::Close) => {
                // Safety net, as on Windows: drop any unsaved live preview and give the app its hotkeys back.
                backend::send_command(&eve_maj_core::protocol::Command::RevertPreview);
                backend::send_command(&eve_maj_core::protocol::Command::DialogResumeHotkeys);
                log::flush();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UiEvent::SetAlwaysOnTop(on)) => window.set_always_on_top(on),
            Event::UserEvent(UiEvent::Browse(kind, reply)) => {
                let _ = reply.send(pick(kind));
            }
            _ => {}
        }
    });
}
