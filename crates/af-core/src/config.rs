//! 配置持久化。落在 `%APPDATA%\AudioFollow\config.json`。
//!
//! 配置里只存**用户意图**：哪块屏幕对哪个音频设备。运行期的状态（改过哪些进程、
//! 每个进程原来的值是什么）不落盘——那是临时数据，重启后应当重新判断。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audio::{endpoint::AudioEndpoint, Role};
use crate::win::monitor;
use crate::win::Monitor;

fn default_true() -> bool {
    true
}
fn default_poll_ms() -> u64 {
    1500
}
fn default_event_driven() -> bool {
    true
}
fn default_version() -> u32 {
    1
}

/// 一条「屏幕 → 音频设备」的绑定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRule {
    /// 显示器设备名，形如 `\\.\DISPLAY1`。这是稳定标识，显示器拔插后编号可能变，
    /// 但同一台机器上它是系统给的、比"第 2 块屏"可靠。
    pub monitor_device: String,
    /// 只用于展示（显示器型号名）。**型号取不到时是空串**，展示时退回「显示器 N」，
    /// 见 [`monitor::name_from_device`]。
    #[serde(default)]
    pub monitor_name: String,
    /// 音频端点原始 ID（不带 SWD 前缀）。
    pub endpoint_id: String,
    #[serde(default)]
    pub endpoint_name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl ScreenRule {
    pub fn from_pair(monitor_device: &str, monitor_name: &str, endpoint: &AudioEndpoint) -> Self {
        Self {
            monitor_device: monitor_device.to_string(),
            monitor_name: monitor_name.to_string(),
            endpoint_id: endpoint.id.clone(),
            endpoint_name: endpoint.name.clone(),
            enabled: true,
        }
    }

    /// 显示器一侧的展示名（不看实时状态）。
    ///
    /// 优先用存下来的型号名；没有就用设备名推（`\\.\DISPLAY2` → `显示器 2`）。
    pub fn monitor_display(&self) -> String {
        if self.monitor_name.is_empty() {
            monitor::name_from_device(&self.monitor_device)
        } else {
            self.monitor_name.clone()
        }
    }

    /// 显示器一侧的展示名，**按当前实际扫到的显示器解析**。
    ///
    /// 比 [`Self::monitor_display`] 准：`显示器 N` 的 N 跟着枚举顺序走，
    /// 所以配置里存的名字可能和界面上当前显示的对不上。只要有实时列表就该走这个。
    pub fn monitor_display_in(&self, monitors: &[Monitor]) -> String {
        monitors
            .iter()
            .enumerate()
            .find(|(_, m)| m.device.eq_ignore_ascii_case(&self.monitor_device))
            .map(|(i, m)| m.label(i))
            .unwrap_or_else(|| self.monitor_display())
    }

    /// 音频设备一侧的展示名。
    pub fn endpoint_display(&self) -> String {
        if self.endpoint_name.is_empty() {
            self.endpoint_id.clone()
        } else {
            self.endpoint_name.clone()
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} → {}{}",
            self.monitor_display(),
            self.endpoint_display(),
            if self.enabled { "" } else { "（已停用）" }
        )
    }

    /// 带实时显示器信息的版本。界面里用这个。
    pub fn describe_in(&self, monitors: &[Monitor]) -> String {
        format!(
            "{} → {}{}",
            self.monitor_display_in(monitors),
            self.endpoint_display(),
            if self.enabled { "" } else { "（已停用）" }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    /// 关掉之后只保留手动应用，不再自动跟随。
    #[serde(default = "default_true")]
    pub auto_follow: bool,
    /// **兜底**轮询周期（毫秒）。
    ///
    /// 事件驱动开着时（默认），真正的触发来自窗口事件钩子，这个周期只用来兜住
    /// 事件覆盖不到的情况：进程重启、路由被别的程序改掉、音频会话刚出现。
    /// 事件驱动关掉时它才恢复成主周期。
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// 是否用窗口事件钩子做实时触发。关掉则退回纯轮询（行为与旧版一致）。
    #[serde(default = "default_event_driven")]
    pub event_driven: bool,
    /// 接管哪些音频角色。默认只接管控制音与媒体，不碰通话音。
    #[serde(default = "Role::default_set")]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub rules: Vec<ScreenRule>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            auto_follow: true,
            poll_ms: default_poll_ms(),
            event_driven: default_event_driven(),
            roles: Role::default_set(),
            rules: Vec::new(),
        }
    }
}

