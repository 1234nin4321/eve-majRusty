//! Bundled Cascadia fonts, registered process-privately so GDI lookups work on installs without Cascadia.

use eve_maj_core::log::Scope;
use windows_sys::Win32::Graphics::Gdi::AddFontMemResourceEx;

const SLOG: Scope = Scope::new("fonts");

struct BundledFont {
    name: &'static str,
    data: &'static [u8],
}

// SemiBold is a separate face because GDI won't embolden Cascadia Code's variable-font weights (see region_select).
const BUNDLED_FONTS: [BundledFont; 3] = [
    BundledFont { name: "Cascadia Code", data: include_bytes!("../../../../src/assets/fonts/CascadiaCode-Regular.ttf") },
    BundledFont { name: "Cascadia Code SemiBold", data: include_bytes!("../../../../src/assets/fonts/CascadiaCode-SemiBold.ttf") },
    BundledFont { name: "Cascadia Mono", data: include_bytes!("../../../../src/assets/fonts/CascadiaMono-Regular.ttf") },
];

/// Registers the bundled fonts for this process only, since not every Windows install ships Cascadia; fonts are released when the process exits.
pub fn load_bundled() {
    for font in &BUNDLED_FONTS {
        let face_count = 0u32;
        let handle = unsafe { AddFontMemResourceEx(font.data.as_ptr().cast(), font.data.len() as u32, std::ptr::null(), &face_count) };
        if handle.is_null() {
            SLOG.warn(format_args!("Failed to load bundled font {}", font.name));
        }
    }
}
