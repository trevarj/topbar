//! Getting a tray icon onto the bar, and making it legible once it is there.
//!
//! Applications hand the tray whatever they happen to have. Some send a
//! Freedesktop name, which follows the user's icon theme and needs nothing
//! from us. Some send a directory of their own to look in. And some send raw
//! pixels — very often a **near-black grayscale glyph**, drawn for a light
//! panel, which on this one is a black icon on a black bar.
//!
//! The contrast pass is v1's, ported: sample the icon, decide whether it is
//! grayscale, measure it against the panel behind it, and if it fails the WCAG
//! 3:1 ratio for user-interface graphics, rescale it toward the panel's own
//! text colour. Antialiasing survives because the rescale is one linear factor
//! over every grayscale pixel, not a threshold.
//!
//! One thing did change on the way across, and [`scale_greys`] says why: v1
//! scaled by `target / 255`, which brings a *light* icon down — right for the
//! light themes, and a no-op on the near-black icons a dark panel
//! actually has trouble with.
//!
//! Everything except [`texture`] and [`apply`] is pure, and tested as such.

use gtk4::gdk;
use gtk4::{Image, gdk_pixbuf, glib};
use topbar_core::Config;
use topbar_core::theme::{Palette, Rgb};
use topbar_services::{IconView, Pixmap};
use tracing::debug;

/// The WCAG minimum contrast ratio for user-interface graphics.
const MIN_CONTRAST: f64 = 3.0;
/// How far apart two channels may be and still count as grey.
const GRAYSCALE_TOLERANCE: u8 = 15;
/// Below this, a sampled pixel is treated as background rather than icon.
const ALPHA_THRESHOLD: u8 = 128;
/// How far the target grey is softened toward mid-grey, in percent.
///
/// Pure white would be louder than every other icon on the bar; a hair of grey
/// keeps a lifted icon in the same visual register as a symbolic one.
const SOFTEN_PERCENT: u16 = 15;
/// Extensions an application's own icon directory is searched for.
const ICON_EXTENSIONS: [&str; 3] = ["png", "svg", "xpm"];

/// The panel colours the contrast pass measures against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contrast {
    /// Relative luminance of the bar behind the icon.
    background: f64,
    /// The grey a lifted icon is scaled toward.
    target: u8,
}

impl Contrast {
    /// Read the panel's own colours.
    ///
    /// The bar's background is what a tray icon actually sits on, and the
    /// foreground is what every other glyph beside it is drawn in — so an icon
    /// lifted to meet it lands in the company it is keeping.
    pub fn of(config: &Config) -> Self {
        let palette = Palette::from_config(config);
        let Rgb { r, g, b } = palette.foreground;
        Self {
            background: palette.bar.relative_luminance(),
            target: ((u16::from(r) + u16::from(g) + u16::from(b)) / 3) as u8,
        }
    }

    /// The grey to scale a lifted icon to.
    fn softened(self) -> u8 {
        ((u16::from(self.target) * (100 - SOFTEN_PERCENT) + 128 * SOFTEN_PERCENT) / 100) as u8
    }
}

/// Draw `icon` into `image` at `size` pixels.
///
/// Returns whether the icon was drawn from pixels rather than by name, which
/// is what decides whether the panel may tint it: a themed symbolic icon takes
/// its colour from CSS, and a picture cannot be recoloured without ruining it.
pub fn apply(image: &Image, icon: &IconView, size: i32, contrast: Contrast) -> bool {
    image.set_pixel_size(size);
    match icon {
        IconView::Themed { name, theme_path } => {
            // The application's own directory first: it shipped those icons
            // precisely because the system theme has nothing for it.
            if let Some(path) = theme_path
                .as_deref()
                .and_then(|directory| resolve(directory, name))
                && let Some(texture) = from_file(&path, size, contrast)
            {
                debug!("tray icon {name} loaded from {}", path.display());
                image.set_paintable(Some(&texture));
                return true;
            }
            // Some applications put an absolute path in `IconName`.
            if name.starts_with('/')
                && let Some(texture) = from_file(std::path::Path::new(name), size, contrast)
            {
                image.set_paintable(Some(&texture));
                return true;
            }
            image.set_icon_name(Some(name));
            false
        }
        IconView::Pixels(pixmap) => {
            image.set_paintable(Some(&texture(pixmap, contrast)));
            true
        }
        IconView::Fallback => {
            image.set_icon_name(Some(topbar_services::tray::FALLBACK_ICON));
            false
        }
    }
}

