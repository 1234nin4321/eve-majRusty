//! Windows-side subsystems shared by the eve-maj-preview and config binaries.
//! Each module mirrors the Zig file of the same name under ../../src.

#[cfg(windows)]
pub mod protocol;
pub mod sound;
pub mod tts;
