//! 测试共用的设备与绑定构造器。仅在测试构建里编译。
//!
//! `config` / `config::heal` / `scan` 的测试都要造 `Monitor` / `AudioEndpoint` /
//! `ScreenRule`，各写一份 8 行的字面量会把断言淹没，也容易漂移。

#![cfg(test)]

use crate::audio::endpoint::AudioEndpoint;
use crate::config::ScreenRule;
use crate::win::ffi::Rect;
use crate::win::Monitor;

/// 造一块显示器。矩形与 DPI 用最常见的默认值——本模块的测试只关心
/// 设备名 / 稳定标识 / 型号名，不关心几何。
pub fn mon(device: &str, key: Option<&str>, name: &str) -> Monitor {
    Monitor {
        handle: 0,
        device: device.into(),
        key: key.map(str::to_string),
        name: name.into(),
        rect: Rect::new(0, 0, 1920, 1080),
        work: Rect::new(0, 0, 1920, 1080),
        is_primary: false,
        dpi: 96,
    }
}

/// 造一个音频端点。
pub fn ep(id: &str, name: &str) -> AudioEndpoint {
    AudioEndpoint { id: id.into(), name: name.into(), is_default: false }
}

/// 造一条绑定。
pub fn rule(
    monitor_device: &str,
    monitor_name: &str,
    key: Option<&str>,
    endpoint_id: &str,
    endpoint_name: &str,
) -> ScreenRule {
    ScreenRule {
        monitor_device: monitor_device.into(),
        monitor_name: monitor_name.into(),
        monitor_key: key.map(str::to_string),
        endpoint_id: endpoint_id.into(),
        endpoint_name: endpoint_name.into(),
        enabled: true,
    }
}
