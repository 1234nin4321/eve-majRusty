//! Tray icon (freedesktop StatusNotifierItem, shown by Waybar/KDE/etc.) with the Windows tray menu's items.
//!
//! ksni runs the D-Bus service on its own thread; menu clicks are sent to the main loop over a channel, with a pipe
//! the main loop polls so it wakes up for them.

use std::os::fd::RawFd;
use std::sync::mpsc::{channel, Receiver, Sender};

use eve_maj_core::log::Scope;
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

const SLOG: Scope = Scope::new("tray");
static ICON_ICO: &[u8] = include_bytes!("../../../../assets/icon.ico");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    OpenSettings,
    LoadProfile(String),
    ToggleDragging,
    ToggleAutoMinimize,
    ToggleTravelMode,
    ToggleVisibility,
    RestoreSavedPositions,
    SaveClientPositions,
    ClearNotificationHistory,
    SuspendHotkeys,
    CloseAll,
    Exit,
}

/// What the menu shows; pushed from the main loop whenever it changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayState {
    pub profiles: Vec<String>,
    pub current_profile: String,
    pub dragging: bool,
    pub auto_minimize: bool,
    pub travel_mode: bool,
    pub thumbnails_visible: bool,
    pub has_suspend_hotkey: bool,
    pub hotkeys_suspended: bool,
}

struct EveTray {
    state: TrayState,
    tx: Sender<TrayCommand>,
    wake: RawFd,
    icon: Vec<ksni::Icon>,
}

impl EveTray {
    fn send(&self, cmd: TrayCommand) {
        if self.tx.send(cmd).is_ok() {
            unsafe { libc::write(self.wake, [1u8].as_ptr() as *const libc::c_void, 1) };
        }
    }
}

fn item(label: &str, cmd: TrayCommand) -> MenuItem<EveTray> {
    StandardItem { label: label.into(), activate: Box::new(move |t: &mut EveTray| t.send(cmd.clone())), ..Default::default() }.into()
}

fn check(label: &str, checked: bool, cmd: TrayCommand) -> MenuItem<EveTray> {
    CheckmarkItem { label: label.into(), checked, activate: Box::new(move |t: &mut EveTray| t.send(cmd.clone())), ..Default::default() }.into()
}

impl ksni::Tray for EveTray {
    fn id(&self) -> String {
        "eve-maj-preview".into()
    }

    fn title(&self) -> String {
        format!("EVE-Maj Preview ({})", self.state.current_profile)
    }

    /// Left click opens the settings window, like double-clicking the Windows tray icon.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::OpenSettings);
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let profiles = if s.profiles.is_empty() {
            vec![StandardItem { label: "(No profiles found)".into(), enabled: false, ..Default::default() }.into()]
        } else {
            let names = s.profiles.clone();
            vec![RadioGroup {
                selected: s.profiles.iter().position(|p| *p == s.current_profile).unwrap_or(usize::MAX),
                select: Box::new(move |t: &mut EveTray, i| {
                    if let Some(name) = names.get(i) {
                        t.send(TrayCommand::LoadProfile(name.clone()));
                    }
                }),
                options: s.profiles.iter().map(|p| RadioItem { label: p.replace('_', "__"), ..Default::default() }).collect(),
            }
            .into()]
        };
        let mut menu = vec![
            item("Settings…", TrayCommand::OpenSettings),
            MenuItem::Separator,
            SubMenu { label: "Load Profile".into(), submenu: profiles, ..Default::default() }.into(),
            MenuItem::Separator,
            check("Enable Dragging", s.dragging, TrayCommand::ToggleDragging),
            check("Enable Auto-Minimize", s.auto_minimize, TrayCommand::ToggleAutoMinimize),
            check("Enable Travel Mode", s.travel_mode, TrayCommand::ToggleTravelMode),
            check("Show Thumbnails", s.thumbnails_visible, TrayCommand::ToggleVisibility),
            item("Restore Saved Positions", TrayCommand::RestoreSavedPositions),
            item("Save Client Window Positions", TrayCommand::SaveClientPositions),
            MenuItem::Separator,
            item("Clear Notification History", TrayCommand::ClearNotificationHistory),
            MenuItem::Separator,
        ];
        if s.has_suspend_hotkey {
            menu.push(check("Suspend Hotkeys", s.hotkeys_suspended, TrayCommand::SuspendHotkeys));
            menu.push(MenuItem::Separator);
        }
        menu.push(item("Close All Clients", TrayCommand::CloseAll));
        menu.push(MenuItem::Separator);
        menu.push(item("Exit", TrayCommand::Exit));
        menu
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<EveTray>,
    rx: Receiver<TrayCommand>,
    wake_read: RawFd,
    state: TrayState,
}

impl Tray {
    /// None when no StatusNotifierWatcher (tray host) is running or D-Bus is unavailable.
    pub fn spawn(state: TrayState) -> Option<Tray> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return None;
        }
        let (tx, rx) = channel();
        let tray = EveTray { state: state.clone(), tx, wake: fds[1], icon: decode_icon().into_iter().collect() };
        match tray.spawn() {
            Ok(handle) => Some(Tray { handle, rx, wake_read: fds[0], state }),
            Err(e) => {
                SLOG.warn(format_args!("No tray icon: {e}"));
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                None
            }
        }
    }

    pub fn fd(&self) -> RawFd {
        self.wake_read
    }

    pub fn drain(&self) -> Vec<TrayCommand> {
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.wake_read, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {}
        self.rx.try_iter().collect()
    }

    pub fn update(&mut self, state: TrayState) {
        if state != self.state {
            self.state = state.clone();
            self.handle.update(move |t| t.state = state);
        }
    }

    pub fn shutdown(&self) {
        self.handle.shutdown().wait();
    }
}

/// The app's 32x32 BMP icon (assets/icon.ico) as the ARGB32 big-endian pixmap the StatusNotifierItem spec wants.
fn decode_icon() -> Option<ksni::Icon> {
    let d = ICON_ICO;
    let u32_at = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?)) };
    // ICONDIRENTRY 0 starts at byte 6; its image offset is at +12.
    let off = u32_at(6 + 12)? as usize;
    let header_len = u32_at(off)? as usize;
    let width = u32_at(off + 4)? as usize;
    let height = u32_at(off + 8)? as usize / 2; // BMP-in-ICO height covers the XOR and AND masks
    let pixels = d.get(off + header_len..off + header_len + width * height * 4)?;
    let mut data = Vec::with_capacity(width * height * 4);
    for row in (0..height).rev() {
        for px in pixels[row * width * 4..(row + 1) * width * 4].chunks_exact(4) {
            data.extend_from_slice(&[px[3], px[2], px[1], px[0]]);
        }
    }
    Some(ksni::Icon { width: width as i32, height: height as i32, data })
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_decodes() {
        let icon = super::decode_icon().expect("bundled icon decodes");
        assert_eq!((icon.width, icon.height), (32, 32));
        assert_eq!(icon.data.len(), 32 * 32 * 4);
        assert!(icon.data.chunks_exact(4).any(|p| p[0] != 0), "some pixels are opaque");
    }
}
