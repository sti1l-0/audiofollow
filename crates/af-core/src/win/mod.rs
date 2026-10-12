//! Windows 平台的底层能力：DPI 感知、显示器枚举、窗口枚举与归属判定。
//!
//! 全部为手写 FFI，不依赖 windows-sys / windows crate，保持零第三方依赖。

pub mod dpi;
pub mod ffi;
pub mod monitor;
pub mod window;

pub use ffi::Rect;
pub use monitor::Monitor;
pub use window::WindowInfo;

/// 本地时间 `HH:MM:SS`。日志打时间戳用，避免为这点需求引入时间库。
pub fn now_hms() -> String {
    let mut st = ffi::SYSTEMTIME::default();
    unsafe { ffi::GetLocalTime(&mut st) };
    format!("{:02}:{:02}:{:02}", st.hour, st.minute, st.second)
}