/// Build a texture from an application's pixels, lifted if it needs lifting.
fn texture(pixmap: &Pixmap, contrast: Contrast) -> gdk::MemoryTexture {
    let mut rgba = pixmap.rgba.clone();
    lift(&mut rgba, pixmap.width, pixmap.height, contrast);
    from_rgba(rgba, pixmap.width, pixmap.height)
}

/// Wrap packed RGBA in a texture GTK can draw.
///
/// `R8g8b8a8` is straight alpha, deliberately: StatusNotifierItem pixmaps are
/// ARGB32, which is not premultiplied, and the service un-premultiplies the
/// applications that send it anyway. Handing premultiplied bytes to a straight
/// format — or the other way round — draws every antialiased edge twice.
fn from_rgba(rgba: Vec<u8>, width: i32, height: i32) -> gdk::MemoryTexture {
    gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(rgba),
        (width * 4) as usize,
    )
}

/// Decode a file-backed icon and run it through the contrast pass.
///
/// SVGs are rasterised at the size they will be drawn at rather than at their
/// intrinsic one, which is the difference between a crisp 18px glyph and a
/// blurry scaled-down 256px one.
fn from_file(path: &std::path::Path, size: i32, contrast: Contrast) -> Option<gdk::MemoryTexture> {
    let pixbuf = gdk_pixbuf::Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    let width = pixbuf.width();
    let height = pixbuf.height();
    let mut rgba = packed(&pixbuf)?;
    lift(&mut rgba, width, height, contrast);
    Some(from_rgba(rgba, width, height))
}

/// Flatten a pixbuf into packed RGBA, dropping its row padding.
fn packed(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<u8>> {
    let width = pixbuf.width() as usize;
    let height = pixbuf.height() as usize;
    let stride = pixbuf.rowstride() as usize;
    let channels = pixbuf.n_channels() as usize;
    let has_alpha = pixbuf.has_alpha();

    let bytes = pixbuf.read_pixel_bytes();
    let raw: &[u8] = bytes.as_ref();

    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for column in 0..width {
            let at = row * stride + column * channels;
            let pixel = raw.get(at..at + channels)?;
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2]]);
            rgba.push(if has_alpha { pixel[3] } else { 255 });
        }
    }
    Some(rgba)
}

