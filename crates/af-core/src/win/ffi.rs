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
pub type WPARAM = usize;
pub type HWINEVENTHOOK = isize;
/// 注册表键句柄。64 位下是 `HANDLE` 大小；预定义根键用**符号扩展**的 32 位值
/// （winreg.h 里写的是 `(HKEY)(ULONG_PTR)((LONG)0x80000002)`）。
pub type HKEY = isize;

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
///
/// 字段顺序必须严格照抄 winuser.h：`DeviceString` 与 `DeviceID` 之间夹着一个
/// `StateFlags`。漏掉它会整体错位 4 字节，`DeviceID` 读出来以 `StateFlags` 的高
/// 16 位（= 0）开头，于是被当成空串——**不报错、不崩溃，只是永远取不到名字**。
/// 下面有 `const` 断言钉住总长度。
pub const CCHDEVICENAME_128: usize = 128;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DISPLAY_DEVICEW {
    pub cb: u32,
    pub DeviceName: [u16; CCHDEVICENAME],
    pub DeviceString: [u16; CCHDEVICENAME_128],
    pub StateFlags: u32,
    pub DeviceID: [u16; CCHDEVICENAME_128],
    pub DeviceKey: [u16; CCHDEVICENAME_128],
}

// 4 + 64 + 256 + 4 + 256 + 256 = 840
const _: () = assert!(std::mem::size_of::<DISPLAY_DEVICEW>() == 840);

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

/// 与 Win32 `FILETIME` 二进制兼容。单位为 100 ns，起点 1601-01-01。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

impl FILETIME {
    /// 拼成便于比较的 64 位值。只比大小，不做日期换算。
    pub fn as_u64(&self) -> u64 {
        ((self.dwHighDateTime as u64) << 32) | self.dwLowDateTime as u64
    }
}

pub const MAX_PATH: usize = 260;

/// 与 Win32 `PROCESSENTRY32W` 二进制兼容。`szExeFile` 只有文件名，没有路径。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PROCESSENTRY32W {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ProcessID: u32,
    /// `ULONG_PTR`。x64 下是 8 字节，Rust 会自动补上前面的对齐填充。
    pub th32DefaultHeapID: usize,
    pub th32ModuleID: u32,
    pub cntThreads: u32,
    pub th32ParentProcessID: u32,
    pub pcPriClassBase: i32,
    pub dwFlags: u32,
    pub szExeFile: [u16; MAX_PATH],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

/// 与 Win32 `MSG` 二进制兼容。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: UINT,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: DWORD,
    pub pt: POINT,
    /// Vista 起多出来的字段，`GetMessageW` 会往里写，必须有。
    pub lPrivate: DWORD,
}

/// `SetWinEventHook` 的回调原型。`CALLBACK` 即 `extern "system"`。
pub type WINEVENTPROC = unsafe extern "system" fn(
    HWINEVENTHOOK,
    DWORD,
    HWND,
    LONG,
    LONG,
    DWORD,
    DWORD,
);

// -------------------------------------------------------------------- 常量 ----

/// `SetWinEventHook` 的事件范围。对象级事件从 0x8000 起。
pub const EVENT_OBJECT_CREATE: u32 = 0x8000;
pub const EVENT_OBJECT_DESTROY: u32 = 0x8001;
pub const EVENT_OBJECT_SHOW: u32 = 0x8002;
pub const EVENT_OBJECT_HIDE: u32 = 0x8003;
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
pub const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
pub const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
pub const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;

/// `idObject` 为 0 表示"事件主体就是窗口本身"。子对象（光标、菜单项、插入点……）
/// 会以极高的频率报事件，必须在回调里筛掉。
pub const OBJID_WINDOW: i32 = 0;

/// 回调在安装钩子的线程上（通过它的消息循环）被调用，而不是被注入到目标进程里。
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

pub const WM_QUIT: u32 = 0x0012;

pub const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
/// `CreateToolhelp32Snapshot` 失败时返回它。
pub const INVALID_HANDLE_VALUE: isize = -1;

pub const SWP_NOSIZE: UINT = 0x0001;
pub const SWP_NOZORDER: UINT = 0x0004;
pub const SWP_NOACTIVATE: UINT = 0x0010;

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

/// [`EnumDisplayDevicesW`] 的 `dwFlags`：返回**设备接口路径**而不是显示名。
/// 它给的 `DeviceID` 形如 `\\?\DISPLAY#ACM2428#7&1a2b3c4d&0&UID0001#{e6f07b5f-...}`，
/// 中间两段正好是注册表实例键（见 `monitor::edid_from_interface_path`）。
pub const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 0x0000_0001;

// ---- 注册表（读显示器 EDID 用） ----

