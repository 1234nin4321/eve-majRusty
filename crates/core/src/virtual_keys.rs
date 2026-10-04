//! Virtual-key codes and the human-readable hotkey format ("Ctrl+Alt+F9") stored in profiles.
//! A hotkey is a single u32: base virtual key in the low byte, MOD_* flags in bits 8-11.

use std::fmt::Write;

use crate::log::Scope;

const SLOG: Scope = Scope::new("virtual_keys");

pub const VK_F1: u32 = 0x70;
pub const VK_F24: u32 = 0x87;
pub const VK_TAB: u32 = 0x09;
pub const VK_PAUSE: u32 = 0x13;
pub const VK_CAPITAL: u32 = 0x14;
pub const VK_SHIFT: u32 = 0x10;
pub const VK_SPACE: u32 = 0x20;
pub const VK_PRIOR: u32 = 0x21;
pub const VK_NEXT: u32 = 0x22;
pub const VK_END: u32 = 0x23;
pub const VK_HOME: u32 = 0x24;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_INSERT: u32 = 0x2D;
pub const VK_DELETE: u32 = 0x2E;
pub const VK_NUMLOCK: u32 = 0x90;
pub const VK_SCROLL: u32 = 0x91;
pub const VK_OEM_1: u32 = 0xBA;
pub const VK_OEM_PLUS: u32 = 0xBB;
pub const VK_OEM_COMMA: u32 = 0xBC;
pub const VK_OEM_MINUS: u32 = 0xBD;
pub const VK_OEM_PERIOD: u32 = 0xBE;
pub const VK_OEM_2: u32 = 0xBF;
pub const VK_OEM_3: u32 = 0xC0;
pub const VK_OEM_4: u32 = 0xDB;
pub const VK_OEM_5: u32 = 0xDC;
pub const VK_OEM_6: u32 = 0xDD;
pub const VK_OEM_7: u32 = 0xDE;
pub const VK_XBUTTON1: u32 = 0x05;
pub const VK_XBUTTON2: u32 = 0x06;
pub const VK_WHEELUP: u32 = 0x0A;
pub const VK_WHEELDOWN: u32 = 0x0B;
pub const VK_NUMPAD0: u32 = 0x60;
pub const VK_NUMPAD9: u32 = 0x69;
pub const VK_MULTIPLY: u32 = 0x6A;
pub const VK_ADD: u32 = 0x6B;
pub const VK_SUBTRACT: u32 = 0x6D;
pub const VK_DECIMAL: u32 = 0x6E;
pub const VK_DIVIDE: u32 = 0x6F;
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
const MOD_SHIFT_AMOUNT: u32 = 8;

const VK_MASK: u32 = 0xFF;
const MOD_MASK: u32 = 0x0F;

/// Extract the base virtual key code from a combined key+modifiers value
pub fn extract_vk(combined: u32) -> u32 {
    combined & VK_MASK
}

/// Extract the modifier flags (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN) from a combined value
pub fn extract_modifiers(combined: u32) -> u32 {
    (combined >> MOD_SHIFT_AMOUNT) & MOD_MASK
}

/// Pack a base virtual key code and modifier flags into a single combined value
pub fn combine_key(vk_code: u32, modifiers: u32) -> u32 {
    (vk_code & VK_MASK) | ((modifiers & MOD_MASK) << MOD_SHIFT_AMOUNT)
}

/// Whether a base virtual key code is a mouse button or wheel direction that must be routed
/// through the low-level mouse hook rather than RegisterHotKey (see hotkeys / mouse_hook)
pub fn is_mouse_hook_vk(vk_code: u32) -> bool {
    matches!(vk_code, VK_XBUTTON1 | VK_XBUTTON2 | VK_WHEELUP | VK_WHEELDOWN)
}