/// Find `name` in an application's own icon directory.
fn resolve(directory: &str, name: &str) -> Option<std::path::PathBuf> {
    let base = std::path::Path::new(directory);
    if !base.is_dir() {
        return None;
    }
    for extension in ICON_EXTENSIONS {
        let candidate = base.join(format!("{name}.{extension}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // The name may already carry its extension.
    let direct = base.join(name);
    direct.is_file().then_some(direct)
}

// ---------------------------------------------------------------------------
// The contrast pass — pure from here down
// ---------------------------------------------------------------------------

/// Lift a near-invisible grayscale icon until it can be seen.
///
/// Colour icons are left alone: a blue Bluetooth glyph is legible on black and
/// scaling it would only wash it out. So are grayscale icons that already have
/// the contrast they need — a white glyph on a black bar passes at 21:1.
pub fn lift(rgba: &mut [u8], width: i32, height: i32, contrast: Contrast) {
    let Some(sample) = sample(rgba, width, height) else {
        return;
    };
    if !sample.grayscale {
        return;
    }
    let ratio = contrast_ratio(sample.luminance, contrast.background);
    if ratio >= MIN_CONTRAST {
        return;
    }
    debug!("lifting a tray icon: {ratio:.2}:1 against the panel");
    scale_greys(rgba, contrast.softened());
}

/// What a handful of pixels say about an icon.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    /// Mean relative luminance of the pixels that are actually there.
    luminance: f64,
    /// Whether most of them are grey.
    grayscale: bool,
}

/// Look at seventeen pixels spread over the icon.
///
/// The outer ring, an inner ring a quarter of the way in, and the centre. The
/// inner ring is what makes this work on the many icons that are a small glyph
/// inside a lot of transparent padding: sampling the border alone would find
/// nothing but empty pixels and conclude the icon was fine.
fn sample(rgba: &[u8], width: i32, height: i32) -> Option<Sample> {
    let w = usize::try_from(width).ok()?;
    let h = usize::try_from(height).ok()?;
    if w < 2 || h < 2 {
        return None;
    }

    let (w25, w75) = (w / 4, w * 3 / 4);
    let (h25, h75) = (h / 4, h * 3 / 4);
    let points = [
        (0, 0),
        (w - 1, 0),
        (0, h - 1),
        (w - 1, h - 1),
        (w / 2, 0),
        (w / 2, h - 1),
        (0, h / 2),
        (w - 1, h / 2),
        (w25, h25),
        (w75, h25),
        (w25, h75),
        (w75, h75),
        (w / 2, h25),
        (w / 2, h75),
        (w25, h / 2),
        (w75, h / 2),
        (w / 2, h / 2),
    ];

    let mut total = 0.0;
    let mut greys = 0usize;
    let mut visible = 0usize;

    for (x, y) in points {
        let at = (y * w + x) * 4;
        let Some(pixel) = rgba.get(at..at + 4) else {
            continue;
        };
        if pixel[3] < ALPHA_THRESHOLD {
            continue;
        }
        visible += 1;
        total += Rgb::new(pixel[0], pixel[1], pixel[2]).relative_luminance();
        if is_grey(pixel[0], pixel[1], pixel[2]) {
            greys += 1;
        }
    }

    (visible > 0).then(|| Sample {
        luminance: total / visible as f64,
        grayscale: greys > visible / 2,
    })
}

/// Whether three channels are close enough together to be grey.
fn is_grey(red: u8, green: u8, blue: u8) -> bool {
    red.abs_diff(green) <= GRAYSCALE_TOLERANCE
        && green.abs_diff(blue) <= GRAYSCALE_TOLERANCE
        && red.abs_diff(blue) <= GRAYSCALE_TOLERANCE
}

/// The WCAG contrast ratio between two relative luminances.
fn contrast_ratio(one: f64, other: f64) -> f64 {
    let (lighter, darker) = if one > other {
        (one, other)
    } else {
        (other, one)
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// Stretch the icon's grey range until its brightest pixel reaches `target`.
///
/// This is where the port deliberately departs from v1. v1 scaled by
/// `target / 255`, which assumes the icon is a *light* glyph that needs
/// bringing down — the right transform for the light themes v1 also had, and a
/// no-op on the case this panel actually has, where a near-black glyph scaled
/// by 0.92 is still near-black. Scaling by `target / peak` instead lifts the
/// icon by however much it needs, and because it is still a single linear
/// factor every pixel keeps its strength relative to every other: an
/// antialiased edge fades exactly as far as it used to, rather than becoming a
/// hard stair.
///
/// Coloured pixels in an otherwise grey icon — a red unread dot on a grey
/// envelope — are left alone, because the colour is the point of them.
fn scale_greys(rgba: &mut [u8], target: u8) {
    let peak = peak_grey(rgba);
    for pixel in rgba.chunks_exact_mut(4) {
        if !is_grey(pixel[0], pixel[1], pixel[2]) {
            continue;
        }
        let mean = (f32::from(pixel[0]) + f32::from(pixel[1]) + f32::from(pixel[2])) / 3.0;
        // A glyph that is pure black has no shading to preserve; its shape is
        // carried entirely by its alpha, so every pixel of it goes to target.
        let lifted = match peak {
            0 => target,
            peak => (mean * f32::from(target) / f32::from(peak) + 0.5).min(255.0) as u8,
        };
        pixel[0] = lifted;
        pixel[1] = lifted;
        pixel[2] = lifted;
    }
}

/// The brightest grey among the pixels that are actually drawn.
///
/// Transparent padding is skipped: an icon surrounded by `rgba(0,0,0,0)` would
/// otherwise be stretched against a peak that is not part of the picture.
fn peak_grey(rgba: &[u8]) -> u8 {
    rgba.chunks_exact(4)
        .filter(|pixel| pixel[3] >= ALPHA_THRESHOLD && is_grey(pixel[0], pixel[1], pixel[2]))
        .map(|pixel| ((u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2])) / 3) as u8)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel as it actually is: an opaque black bar with white text.
    const PANEL: Contrast = Contrast {
        background: 0.0,
        target: 255,
    };

    /// A solid square of one colour.
    fn solid(size: i32, pixel: [u8; 4]) -> Vec<u8> {
        pixel
            .iter()
            .copied()
            .cycle()
            .take((size * size * 4) as usize)
            .collect()
    }

    #[test]
    fn the_panels_own_colours_are_what_an_icon_is_measured_against() {
        let mut config = Config::default();
        config.bar.background_color = Some("#000000".to_string());
        let contrast = Contrast::of(&config);
        assert_eq!(contrast.background, 0.0);
        assert_eq!(contrast.target, 255);

        config.bar.background_color = Some("#ffffff".to_string());
        assert!(
            Contrast::of(&config).background > 0.9,
            "a white bar is a bright bar"
        );

        config.bar.background_color = Some("not a colour".to_string());
        assert_eq!(
            Contrast::of(&config).background,
            0.0,
            "an unreadable colour falls back to the black the panel really is"
        );
    }

    #[test]
    fn the_target_grey_is_softened_off_pure_white() {
        // 255 * 0.85 + 128 * 0.15, rounded down.
        assert_eq!(PANEL.softened(), 235);
    }

    #[test]
    fn the_contrast_ratio_is_the_one_wcag_defines() {
        // White on black: (1.0 + 0.05) / (0.0 + 0.05) = 21.
        assert!((contrast_ratio(1.0, 0.0) - 21.0).abs() < 1e-9);
        assert!((contrast_ratio(0.0, 1.0) - 21.0).abs() < 1e-9, "symmetric");
        assert!((contrast_ratio(0.5, 0.5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn greyness_is_a_tolerance_not_an_equality() {
        assert!(is_grey(0x20, 0x20, 0x20));
        assert!(is_grey(0x20, 0x28, 0x2c), "within 15 on every pair");
        assert!(!is_grey(0x35, 0x84, 0xe4), "a blue is not a grey");
        assert!(!is_grey(0xff, 0x00, 0x00), "nor is a red");
        assert!(
            !is_grey(0x20, 0x20, 0x40),
            "one channel far enough out is enough"
        );
    }

    #[test]
    fn a_near_black_grayscale_icon_is_lifted_off_the_bar() {
        // #202020 on black: a ratio of about 1.2:1, which is invisible.
        let mut rgba = solid(8, [0x20, 0x20, 0x20, 0xff]);
        let before = rgba.clone();
        lift(&mut rgba, 8, 8, PANEL);

        assert_ne!(rgba, before, "an unreadable icon is not left unreadable");
        let lifted = Rgb::new(rgba[0], rgba[1], rgba[2]).relative_luminance();
        assert!(
            contrast_ratio(lifted, PANEL.background) >= MIN_CONTRAST,
            "the lifted icon clears 3:1 against the panel"
        );
        assert_eq!(rgba[3], 0xff, "alpha is never touched");
    }

    #[test]
    fn a_white_icon_is_already_legible_and_is_left_alone() {
        let mut rgba = solid(8, [0xff, 0xff, 0xff, 0xff]);
        let before = rgba.clone();
        lift(&mut rgba, 8, 8, PANEL);
        assert_eq!(rgba, before);
    }

    #[test]
    fn a_colour_icon_is_never_touched_however_dark_it_is() {
        // A deep blue that would fail the ratio, but scaling it grey would
        // destroy the only thing that identifies it.
        let mut rgba = solid(8, [0x00, 0x10, 0x40, 0xff]);
        let before = rgba.clone();
        lift(&mut rgba, 8, 8, PANEL);
        assert_eq!(rgba, before);
    }

    #[test]
    fn a_dark_icon_on_a_light_bar_is_left_alone() {
        let light = Contrast {
            background: 1.0,
            target: 0,
        };
        let mut rgba = solid(8, [0x20, 0x20, 0x20, 0xff]);
        let before = rgba.clone();
        lift(&mut rgba, 8, 8, light);
        assert_eq!(
            rgba, before,
            "dark on white is 15:1; there is nothing to fix"
        );
    }

    #[test]
    fn antialiasing_survives_the_lift() {
        // A glyph at three strengths. After lifting they must still be in the
        // same order and still distinct, or the edge has become a stair.
        let mut rgba = Vec::new();
        for grey in [0x08u8, 0x14, 0x20] {
            rgba.extend_from_slice(&[grey, grey, grey, 0xff]);
        }
        rgba.extend_from_slice(&[0x20, 0x20, 0x20, 0xff]);
        lift(&mut rgba, 2, 2, PANEL);

        let strengths: Vec<u8> = rgba.chunks_exact(4).take(3).map(|p| p[0]).collect();
        assert!(
            strengths[0] < strengths[1] && strengths[1] < strengths[2],
            "the gradient is still a gradient: {strengths:?}"
        );
        assert!(strengths[2] > 0x80, "and the strongest is now visible");
    }

    #[test]
    fn light_palette_darkens_only_greys_and_preserves_badges_and_alpha() {
        let config =
            Config::parse("[theme]\nmode = \"light\"\n[theme.palette]\nforeground = \"#123\"")
                .unwrap()
                .0;
        let contrast = Contrast::of(&config);
        assert_eq!(contrast.target, 34);
        let mut rgba = solid(8, [240, 240, 240, 255]);
        let badge = (3 * 8 + 3) * 4;
        rgba[badge..badge + 4].copy_from_slice(&[230, 20, 60, 190]);
        lift(&mut rgba, 8, 8, contrast);
        assert!(rgba[0] < 100);
        assert_eq!(rgba[3], 255);
        assert_eq!(&rgba[badge..badge + 4], &[230, 20, 60, 190]);
    }

    #[test]
    fn a_glyph_in_a_lot_of_transparent_padding_is_still_found() {
        // 16x16, transparent except for a dark 8x8 block in the middle. The
        // outer ring alone would find nothing; the inner ring finds the glyph.
        let mut rgba = vec![0u8; 16 * 16 * 4];
        for y in 4..12 {
            for x in 4..12 {
                let at = (y * 16 + x) * 4;
                rgba[at..at + 4].copy_from_slice(&[0x18, 0x18, 0x18, 0xff]);
            }
        }
        let before = rgba.clone();
        lift(&mut rgba, 16, 16, PANEL);
        assert_ne!(rgba, before, "the glyph inside the padding was lifted");

        let centre = (8 * 16 + 8) * 4;
        assert!(rgba[centre] > 0x80);
        assert_eq!(&rgba[..4], &[0, 0, 0, 0], "the padding stays transparent");
    }

    #[test]
    fn an_icon_that_is_entirely_transparent_is_left_alone() {
        let mut rgba = vec![0u8; 8 * 8 * 4];
        let before = rgba.clone();
        lift(&mut rgba, 8, 8, PANEL);
        assert_eq!(rgba, before);
        assert_eq!(sample(&rgba, 8, 8), None, "nothing visible to sample");
    }

    #[test]
    fn an_icon_too_small_to_sample_is_left_alone() {
        let mut rgba = solid(1, [0x20, 0x20, 0x20, 0xff]);
        let before = rgba.clone();
        lift(&mut rgba, 1, 1, PANEL);
        assert_eq!(rgba, before);
        assert_eq!(sample(&rgba, 1, 1), None);
    }

    #[test]
    fn a_truncated_buffer_is_sampled_rather_than_indexed_off_the_end() {
        // A buffer that claims 8x8 and carries four pixels. The pass must not
        // panic; whether it lifts anything is beside the point.
        let mut rgba = solid(2, [0x20, 0x20, 0x20, 0xff]);
        lift(&mut rgba, 8, 8, PANEL);
    }

    #[test]
    fn a_mostly_grey_icon_with_a_spot_of_colour_keeps_the_spot() {
        // A grey envelope with a red unread dot: the envelope is lifted, the
        // dot is not, because the dot is the thing the eye is meant to catch.
        let mut rgba = solid(8, [0x18, 0x18, 0x18, 0xff]);
        let dot = (3 * 8 + 3) * 4;
        rgba[dot..dot + 4].copy_from_slice(&[0xef, 0x44, 0x44, 0xff]);

        lift(&mut rgba, 8, 8, PANEL);
        assert!(rgba[0] > 0x80, "the envelope was lifted");
        assert_eq!(
            &rgba[dot..dot + 4],
            &[0xef, 0x44, 0x44, 0xff],
            "the red dot is exactly as it arrived"
        );
    }

    #[test]
    fn a_thresholds_table_covers_the_ratio_the_pass_turns_on_at() {
        // Uniform greys, and whether each is lifted. The boundary is wherever
        // 3:1 falls, and it must not move without somebody meaning it to.
        // The pass turns on below a relative luminance of 0.1, which for a
        // uniform grey is somewhere between 89 and 90; the table stays clear
        // of the boundary so a rounding change is not a test failure.
        for (grey, expected) in [
            (0x00u8, true),
            (0x20, true),
            (0x40, true),
            (0x50, true),
            (0x60, false),
            (0x80, false),
            (0xc0, false),
            (0xff, false),
        ] {
            let mut rgba = solid(8, [grey, grey, grey, 0xff]);
            let before = rgba.clone();
            let ratio = contrast_ratio(
                Rgb::new(grey, grey, grey).relative_luminance(),
                PANEL.background,
            );
            lift(&mut rgba, 8, 8, PANEL);
            assert_eq!(
                rgba != before,
                expected,
                "grey {grey:#04x} measures {ratio:.2}:1"
            );
        }
    }

    #[test]
    fn the_peak_is_taken_from_the_pixels_that_are_drawn() {
        // A dark glyph inside transparent padding: the padding is black too,
        // but it is not part of the picture and must not set the peak.
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for at in (0..rgba.len()).step_by(4).take(4) {
            rgba[at..at + 4].copy_from_slice(&[0x30, 0x30, 0x30, 0xff]);
        }
        assert_eq!(peak_grey(&rgba), 0x30);
        assert_eq!(peak_grey(&[0, 0, 0, 0]), 0, "nothing drawn, no peak");
    }

    #[test]
    fn a_pure_black_glyph_is_lifted_whole_rather_than_divided_by_nothing() {
        let mut rgba = solid(8, [0x00, 0x00, 0x00, 0xff]);
        lift(&mut rgba, 8, 8, PANEL);
        assert_eq!(rgba[0], PANEL.softened());
        assert_eq!(rgba[3], 0xff);
    }

    #[test]
    fn a_half_transparent_glyph_is_not_mistaken_for_background() {
        // Alpha exactly at the threshold counts; one below it does not.
        let mut rgba = solid(8, [0x20, 0x20, 0x20, ALPHA_THRESHOLD]);
        let before = rgba.clone();
        lift(&mut rgba, 8, 8, PANEL);
        assert_ne!(rgba, before);

        let mut faint = solid(8, [0x20, 0x20, 0x20, ALPHA_THRESHOLD - 1]);
        let before = faint.clone();
        lift(&mut faint, 8, 8, PANEL);
        assert_eq!(faint, before, "a ghost is not an icon");
    }
}
