//! evemajpreview:// URL parsing, the command set and the byte layouts exchanged between processes
//! (WM_COPYDATA payloads and the region-select result mapping). The Win32 side (registry
//! registration, FindWindow/SendMessage, the shared-memory mapping) lives in eve_maj_app::protocol;
//! everything here is platform-neutral so it unit-tests anywhere.

use crate::config::is_safe_profile_name;
use crate::log::Scope;

const SLOG: Scope = Scope::new("protocol");

/// Class name of the main app's hidden timer window, which receives the protocol IPC (main.zig / config_dialog.zig).
pub const TIMER_CLASS_NAME: &str = "EVE_TIMER_CLASS";

const WM_APP: u32 = 0x8000;
/// Window message carrying a `HotkeyAction` as its wParam.
pub const WM_PROTOCOL_HOTKEY: u32 = WM_APP + 7;

/// WM_COPYDATA `dwData` ids.
pub const PROTOCOL_SWITCH_CHARACTER: usize = 1;
pub const PROTOCOL_SWITCH_PROFILE: usize = 2;
pub const PROTOCOL_PREVIEW_THUMBNAIL: usize = 3;
pub const PROTOCOL_REVERT_PREVIEW: usize = 4;
pub const PROTOCOL_DIALOG_SUSPEND_HOTKEYS: usize = 5;
pub const PROTOCOL_DIALOG_RESUME_HOTKEYS: usize = 6;
pub const PROTOCOL_START_REGION_SELECT: usize = 7;
pub const PROTOCOL_TEST_NOTIFICATION: usize = 8;

