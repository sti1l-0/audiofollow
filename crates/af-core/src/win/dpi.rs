//! DPI 感知。
//!
//! 这件事必须在**创建任何窗口之前**做完，否则整个几何判定都会在缩放显示器上出错：
//! 一个 DPI 不感知的进程拿到的 `GetWindowRect` / 显示器矩形都是被系统虚拟化（缩放）过的，
//! 两套坐标虽然能大致对上，但在两块缩放比不同的屏幕交界处会错开，导致窗口被归到隔壁屏。

use std::ffi::c_void;
use std::sync::OnceLock;

use super::ffi::{self, HMONITOR};

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`（Win10 1703+）。
const PER_MONITOR_AWARE_V2: isize = -4;
/// `PROCESS_PER_MONITOR_DPI_AWARE`（Win8.1+）。
const PROCESS_PER_MONITOR_DPI_AWARE: i32 = 2;
/// `MDT_EFFECTIVE_DPI`。
const MDT_EFFECTIVE_DPI: i32 = 0;

/// `enable()` 的返回值，用于在自检里如实汇报生效方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpiAwareness {
    /// Per-Monitor V2，理想状态。
    PerMonitorV2,
    /// 每显示器 DPI 感知（Win8.1 路径）。
    PerMonitor,
    /// 系统级 DPI 感知（最老的兜底）。
    System,
    /// 全部失败——几何判定可能不准。
    Failed,
}

impl DpiAwareness {
    pub fn as_str(&self) -> &'static str {
        match self {
            DpiAwareness::PerMonitorV2 => "Per-Monitor V2",
            DpiAwareness::PerMonitor => "Per-Monitor (v1)",
            DpiAwareness::System => "System",
            DpiAwareness::Failed => "未设置（几何判定可能不准）",
        }
    }
}

type SetProcessDpiAwarenessFn = unsafe extern "system" fn(i32) -> i32;
type GetDpiForMonitorFn = unsafe extern "system" fn(HMONITOR, i32, *mut u32, *mut u32) -> i32;

struct Shcore {
    set_awareness: Option<SetProcessDpiAwarenessFn>,
    get_dpi_for_monitor: Option<GetDpiForMonitorFn>,
}

fn shcore() -> &'static Shcore {
    static S: OnceLock<Shcore> = OnceLock::new();
    S.get_or_init(|| unsafe {
        let name = ffi::wide("shcore.dll");
        let module = ffi::LoadLibraryW(name.as_ptr());
        if module.is_null() {
            return Shcore { set_awareness: None, get_dpi_for_monitor: None };
        }
        let resolve = |sym: &[u8]| -> Option<*mut c_void> {
            let p = ffi::GetProcAddress(module, sym.as_ptr());
            if p.is_null() {
                None
            } else {
                Some(p)
            }
        };
        Shcore {
            set_awareness: resolve(b"SetProcessDpiAwareness\0")
                .map(|p| std::mem::transmute::<*mut c_void, SetProcessDpiAwarenessFn>(p)),
            get_dpi_for_monitor: resolve(b"GetDpiForMonitor\0")
                .map(|p| std::mem::transmute::<*mut c_void, GetDpiForMonitorFn>(p)),
        }
    })
}

/// 声明 Per-Monitor V2 DPI 感知，逐级降级。进程生命周期内只需调用一次。
pub fn enable() -> DpiAwareness {
    unsafe {
        if ffi::SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) != 0 {
            return DpiAwareness::PerMonitorV2;
        }
        if let Some(f) = shcore().set_awareness {
            if f(PROCESS_PER_MONITOR_DPI_AWARE) == 0 {
                return DpiAwareness::PerMonitor;
            }
        }
        if ffi::SetProcessDPIAware() != 0 {
            return DpiAwareness::System;
        }
    }
    DpiAwareness::Failed
}

/// 取某块显示器的有效 DPI。取不到时返回 96（即 100% 缩放）。
pub fn monitor_dpi(monitor: HMONITOR) -> u32 {
    if let Some(f) = shcore().get_dpi_for_monitor {
        let mut x = 0u32;
        let mut y = 0u32;
        if unsafe { f(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) } == 0 && x > 0 {
            return x;
        }
    }
    96
}

/// DPI → 缩放百分比，用于展示。
pub fn scale_percent(dpi: u32) -> u32 {
    (dpi as u64 * 100 / 96) as u32
}
