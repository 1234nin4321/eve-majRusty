//! Windows-side subsystems shared by the eve-maj-preview and config binaries.
//! Each module mirrors the Zig file of the same name under ../../src.

#[cfg(windows)]
pub mod protocol;
pub mod sound;
pub mod tts;
pub mod update_stage;
#[cfg(windows)]
pub mod updater;
pub mod paste_upload;
