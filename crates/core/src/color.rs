// Matrix constants are the published OKLab ones, kept verbatim even where f32 rounds them.
#![allow(clippy::excessive_precision)]

use std::sync::OnceLock;

use crate::log::Scope;

#[allow(dead_code)]
const SLOG: Scope = Scope::new("color");

#[derive(Clone, Copy, Debug)]
struct Oklab {
    l: f32,
    a: f32,
    b: f32,
}

fn srgb_byte_to_linear(byte: u32) -> f32 {
    let c = (byte & 0xFF) as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb_byte(linear: f32) -> u32 {
    let c = linear.clamp(0.0, 1.0);
    let encoded = if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (encoded * 255.0).round() as u32
}

fn rgb_to_oklab(rgb: u32) -> Oklab {
    let r = srgb_byte_to_linear(rgb >> 16);
    let g = srgb_byte_to_linear(rgb >> 8);
    let b = srgb_byte_to_linear(rgb);

    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();

    Oklab {
        l: 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        a: 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        b: 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    }
}

/// None when the color falls outside the sRGB gamut.
fn oklch_to_rgb(lightness: f32, chroma: f32, hue_degrees: f32) -> Option<u32> {
    let radians = hue_degrees.to_radians();
    let a = chroma * radians.cos();
    let b = chroma * radians.sin();

    let l_ = lightness + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = lightness - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = lightness - 0.0894841775 * a - 1.2914855480 * b;
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s;
    let g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s;
    let bl = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s;

    const TOLERANCE: f32 = 0.002;
    if [r, g, bl].iter().any(|&c| !(-TOLERANCE..=1.0 + TOLERANCE).contains(&c)) {
        return None;
    }
    Some((linear_to_srgb_byte(r) << 16) | (linear_to_srgb_byte(g) << 8) | linear_to_srgb_byte(bl))
}

fn oklab_distance(x: Oklab, y: Oklab) -> f32 {
    let (dl, da, db) = (x.l - y.l, x.a - y.a, x.b - y.b);
    (dl * dl + da * da + db * db).sqrt()
}

const DISTINCT_HUE_STEPS: usize = 36;
const DISTINCT_LIGHTNESS: [f32; 3] = [0.70, 0.80, 0.90];
const DISTINCT_CHROMA: [f32; 4] = [0.10, 0.15, 0.20, 0.26];
const MAX_DISTINCT_TAKEN: usize = 128;

#[derive(Clone, Copy)]
struct Candidate {
    rgb: u32,
    lab: Oklab,
}

fn distinct_candidates() -> &'static [Candidate] {
    static TABLE: OnceLock<Vec<Candidate>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Vec::with_capacity(DISTINCT_HUE_STEPS * DISTINCT_LIGHTNESS.len() * DISTINCT_CHROMA.len());
        for hue_step in 0..DISTINCT_HUE_STEPS {
            let hue = hue_step as f32 * (360.0 / DISTINCT_HUE_STEPS as f32);
            for &lightness in &DISTINCT_LIGHTNESS {
                for &chroma in &DISTINCT_CHROMA {
                    if let Some(rgb) = oklch_to_rgb(lightness, chroma, hue) {
                        table.push(Candidate { rgb, lab: rgb_to_oklab(rgb) });
                    }
                }
            }
        }
        table
    })
}

