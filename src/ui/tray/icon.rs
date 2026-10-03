//! Tray icon bitmaps, made from the app icon: full colour while connected,
//! greyscale otherwise. Decoded once per size and state; no gpui.

use image::{imageops::FilterType, RgbaImage};

const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");

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
    fn argb32_reorders_channels() {
        assert_eq!(
            argb32_from_rgba(&[1, 2, 3, 4, 10, 20, 30, 40]),
            vec![4, 1, 2, 3, 40, 10, 20, 30]
        );
    }
}
