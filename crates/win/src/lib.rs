//! Thin, app-agnostic helpers over the Win32 API: the Rust counterpart of the helper functions in win32.zig.
//! Raw bindings come from `windows-sys` (re-exported as `sys`); nothing here knows about EVE or the config.
//!
//! The Zig build called the ANSI (`...A`) entry points; these use the wide (`...W`) ones and convert to/from
//! UTF-8, so non-ASCII window titles and paths survive instead of being squeezed through the ANSI code page.

#![cfg(windows)]
// windows-sys handles (HWND, HMONITOR, ...) are raw pointers that are only ever passed back to Win32, never dereferenced here.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub use windows_sys as sys;

pub mod geometry;
pub mod window;
pub mod shell;
pub mod input;
pub mod time;

/// Encodes `s` as a NUL-terminated UTF-16 string for wide Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a NUL-terminated UTF-16 pointer; `ptr` must be valid up to its terminator.
///
/// # Safety
/// `ptr` must be null or point to a NUL-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)))
}
