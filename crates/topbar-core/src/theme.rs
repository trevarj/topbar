//! Resolved theme colors shared by CSS and custom drawing.

use crate::Config;

/// An opaque 8-bit-per-channel RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Construct a color from its channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Render as a lowercase `#rrggbb` string.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Render as a CSS `rgba()` expression with the given alpha.
    pub fn to_rgba(self, alpha: f64) -> String {
        format!("rgba({}, {}, {}, {})", self.r, self.g, self.b, alpha)
    }

    /// WCAG relative luminance in the 0.0..=1.0 range.
    pub fn relative_luminance(self) -> f64 {
        fn channel(value: u8) -> f64 {
            let v = f64::from(value) / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// Black or white, whichever reads best on this background.
    pub fn contrasting_foreground(self) -> Self {
        if self.relative_luminance() > 0.179 {
            Self::new(0, 0, 0)
        } else {
            Self::new(255, 255, 255)
        }
    }
}

/// Parse a `#rgb` or `#rrggbb` hex color (the leading `#` is optional).
///
/// Returns `None` for anything else, which is what config validation uses to
/// reject bad color values.
///
/// # Example
/// ```
/// use topbar_core::theme::{Rgb, parse_hex_color};
///
/// assert_eq!(parse_hex_color("#fff"), Some(Rgb::new(255, 255, 255)));
/// assert_eq!(parse_hex_color("#70B49B"), Some(Rgb::new(0x70, 0xB4, 0x9B)));
/// assert_eq!(parse_hex_color("nope"), None);
/// ```
pub fn parse_hex_color(color: &str) -> Option<Rgb> {
    let color = color.trim().trim_start_matches('#');

    // Expand shorthand (e.g. "fff" -> "ffffff").
    let color = if color.len() == 3 {
        color.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        color.to_string()
    };

    if color.len() != 6 || !color.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    let r = u8::from_str_radix(&color[0..2], 16).ok()?;
    let g = u8::from_str_radix(&color[2..4], 16).ok()?;
    let b = u8::from_str_radix(&color[4..6], 16).ok()?;

    Some(Rgb::new(r, g, b))
}

/// Whether a string is an acceptable hex color for config validation.
pub fn is_valid_hex_color(color: &str) -> bool {
    color.starts_with('#') && parse_hex_color(color).is_some()
}

/// One resolved palette, computed when configuration is applied or a widget is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Base panel/popover background.
    pub background: Rgb,
    /// Elevated neutral surface.
    pub surface: Rgb,
    /// Text and neutral fills.
    pub foreground: Rgb,
    /// Effective bar background after its explicit override.
    pub bar: Rgb,
    /// Effective widget background after its explicit override.
    pub widget: Rgb,
    /// Accent, or foreground when configured as `none`.
    pub accent: Rgb,
    /// Success tint.
    pub success: Rgb,
    /// Warning tint.
    pub warning: Rgb,
    /// Urgency/error tint.
    pub urgent: Rgb,
}

impl Palette {
    /// Resolve validated config; deterministic defaults also support hand-built configs.
    pub fn from_config(config: &Config) -> Self {
        let (background, surface, foreground) = if config.theme.mode == "light" {
            (
                Rgb::new(255, 255, 255),
                Rgb::new(241, 241, 243),
                Rgb::new(0, 0, 0),
            )
        } else {
            (
                Rgb::new(0, 0, 0),
                Rgb::new(30, 30, 34),
                Rgb::new(255, 255, 255),
            )
        };
        let color =
            |value: Option<&str>, fallback| value.and_then(parse_hex_color).unwrap_or(fallback);
        let palette = &config.theme.palette;
        let background = color(palette.background.as_deref(), background);
        let surface = color(palette.surface.as_deref(), surface);
        let foreground = color(palette.foreground.as_deref(), foreground);
        Self {
            background,
            surface,
            foreground,
            bar: color(config.bar.background_color.as_deref(), background),
            widget: color(config.widgets.background_color.as_deref(), surface),
            accent: if config.theme.accent == "none" {
                foreground
            } else {
                color(Some(&config.theme.accent), Rgb::new(0x35, 0x84, 0xe4))
            },
            success: color(
                Some(&config.theme.states.success),
                Rgb::new(0x4a, 0x7a, 0x4a),
            ),
            warning: color(
                Some(&config.theme.states.warning),
                Rgb::new(0xe5, 0xc0, 0x7b),
            ),
            urgent: color(
                Some(&config.theme.states.urgent),
                Rgb::new(0xff, 0x6b, 0x6b),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_overrides_monochrome_and_contrast_resolve_together() {
        let mut config = Config::parse("[theme]\nmode = \"light\"\naccent = \"none\"")
            .unwrap()
            .0;
        let light = Palette::from_config(&config);
        assert_eq!(light.bar, Rgb::new(255, 255, 255));
        assert_eq!(light.widget, Rgb::new(241, 241, 243));
        assert_eq!(light.accent, Rgb::new(0, 0, 0));
        config.theme.palette.background = Some("#abc".into());
        config.theme.palette.surface = Some("#def".into());
        config.theme.palette.foreground = Some("#123".into());
        let palette = Palette::from_config(&config);
        assert_eq!(palette.bar, Rgb::new(170, 187, 204));
        assert_eq!(palette.widget, Rgb::new(221, 238, 255));
        assert_eq!(palette.accent, Rgb::new(17, 34, 51));
        config.bar.background_color = Some("#456".into());
        config.widgets.background_color = Some("#789".into());
        let overrides = Palette::from_config(&config);
        assert_eq!(overrides.bar, Rgb::new(68, 85, 102));
        assert_eq!(overrides.widget, Rgb::new(119, 136, 153));
        for (background, text) in [
            (Rgb::new(255, 107, 107), Rgb::new(0, 0, 0)),
            (Rgb::new(156, 50, 65), Rgb::new(255, 255, 255)),
        ] {
            assert_eq!(background.contrasting_foreground(), text);
        }
    }

    #[test]
    fn parses_shorthand_and_full_hex() {
        assert_eq!(parse_hex_color("#000"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(parse_hex_color("000000"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(
            parse_hex_color("  #70b49b  "),
            Some(Rgb::new(112, 180, 155))
        );
    }

    #[test]
    fn rejects_malformed_hex() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#12345"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color("rgb(1,2,3)"), None);
    }

    #[test]
    fn hex_color_validation_requires_hash() {
        assert!(is_valid_hex_color("#3584e4"));
        assert!(!is_valid_hex_color("3584e4"));
        assert!(!is_valid_hex_color("accent"));
    }

    #[test]
    fn renders_css_forms() {
        let color = Rgb::new(0x70, 0xB4, 0x9B);
        assert_eq!(color.to_hex(), "#70b49b");
        assert_eq!(color.to_rgba(0.5), "rgba(112, 180, 155, 0.5)");
    }

    #[test]
    fn luminance_orders_black_below_white() {
        assert!(
            Rgb::new(0, 0, 0).relative_luminance() < Rgb::new(255, 255, 255).relative_luminance()
        );
    }
}