/// Virtual key code (plus any modifiers) as a human-readable string, e.g. "Ctrl+F9"
pub fn format_virtual_key(combined: u32) -> String {
    let mut out = String::new();
    let modifiers = extract_modifiers(combined);
    if modifiers & MOD_CONTROL != 0 {
        out.push_str("Ctrl+");
    }
    if modifiers & MOD_ALT != 0 {
        out.push_str("Alt+");
    }
    if modifiers & MOD_SHIFT != 0 {
        out.push_str("Shift+");
    }
    if modifiers & MOD_WIN != 0 {
        out.push_str("Win+");
    }

    let vk_code = extract_vk(combined);

    if (VK_F1..=VK_F24).contains(&vk_code) {
        let _ = write!(out, "F{}", vk_code - VK_F1 + 1);
        return out;
    }
    if (b'A' as u32..=b'Z' as u32).contains(&vk_code) || (b'0' as u32..=b'9' as u32).contains(&vk_code) {
        out.push(vk_code as u8 as char);
        return out;
    }
    if (VK_NUMPAD0..=VK_NUMPAD9).contains(&vk_code) {
        let _ = write!(out, "Numpad{}", vk_code - VK_NUMPAD0);
        return out;
    }

    let key_name = match vk_code {
        VK_TAB => "Tab",
        VK_PAUSE => "Pause",
        VK_CAPITAL => "CapsLock",
        VK_NUMLOCK => "NumLock",
        VK_SCROLL => "ScrollLock",
        VK_SPACE => "Space",
        VK_PRIOR => "PageUp",
        VK_NEXT => "PageDown",
        VK_END => "End",
        VK_HOME => "Home",
        VK_LEFT => "Left",
        VK_UP => "Up",
        VK_RIGHT => "Right",
        VK_DOWN => "Down",
        VK_INSERT => "Insert",
        VK_DELETE => "Delete",
        VK_MULTIPLY => "NumpadMultiply",
        VK_ADD => "NumpadAdd",
        VK_SUBTRACT => "NumpadSubtract",
        VK_DECIMAL => "NumpadDecimal",
        VK_DIVIDE => "NumpadDivide",
        VK_XBUTTON1 => "XButton1",
        VK_XBUTTON2 => "XButton2",
        VK_WHEELUP => "WheelUp",
        VK_WHEELDOWN => "WheelDown",
        VK_OEM_1 => ";",
        VK_OEM_PLUS => "=",
        VK_OEM_COMMA => ",",
        VK_OEM_MINUS => "-",
        VK_OEM_PERIOD => ".",
        VK_OEM_2 => "/",
        VK_OEM_3 => "`",
        VK_OEM_4 => "[",
        VK_OEM_5 => "\\",
        VK_OEM_6 => "]",
        VK_OEM_7 => "'",
        _ => {
            let _ = write!(out, "VK{vk_code:X}");
            return out;
        }
    };
    out.push_str(key_name);
    out
}

/// Parse a single modifier token ("Ctrl", "Control", "Alt", "Shift", "Win", "LWin", "RWin").
fn parse_modifier_token(token: &str) -> Option<u32> {
    let is = |s: &str| token.eq_ignore_ascii_case(s);
    if is("ctrl") || is("control") {
        Some(MOD_CONTROL)
    } else if is("alt") {
        Some(MOD_ALT)
    } else if is("shift") {
        Some(MOD_SHIFT)
    } else if is("win") || is("lwin") || is("rwin") {
        Some(MOD_WIN)
    } else {
        None
    }
}

/// Parse a single (non-combo) key token into its base virtual key code.
/// Supports: F1-F24, A-Z, 0-9, ; = , - . / ` [ \ ] ', Space, PageUp, PageDown, End, Home,
///           Left, Up, Right, Down, Insert, Delete, Numpad0-Numpad9, NumpadMultiply,
///           NumpadAdd, NumpadSubtract, NumpadDecimal, NumpadDivide
fn parse_base_key(key_str: &str) -> Option<u32> {
    let bytes = key_str.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    if bytes.len() == 1 {
        let ch = bytes[0].to_ascii_uppercase();
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Some(ch as u32);
        }
        // '+' itself is never a valid base key here since it's the modifier-combo delimiter; only '=' maps to VK_OEM_PLUS.
        let oem = match bytes[0] {
            b';' | b':' => Some(VK_OEM_1),
            b'=' => Some(VK_OEM_PLUS),
            b',' | b'<' => Some(VK_OEM_COMMA),
            b'-' | b'_' => Some(VK_OEM_MINUS),
            b'.' | b'>' => Some(VK_OEM_PERIOD),
            b'/' | b'?' => Some(VK_OEM_2),
            b'`' | b'~' => Some(VK_OEM_3),
            b'[' | b'{' => Some(VK_OEM_4),
            b'\\' | b'|' => Some(VK_OEM_5),
            b']' | b'}' => Some(VK_OEM_6),
            b'\'' | b'"' => Some(VK_OEM_7),
            _ => None,
        };
        if oem.is_some() {
            return oem;
        }
    }

    if bytes.len() >= 2 && (bytes[0] == b'F' || bytes[0] == b'f') {
        let num_str = &key_str[1..];
        match num_str.parse::<u32>() {
            Ok(num) if (1..=24).contains(&num) => return Some(VK_F1 + (num - 1)),
            Ok(_) => {}
            Err(err) => {
                SLOG.warn(format_args!("Failed to parse function key number '{num_str}': {err}"));
                return None;
            }
        }
    }

    const NAMED: [(&str, u32); 20] = [
        ("tab", VK_TAB),
        ("pause", VK_PAUSE),
        ("capslock", VK_CAPITAL),
        ("numlock", VK_NUMLOCK),
        ("scrolllock", VK_SCROLL),
        ("space", VK_SPACE),
        ("pageup", VK_PRIOR),
        ("pagedown", VK_NEXT),
        ("end", VK_END),
        ("home", VK_HOME),
        ("left", VK_LEFT),
        ("up", VK_UP),
        ("right", VK_RIGHT),
        ("down", VK_DOWN),
        ("insert", VK_INSERT),
        ("delete", VK_DELETE),
        ("xbutton1", VK_XBUTTON1),
        ("xbutton2", VK_XBUTTON2),
        ("wheelup", VK_WHEELUP),
        ("wheeldown", VK_WHEELDOWN),
    ];
    if let Some(&(_, vk)) = NAMED.iter().find(|(name, _)| key_str.eq_ignore_ascii_case(name)) {
        return Some(vk);
    }

    const NUMPAD: &str = "numpad";
    if bytes.len() > NUMPAD.len() && bytes[..NUMPAD.len()].eq_ignore_ascii_case(NUMPAD.as_bytes()) {
        let rest = &key_str[NUMPAD.len()..];
        if rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit() {
            return Some(VK_NUMPAD0 + (rest.as_bytes()[0] - b'0') as u32);
        }
        const NUMPAD_OPS: [(&str, u32); 5] = [
            ("multiply", VK_MULTIPLY),
            ("add", VK_ADD),
            ("subtract", VK_SUBTRACT),
            ("decimal", VK_DECIMAL),
            ("divide", VK_DIVIDE),
        ];
        if let Some(&(_, vk)) = NUMPAD_OPS.iter().find(|(name, _)| rest.eq_ignore_ascii_case(name)) {
            return Some(vk);
        }
    }

    SLOG.warn(format_args!("Unrecognized key format: '{key_str}'"));
    None
}

