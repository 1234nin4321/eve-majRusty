//! Notification sounds and text-to-speech on Linux, through whatever player/speech tool is installed (the Windows build
//! uses Media Foundation/winmm and SAPI). Both run as detached child processes, so nothing blocks the UI loop.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use eve_maj_core::log::Scope;

const SLOG: Scope = Scope::new("alerts");

static NO_PLAYER_WARNED: AtomicBool = AtomicBool::new(false);
static NO_TTS_WARNED: AtomicBool = AtomicBool::new(false);
static WINDOWS_PATH_WARNED: AtomicBool = AtomicBool::new(false);

fn installed(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
}

fn spawn_quiet(cmd: &mut Command) -> bool {
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        // Reaped on a helper thread so finished players don't linger as zombies.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(e) => {
            SLOG.warn(format_args!("Couldn't start {:?}: {e}", cmd.get_program()));
            false
        }
    }
}

/// Plays a WAV/MP3 file at `volume_percent` (0-100).
pub fn play_sound(path: &str, volume_percent: u8) {
    // Profiles carried over from Windows point at C:\... files that don't exist here.
    if path.len() > 2 && path.as_bytes()[1] == b':' {
        if !WINDOWS_PATH_WARNED.swap(true, Ordering::Relaxed) {
            SLOG.warn(format_args!("Sound path {path} is a Windows path; pick a Linux file in the profile to hear it"));
        }
        return;
    }
    if !std::path::Path::new(path).is_file() {
        SLOG.warn(format_args!("Sound file not found: {path}"));
        return;
    }
    let volume = volume_percent.min(100);
    let started = if installed("ffplay") {
        spawn_quiet(Command::new("ffplay").args(["-nodisp", "-autoexit", "-loglevel", "quiet", "-volume", &volume.to_string(), path]))
    } else if installed("mpv") {
        spawn_quiet(Command::new("mpv").args(["--no-video", "--really-quiet", &format!("--volume={volume}"), path]))
    } else if installed("pw-play") {
        spawn_quiet(Command::new("pw-play").args([&format!("--volume={:.2}", volume as f32 / 100.0), path]))
    } else if installed("paplay") {
        spawn_quiet(Command::new("paplay").args([&format!("--volume={}", volume as u32 * 65536 / 100), path]))
    } else {
        false
    };
    if !started && !NO_PLAYER_WARNED.swap(true, Ordering::Relaxed) {
        SLOG.warn(format_args!("No audio player found for notification sounds (install ffmpeg, mpv or pipewire)"));
    }
}

/// Speaks `text`. `volume` is 0-100 and `rate` -10..10, as in the profile (SAPI's ranges).
pub fn speak(text: &str, volume: u8, rate: i8) {
    let started = if installed("spd-say") {
        // speech-dispatcher takes -100..100 for both.
        let vol = (volume.min(100) as i32 * 2 - 100).to_string();
        let rate = (rate.clamp(-10, 10) as i32 * 10).to_string();
        spawn_quiet(Command::new("spd-say").args(["-i", &vol, "-r", &rate, "--", text]))
    } else if let Some(espeak) = ["espeak-ng", "espeak"].into_iter().find(|t| installed(t)) {
        // espeak: amplitude 0-200 (100 normal), speed in words per minute (175 normal).
        let amp = (volume.min(100) as u32 * 2).to_string();
        let wpm = (175 + rate.clamp(-10, 10) as i32 * 10).to_string();
        spawn_quiet(Command::new(espeak).args(["-a", &amp, "-s", &wpm, "--", text]))
    } else {
        false
    };
    if !started && !NO_TTS_WARNED.swap(true, Ordering::Relaxed) {
        SLOG.warn(format_args!("No text-to-speech tool found (install speech-dispatcher or espeak-ng); spoken alerts are off"));
    }
}
