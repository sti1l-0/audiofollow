//! 显示器枚举与「窗口归哪块屏」的判定。

use super::dpi;
use super::ffi::{self, Rect, HMONITOR, MONITORINFOEXW, MONITORINFOF_PRIMARY};

/// 一块物理显示器。
#[derive(Debug, Clone)]
pub struct Monitor {
    pub handle: HMONITOR,
    /// 设备名，形如 `\\.\DISPLAY1`。稳定标识，配置里存这个。
    pub device: String,
    /// 系统可读名，形如 `Generic PnP Monitor`（不是用户改的"显示器 1/2"，那需要 QueryDisplayConfig）。
    pub name: String,
    /// 显示器全景矩形（物理像素）。
    pub rect: Rect,
    /// 工作区矩形（扣掉任务栏）。
    pub work: Rect,
    pub is_primary: bool,
    pub dpi: u32,
}

impl Monitor {
    pub fn scale_percent(&self) -> u32 {
        dpi::scale_percent(self.dpi)
    }

    /// 用于展示的一行摘要。
    pub fn label(&self, index: usize) -> String {
        format!(
            "显示器 {} · {} · {}×{}{}{}",
            index + 1,
            if self.name.is_empty() {
                self.device.trim_start_matches(r"\\.\").to_string()
            } else {
                self.name.clone()
            },
            self.rect.width(),
            self.rect.height(),
            if self.dpi != 96 {
                format!(" @{}%", self.scale_percent())
            } else {
                String::new()
            },
            if self.is_primary { " · 主屏" } else { "" },
        )
    }
}

/// 枚举所有显示器。
pub fn enumerate() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        ffi::EnumDisplayMonitors(
            0, // HDC = isize，0 表示枚举全部显示器
            std::ptr::null(),
            Some(enum_proc),
            &mut out as *mut Vec<Monitor> as isize,
        );
    }
    // 主屏排前面，其余按 x 坐标（左→右），让配置界面里的顺序符合直觉
    out.sort_by_key(|m| (!m.is_primary, m.rect.left, m.rect.top));
    out
}

unsafe extern "system" fn enum_proc(
    hmonitor: HMONITOR,
    _hdc: isize,
    _clip: *mut Rect,
    lparam: isize,
) -> i32 {
    let out = &mut *(lparam as *mut Vec<Monitor>);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if ffi::GetMonitorInfoW(hmonitor, &mut info) != 0 {
        out.push(Monitor {
            handle: hmonitor,
            device: ffi::utf16_arr_to_string(&info.szDevice),
            name: describe(&info.szDevice),
            rect: info.rcMonitor,
            work: info.rcWork,
            is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
            dpi: dpi::monitor_dpi(hmonitor),
        });
    }
    1 // 继续枚举
}

/// 取显示器型号名。`MONITORINFOEXW::szDevice` 只给 `\\.\DISPLAY1`，
/// 拿它再问一次 `EnumDisplayDevicesW` 才能得到可辨认的名字。
///
/// 很多显示器（尤其是虚拟显示器、没有 EDID 型号段的）报回来的 `DeviceString`
/// 就是一句 "Generic PnP Monitor"，信息量为零。这时换用
/// `EDD_GET_DEVICE_INTERFACE_NAME` 再问一次——它给的设备接口路径形如
/// `\\?\DISPLAY#MON-A#5&1a2b3c4d&0&UID256#{e6f07b5f-...}`，第二段就是型号。
/// 这个型号通常正好和音频设备名里的型号一致（例如 "MON-A (High Definition Audio)"），
/// 用户就能一眼把哪块屏对哪个音箱对上号。
fn describe(device: &[u16; ffi::CCHDEVICENAME]) -> String {
    const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 0x0000_0001;

    let usable = |s: &str| !s.is_empty() && !s.to_ascii_lowercase().contains("generic");

    unsafe {
        let mut dd: ffi::DISPLAY_DEVICEW = std::mem::zeroed();
        dd.cb = std::mem::size_of::<ffi::DISPLAY_DEVICEW>() as u32;
        if ffi::EnumDisplayDevicesW(device.as_ptr(), 0, &mut dd, 0) == 0 {
            return String::new();
        }
        let generic = ffi::utf16_arr_to_string(&dd.DeviceString);
        if usable(&generic) {
            return generic;
        }

        // 设备接口路径里的型号段
        let mut dd2: ffi::DISPLAY_DEVICEW = std::mem::zeroed();
        dd2.cb = std::mem::size_of::<ffi::DISPLAY_DEVICEW>() as u32;
        if ffi::EnumDisplayDevicesW(device.as_ptr(), 0, &mut dd2, EDD_GET_DEVICE_INTERFACE_NAME) != 0
        {
            let path = ffi::utf16_arr_to_string(&dd2.DeviceID);
            // \\?\DISPLAY#<型号>#<实例>#{接口 GUID}
            if let Some(model) = path.split('#').nth(1) {
                if usable(model) {
                    return model.to_string();
                }
            }
        }

        generic
    }
}

