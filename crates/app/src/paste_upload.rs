//! Uploads clipboard text to a paste site and opens the result (the URL hotkey's uploadClipboard mode).

use std::sync::atomic::{AtomicBool, Ordering};

use eve_maj_core::log::Scope;
use eve_maj_core::paste_upload::{build_upload_body, is_allowed_upload_url, post_and_follow_redirect};
use eve_maj_win::shell;

const SLOG: Scope = Scope::new("paste_upload");

/// Longest URL the fallback open accepts, matching the Zig build's 1024-byte null-terminated buffer.
const MAX_FALLBACK_URL_LEN: usize = 1023;

/// Guards against a double-press spawning two overlapping uploads (and two browser tabs); cleared once the background thread finishes.
static UPLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Clears UPLOAD_IN_FLIGHT when dropped, so the flag is released however the upload thread ends.
struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        UPLOAD_IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn open_fallback(url: &str) {
    if url.len() > MAX_FALLBACK_URL_LEN {
        SLOG.warn(format_args!("URL too long to open: {url}"));
        return;
    }
    if !shell::shell_open(url, None) {
        SLOG.err(format_args!("Failed to open URL: {url}"));
    }
}

/// Uploads clipboard text to url via aDashboard's paste-intake form shape and opens the resulting page, falling back to opening the plain url if there's no clipboard text or the upload fails.
fn upload_clipboard_and_open(url: &str) {
    if !is_allowed_upload_url(url) {
        SLOG.warn(format_args!("Not uploading clipboard to {url}: only https://adashboard.info is allowed"));
        open_fallback(url);
        return;
    }
    let Some(clipboard_text) = shell::clipboard_text() else {
        SLOG.warn(format_args!("Clipboard has no text; opening {url} without uploading"));
        open_fallback(url);
        return;
    };

    let body = build_upload_body(clipboard_text.as_bytes());

    let final_url = match post_and_follow_redirect(url, &body) {
        Ok(u) => u,
        Err(err) => {
            SLOG.err(format_args!("Clipboard upload to {url} failed: {err}"));
            open_fallback(url);
            return;
        }
    };

    SLOG.info(format_args!("Uploaded clipboard, opening {final_url}"));
    if !shell::set_clipboard_text(&final_url) {
        SLOG.warn(format_args!("Failed to copy paste URL to clipboard: {final_url}"));
    }
    if !shell::shell_open(&final_url, None) {
        SLOG.err(format_args!("Failed to open uploaded paste URL: {final_url}"));
    }
}

/// Runs the upload+open on a background thread so the HTTP round-trip doesn't block the main message loop (hotkey handling, thumbnail rendering); a no-op if one is already running.
pub fn upload_clipboard_and_open_async(url: &str) {
    if UPLOAD_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        SLOG.info(format_args!("Clipboard upload already in progress; ignoring"));
        return;
    }

    let url = url.to_owned();
    let spawned = std::thread::Builder::new().name("paste_upload".to_owned()).spawn(move || {
        let _guard = InFlightGuard;
        upload_clipboard_and_open(&url);
    });
    if let Err(err) = spawned {
        SLOG.warn(format_args!("Failed to start clipboard upload thread: {err}"));
        UPLOAD_IN_FLIGHT.store(false, Ordering::Release);
    }
}