impl AppConfig {
    /// 配置目录。取不到 `%APPDATA%` 时退回到当前目录，保证程序仍然能跑。
    pub fn dir() -> PathBuf {
        match std::env::var_os("APPDATA") {
            Some(appdata) => PathBuf::from(appdata).join("AudioFollow"),
            None => PathBuf::from(".audiofollow"),
        }
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    /// 读取配置。返回 `(配置, 警告)`——读不出来不是致命错误，用默认值启动并给出提示。
    pub fn load() -> (Self, Option<String>) {
        let path = Self::path();
        if !path.exists() {
            return (
                Self::default(),
                Some(format!("配置文件不存在，已使用默认配置：{}", path.display())),
            );
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<AppConfig>(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (
                    Self::default(),
                    Some(format!("配置解析失败（{}），已使用默认配置：{}", e, path.display())),
                ),
            },
            Err(e) => (
                Self::default(),
                Some(format!("配置读取失败（{}），已使用默认配置", e)),
            ),
        }
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
        let path = dir.join("config.json");
        let text = serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败：{e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("写入失败：{e}"))?;
        Ok(path)
    }

    /// 某块屏幕上生效的规则。
    pub fn rule_for_monitor(&self, monitor_device: &str) -> Option<&ScreenRule> {
        self.rules
            .iter()
            .find(|r| r.monitor_device.eq_ignore_ascii_case(monitor_device) && r.enabled)
    }

    /// 同一块屏只保留一条规则。
    pub fn upsert_rule(&mut self, rule: ScreenRule) {
        self.rules
            .retain(|r| !r.monitor_device.eq_ignore_ascii_case(&rule.monitor_device));
        self.rules.push(rule);
    }

    pub fn remove_rule_for_monitor(&mut self, monitor_device: &str) -> bool {
        let before = self.rules.len();
        self.rules
            .retain(|r| !r.monitor_device.eq_ignore_ascii_case(monitor_device));
        self.rules.len() != before
    }

    /// 清掉指向已不存在设备的规则（显示器被拔掉、声卡被移除之后）。
    pub fn prune_missing(&mut self, monitors: &[&str], endpoint_ids: &[&str]) -> Vec<String> {
        let mut dropped = Vec::new();
        self.rules.retain(|r| {
            let ok_monitor = monitors.iter().any(|d| d.eq_ignore_ascii_case(&r.monitor_device));
            let ok_endpoint = endpoint_ids.iter().any(|id| *id == r.endpoint_id);
            if !ok_monitor || !ok_endpoint {
                dropped.push(r.describe());
            }
            ok_monitor && ok_endpoint
        });
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips() {
        let cfg = AppConfig::default();
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.roles, cfg.roles);
        assert!(back.auto_follow);
        assert!(back.rules.is_empty());
    }

    #[test]
    fn missing_fields_get_defaults() {
        // 只写了 rules，其余字段应该用默认值补齐（向前兼容旧配置）
        let text = r#"{"rules":[{"monitor_device":"\\\\.\\DISPLAY2","endpoint_id":"{x}"}]}"#;
        let cfg: AppConfig = serde_json::from_str(text).unwrap();
        assert!(cfg.auto_follow);
        assert_eq!(cfg.poll_ms, 1500);
        assert!(cfg.event_driven, "旧配置没有这个字段，应当默认开启事件驱动");
        assert_eq!(cfg.rules.len(), 1);
        assert!(cfg.rules[0].enabled);
        assert_eq!(cfg.rules[0].monitor_name, "");
    }

    #[test]
    fn rule_display_falls_back_to_monitor_number() {
        // 型号取不到（空串）——两块屏都这样时，必须靠设备名推出「显示器 N」来区分
        let a = ScreenRule {
            monitor_device: r"\\.\DISPLAY1".into(),
            monitor_name: String::new(),
            endpoint_id: "{x}".into(),
            endpoint_name: "扬声器 A".into(),
            enabled: true,
        };
        let b = ScreenRule {
            monitor_device: r"\\.\DISPLAY2".into(),
            monitor_name: String::new(),
            endpoint_id: "{y}".into(),
            endpoint_name: "扬声器 B".into(),
            enabled: true,
        };
        assert_eq!(a.describe(), "显示器 1 → 扬声器 A");
        assert_eq!(b.describe(), "显示器 2 → 扬声器 B");
        assert_ne!(a.describe(), b.describe());
    }

    #[test]
    fn rule_display_prefers_live_monitor_label() {
        use crate::win::ffi::Rect;
        let rule = ScreenRule {
            monitor_device: r"\\.\DISPLAY2".into(),
            monitor_name: String::new(),
            endpoint_id: "{x}".into(),
            endpoint_name: "扬声器".into(),
            enabled: true,
        };
        // 实时列表里 DISPLAY2 排在第一位（比如它是主屏）
        let live = vec![
            Monitor {
                handle: 0,
                device: r"\\.\DISPLAY2".into(),
                name: String::new(),
                rect: Rect::new(0, 0, 1920, 1080),
                work: Rect::new(0, 0, 1920, 1080),
                is_primary: true,
                dpi: 96,
            },
            Monitor {
                handle: 0,
                device: r"\\.\DISPLAY1".into(),
                name: "MON-A".into(),
                rect: Rect::new(1920, 0, 4480, 1440),
                work: Rect::new(1920, 0, 4480, 1440),
                is_primary: false,
                dpi: 96,
            },
        ];
        // 配置里存的是 DISPLAY2，实时排第一 → 应显示「显示器 1」，而不是靠字符串推的「显示器 2」
        assert!(rule.describe_in(&live).starts_with("显示器 1 · 1920×1080 · 主屏 →"));
        // 显示器不在线时退回字符串推导
        assert!(rule.describe_in(&[]).starts_with("显示器 2 →"));
    }

    #[test]
    fn upsert_replaces_same_monitor() {
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(ScreenRule {
            monitor_device: r"\\.\DISPLAY1".into(),
            monitor_name: "A".into(),
            endpoint_id: "old".into(),
            endpoint_name: String::new(),
            enabled: true,
        });
        cfg.upsert_rule(ScreenRule {
            monitor_device: r"\\.\DISPLAY1".into(),
            monitor_name: "A".into(),
            endpoint_id: "new".into(),
            endpoint_name: String::new(),
            enabled: true,
        });
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].endpoint_id, "new");
    }
}
