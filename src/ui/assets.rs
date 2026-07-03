//! App 级 AssetSource:自带 gpui-component 图标集里缺的图标,
//! 其余路径委托给 gpui-component 的内嵌资源。

use gpui::{AssetSource, Result, SharedString};
use gpui_component_assets::Assets;
use std::borrow::Cow;

const POWER_SVG: &[u8] = include_bytes!("../../assets/icons/power.svg");
const PENCIL_SVG: &[u8] = include_bytes!("../../assets/icons/pencil.svg");
const REFRESH_CW_SVG: &[u8] = include_bytes!("../../assets/icons/refresh-cw.svg");

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icons/power.svg" => Ok(Some(Cow::Borrowed(POWER_SVG))),
            "icons/pencil.svg" => Ok(Some(Cow::Borrowed(PENCIL_SVG))),
            "icons/refresh-cw.svg" => Ok(Some(Cow::Borrowed(REFRESH_CW_SVG))),
            _ => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Assets.list(path)
    }
}
