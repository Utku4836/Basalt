//! Font magnification: independent of display DPI, 50–200% in steps of ten.
pub fn normalize_percent(value: i32) -> i32 {
    let bounded = value.clamp(50, 200);
    ((f64::from(bounded) / 10.0).round() as i32 * 10).clamp(50, 200)
}

/// User-requested responsive terminal typography, independent for each pane.
/// Preserve the chosen zoom, with a readable 12px floor at the default 14px.
/// Prefer 60 columns and 16 rows; narrower panes wrap instead of tiny text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneTypography {
    pub font_size: f64,
    pub cell_width: f64,
    pub cell_height: f64,
}
impl PaneTypography {
    pub fn fitted(self, body_width: f64, body_height: f64) -> Self {
        if ![
            self.font_size,
            self.cell_width,
            self.cell_height,
            body_width,
            body_height,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
        {
            return self;
        }
        let ratio = (body_width / (60.0 * self.cell_width))
            .min(body_height / (16.0 * self.cell_height))
            .clamp(12.0 / 14.0, 1.0);
        // Half-pixel steps at the base 14px size keep shaping caches stable during subpixel drags.
        let ratio = (ratio * 28.0).floor() / 28.0;
        Self {
            font_size: self.font_size * ratio,
            cell_width: self.cell_width * ratio,
            cell_height: self.cell_height * ratio,
        }
    }
}

/// The interface and terminal monospace face: JetBrains Mono Nerd Font (with its icon
/// and powerline glyphs, single-cell "Mono" variant) when installed, else Cascadia Mono,
/// which ships with Windows 11.
pub fn preferred_mono() -> &'static str {
    FAMILY
}

/// The terminal and interface font, embedded in the program: JetBrains Mono with the
/// Nerd Font symbols, in the faces the terminal draws (see `gpu/glyphs.rs`).
pub const FAMILY: &str = "JetBrainsMono Nerd Font Mono";
const FACES: [&[u8]; 4] = [
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFontMono-Medium.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFontMono-MediumItalic.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFontMono-Bold.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFontMono-BoldItalic.ttf"),
];

/// Make the embedded font available to the terminal grid and the interface, whether
/// or not it is installed. Call on the interface's thread before the first window.
pub fn embed_font() {
    for face in FACES {
        #[cfg(windows)]
        crossfont::directwrite::add_font(face);
        let _ = i_slint_renderer_skia::register_font_from_memory(face);
    }
}
