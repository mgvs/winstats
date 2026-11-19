//! Visual styles: presets for how the strip and the popups look, chosen in the Style menu.
//!
//! - Windows: matches the Windows 11 taskbar and its flyouts — the system accent colour, the
//!   Fluent text contrast (secondary text at 78 %), a dark tinted backdrop like the taskbar's,
//!   quiet card fills.
//! - Vivid: saturated per-metric colours on a deep, nearly solid background; strong fills.
//! - Mono: text-coloured charts with one accent; the calmest look, closest to the tray icons.
//! - Classic: the original winstats look (configured accent, the popup opacity setting as is).

use crate::canvas::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Windows,
    Vivid,
    Mono,
    Classic,
}

/// Menu order: (config value, menu string key).
pub const STYLES: [(&str, &str); 4] = [("windows", "menu.style_windows"), ("vivid", "menu.style_vivid"), ("mono", "menu.style_mono"), ("classic", "menu.style_classic")];

impl Style {
    pub fn from_config(s: &str) -> Self {
        match s {
            "vivid" => Style::Vivid,
            "mono" => Style::Mono,
            "classic" => Style::Classic,
            _ => Style::Windows,
        }
    }

    /// Alpha of secondary text (labels, captions) over the theme's text colour.
    pub fn dim_alpha(self, light: bool) -> u8 {
        match (self, light) {
            // Fluent TextFillColorSecondary (#FFFFFF C5 on dark); a touch above its #000000 9E on light
            (Style::Windows | Style::Mono, false) => 197,
            (Style::Windows | Style::Mono, true) => 165,
            (Style::Vivid, false) => 205,
            (Style::Vivid, true) => 170,
            (Style::Classic, false) => 150,
            (Style::Classic, true) => 160,
        }
    }

    /// Popup background drawn over the system material: (colour, opacity). None = the
    /// configured popup opacity decides (Classic).
    pub fn popup_fill(self, light: bool) -> Option<(Color, f32)> {
        match (self, light) {
            // the taskbar's tinted dark / light (Mica-like: the backdrop only shimmers through)
            (Style::Windows, false) => Some((Color::rgb(0x1C, 0x1C, 0x1C), 0.84)),
            (Style::Windows, true) => Some((Color::rgb(0xEE, 0xEE, 0xEE), 0.84)),
            (Style::Vivid, false) => Some((Color::rgb(0x12, 0x12, 0x14), 0.95)),
            (Style::Vivid, true) => Some((Color::rgb(0xFA, 0xFA, 0xFA), 0.95)),
            (Style::Mono, false) => Some((Color::rgb(0x20, 0x20, 0x20), 0.78)),
            (Style::Mono, true) => Some((Color::rgb(0xF3, 0xF3, 0xF3), 0.78)),
            (Style::Classic, _) => None,
        }
    }

    /// Card (stat cards, flow nodes, chart boxes) fill and outline. None as the outline: the
    /// theme's secondary text colour, faint (the Classic look).
    pub fn card(self, light: bool) -> (Color, Option<Color>) {
        let a = |v: f32| (v * 255.0).round() as u8;
        match (self, light) {
            // Fluent CardBackgroundFillColorDefault / CardStrokeColorDefault; light cards are
            // whiter than the background, not darker
            (Style::Windows | Style::Mono, false) => (Color::rgba(255, 255, 255, a(0.055)), Some(Color::rgba(255, 255, 255, a(0.09)))),
            (Style::Windows | Style::Mono, true) => (Color::rgba(255, 255, 255, a(0.70)), Some(Color::rgba(0, 0, 0, a(0.058)))),
            (Style::Vivid, false) => (Color::rgba(255, 255, 255, a(0.08)), Some(Color::rgba(255, 255, 255, a(0.12)))),
            (Style::Vivid, true) => (Color::rgba(255, 255, 255, a(0.9)), Some(Color::rgba(0, 0, 0, a(0.09)))),
            (Style::Classic, false) => (Color::rgba(255, 255, 255, 14), None),
            (Style::Classic, true) => (Color::rgba(0, 0, 0, 10), None),
        }
    }

    /// Opacity of a chart's area fill at the line.
    pub fn chart_fill(self) -> f32 {
        match self {
            Style::Windows => 0.38,
            Style::Vivid => 0.55,
            Style::Mono => 0.22,
            Style::Classic => 0.3,
        }
    }

    /// Per-module chart colours of this style where it sets them (cpu, gpu, mem, disk, net,
    /// battery); None = the accent / the configured colours.
    pub fn module_colors(self, light: bool) -> Option<[Color; 6]> {
        match self {
            Style::Vivid if light => Some([Color::rgb(0x00, 0x6C, 0xE0), Color::rgb(0x8E, 0x3C, 0xD8), Color::rgb(0x10, 0x89, 0x3E), Color::rgb(0xC2, 0x5E, 0x00), Color::rgb(0x00, 0x83, 0x9E), Color::rgb(0x10, 0x89, 0x3E)]),
            Style::Vivid => Some([Color::rgb(0x0A, 0x84, 0xFF), Color::rgb(0xBF, 0x5A, 0xF2), Color::rgb(0x30, 0xD1, 0x58), Color::rgb(0xFF, 0x9F, 0x0A), Color::rgb(0x64, 0xD2, 0xFF), Color::rgb(0x30, 0xD1, 0x58)]),
            _ => None,
        }
    }

    /// Accent from the system (Windows, Mono) or from the configuration (Vivid, Classic).
    pub fn system_accent(self) -> bool {
        matches!(self, Style::Windows | Style::Mono)
    }

    /// Charts in the text colour (Mono).
    pub fn mono(self) -> bool {
        self == Style::Mono
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_fall_back_to_windows() {
        assert_eq!(Style::from_config(""), Style::Windows);
        assert_eq!(Style::from_config("nonsense"), Style::Windows);
        for (v, _) in STYLES {
            assert_eq!(Style::from_config(v), STYLES.iter().map(|(v, _)| Style::from_config(v)).find(|s| *s == Style::from_config(v)).unwrap());
        }
    }

    #[test]
    fn windows_text_is_less_washed_out_than_classic() {
        for light in [false, true] {
            assert!(Style::Windows.dim_alpha(light) >= Style::Classic.dim_alpha(light));
        }
    }
}
