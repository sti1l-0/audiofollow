//! 原始 Win32 FFI 声明。
//!
//! 只声明本项目实际用到的函数与常量。手写而非引入 windows-sys，是为了让依赖树保持为零，
//! 同时把"用到哪些系统能力"这件事显式地摆在代码里。

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use std::ffi::c_void;

pub type BOOL = i32;
pub type HWND = isize;
pub type HMONITOR = isize;
pub type HDC = isize;
pub type HANDLE = isize;
pub type LPARAM = isize;
pub type DWORD = u32;
pub type UINT = u32;
pub type LONG = i32;
pub type HRESULT = i32;

// ------------------------------------------------------------------ 基础结构 --

/// 与 Win32 `RECT` 二进制兼容。坐标为**物理像素**（前提是进程已声明 Per-Monitor V2 DPI 感知）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { left, top, right, bottom }
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn area(&self) -> i64 {
        self.width().max(0) as i64 * self.height().max(0) as i64
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    pub fn center(&self) -> (i32, i32) {
        (self.left + self.width() / 2, self.top + self.height() / 2)
    }

    /// 与另一个矩形的交集；无交集时返回宽高为负或零的矩形。
    pub fn intersection(&self, other: &Rect) -> Rect {
        Rect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }

    pub fn intersection_area(&self, other: &Rect) -> i64 {
        let r = self.intersection(other);
        if r.width() <= 0 || r.height() <= 0 {
            0
        } else {
            r.area()
        }
    }

    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn is_inside(&self, other: &Rect) -> bool {
        self.left >= other.left
            && self.top >= other.top
            && self.right <= other.right
            && self.bottom <= other.bottom
    }
}

/// 与 Win32 `MONITORINFOEXW` 二进制兼容。
pub const CCHDEVICENAME: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MONITORINFOEXW {
    pub cbSize: u32,
    pub rcMonitor: Rect,
    pub rcWork: Rect,
    pub dwFlags: u32,
    pub szDevice: [u16; CCHDEVICENAME],
}

/// 与 Win32 `DISPLAY_DEVICEW` 二进制兼容。用来取显示器的**用户可辨认名称**
/// （`MONITORINFOEXW` 只给 `\\.\DISPLAY1` 这种设备名）。
pub const CCHDEVICENAME_128: usize = 128;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DISPLAY_DEVICEW {
    pub cb: u32,
    pub DeviceName: [u16; CCHDEVICENAME],
    pub DeviceString: [u16; CCHDEVICENAME_128],
    pub DeviceID: [u16; CCHDEVICENAME_128],
    pub DeviceKey: [u16; CCHDEVICENAME_128],
}

/// 与 Win32 `SYSTEMTIME` 二进制兼容。只为日志打时间戳。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SYSTEMTIME {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

// -------------------------------------------------------------------- 常量 ----

pub const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

pub const GWL_EXSTYLE: i32 = -20;
pub const WS_EX_TOOLWINDOW: i64 = 0x0000_0080;
pub const WS_EX_APPWINDOW: i64 = 0x0004_0000;
pub const GW_OWNER: u32 = 4;

/// `DwmGetWindowAttribute` 的属性编号。取"可见边框"而非 `GetWindowRect` 的
/// "含不可见阴影区"矩形——多屏紧密拼接时，阴影区会让归属判定错到隔壁屏。
pub const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
/// 被 UWP 挂起/虚拟桌面隐藏的窗口会置 1，这类窗口不该参与路由。
pub const DWMWA_CLOAKED: u32 = 14;

pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

// --------------------------------------------------------------- extern 块 --

