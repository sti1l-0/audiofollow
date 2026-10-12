//! 应用图标 —— 唯一来源是 `ui/` 下的 PNG（由 `tools/gen_icon.py` 生成）。
//!
//! 各处如何消费同一张图：
//! - 窗口标题栏 / 任务栏 / Alt+Tab：slint 的 `Window.icon` 属性（编译期嵌图）；
//! - 托盘：本模块把 `icon-tray.png` 解码成 RGBA 交给 `tray-icon`。
//!
//! 为什么托盘单独一张 32×32：`tray-icon` 要原始 RGBA，而 Windows 通知区域
//! 实际显示 16×16（高 DPI 更大），从 256 主图现缩放反而糊；离线用
//! LANCZOS 生成一张 32 的专用图，小尺寸锐利得多。

use slint::Image;

/// 托盘图标源图（32×32，与 `ui/icon.png` 同几何）。
const TRAY_PNG: &[u8] = include_bytes!("../ui/icon-tray.png");

/// 从内嵌 PNG 解码出托盘用的 RGBA 位图。
pub fn tray_rgba() -> Result<Vec<u8>, String> {
    let img = Image::load_from_data(TRAY_PNG, None).map_err(|e| e.to_string())?;
    let buf = img
        .to_rgba8()
        .ok_or_else(|| "托盘图标缺少 RGBA 数据".to_string())?;
    Ok(buf.as_bytes().to_vec())
}

/// 托盘图标源图边长（与 TRAY_PNG 实际尺寸对应）。
pub const TRAY_PX: u32 = 32;
