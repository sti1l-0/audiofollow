//! 配置持久化。落在 `%APPDATA%\AudioFollow\config.json`。
//!
//! 配置里存的是**用户意图**：哪块屏幕对哪个音频设备。运行期的状态（改过哪些进程、
//! 每个进程原来的值是什么）不落盘——那是临时数据，重启后应当重新判断。
//!
//! # 设备记忆
//!
//! 绑定从来**不因设备离线而删除**：显示器拔掉重插、USB 声卡换口，都是用户迟早会
//! 接回来的东西，删了绑定等于替用户忘掉他配好的组合。离线的绑定只是挂起
//! （界面标注「不在线」），设备回来时由 [`AppConfig::heal_against`] 按稳定标识认出
//! 「同一块屏 / 同一个声卡」，自动把绑定修回可用状态。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audio::{endpoint::AudioEndpoint, Role};
use crate::win::monitor;
use crate::win::Monitor;

/// 治愈逻辑（`heal_against` 与其测试）：认回换编号的显示器、换接口 ID 的声卡。
mod heal;

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
    /// 显示器设备名，形如 `\\.\DISPLAY1`。
    ///
    /// **只在本次连接内稳定**：拔插后系统可能重新编号。跨拔插认出"同一块屏"靠
    /// [`Self::monitor_key`]，设备名由 [`AppConfig::heal_against`] 在扫描时对齐到当前值。
    pub monitor_device: String,
    /// 只用于展示（显示器型号名）。**型号取不到时是空串**，展示时退回「显示器 N」，
    /// 见 [`monitor::name_from_device`]。
    #[serde(default)]
    pub monitor_name: String,
    /// 显示器的稳定标识（EDID 身份哈希，见 [`crate::win::Monitor::key`]）。
    ///
    /// 旧配置里没有这个字段（`None`），此时只能按设备名匹配——和旧版行为一致。
    /// 同型号双屏（EDID 序列号都是 0）会撞同一个键，这种键在上层按"有歧义"处理。
    #[serde(default)]
    pub monitor_key: Option<String>,
    /// 音频端点原始 ID（不带 SWD 前缀）。
    ///
    /// 同一个物理声卡换接口/端口后 ID 会变；名字没变且当前无同名混淆时，
    /// [`AppConfig::heal_against`] 会把这里修正到新 ID。
    pub endpoint_id: String,
    #[serde(default)]
    pub endpoint_name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl ScreenRule {
    pub fn from_pair(
        monitor_device: &str,
        monitor_name: &str,
        monitor_key: Option<&str>,
        endpoint: &AudioEndpoint,
    ) -> Self {
        Self {
            monitor_device: monitor_device.to_string(),
            monitor_name: monitor_name.to_string(),
            monitor_key: monitor_key.map(str::to_string),
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

    /// 这条绑定当前是否真的挂在这块在线显示器上。
    ///
    /// 与路由判定用同一把尺子（设备名 + 键 + 型号名三层，见 [`AppConfig::rule_for_monitor`]），
    /// 界面据此区分「在线」「不在线」和「设备名被别的显示器顶了」三种状态。
    pub fn matches_monitor(&self, m: &Monitor) -> bool {
        rule_matches(self, &m.device, m.key.as_deref(), &m.name)
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
    /// 上次退出时的窗口尺寸（逻辑像素）。下次启动恢复，免得每次都是默认 880×640。
    ///
    /// 旧配置没有这两个字段 → `None` → 用界面声明的默认尺寸。
    #[serde(default)]
    pub window_width: Option<u32>,
    #[serde(default)]
    pub window_height: Option<u32>,
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
            window_width: None,
            window_height: None,
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
        Self::save_to(self, &path)?;
        Ok(path)
    }

    /// 原子地把配置写到指定路径。
    ///
    /// 先写**同目录**的临时文件并刷盘，再 rename 顶替正式文件。直接写正式文件的话，
    /// 进程在写一半时被杀（断电、任务管理器）会留下半截 JSON——下次启动解析失败、
    /// 回退默认配置，**全部绑定（也就是全部记忆）一次性丢光**。rename 顶替对读者
    /// 而言只有"旧文件 / 新文件"两种状态，没有中间态（Windows 上 std 的 rename 带
    /// `MOVEFILE_REPLACE_EXISTING`，可以顶掉已存在的目标）。
    ///
    /// rename 失败时（比如杀软短暂锁着正式文件）退回直接写：这时旧内容反正
    /// 保不住了，把这次改动写进去比整体报错强。
    fn save_to(cfg: &AppConfig, path: &std::path::Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化失败：{e}"))?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let tmp = dir.join(format!(
            "{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("config")
        ));

        let atomic = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?; // 刷到磁盘：rename 成功后断电，文件也是完整的
            drop(f);
            std::fs::rename(&tmp, path)
        })();

        match atomic {
            Ok(()) => Ok(()),
            Err(rename_err) => {
                let _ = std::fs::remove_file(&tmp);
                std::fs::write(path, &text).map_err(|e| {
                    format!("写入失败（原子替换失败：{rename_err}；直写也失败：{e}）")
                })
            }
        }
    }

    /// 某块屏幕上生效的规则。
    ///
    /// 匹配看三层：设备名相等、**键不冲突**、**型号名不冲突**。
    /// 键冲突 = 这个设备名现在被另一块显示器顶着（拔插后系统重新编号）；
    /// 型号名冲突 = 同样的位置换了台别的屏。两种情况都绝不能把旧规则套上去。
    /// 任一侧没有键 / 名字（旧配置、读不到 EDID）时，退回纯设备名匹配。
    pub fn rule_for_monitor(
        &self,
        monitor_device: &str,
        monitor_key: Option<&str>,
        monitor_name: &str,
    ) -> Option<&ScreenRule> {
        self.rules
            .iter()
            .find(|r| rule_matches(r, monitor_device, monitor_key, monitor_name))
    }

    /// 同一块屏只保留一条规则：设备名相同**或**稳定标识相同都会被顶掉。
    ///
    /// 按键顶掉是为了防这种情况：显示器 X 的绑定还挂在旧设备名下（离线期间没来得及
    /// 治愈），用户又手动给 X 现在的名字加了一条——不顶掉就会出现同一块屏两条规则。
    /// 同型号双屏的键在上游（加规则时）已被判为歧义、不会写进规则，所以这里的按键
    /// 顶掉不会误伤双屏各自的两条规则。
    pub fn upsert_rule(&mut self, rule: ScreenRule) {
        self.rules.retain(|r| {
            let same_device = r.monitor_device.eq_ignore_ascii_case(&rule.monitor_device);
            let same_key =
                rule.monitor_key.is_some() && r.monitor_key.as_deref() == rule.monitor_key.as_deref();
            !(same_device || same_key)
        });
        self.rules.push(rule);
    }

    pub fn remove_rule_for_monitor(&mut self, monitor_device: &str) -> bool {
        let before = self.rules.len();
        self.rules
            .retain(|r| !r.monitor_device.eq_ignore_ascii_case(monitor_device));
        self.rules.len() != before
    }

}