/// Parse virtual key (+ optional modifiers) from a string.
/// Accepts a plain key ("F9"), a modifier combo ("Ctrl+Alt+F9", "LWin+M"), or a raw
/// hex-encoded combined value ("0x0278") as previously written to disk.
/// Returns a combined value packing the base virtual key in the low byte and modifier
/// flags in bits 8-11 - see combine_key/extract_vk/extract_modifiers.
pub fn parse_virtual_key(key_str: &str) -> Option<u32> {
    let bytes = key_str.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    if bytes.len() >= 3 && bytes[0] == b'0' && (bytes[1] == b'x' || bytes[1] == b'X') {
        let hex_str = &key_str[2..];
        let combined = match u32::from_str_radix(hex_str, 16) {
            Ok(v) => v,
            Err(err) => {
                SLOG.warn(format_args!("Failed to parse hex key value '{hex_str}': {err}"));
                return None;
            }
        };
        let vk_code = combined & VK_MASK;
        return (0x01..=0xFE).contains(&vk_code).then_some(combined);
    }

    // Combo format: everything before the last '+' is modifiers, the final token is the key.
    if let Some(last_plus) = key_str.rfind('+') {
        let key_part = key_str[last_plus + 1..].trim_matches(' ');
        let mut modifiers = 0;
        for tok in key_str[..last_plus].split('+') {
            let mod_name = tok.trim_matches(' ');
            if mod_name.is_empty() {
                continue;
            }
            let Some(mod_bit) = parse_modifier_token(mod_name) else {
                SLOG.warn(format_args!("Unrecognized modifier: '{mod_name}'"));
                return None;
            };
            modifiers |= mod_bit;
        }

        let Some(vk_code) = parse_base_key(key_part) else {
            SLOG.warn(format_args!("Unrecognized key: '{key_part}'"));
            return None;
        };
        return Some(combine_key(vk_code, modifiers));
    }

    parse_base_key(key_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_combo_keys() {
        assert_eq!(parse_virtual_key("F9"), Some(0x78));
        assert_eq!(parse_virtual_key("a"), Some(b'A' as u32));
        assert_eq!(parse_virtual_key("Ctrl+Alt+F9"), Some(combine_key(0x78, MOD_CONTROL | MOD_ALT)));
        assert_eq!(parse_virtual_key("LWin + M"), Some(combine_key(b'M' as u32, MOD_WIN)));
        assert_eq!(parse_virtual_key("Numpad7"), Some(VK_NUMPAD0 + 7));
        assert_eq!(parse_virtual_key("numpadDivide"), Some(VK_DIVIDE));
        assert_eq!(parse_virtual_key("Hyper+F1"), None);
        assert_eq!(parse_virtual_key("F25"), None);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(parse_virtual_key("0x0278"), Some(0x0278));
        assert_eq!(parse_virtual_key("0x00"), None);
        assert_eq!(parse_virtual_key("0xFF"), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_virtual_key(combine_key(0x78, MOD_CONTROL | MOD_SHIFT)), "Ctrl+Shift+F9");
        assert_eq!(format_virtual_key(VK_OEM_5), "\\");
        assert_eq!(format_virtual_key(0xE7), "VKE7");
        for s in ["Alt+PageUp", "Win+Numpad3", "WheelDown", "Ctrl+'"] {
            assert_eq!(format_virtual_key(parse_virtual_key(s).unwrap()), s);
        }
    }
}