#[link(name = "user32")]
extern "system" {
    pub fn EnumWindows(
        lpEnumFunc: Option<unsafe extern "system" fn(HWND, LPARAM) -> BOOL>,
        lParam: LPARAM,
    ) -> BOOL;

    pub fn EnumDisplayMonitors(
        hdc: HDC,
        lprcClip: *const Rect,
        lpfnEnum: Option<unsafe extern "system" fn(HMONITOR, HDC, *mut Rect, LPARAM) -> BOOL>,
        dwData: LPARAM,
    ) -> BOOL;

    pub fn GetMonitorInfoW(hMonitor: HMONITOR, lpmi: *mut MONITORINFOEXW) -> BOOL;
    pub fn MonitorFromWindow(hwnd: HWND, dwFlags: DWORD) -> HMONITOR;

    /// `iDevNum = 0` 时用 `lpDevice`（形如 `\\.\DISPLAY1`）查它挂着的显示器，
    /// `lpDisplayDevice.DeviceString` 就是型号名。
    pub fn EnumDisplayDevicesW(
        lpDevice: *const u16,
        iDevNum: DWORD,
        lpDisplayDevice: *mut DISPLAY_DEVICEW,
        dwFlags: DWORD,
    ) -> BOOL;

    pub fn IsWindow(hwnd: HWND) -> BOOL;
    pub fn IsWindowVisible(hwnd: HWND) -> BOOL;
    pub fn IsIconic(hwnd: HWND) -> BOOL;

    pub fn GetWindowTextW(hwnd: HWND, lpString: *mut u16, nMaxCount: i32) -> i32;
    pub fn GetWindowTextLengthW(hwnd: HWND) -> i32;
    pub fn GetWindowThreadProcessId(hwnd: HWND, lpdwProcessId: *mut DWORD) -> DWORD;
    pub fn GetWindow(hwnd: HWND, uCmd: UINT) -> HWND;
    pub fn GetWindowRect(hwnd: HWND, lpRect: *mut Rect) -> BOOL;
    pub fn GetWindowLongW(hwnd: HWND, nIndex: i32) -> LONG;

    pub fn SetProcessDPIAware() -> BOOL;
    /// `value` 取 `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`（即 -4）。
    pub fn SetProcessDpiAwarenessContext(value: HANDLE) -> BOOL;
}

#[link(name = "dwmapi")]
extern "system" {
    pub fn DwmGetWindowAttribute(
        hwnd: HWND,
        dwAttribute: DWORD,
        pvAttribute: *mut c_void,
        cbAttribute: DWORD,
    ) -> HRESULT;
}

#[link(name = "kernel32")]
extern "system" {
    pub fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
    pub fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    pub fn GetModuleHandleW(lpModuleName: *const u16) -> *mut c_void;
    pub fn GetCurrentProcessId() -> u32;
    pub fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
    /// 把控制台输出代码页切到 UTF-8，否则中文日志在默认 936 代码页的窗口里是乱码。
    pub fn SetConsoleOutputCP(wCodePageID: UINT) -> BOOL;

    pub fn OpenProcess(dwDesiredAccess: DWORD, bInheritHandle: BOOL, dwProcessId: DWORD)
        -> HANDLE;
    pub fn QueryFullProcessImageNameW(
        hProcess: HANDLE,
        dwFlags: DWORD,
        lpExeName: *mut u16,
        lpdwSize: *mut DWORD,
    ) -> BOOL;
    pub fn CloseHandle(hObject: HANDLE) -> BOOL;
}

#[link(name = "ole32")]
extern "system" {
    pub fn CoInitializeEx(pvReserved: *mut c_void, dwCoInit: DWORD) -> HRESULT;
    pub fn CoUninitialize();
    pub fn CoCreateInstance(
        rclsid: *const crate::audio::com::Guid,
        pUnkOuter: *mut c_void,
        dwClsContext: DWORD,
        riid: *const crate::audio::com::Guid,
        ppv: *mut *mut c_void,
    ) -> HRESULT;
    pub fn CoTaskMemFree(pv: *mut c_void);
    pub fn PropVariantClear(pvar: *mut c_void) -> HRESULT;
}

// -------------------------------------------------------------- 小工具函数 --

/// Rust `&str` → 以 NUL 结尾的 UTF-16。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Rust `&str` → 不带 NUL 的 UTF-16（拼接 HSTRING 时用）。
pub fn wide_raw(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// NUL 结尾的 UTF-16 指针 → `String`，最多读 `max` 个码元。
///
/// # Safety
/// `p` 必须指向一块可读的、以 NUL 结尾的 UTF-16 缓冲区。
pub unsafe fn pwsz(p: *const u16, max: usize) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut v: Vec<u16> = Vec::new();
    let mut q = p;
    while *q != 0 && v.len() < max {
        v.push(*q);
        q = q.add(1);
    }
    String::from_utf16_lossy(&v)
}

/// 从定长 UTF-16 数组里取字符串（遇到 NUL 截断）。
pub fn utf16_arr_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