/// 规则与一块在线显示器是否就是"同一块屏"。
///
/// 三层判据（见 [`AppConfig::rule_for_monitor`]）：设备名相等、键不冲突、型号名不冲突。
/// 键或名字任一侧缺失时那一层自动放行——这是旧配置与读不到 EDID 时的兼容路径，
/// 不是漏洞：缺失意味着没有反面证据，只有设备名一条信息可用。
fn rule_matches(r: &ScreenRule, device: &str, key: Option<&str>, name: &str) -> bool {
    r.enabled
        && r.monitor_device.eq_ignore_ascii_case(device)
        && keys_compatible(r.monitor_key.as_deref(), key)
        && names_compatible(&r.monitor_name, name)
}

/// 规则里的键与在线显示器的键是否指向同一块屏。
///
/// 任一侧没有键（旧配置没存 / 这次没读到 EDID）→ 只能信设备名，算兼容；
/// 两侧都有且不同 → 设备名是别的显示器顶上的，不算。
fn keys_compatible(rule_key: Option<&str>, live_key: Option<&str>) -> bool {
    match (rule_key, live_key) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// 规则里的型号名与在线显示器的型号名是否矛盾。
///
/// 任一侧为空（规则没存名字 / 这块屏读不到 EDID 型号）→ 算不矛盾；
/// 两侧都有且不同 → 同一个设备名位置上换了一台别的屏，算矛盾。
fn names_compatible(rule_name: &str, live_name: &str) -> bool {
    rule_name.is_empty() || live_name.is_empty() || rule_name == live_name
}

/// 按型号名把在线显示器和音频端点配对，供「一键匹配」用。
///
/// HDMI/DP 音频的普遍形态：显示器的 EDID 型号名（如 `H24T7`）会出现在它挂的
/// 音频端点的友好名里（`H24T7 (NVIDIA High Definition Audio)`）。拿这个规律
/// 配对，能把"每块屏绑它自己的音箱"从手工选 8 次下拉框变成点一下。
///
/// 返回 `(设备名, 型号名, 稳定标识, 端点)`，只收配得上的；某块屏**是否已有绑定**
/// 由调用方判断（这里只管"谁和谁是一对"）。名字匹配不区分大小写。
pub fn name_matched_pairs(
    monitors: &[Monitor],
    endpoints: &[AudioEndpoint],
) -> Vec<(String, String, Option<String>, AudioEndpoint)> {
    let mut out = Vec::new();
    for m in monitors {
        // 没读到型号名的屏（退回「显示器 N」的那种）没有可匹配的锚点
        if m.name.is_empty() {
            continue;
        }
        let needle = m.name.to_lowercase();
        if let Some(ep) = endpoints.iter().find(|e| e.name.to_lowercase().contains(&needle)) {
            out.push((m.device.clone(), m.name.clone(), m.key.clone(), ep.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{ep, mon, rule};

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
        assert_eq!(cfg.rules[0].monitor_key, None, "旧配置没有键，按无键处理");
        assert_eq!(cfg.window_width, None, "旧配置没有窗口尺寸，按默认");
        assert_eq!(cfg.window_height, None);
    }

    #[test]
    fn name_matched_pairs_follows_model_name() {
        // 本机形态：显示器型号名出现在音频端点友好名里
        let monitors = vec![
            mon(r"\\.\DISPLAY1", Some("edid:a"), "H24T7"),
            mon(r"\\.\DISPLAY2", Some("edid:b"), "FHD"),
            mon(r"\\.\DISPLAY3", None, ""), // 没读到型号名 → 无锚点
        ];
        let endpoints = vec![
            ep("{h}", "H24T7 (NVIDIA High Definition Audio)"),
            ep("{f}", "FHD (NVIDIA High Definition Audio)"),
            ep("{u}", "扬声器 (USB Audio Device)"),
        ];
        let pairs = name_matched_pairs(&monitors, &endpoints);
        assert_eq!(pairs.len(), 2, "实际：{pairs:?}");
        assert_eq!(pairs[0].0, r"\\.\DISPLAY1");
        assert_eq!(pairs[0].3.id, "{h}");
        assert_eq!(pairs[1].0, r"\\.\DISPLAY2");
        assert_eq!(pairs[1].3.id, "{f}");
        // 大小写不敏感：型号名与端点名的大小写不一致也要配上
        let odd_mon = vec![mon(r"\\.\DISPLAY1", None, "h24t7")];
        let odd_ep = vec![ep("{x}", "H24T7 (NVIDIA High Definition Audio)")];
        assert_eq!(name_matched_pairs(&odd_mon, &odd_ep).len(), 1);
        // 型号名在所有端点里都找不到 → 这块屏不产生配对
        let lone = vec![mon(r"\\.\DISPLAY1", None, "WEIRD")];
        assert!(name_matched_pairs(&lone, &endpoints).is_empty());
    }

    #[test]
    fn save_replaces_existing_file_and_leaves_no_temp() {
        // 真实文件系统上的往返：目录里先有一份旧配置，再保存新配置，
        // 验证顶替成功、读回一致、临时文件无残留（原子写的三条可观察行为）
        let dir = std::env::temp_dir().join(format!("af-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut first = AppConfig::default();
        first.upsert_rule(rule(r"\\.\DISPLAY1", "A", None, "x", "音箱 A"));
        AppConfig::save_to(&first, &path).unwrap();
        assert!(path.exists(), "首次保存应创建文件");

        let mut second = AppConfig::default();
        second.auto_follow = false;
        second.upsert_rule(rule(r"\\.\DISPLAY2", "B", Some("edid:b"), "y", "耳机"));
        AppConfig::save_to(&second, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert!(!back.auto_follow, "保存应顶替旧内容");
        assert_eq!(back.rules.len(), 1);
        assert_eq!(back.rules[0].monitor_device, r"\\.\DISPLAY2");
        assert_eq!(back.rules[0].monitor_key.as_deref(), Some("edid:b"));
        assert!(!dir.join("config.json.tmp").exists(), "临时文件应被 rename 顶掉");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_display_falls_back_to_monitor_number() {
        // 型号取不到（空串）——两块屏都这样时，必须靠设备名推出「显示器 N」来区分
        let a = rule(r"\\.\DISPLAY1", "", None, "{x}", "扬声器 A");
        let b = rule(r"\\.\DISPLAY2", "", None, "{y}", "扬声器 B");
        assert_eq!(a.describe(), "显示器 1 → 扬声器 A");
        assert_eq!(b.describe(), "显示器 2 → 扬声器 B");
        assert_ne!(a.describe(), b.describe());
    }

    #[test]
    fn rule_display_prefers_live_monitor_label() {
        use crate::win::ffi::Rect;
        let rule = rule(r"\\.\DISPLAY2", "", None, "{x}", "扬声器");
        // 实时列表里 DISPLAY2 排在第一位（比如它是主屏）
        let live = vec![
            Monitor {
                handle: 0,
                device: r"\\.\DISPLAY2".into(),
                key: None,
                name: String::new(),
                rect: Rect::new(0, 0, 1920, 1080),
                work: Rect::new(0, 0, 1920, 1080),
                is_primary: true,
                dpi: 96,
            },
            Monitor {
                handle: 0,
                device: r"\\.\DISPLAY1".into(),
                key: None,
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
        cfg.upsert_rule(rule(r"\\.\DISPLAY1", "A", None, "old", ""));
        cfg.upsert_rule(rule(r"\\.\DISPLAY1", "A", None, "new", ""));
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].endpoint_id, "new");
    }

    #[test]
    fn upsert_replaces_by_key_even_under_another_device_name() {
        // 显示器 X 离线期间没来得及治愈，用户又手动给它（现在的设备名）加了一条。
        // 旧键的那条必须被顶掉，否则同一块屏有两条规则。
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(rule(r"\\.\DISPLAY2", "FHD", Some("edid:0001"), "old", "FHD"));
        cfg.upsert_rule(rule(r"\\.\DISPLAY3", "FHD", Some("edid:0001"), "new", "FHD"));
        assert_eq!(cfg.rules.len(), 1, "同键必须顶掉旧规则");
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY3");
        assert_eq!(cfg.rules[0].endpoint_id, "new");
    }

    #[test]
    fn rule_for_monitor_rejects_key_conflict() {
        // 设备名对上、但键不同 = 这个设备名现在被**另一块**显示器顶上了，不能套用
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(rule(r"\\.\DISPLAY1", "H24T7", Some("edid:aaa"), "x", ""));
        assert!(cfg.rule_for_monitor(r"\\.\DISPLAY1", Some("edid:aaa"), "").is_some());
        assert!(
            cfg.rule_for_monitor(r"\\.\DISPLAY1", Some("edid:bbb"), "").is_none(),
            "键冲突时设备名再像也不能用"
        );
        // 型号名冲突同理：同一个位置换了台别的屏
        assert!(
            cfg.rule_for_monitor(r"\\.\DISPLAY1", Some("edid:aaa"), "别人的屏").is_none(),
            "型号名不同说明位置被别的屏顶了"
        );
        // 任一侧没有键 / 名字 → 退回设备名匹配（旧配置 / 读不到 EDID 的兼容路径）
        assert!(cfg.rule_for_monitor(r"\\.\DISPLAY1", None, "").is_some());
        let mut legacy = AppConfig::default();
        legacy.upsert_rule(rule(r"\\.\DISPLAY1", "", None, "x", ""));
        assert!(legacy.rule_for_monitor(r"\\.\DISPLAY1", Some("edid:bbb"), "随便什么屏").is_some());
    }
}