/// 判定一个矩形属于哪块显示器：按**交集面积**取最大者。
///
/// 为什么不用窗口中心点或 `MonitorFromWindow`：
/// - 中心点法在窗口被拖到屏幕交界处时会跳变；
/// - `MonitorFromWindow` 只看窗口最大可见区域的归属，对"一半在 A 一半在 B"没有可控的偏好。
///   面积法能给出可解释、可调的结果，多窗口仲裁也用同一把尺子。
pub fn best_for_rect(monitors: &[Monitor], rect: &Rect) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }

    let mut best: Option<(usize, i64)> = None;
    for (i, m) in monitors.iter().enumerate() {
        let area = m.rect.intersection_area(rect);
        if area > 0 && best.map_or(true, |(_, a)| area > a) {
            best = Some((i, area));
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }

    // 完全没交集（窗口被拖到所有屏幕之外，或最小化到 -32000）：退回到"包含中心点"，
    // 再退回到"距离最近的屏"。
    let (cx, cy) = rect.center();
    if let Some(i) = monitors.iter().position(|m| m.rect.contains_point(cx, cy)) {
        return Some(i);
    }
    monitors
        .iter()
        .enumerate()
        .min_by_key(|(_, m)| {
            let (mx, my) = m.rect.center();
            let dx = (mx - cx) as i64;
            let dy = (my - cy) as i64;
            dx * dx + dy * dy
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(left: i32, top: i32, right: i32, bottom: i32) -> Monitor {
        Monitor {
            handle: 0,
            device: format!(r"\\.\DISPLAY@{left}"),
            name: String::new(),
            rect: Rect::new(left, top, right, bottom),
            work: Rect::new(left, top, right, bottom),
            is_primary: left == 0,
            dpi: 96,
        }
    }

    #[test]
    fn rect_intersection() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 200, 200);
        assert_eq!(a.intersection_area(&b), 50 * 50);
        assert_eq!(a.intersection_area(&Rect::new(200, 200, 300, 300)), 0);
    }

    #[test]
    fn picks_larger_overlap() {
        // 左屏 0..1920，右屏 1920..3840
        let mons = [mon(0, 0, 1920, 1080), mon(1920, 0, 3840, 1080)];
        // 窗口横跨交界，2/3 在左屏
        let w = Rect::new(1200, 100, 2400, 900);
        assert_eq!(best_for_rect(&mons, &w), Some(0));
        // 反过来 2/3 在右屏
        let w = Rect::new(1500, 100, 2700, 900);
        assert_eq!(best_for_rect(&mons, &w), Some(1));
    }

    #[test]
    fn falls_back_when_offscreen() {
        let mons = [mon(0, 0, 1920, 1080), mon(1920, 0, 3840, 1080)];
        // 完全在屏幕外：按最近中心点归到右屏
        let w = Rect::new(5000, 5000, 5100, 5100);
        assert_eq!(best_for_rect(&mons, &w), Some(1));
    }
}