macro_rules! hotkey_actions {
    ($($variant:ident => $name:literal,)*) => {
        /// Global hotkey actions, mirroring hotkeys.zig's GlobalActionId.
        /// Discriminants are the WM_PROTOCOL_HOTKEY wParam, so the order must stay as is.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(usize)]
        pub enum HotkeyAction {
            $($variant,)*
        }

        impl HotkeyAction {
            pub const ALL: &'static [HotkeyAction] = &[$(HotkeyAction::$variant,)*];

            /// The snake_case name used in evemajpreview://hotkey/<name> URLs and logs.
            pub fn name(self) -> &'static str {
                match self {
                    $(HotkeyAction::$variant => $name,)*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(HotkeyAction::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

hotkey_actions! {
    MinimizeAll => "minimize_all",
    CloseAll => "close_all",
    ToggleVisibility => "toggle_visibility",
    NextProfile => "next_profile",
    PreviousProfile => "previous_profile",
    ToggleExclusion => "toggle_exclusion",
    NextExcluded => "next_excluded",
    PreviousExcluded => "previous_excluded",
    SuspendHotkeys => "suspend_hotkeys",
    ToggleAutoMinimize => "toggle_auto_minimize",
    CycleNotified => "cycle_notified",
    PreviousNotified => "previous_notified",
    NextAllClients => "next_all_clients",
    PreviousAllClients => "previous_all_clients",
    NextNotLoggedIn => "next_not_logged_in",
    PreviousNotLoggedIn => "previous_not_logged_in",
    MoveToSavedPositions => "move_to_saved_positions",
    ReturnToLastApp => "return_to_last_app",
}

impl HotkeyAction {
    /// The WM_PROTOCOL_HOTKEY wParam for this action.
    pub fn to_wparam(self) -> usize {
        self as usize
    }

    /// Receiving side of WM_PROTOCOL_HOTKEY; None for an out-of-range wParam.
    pub fn from_wparam(wparam: usize) -> Option<Self> {
        Self::ALL.get(wparam).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// URL-decoded character name; raw bytes since percent-escapes can produce any byte sequence.
    Switch(Vec<u8>),
    /// URL-decoded profile file name; the receiver must check it with `safe_profile_name` before use.
    Profile(Vec<u8>),
    Hotkey(HotkeyAction),
    PreviewThumbnail(String),
    RevertPreview,
    DialogSuspendHotkeys,
    DialogResumeHotkeys,
    StartRegionSelect(Box<RegionSelectRequest>),
    TestNotification(String),
}

/// What `Command` turns into on the wire: either a WM_COPYDATA or a WM_PROTOCOL_HOTKEY message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage<'a> {
    /// `payload` None means lpData = NULL, cbData = 0.
    CopyData { dw_data: usize, payload: Option<std::borrow::Cow<'a, [u8]>> },
    Hotkey { wparam: usize },
}

impl Command {
    pub fn to_wire(&self) -> WireMessage<'_> {
        use std::borrow::Cow;
        fn copy(dw_data: usize, payload: Option<Cow<'_, [u8]>>) -> WireMessage<'_> {
            WireMessage::CopyData { dw_data, payload }
        }
        match self {
            Command::Switch(name) => copy(PROTOCOL_SWITCH_CHARACTER, Some(Cow::Borrowed(name))),
            Command::Profile(name) => copy(PROTOCOL_SWITCH_PROFILE, Some(Cow::Borrowed(name))),
            Command::Hotkey(action) => WireMessage::Hotkey { wparam: action.to_wparam() },
            Command::PreviewThumbnail(json) => copy(PROTOCOL_PREVIEW_THUMBNAIL, Some(Cow::Borrowed(json.as_bytes()))),
            Command::RevertPreview => copy(PROTOCOL_REVERT_PREVIEW, None),
            Command::DialogSuspendHotkeys => copy(PROTOCOL_DIALOG_SUSPEND_HOTKEYS, None),
            Command::DialogResumeHotkeys => copy(PROTOCOL_DIALOG_RESUME_HOTKEYS, None),
            Command::StartRegionSelect(request) => {
                copy(PROTOCOL_START_REGION_SELECT, Some(Cow::Owned(request.to_wire_bytes())))
            }
            Command::TestNotification(json) => copy(PROTOCOL_TEST_NOTIFICATION, Some(Cow::Borrowed(json.as_bytes()))),
        }
    }

    /// Receiving side of WM_COPYDATA, mirroring main.zig's dispatch: payload-carrying commands are dropped when
    /// lpData is NULL, and unknown ids yield None. JSON payloads that aren't UTF-8 are converted lossily.
    pub fn from_copy_data(dw_data: usize, data: Option<&[u8]>) -> Option<Command> {
        let text = |d: &[u8]| String::from_utf8_lossy(d).into_owned();
        match dw_data {
            PROTOCOL_SWITCH_CHARACTER => data.map(|d| Command::Switch(d.to_vec())),
            PROTOCOL_SWITCH_PROFILE => data.map(|d| Command::Profile(d.to_vec())),
            PROTOCOL_PREVIEW_THUMBNAIL => data.map(|d| Command::PreviewThumbnail(text(d))),
            PROTOCOL_TEST_NOTIFICATION => data.map(|d| Command::TestNotification(text(d))),
            PROTOCOL_REVERT_PREVIEW => Some(Command::RevertPreview),
            PROTOCOL_DIALOG_SUSPEND_HOTKEYS => Some(Command::DialogSuspendHotkeys),
            PROTOCOL_DIALOG_RESUME_HOTKEYS => Some(Command::DialogResumeHotkeys),
            PROTOCOL_START_REGION_SELECT => Some(Command::StartRegionSelect(Box::new(RegionSelectRequest::from_wire_bytes(data)))),
            _ => None,
        }
    }
}

/// The profile name from a `Command::Profile`, if it is a plain profile file name (see profile_name); names
/// reach the receiver from evemajpreview:// URLs and IPC, so anything else must be refused before use.
pub fn safe_profile_name(name: &[u8]) -> Option<&str> {
    std::str::from_utf8(name).ok().filter(|n| is_safe_profile_name(n))
}

/// Zero-padded fixed-size copy of `text` (UTF-8), truncated at a character boundary so a NUL always fits.
pub fn fixed_text<const N: usize>(text: &str) -> [u8; N] {
    let bytes = text.as_bytes();
    let mut out = [0u8; N];
    let mut len = bytes.len().min(N - 1);
    while len > 0 && len < bytes.len() && (bytes[len] & 0xC0) == 0x80 {
        len -= 1;
    }
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

/// The text in a fixed-size, NUL-padded label buffer.
pub fn label_text(buf: &[u8]) -> &[u8] {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    &buf[..end]
}

/// Win32 RECT layout, so wire structs stay byte-identical without depending on windows-sys here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(self) -> i32 {
        self.right - self.left
    }

    pub fn height(self) -> i32 {
        self.bottom - self.top
    }
}

/// The overlay's on-screen text, translated by the config dialog since only it has the language files; English defaults if the dialog sends nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RegionSelectLabels {
    pub save: [u8; 32],
    pub cancel: [u8; 32],
    pub hint_new: [u8; 192],
    pub hint_edit: [u8; 192],
    pub hint_confirm: [u8; 192],
}

impl Default for RegionSelectLabels {
    fn default() -> Self {
        Self {
            save: fixed_text("Save"),
            cancel: fixed_text("Cancel"),
            hint_new: fixed_text("Drag to draw the region, then drag its edges to adjust"),
            hint_edit: fixed_text("Drag the edges to resize, or the inside to move"),
            hint_confirm: fixed_text("Enter or Save to confirm, Esc or right-click to cancel"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionSelectRequest {
    /// Hide the visible thumbnails for the duration of the selection so they don't cover the overlay.
    pub hide_thumbnails: bool,
    /// The region to adjust; None starts a fresh drag.
    pub edit_region: Option<Rect>,
    pub labels: RegionSelectLabels,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RegionSelectRequestWire {
    hide_thumbnails: u32,
    has_edit_region: u32,
    edit_region: Rect,
    labels: RegionSelectLabels,
}

/// Size of the StartRegionSelect WM_COPYDATA payload.
pub const REGION_SELECT_REQUEST_WIRE_SIZE: usize = std::mem::size_of::<RegionSelectRequestWire>();

impl RegionSelectRequest {
    /// The StartRegionSelect WM_COPYDATA payload.
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let wire = RegionSelectRequestWire {
            hide_thumbnails: self.hide_thumbnails as u32,
            has_edit_region: self.edit_region.is_some() as u32,
            edit_region: self.edit_region.unwrap_or_default(),
            labels: self.labels,
        };
        // SAFETY: RegionSelectRequestWire is repr(C), made only of u32/i32/u8 fields with no padding.
        unsafe { std::slice::from_raw_parts((&wire as *const RegionSelectRequestWire).cast::<u8>(), REGION_SELECT_REQUEST_WIRE_SIZE) }
            .to_vec()
    }

    /// Receiving side of `Command::StartRegionSelect`; a malformed payload falls back to a plain fresh drag.
    pub fn from_wire_bytes(data: Option<&[u8]>) -> Self {
        let Some(data) = data else { return Self::default() };
        if data.len() != REGION_SELECT_REQUEST_WIRE_SIZE {
            return Self::default();
        }
        // SAFETY: length checked above; every bit pattern is a valid RegionSelectRequestWire.
        let wire = unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<RegionSelectRequestWire>()) };
        Self {
            hide_thumbnails: wire.hide_thumbnails != 0,
            edit_region: (wire.has_edit_region != 0).then_some(wire.edit_region),
            labels: wire.labels,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum RegionSelectStatus {
    #[default]
    None = 0,
    Success = 1,
    Cancelled = 2,
    TooSmall = 3,
}

impl RegionSelectStatus {
    /// Unknown values (only possible from a foreign writer) read as None.
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Success,
            2 => Self::Cancelled,
            3 => Self::TooSmall,
            _ => Self::None,
        }
    }
}

/// Cross-process result of a "Start Region Selection" drag; a named shared-memory mapping since today's WM_COPYDATA IPC is one-way dialog->app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionSelectResult {
    pub sequence: u32,
    pub status: RegionSelectStatus,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Byte layout of `RegionSelectResult` inside the mapping; status kept as a raw u32 so any value read back is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RegionSelectResultWire {
    pub sequence: u32,
    pub status: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub const REGION_SELECT_RESULT_SIZE: usize = std::mem::size_of::<RegionSelectResultWire>();

pub const REGION_SELECT_MAPPING_NAME: &str = "Local\\EVE-Maj-Preview-RegionSelectResult";

impl From<RegionSelectResultWire> for RegionSelectResult {
    fn from(w: RegionSelectResultWire) -> Self {
        Self {
            sequence: w.sequence,
            status: RegionSelectStatus::from_u32(w.status),
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
        }
    }
}

impl RegionSelectResultWire {
    /// The next result after `self`: sequence incremented (wrapping), status and rect replaced.
    pub fn next(self, status: RegionSelectStatus, rect: Rect) -> Self {
        Self {
            sequence: self.sequence.wrapping_add(1),
            status: status as u32,
            x: rect.left,
            y: rect.top,
            width: rect.width(),
            height: rect.height(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    InvalidProtocol,
    MissingAction,
    MissingParameter,
    UnknownHotkeyAction,
    UnknownAction,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ParseError {}

/// Format: evemajpreview://action/params
pub fn parse_url(url: &str) -> Result<Command, ParseError> {
    const PROTOCOL_PREFIX: &str = "evemajpreview://";
    let Some(path) = url.strip_prefix(PROTOCOL_PREFIX) else {
        SLOG.err(format_args!("Invalid protocol URL: {url}"));
        return Err(ParseError::InvalidProtocol);
    };

    let mut iter = path.split('/');

    let Some(action) = iter.next() else {
        SLOG.err(format_args!("Missing action in protocol URL: {url}"));
        return Err(ParseError::MissingAction);
    };

    match action {
        "switch" => {
            let Some(char_name_encoded) = iter.next() else {
                SLOG.err(format_args!("Missing character name in switch command"));
                return Err(ParseError::MissingParameter);
            };
            Ok(Command::Switch(url_decode(char_name_encoded.as_bytes())))
        }
        "profile" => {
            let Some(profile_name_encoded) = iter.next() else {
                SLOG.err(format_args!("Missing profile name in profile command"));
                return Err(ParseError::MissingParameter);
            };
            Ok(Command::Profile(url_decode(profile_name_encoded.as_bytes())))
        }
        "hotkey" => {
            let Some(hotkey_action) = iter.next() else {
                SLOG.err(format_args!("Missing hotkey action in hotkey command"));
                return Err(ParseError::MissingParameter);
            };
            let Some(parsed_action) = HotkeyAction::from_name(hotkey_action) else {
                SLOG.err(format_args!("Unknown hotkey action: {hotkey_action}"));
                return Err(ParseError::UnknownHotkeyAction);
            };
            Ok(Command::Hotkey(parsed_action))
        }
        _ => {
            SLOG.err(format_args!("Unknown protocol action: {action}"));
            Err(ParseError::UnknownAction)
        }
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// The two characters after a '%' as Zig's `std.fmt.parseInt(u8, hex, 16)` reads them: an optional sign
/// ('+' keeps the single digit after it, '-' only allows zero), no underscores at either end.
fn parse_hex_u8(hex: [u8; 2]) -> Option<u8> {
    match hex[0] {
        b'+' => hex_digit(hex[1]),
        b'-' => hex_digit(hex[1]).filter(|&d| d == 0),
        c => Some(hex_digit(c)? * 16 + hex_digit(hex[1])?),
    }
}

fn url_decode(encoded: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(encoded.len());
    let mut i = 0;
    while i < encoded.len() {
        if encoded[i] == b'%' && i + 2 < encoded.len() {
            match parse_hex_u8([encoded[i + 1], encoded[i + 2]]) {
                Some(value) => {
                    result.push(value);
                    i += 3;
                }
                None => {
                    result.push(encoded[i]);
                    i += 1;
                }
            }
        } else if encoded[i] == b'+' {
            result.push(b' ');
            i += 1;
        } else {
            result.push(encoded[i]);
            i += 1;
        }
    }
    result
}

/// Returns the protocol URL if --protocol was passed, otherwise None. `args` includes the program name first.
pub fn protocol_url_from_args<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<S> = args.into_iter().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i].as_ref() == "--protocol" && i + 1 < args.len() {
            return Some(args[i + 1].as_ref().to_owned());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_switch_and_decodes() {
        assert_eq!(parse_url("evemajpreview://switch/Some%20Pilot"), Ok(Command::Switch(b"Some Pilot".to_vec())));
        assert_eq!(parse_url("evemajpreview://switch/A+B/extra"), Ok(Command::Switch(b"A B".to_vec())));
        assert_eq!(parse_url("evemajpreview://switch/"), Ok(Command::Switch(Vec::new())));
        assert_eq!(parse_url("evemajpreview://switch"), Err(ParseError::MissingParameter));
    }

    #[test]
    fn parses_profile_without_validating() {
        assert_eq!(parse_url("evemajpreview://profile/pvp.json"), Ok(Command::Profile(b"pvp.json".to_vec())));
        let Ok(Command::Profile(name)) = parse_url("evemajpreview://profile/..%5Cevil.json") else { panic!() };
        assert_eq!(name, b"..\\evil.json");
        assert_eq!(safe_profile_name(&name), None);
        assert_eq!(safe_profile_name(b"pvp.json"), Some("pvp.json"));
        assert_eq!(safe_profile_name(b"\xFFx.json"), None);
        assert_eq!(parse_url("evemajpreview://profile"), Err(ParseError::MissingParameter));
    }

    #[test]
    fn parses_hotkeys() {
        assert_eq!(parse_url("evemajpreview://hotkey/close_all"), Ok(Command::Hotkey(HotkeyAction::CloseAll)));
        assert_eq!(parse_url("evemajpreview://hotkey/CLOSE_ALL"), Err(ParseError::UnknownHotkeyAction));
        assert_eq!(parse_url("evemajpreview://hotkey"), Err(ParseError::MissingParameter));
        for (i, action) in HotkeyAction::ALL.iter().enumerate() {
            assert_eq!(action.to_wparam(), i);
            assert_eq!(HotkeyAction::from_wparam(i), Some(*action));
            assert_eq!(HotkeyAction::from_name(action.name()), Some(*action));
        }
        assert_eq!(HotkeyAction::ALL.len(), 18);
        assert_eq!(HotkeyAction::ReturnToLastApp.to_wparam(), 17);
        assert_eq!(HotkeyAction::from_wparam(18), None);
    }

    #[test]
    fn rejects_bad_urls() {
        assert_eq!(parse_url("http://switch/x"), Err(ParseError::InvalidProtocol));
        assert_eq!(parse_url("EVEMAJPREVIEW://switch/x"), Err(ParseError::InvalidProtocol));
        assert_eq!(parse_url("evemajpreview://"), Err(ParseError::UnknownAction));
        assert_eq!(parse_url("evemajpreview://Switch/x"), Err(ParseError::UnknownAction));
    }

    #[test]
    fn url_decode_matches_zig_quirks() {
        assert_eq!(url_decode(b"%41"), b"A");
        assert_eq!(url_decode(b"%4"), b"%4");
        assert_eq!(url_decode(b"%41x"), b"Ax");
        assert_eq!(url_decode(b"%zz1"), b"%zz1");
        assert_eq!(url_decode(b"%+41"), [4, b'1']);
        assert_eq!(url_decode(b"%-01"), [0, b'1']);
        assert_eq!(url_decode(b"%-11"), b"%-11");
        assert_eq!(url_decode(b"%1_1"), b"%1_1");
        assert_eq!(url_decode(b"%_11"), b"%_11");
        assert_eq!(url_decode(b"%ff%2Fa"), [0xFF, b'/', b'a']);
        assert_eq!(url_decode(b"a+b%"), b"a b%");
    }

    #[test]
    fn fixed_text_truncates_on_char_boundary() {
        let t: [u8; 4] = fixed_text("abcdef");
        assert_eq!(t, *b"abc\0");
        let t: [u8; 4] = fixed_text("aé€");
        assert_eq!(label_text(&t), "aé".as_bytes());
        let t: [u8; 4] = fixed_text("€x");
        assert_eq!(label_text(&t), "€".as_bytes());
        let t: [u8; 3] = fixed_text("€");
        assert_eq!(t, [0, 0, 0]);
        assert_eq!(label_text(&RegionSelectLabels::default().cancel), b"Cancel");
    }

    #[test]
    fn region_select_request_round_trips() {
        assert_eq!(REGION_SELECT_REQUEST_WIRE_SIZE, 4 + 4 + 16 + 32 + 32 + 192 * 3);
        let request = RegionSelectRequest {
            hide_thumbnails: true,
            edit_region: Some(Rect { left: -10, top: 20, right: 300, bottom: 400 }),
            labels: RegionSelectLabels { save: fixed_text("Speichern"), ..Default::default() },
        };
        let bytes = request.to_wire_bytes();
        assert_eq!(bytes.len(), REGION_SELECT_REQUEST_WIRE_SIZE);
        assert_eq!(&bytes[0..8], &[1, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&bytes[8..12], &(-10i32).to_le_bytes());
        assert_eq!(&bytes[24..33], b"Speichern");
        assert_eq!(RegionSelectRequest::from_wire_bytes(Some(&bytes)), request);

        let fresh = RegionSelectRequest::default().to_wire_bytes();
        assert_eq!(&fresh[0..24], &[0u8; 24]);
        assert_eq!(RegionSelectRequest::from_wire_bytes(Some(&fresh)), RegionSelectRequest::default());
        assert_eq!(RegionSelectRequest::from_wire_bytes(Some(&bytes[1..])), RegionSelectRequest::default());
        assert_eq!(RegionSelectRequest::from_wire_bytes(None), RegionSelectRequest::default());
    }

    #[test]
    fn copy_data_round_trips() {
        let commands = [
            Command::Switch(b"Pilot".to_vec()),
            Command::Profile(b"a.json".to_vec()),
            Command::PreviewThumbnail("{\"a\":1}".into()),
            Command::RevertPreview,
            Command::DialogSuspendHotkeys,
            Command::DialogResumeHotkeys,
            Command::StartRegionSelect(Box::new(RegionSelectRequest { hide_thumbnails: true, ..Default::default() })),
            Command::TestNotification("{}".into()),
        ];
        for (i, cmd) in commands.iter().enumerate() {
            let WireMessage::CopyData { dw_data, payload } = cmd.to_wire() else { panic!() };
            assert_eq!(dw_data, i + 1);
            assert_eq!(Command::from_copy_data(dw_data, payload.as_deref()).as_ref(), Some(cmd));
        }
        assert_eq!(
            Command::Hotkey(HotkeyAction::NextProfile).to_wire(),
            WireMessage::Hotkey { wparam: 3 }
        );
        assert_eq!(Command::from_copy_data(PROTOCOL_SWITCH_CHARACTER, None), None);
        assert_eq!(Command::from_copy_data(99, Some(b"x")), None);
        assert_eq!(WM_PROTOCOL_HOTKEY, 0x8007);
    }

    #[test]
    fn region_select_result_layout() {
        assert_eq!(REGION_SELECT_RESULT_SIZE, 24);
        let first = RegionSelectResultWire::default().next(
            RegionSelectStatus::Success,
            Rect { left: 5, top: 6, right: 105, bottom: 56 },
        );
        assert_eq!(first, RegionSelectResultWire { sequence: 1, status: 1, x: 5, y: 6, width: 100, height: 50 });
        let wrapped = RegionSelectResultWire { sequence: u32::MAX, ..first }.next(RegionSelectStatus::TooSmall, Rect::default());
        assert_eq!(RegionSelectResult::from(wrapped).sequence, 0);
        assert_eq!(RegionSelectResult::from(wrapped).status, RegionSelectStatus::TooSmall);
        assert_eq!(RegionSelectStatus::from_u32(7), RegionSelectStatus::None);
    }

    #[test]
    fn finds_protocol_arg() {
        assert_eq!(protocol_url_from_args(["exe", "--protocol", "evemajpreview://x"]), Some("evemajpreview://x".into()));
        assert_eq!(protocol_url_from_args(["exe", "--protocol"]), None);
        assert_eq!(protocol_url_from_args(["--protocol", "x"]), None);
        assert_eq!(protocol_url_from_args(["exe", "a", "--protocol", "--protocol", "y"]), Some("--protocol".into()));
    }
}
