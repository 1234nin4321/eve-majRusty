//! Field-level (de)serializers for the profile JSON's string-encoded values.

use serde::de::{Error as _, IntoDeserializer};
use serde::{Deserialize, Deserializer, Serializer};

use super::ConfigError;
use crate::virtual_keys;

/// Accepts 0xRRGGBB, 0xAARRGGBB, #RRGGBB, or RRGGBB.
pub fn parse_hex_color(s: &str) -> Result<u32, ConfigError> {
    if s.len() < 3 {
        return Err(ConfigError::InvalidColorFormat);
    }
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix('#'))
        .unwrap_or(s);
    u32::from_str_radix(hex, 16).map_err(|_| ConfigError::InvalidColorFormat)
}

pub fn format_argb(value: u32) -> String {
    format!("0x{value:08X}")
}

/// Unit-variant enum from its JSON tag name, e.g. "TopLeft" -> TextPosition::TopLeft.
pub fn enum_from_str<'de, T: Deserialize<'de>>(s: &'de str) -> Option<T> {
    T::deserialize(IntoDeserializer::<serde::de::value::Error>::into_deserializer(s)).ok()
}

/// ARGB color, serialized as an 8-digit hex string, e.g. "0xFF606060".
pub mod argb {
    use super::*;

    pub fn serialize<S: Serializer>(value: &u32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_argb(*value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
        let s = String::deserialize(d)?;
        parse_hex_color(&s).map_err(D::Error::custom)
    }
}

pub mod argb_opt {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<u32>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => s.serialize_str(&format_argb(*v)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => parse_hex_color(&s).map(Some).map_err(D::Error::custom),
            None => Ok(None),
        }
    }
}

/// Virtual-key hotkey code, serialized as an (at least) 2-digit hex string, e.g. "0x1B"; parsing goes through parse_virtual_key, which also accepts combo strings like "Ctrl+F9" for hand-edited profiles.
pub mod vk_opt {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<u32>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => s.serialize_str(&format!("0x{v:02X}")),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => virtual_keys::parse_virtual_key(&s)
                .map(Some)
                .ok_or_else(|| D::Error::custom(format!("unrecognized hotkey '{s}'"))),
            None => Ok(None),
        }
    }
}

/// String-to-string map serialized as a JSON object (used for characterIdMap); a non-object value reads as empty, as in the Zig build, but a non-string value is an error.
pub mod string_map_lenient {
    use std::collections::BTreeMap;

    use super::*;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
        let serde_json::Value::Object(obj) = serde_json::Value::deserialize(d)? else {
            return Ok(BTreeMap::new());
        };
        obj.into_iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => Ok((k, s)),
                _ => Err(D::Error::custom(format!("characterIdMap value for '{k}' is not a string"))),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_formats() {
        assert_eq!(parse_hex_color("0xFF606060").unwrap(), 0xFF606060);
        assert_eq!(parse_hex_color("#123456").unwrap(), 0x123456);
        assert_eq!(parse_hex_color("abcdef").unwrap(), 0xABCDEF);
        assert!(parse_hex_color("0x").is_err());
        assert!(parse_hex_color("zzzz").is_err());
        assert_eq!(format_argb(0xFF), "0x000000FF");
    }
}
