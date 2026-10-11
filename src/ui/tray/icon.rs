//! Tray icon bitmaps. Windows and Linux use the app icon: full colour while
//! connected, greyscale otherwise. The macOS menu bar uses a one-colour
//! glyph instead, drawn as a template image (only its alpha counts; AppKit
//! colours it for the menu bar's appearance and the highlighted state).
//! Decoded once per size and state; no gpui.

use image::{imageops::FilterType, RgbaImage};

const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");
/// Rendered from `assets/tray/box-*.svg` at 44px (22pt at 2x, the menu
/// bar's height): `resvg -w 44 -h 44 box-outline.svg box-outline.png`.
const MENU_BAR_DISCONNECTED_PNG: &[u8] = include_bytes!("../../../assets/tray/box-outline.png");
const MENU_BAR_CONNECTED_PNG: &[u8] = include_bytes!("../../../assets/tray/box-filled.png");
/// The menu bar glyphs' pixel size.
pub const MENU_BAR_ICON_SIZE: u32 = 44;

/// A square RGBA8 bitmap (row-major, straight alpha).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconImage {
    pub size: u32,
    pub rgba: Vec<u8>,
}

impl IconImage {
    /// ARGB32 in network byte order, as StatusNotifierItem pixmaps want it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn to_argb32(&self) -> Vec<u8> {
        argb32_from_rgba(&self.rgba)
    }
}

/// The app icon at `size`×`size`; greyscale unless `connected`.
pub fn tray_icon(size: u32, connected: bool) -> IconImage {
    let source = match image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png) {
        Ok(image) => image.to_rgba8(),
        // Bundled asset, so this "can't" happen; a blank square still keeps
        // the tray (and its menu) usable if it ever does.
        Err(_) => RgbaImage::new(size, size),
    };
    let mut resized = image::imageops::resize(&source, size, size, FilterType::Lanczos3);
    if !connected {
        greyscale_in_place(&mut resized);
    }
    IconImage {
        size,
        rgba: resized.into_raw(),
    }
}

/// The macOS menu bar glyph: a shipping box in outline while disconnected,
/// filled while connected — the system's own on/off convention, no colour.
/// Black with alpha, for use as a template image.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn menu_bar_icon(connected: bool) -> IconImage {
    let png = if connected {
        MENU_BAR_CONNECTED_PNG
    } else {
        MENU_BAR_DISCONNECTED_PNG
    };
    let size = MENU_BAR_ICON_SIZE;
    let image = match image::load_from_memory_with_format(png, image::ImageFormat::Png) {
        Ok(image) => image.to_rgba8(),
        // Bundled asset; a blank square keeps the menu usable regardless.
        Err(_) => RgbaImage::new(size, size),
    };
    IconImage {
        size: image.width(),
        rgba: image.into_raw(),
    }
}

/// Luma (Rec. 601) in place of each pixel's colour, alpha kept. Slightly
/// faded too, so a disconnected icon reads as "off" next to coloured ones.
fn greyscale_in_place(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let [r, g, b, a] = pixel.0;
        let luma = (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000;
        // Rounded up: a faint edge pixel never turns fully transparent.
        let alpha = (u32::from(a) * 85).div_ceil(100);
        pixel.0 = [luma as u8, luma as u8, luma as u8, alpha as u8];
    }
}

fn argb32_from_rgba(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|px| [px[3], px[0], px[1], px[2]])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_have_the_requested_size() {
        for size in [16, 32, 64] {
            for connected in [false, true] {
                let icon = tray_icon(size, connected);
                assert_eq!(icon.size, size);
                assert_eq!(icon.rgba.len(), (size * size * 4) as usize);
            }
        }
    }

    #[test]
    fn disconnected_icon_is_grey_and_connected_is_not() {
        let grey = tray_icon(32, false);
        assert!(grey
            .rgba
            .chunks_exact(4)
            .all(|px| px[0] == px[1] && px[1] == px[2]));
        let colour = tray_icon(32, true);
        assert!(colour
            .rgba
            .chunks_exact(4)
            .any(|px| px[3] > 0 && (px[0] != px[1] || px[1] != px[2])));
    }

    #[test]
    fn greyscale_keeps_transparency() {
        let colour = tray_icon(32, true);
        let grey = tray_icon(32, false);
        for (c, g) in colour.rgba.chunks_exact(4).zip(grey.rgba.chunks_exact(4)) {
            assert_eq!(c[3] == 0, g[3] == 0);
        }
    }

    #[test]
    fn menu_bar_icons_are_black_glyphs_with_a_margin() {
        let size = MENU_BAR_ICON_SIZE as usize;
        for connected in [false, true] {
            let icon = menu_bar_icon(connected);
            assert_eq!(icon.size, MENU_BAR_ICON_SIZE);
            assert_eq!(icon.rgba.len(), size * size * 4);
            // A template image: only alpha carries the shape.
            assert!(icon
                .rgba
                .chunks_exact(4)
                .all(|px| px[3] == 0 || px[..3] == [0, 0, 0]));
            let alpha = |x: usize, y: usize| icon.rgba[(y * size + x) * 4 + 3];
            for i in 0..size {
                for (x, y) in [(i, 0), (i, size - 1), (0, i), (size - 1, i)] {
                    assert_eq!(alpha(x, y), 0, "({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn connected_menu_bar_icon_is_the_filled_one() {
        let ink = |icon: IconImage| {
            icon.rgba
                .chunks_exact(4)
                .map(|px| u32::from(px[3]))
                .sum::<u32>()
        };
        assert!(ink(menu_bar_icon(true)) > ink(menu_bar_icon(false)) * 6 / 5);
    }

    #[test]
    fn argb32_reorders_channels() {
        assert_eq!(
            argb32_from_rgba(&[1, 2, 3, 4, 10, 20, 30, 40]),
            vec![4, 1, 2, 3, 40, 10, 20, 30]
        );
    }
}