/// `HKEY_LOCAL_MACHINE`。`0x80000002` 按 32 位有符号解释再符号扩展。
pub const HKEY_LOCAL_MACHINE: HKEY = 0x8000_0002u32 as i32 as HKEY;
/// `KEY_READ` = `STANDARD_RIGHTS_READ | KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS | KEY_NOTIFY`。
pub const KEY_READ: u32 = 0x0002_0019;
pub const ERROR_SUCCESS: i32 = 0;
pub const REG_BINARY: u32 = 3;
/// EDID 基础块 128 字节，最多 3 个扩展块；给点余量防止被超长值撑爆。
pub const EDID_MAX_LEN: u32 = 4 * 1024;

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

    // ---- 窗口事件钩子 ----
    /// 装一个事件钩子。返回 0 表示失败（`GetLastError` 有细节）。
    ///
    /// `WINEVENT_OUTOFCONTEXT` 模式下，回调**不是**被注入到目标进程，而是在
    /// **安装钩子的这条线程**上、由它的消息循环派发。所以装钩子的线程必须泵消息。
    pub fn SetWinEventHook(
        eventMin: DWORD,
        eventMax: DWORD,
        hmodWinEventProc: HANDLE,
        pfnWinEventProc: Option<WINEVENTPROC>,
        idProcess: DWORD,
        idThread: DWORD,
        dwFlags: DWORD,
    ) -> HWINEVENTHOOK;

    pub fn UnhookWinEvent(hWinEventHook: HWINEVENTHOOK) -> BOOL;

    /// 自造一个事件。只在自检里用来验证钩子确实通着。
    pub fn NotifyWinEvent(event: DWORD, hwnd: HWND, idObject: LONG, idChild: LONG);

    // ---- 消息循环 ----
    pub fn GetMessageW(lpMsg: *mut MSG, hWnd: HWND, wMsgFilterMin: UINT, wMsgFilterMax: UINT)
        -> BOOL;
    pub fn TranslateMessage(lpMsg: *const MSG) -> BOOL;
    pub fn DispatchMessageW(lpMsg: *const MSG) -> isize;
    pub fn PostThreadMessageW(idThread: DWORD, Msg: UINT, wParam: WPARAM, lParam: LPARAM) -> BOOL;

    pub fn SetWindowPos(
        hWnd: HWND,
        hWndInsertAfter: HWND,
        X: i32,
        Y: i32,
        cx: i32,
        cy: i32,
        uFlags: UINT,
    ) -> BOOL;
    pub fn GetDesktopWindow() -> HWND;
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
    /// 把当前进程附到父进程的控制台。GUI 子系统的进程默认不带控制台，
    /// 命令行模式下靠它把输出接回来。
    pub fn AttachConsole(dwProcessId: DWORD) -> BOOL;

    pub fn OpenProcess(dwDesiredAccess: DWORD, bInheritHandle: BOOL, dwProcessId: DWORD)
        -> HANDLE;
    pub fn QueryFullProcessImageNameW(
        hProcess: HANDLE,
        dwFlags: DWORD,
        lpExeName: *mut u16,
        lpdwSize: *mut DWORD,
    ) -> BOOL;
    pub fn CloseHandle(hObject: HANDLE) -> BOOL;

    // ---- 进程快照与进程信息 ----
    pub fn CreateToolhelp32Snapshot(dwFlags: DWORD, th32ProcessID: DWORD) -> HANDLE;
    pub fn Process32FirstW(hSnapshot: HANDLE, lppe: *mut PROCESSENTRY32W) -> BOOL;
    pub fn Process32NextW(hSnapshot: HANDLE, lppe: *mut PROCESSENTRY32W) -> BOOL;
    /// 四个出参依次是：创建时间、退出时间、内核态时间、用户态时间。
    /// 我们只用创建时间——`PROCESSENTRY32W` 里的父 pid 可能已被复用，靠时间戳才能识破。
    pub fn GetProcessTimes(
        hProcess: HANDLE,
        lpCreationTime: *mut FILETIME,
        lpExitTime: *mut FILETIME,
        lpKernelTime: *mut FILETIME,
        lpUserTime: *mut FILETIME,
    ) -> BOOL;

    pub fn GetCurrentThreadId() -> DWORD;
    pub fn GetLastError() -> DWORD;
}

#[link(name = "advapi32")]
extern "system" {
    /// 打开注册表子键。只读场景用 `KEY_READ`。
    pub fn RegOpenKeyExW(
        hKey: HKEY,
        lpSubKey: *const u16,
        ulOptions: DWORD,
        samDesired: DWORD,
        phkResult: *mut HKEY,
    ) -> i32;

    /// 读一个值。`lpData` 传 null、`lpcbData` 传 0 时只回填所需长度。
    pub fn RegQueryValueExW(
        hKey: HKEY,
        lpValueName: *const u16,
        lpReserved: *mut u32,
        lpType: *mut u32,
        lpData: *mut u8,
        lpcbData: *mut u32,
    ) -> i32;

    pub fn RegCloseKey(hKey: HKEY) -> i32;
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
