//! App 级 AssetSource:自带 gpui-component 图标集里缺的图标,
//! 其余路径委托给 gpui-component 的内嵌资源。

use gpui::{AssetSource, Result, SharedString};
use gpui_component_assets::Assets;
use std::borrow::Cow;

const POWER_SVG: &[u8] = include_bytes!("../../assets/icons/power.svg");
const PENCIL_SVG: &[u8] = include_bytes!("../../assets/icons/pencil.svg");
const REFRESH_CW_SVG: &[u8] = include_bytes!("../../assets/icons/refresh-cw.svg");
const GAUGE_SVG: &[u8] = include_bytes!("../../assets/icons/gauge.svg");
const SHIELD_CHECK_SVG: &[u8] = include_bytes!("../../assets/icons/shield-check.svg");
const ZAP_SVG: &[u8] = include_bytes!("../../assets/icons/zap.svg");
/// Toast level glyphs, bold enough to read at 12px inside their disc.
const TOAST_SUCCESS_SVG: &[u8] = include_bytes!("../../assets/icons/toast-success.svg");
const TOAST_INFO_SVG: &[u8] = include_bytes!("../../assets/icons/toast-info.svg");
const TOAST_WARNING_SVG: &[u8] = include_bytes!("../../assets/icons/toast-warning.svg");
const TOAST_ERROR_SVG: &[u8] = include_bytes!("../../assets/icons/toast-error.svg");
/// The app icon, shown beside the name at the top of the sidebar.
const BRAND_ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");
/// The app's mark as a glyph (the tray's box), on the sidebar's accent tile.
const BRAND_BOX_SVG: &[u8] = include_bytes!("../../assets/tray/box-outline.svg");

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icons/power.svg" => Ok(Some(Cow::Borrowed(POWER_SVG))),
            "icons/pencil.svg" => Ok(Some(Cow::Borrowed(PENCIL_SVG))),
            "icons/refresh-cw.svg" => Ok(Some(Cow::Borrowed(REFRESH_CW_SVG))),
            "icons/gauge.svg" => Ok(Some(Cow::Borrowed(GAUGE_SVG))),
            "icons/shield-check.svg" => Ok(Some(Cow::Borrowed(SHIELD_CHECK_SVG))),
            "icons/zap.svg" => Ok(Some(Cow::Borrowed(ZAP_SVG))),
            "icons/toast-success.svg" => Ok(Some(Cow::Borrowed(TOAST_SUCCESS_SVG))),
            "icons/toast-info.svg" => Ok(Some(Cow::Borrowed(TOAST_INFO_SVG))),
            "icons/toast-warning.svg" => Ok(Some(Cow::Borrowed(TOAST_WARNING_SVG))),
            "icons/toast-error.svg" => Ok(Some(Cow::Borrowed(TOAST_ERROR_SVG))),
            "brand/icon.png" => Ok(Some(Cow::Borrowed(BRAND_ICON_PNG))),
            "brand/box.svg" => Ok(Some(Cow::Borrowed(BRAND_BOX_SVG))),
            _ => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Assets.list(path)
    }
}
