//! 顶层窗口枚举，以及把窗口落到具体一块显示器上。

use super::ffi::{self, HWND, Rect, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
                 GWL_EXSTYLE, GWL_STYLE, GW_OWNER, PROCESS_QUERY_LIMITED_INFORMATION,
                 SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
                 WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_THICKFRAME};
use super::monitor::{self, Monitor};

/// 一个参与归属判定的顶层窗口。
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
    /// 进程可执行文件名（不含路径），用于展示。
    pub exe: String,
    /// 视觉边框矩形（物理像素，已排除不可见阴影）。
    pub rect: Rect,
    /// 是否最小化。最小化的窗口位置无意义，不参与判定。
    pub minimized: bool,
}

impl WindowInfo {
    pub fn display_name(&self) -> String {
        if self.title.is_empty() {
            format!("<无标题> ({})", self.exe)
        } else {
            self.title.clone()
        }
    }
}

/// 枚举所有"像是用户可见应用窗口"的顶层窗口。
///
/// 过滤规则（沿用 Windows 自身的 alt-tab 可列举性判断）：
/// - 不可见 / 已 cloaked（UWP 挂起、被虚拟桌面隐藏）
/// - `WS_EX_TOOLWINDOW`（工具窗口，通常不出现在任务栏）
/// - 有 owner 且未设 `WS_EX_APPWINDOW`（对话框、浮动面板等附属窗口）
/// - 矩形为空
pub fn enumerate_app_windows() -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe {
        ffi::EnumWindows(Some(enum_proc), &mut out as *mut Vec<WindowInfo> as isize);
    }
    out
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: isize) -> i32 {
    let out = &mut *(lparam as *mut Vec<WindowInfo>);
    if let Some(w) = inspect(hwnd) {
        out.push(w);
    }
    1 // 继续枚举
}

/// 给**本进程**指定标题的顶层窗口补上可缩放样式位。
///
/// 为什么需要：winit 建窗时给的样式是 `WS_CAPTION | WS_MINIMIZEBOX`，
/// 没有 `WS_THICKFRAME`（拖边框缩放）和 `WS_MAXIMIZEBOX`（最大化按钮），
/// 表现为窗口只能移动、拖不动边框、最大化按钮是灰的。
/// 这里把两个样式位补齐，再用 `SWP_FRAMECHANGED` 让系统重算非客户区。
///
/// 返回是否找到并改成功。窗口还没创建时返回 `false`，调用方应当重试
/// （slint 的 winit 后端把窗口实体化推迟到 show/run，创建时机外部不可见）。
pub fn make_window_resizable(title: &str) -> bool {
    #[repr(C)]
    struct Ctx<'a> {
        title: &'a str,
        hwnd: HWND,
    }

    unsafe extern "system" fn find_proc(hwnd: HWND, lparam: isize) -> i32 {
        let ctx = &mut *(lparam as *mut Ctx);
        if ffi::IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut pid = 0u32;
        ffi::GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != ffi::GetCurrentProcessId() {
            return 1;
        }
        if window_title(hwnd) == ctx.title {
            ctx.hwnd = hwnd;
            return 0; // 找到了，停止枚举
        }
        1
    }

    let mut ctx = Ctx { title, hwnd: 0 };
    unsafe {
        ffi::EnumWindows(Some(find_proc), &mut ctx as *mut Ctx as isize);
    }
    if ctx.hwnd == 0 {
        return false;
    }

    unsafe {
        let style = ffi::GetWindowLongW(ctx.hwnd, GWL_STYLE) as i64;
        let want = style | WS_THICKFRAME | WS_MAXIMIZEBOX;
        if want != style {
            // 样式值都在低 32 位，`SetWindowLongW` 足够
            ffi::SetWindowLongW(ctx.hwnd, GWL_STYLE, want as i32);
            ffi::SetWindowPos(
                ctx.hwnd,
                0,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
    true
}

unsafe fn inspect(hwnd: HWND) -> Option<WindowInfo> {
    if ffi::IsWindowVisible(hwnd) == 0 {
        return None;
    }

    let ex_style = ffi::GetWindowLongW(hwnd, GWL_EXSTYLE) as i64;
    if ex_style & WS_EX_TOOLWINDOW != 0 {
        return None;
    }

    let owner = ffi::GetWindow(hwnd, GW_OWNER);
    if owner != 0 && ex_style & WS_EX_APPWINDOW == 0 {
        return None;
    }

    let mut cloaked = 0u32;
    if ffi::DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    ) >= 0
        && cloaked != 0
    {
        return None;
    }

    let rect = visual_bounds(hwnd)?;
    if rect.is_empty() {
        return None;
    }

    let mut pid = 0u32;
    ffi::GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return None;
    }

    Some(WindowInfo {
        hwnd,
        pid,
        title: window_title(hwnd),
        exe: process_exe(pid),
        rect,
        minimized: ffi::IsIconic(hwnd) != 0,
    })
}

/// 取窗口的**可见边框**。
///
/// 为什么不用 `GetWindowRect`：它返回的矩形包含为投影效果预留的不可见边距，
/// 在相邻两块屏幕紧贴时，这段边距会把归属判定推到隔壁屏上去。
/// `DWMWA_EXTENDED_FRAME_BOUNDS` 给的是实际画出来的边框。
unsafe fn visual_bounds(hwnd: HWND) -> Option<Rect> {
    let mut r = Rect::default();
    let hr = ffi::DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut Rect as *mut std::ffi::c_void,
        std::mem::size_of::<Rect>() as u32,
    );
    if hr >= 0 && !r.is_empty() {
        return Some(r);
    }
    let mut fallback = Rect::default();
    if ffi::GetWindowRect(hwnd, &mut fallback) != 0 && !fallback.is_empty() {
        return Some(fallback);
    }
    None
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = ffi::GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    // 标题可能被别的线程改长，多留一点余量
    let mut buf = vec![0u16; len as usize + 2];
    let got = ffi::GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if got <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..got as usize])
}

/// 由 PID 取可执行文件名。拿不到返回 `None`（受保护进程拒绝查询是正常的）。
pub fn process_name(pid: u32) -> Option<String> {
    let name = process_exe(pid);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 由 PID 取可执行文件名。拿不到就返回空串（不是错误：受保护进程拒绝查询是正常的）。
fn process_exe(pid: u32) -> String {
    unsafe {
        let h = ffi::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; 512];
        let mut size = buf.len() as u32;
        let ok = ffi::QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        ffi::CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
    }
}

/// 窗口 → 显示器索引。见 [`monitor::best_for_rect`] 关于面积判定法的说明。
pub fn monitor_index_for(monitors: &[Monitor], window: &WindowInfo) -> Option<usize> {
    monitor::best_for_rect(monitors, &window.rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_falls_back_to_exe() {
        let w = WindowInfo {
            hwnd: 0,
            pid: 1,
            title: String::new(),
            exe: "chrome.exe".into(),
            rect: Rect::new(0, 0, 10, 10),
            minimized: false,
        };
        assert_eq!(w.display_name(), "<无标题> (chrome.exe)");
    }
}