/// FNV-1a; only picks the palette starting point, so any stable hash will do.
// The Zig build used std.hash.Wyhash here, so a brand-new name may get a different (equally distinct)
// first color than it did there. Colors already persisted in the profile are loaded as-is.
fn seed_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Picks the palette color farthest (in OKLab) from every color in `taken`; with nothing taken, `seed_string` picks the starting point and breaks ties, so the result is deterministic. Returns 0xRRGGBB.
pub fn pick_distinct_color(seed_string: &str, taken: &[u32]) -> u32 {
    let candidates = distinct_candidates();
    let count = candidates.len();

    let taken_labs: Vec<Oklab> = taken.iter().take(MAX_DISTINCT_TAKEN).map(|&rgb| rgb_to_oklab(rgb)).collect();

    let start = (seed_hash(seed_string) % count as u64) as usize;
    let mut best_rgb = candidates[start].rgb;
    let mut best_distance = -1.0f32;
    for offset in 0..count {
        let candidate = candidates[(start + offset) % count];
        let nearest = taken_labs
            .iter()
            .map(|&lab| oklab_distance(candidate.lab, lab))
            .fold(f32::INFINITY, f32::min);
        if nearest > best_distance {
            best_distance = nearest;
            best_rgb = candidate.rgb;
        }
    }
    best_rgb
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoColorEntry {
    pub name: String,
    pub color: u32,
}

/// Names mapped to colors that stay fixed once assigned, least recently seen first; persistence is the owner's job (see `dirty`).
#[derive(Debug, Clone, Default)]
pub struct AutoColors {
    pub entries: Vec<AutoColorEntry>,
    /// Set whenever an entry is added or evicted; the owner clears it after persisting.
    pub dirty: bool,
}

impl AutoColors {
    pub const MAX_ENTRIES: usize = 64;
    pub const MAX_AVOIDED: usize = 32;

    /// Adds an already-assigned color (e.g. from persisted state) without marking the store dirty.
    pub fn put(&mut self, name: &str, rgb: u32) {
        self.entries.push(AutoColorEntry { name: name.to_owned(), color: rgb });
    }

    /// The name's existing color, or a new one: the palette color farthest from `avoid` (at most `MAX_AVOIDED` used) and every entry already assigned.
    pub fn color_for(&mut self, name: &str, avoid: &[u32]) -> u32 {
        if let Some(i) = self.entries.iter().position(|e| e.name.eq_ignore_ascii_case(name)) {
            let seen = self.entries.remove(i);
            let color = seen.color;
            self.entries.push(seen);
            return color;
        }

        let taken: Vec<u32> = avoid
            .iter()
            .take(Self::MAX_AVOIDED)
            .copied()
            .chain(self.entries.iter().map(|e| e.color))
            .collect();

        let picked = 0xFF00_0000 | pick_distinct_color(name, &taken);
        self.record(name, picked);
        picked
    }

    fn record(&mut self, name: &str, rgb: u32) {
        while self.entries.len() >= Self::MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.dirty = true;
        self.put(name, rgb);
    }
}

pub fn with_alpha(rgb: u32, alpha: u8) -> u32 {
    ((alpha as u32) << 24) | (rgb & 0x00FF_FFFF)
}

/// Mixes each channel `percent`% of the way toward white, keeping alpha.
pub fn lighten(color: u32, percent: u32) -> u32 {
    let mut out = color & 0xFF00_0000;
    for shift in [16, 8, 0] {
        let channel = (color >> shift) & 0xFF;
        out |= (channel + (255 - channel) * percent / 100) << shift;
    }
    out
}

/// Text color that stays readable on `background`: dark ink on light colors, light ink on dark ones.
pub fn ink_for(background: u32) -> u32 {
    let r = (background >> 16) & 0xFF;
    let g = (background >> 8) & 0xFF;
    let b = background & 0xFF;
    if 299 * r + 587 * g + 114 * b > 550 * 255 { 0xFF1A1408 } else { 0xFFF5F0E6 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_nonempty_and_in_gamut() {
        let c = distinct_candidates();
        assert!(c.len() > 100, "only {} candidates", c.len());
        assert!(c.iter().all(|c| c.rgb <= 0xFF_FFFF));
    }

    #[test]
    fn oklab_round_trip_white_is_lightness_one() {
        let lab = rgb_to_oklab(0xFFFFFF);
        assert!((lab.l - 1.0).abs() < 1e-3 && lab.a.abs() < 1e-3 && lab.b.abs() < 1e-3);
    }

    #[test]
    fn pick_is_deterministic_and_avoids_taken() {
        let a = pick_distinct_color("Jita Trader", &[]);
        assert_eq!(a, pick_distinct_color("Jita Trader", &[]));
        let b = pick_distinct_color("Jita Trader", &[a]);
        assert_ne!(a, b);
    }

    #[test]
    fn auto_colors_reuse_case_insensitive_and_evict_lru() {
        let mut store = AutoColors::default();
        let first = store.color_for("Alpha", &[]);
        assert!(store.dirty);
        store.dirty = false;
        assert_eq!(store.color_for("ALPHA", &[]), first);
        assert!(!store.dirty);

        for i in 0..AutoColors::MAX_ENTRIES {
            store.color_for(&format!("n{i}"), &[]);
        }
        assert_eq!(store.entries.len(), AutoColors::MAX_ENTRIES);
        assert!(!store.entries.iter().any(|e| e.name == "Alpha"));
    }

    #[test]
    fn lighten_alpha_ink() {
        assert_eq!(lighten(0x80000000, 100), 0x80FFFFFF);
        assert_eq!(lighten(0xFF102030, 0), 0xFF102030);
        assert_eq!(with_alpha(0x12345678, 0xAB), 0xAB345678);
        assert_eq!(ink_for(0xFFFFFF), 0xFF1A1408);
        assert_eq!(ink_for(0x000000), 0xFFF5F0E6);
    }
}
