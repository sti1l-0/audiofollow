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

use std::collections::{HashMap, HashSet};
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

    /// 按当前实际在线的设备**治愈**绑定，实现"设备回来后按记忆恢复"。
    ///
    /// 只在真的改动了什么时返回非空说明（调用方据此落盘和提示）；没改动就返回空，
    /// 这样周期扫描可以放心每几秒调一次而不会刷屏。分四个阶段：
    ///
    /// 1. **按键重绑**：规则的 [`ScreenRule::monitor_key`] 命中某块在线显示器
    ///    （键无歧义），但设备名对不上 → 拔插后系统重新编号了，把设备名对齐到当前值。
    ///    这是"记忆认人"的主路径。
    /// 2. **按名认领**：没有键的旧规则（升级前的配置），设备名已经对不上任何在线屏
    ///    （编号变了、或老位置被别的屏顶了），但型号名与某块在线屏一致且无歧义
    ///    （在线屏里这个名字唯一、也没有别的同名规则）→ 认回。
    /// 3. **补键**：无键规则按上面的判据还认得出自己的屏 → 把屏的稳定标识记下来。
    ///    存量配置升级一次后就拥有 1 的记忆能力。
    /// 4. **音频端点换 ID**：规则指向的端点 ID 不在活跃列表里，但恰有一个活跃端点
    ///    与记住的设备名相同 → 同一个声卡换了接口（ID 变了名字没变），更新 ID。
    ///    同名活跃端点多于一个（两个同型号 USB 声卡）时分不清，不动。
    ///
    /// 什么都不删：指向离线设备的绑定原样挂起，等设备回来。
    pub fn heal_against(&mut self, monitors: &[Monitor], endpoints: &[AudioEndpoint]) -> Vec<String> {
        let mut notes = Vec::new();

        // 有歧义的键不参与任何认领：两块在线显示器共用一个键（同型号、EDID 序列号
        // 都是 0）时，谁是谁分不清，动不如不动
        let mut live_key_counts: HashMap<&str, usize> = HashMap::new();
        for m in monitors {
            if let Some(k) = m.key.as_deref() {
                *live_key_counts.entry(k).or_insert(0) += 1;
            }
        }
        let by_key: HashMap<&str, &Monitor> = monitors
            .iter()
            .filter_map(|m| {
                let k = m.key.as_deref()?;
                (live_key_counts.get(k) == Some(&1)).then_some((k, m))
            })
            .collect();
        let dup_rule_keys: HashSet<String> = {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for r in &self.rules {
                if let Some(k) = r.monitor_key.as_deref() {
                    *counts.entry(k).or_insert(0) += 1;
                }
            }
            counts
                .into_iter()
                .filter(|(_, c)| *c > 1)
                .map(|(k, _)| k.to_string())
                .collect()
        };

        // ---- 1) 按键重绑 ----
        for r in &mut self.rules {
            let Some(k) = r.monitor_key.clone() else { continue };
            if dup_rule_keys.contains(&k) {
                continue;
            }
            let Some(m) = by_key.get(k.as_str()).copied() else { continue };
            if m.device.eq_ignore_ascii_case(&r.monitor_device) {
                continue; // 设备名还对得上，无事可做
            }
            let old = r.monitor_device.clone();
            r.monitor_device = m.device.clone();
            if !m.name.is_empty() {
                r.monitor_name = m.name.clone();
            }
            let label = r.monitor_display_in(monitors);
            notes.push(format!(
                "显示器「{label}」重新接入（{} → {}），已按记忆恢复绑定 → {}",
                old, m.device, r.endpoint_display()
            ));
        }

        // ---- 2) 无键旧规则按型号名认领 ----
        //
        // 前提：这条规则现在**认不出**任何在线屏（设备名全对不上）。认得出的不猜。
        // 歧义保护：在线屏里同名的不止一块、或同名规则不止一条 → 宁可挂起。
        let mut adoptions: Vec<(usize, String, String)> = Vec::new();
        for (i, r) in self.rules.iter().enumerate() {
            if r.monitor_key.is_some() || r.monitor_name.is_empty() {
                continue;
            }
            let matches_live =
                |m: &Monitor| rule_matches(r, &m.device, m.key.as_deref(), &m.name);
            if monitors.iter().any(matches_live) {
                continue;
            }
            let same_name: Vec<&Monitor> =
                monitors.iter().filter(|m| m.name == r.monitor_name).collect();
            let [m] = same_name.as_slice() else { continue };
            // 这块屏已有别的规则认领 → 不抢
            let claimed = self.rules.iter().enumerate().any(|(j, o)| {
                j != i && rule_matches(o, &m.device, m.key.as_deref(), &m.name)
            });
            if claimed {
                continue;
            }
            // 同名规则不止这一条 → 分不清哪条该认
            if self.rules.iter().filter(|o| o.monitor_name == r.monitor_name).count() > 1 {
                continue;
            }
            adoptions.push((i, m.device.clone(), m.name.clone()));
        }
        for (i, device, name) in adoptions {
            let r = &mut self.rules[i];
            let old = r.monitor_device.clone();
            r.monitor_device = device;
            r.monitor_name = name;
            let label = r.monitor_display_in(monitors);
            notes.push(format!(
                "按型号名「{label}」认回重新接入的显示器（{} → {}），已恢复绑定 → {}",
                old, r.monitor_device, r.endpoint_display()
            ));
        }

        // ---- 3) 给还认得出设备的旧规则补上稳定标识 ----
        //
        // 让存量配置升级后也拥有按键重绑的能力。键已被别的规则占用（理论上不该有，
        // upsert 会按键去重）或当前有歧义时跳过。
        let taken_keys: HashSet<String> =
            self.rules.iter().filter_map(|r| r.monitor_key.clone()).collect();
        for i in 0..self.rules.len() {
            if self.rules[i].monitor_key.is_some() {
                continue;
            }
            let r = &self.rules[i];
            let Some(m) = monitors.iter().find(|m| {
                rule_matches(r, &m.device, m.key.as_deref(), &m.name)
            }) else {
                continue;
            };
            let Some(k) = m.key.as_ref().filter(|k| live_key_counts.get(k.as_str()) == Some(&1))
            else {
                continue;
            };
            if taken_keys.contains(k.as_str()) {
                continue;
            }
            let label = r.monitor_display_in(monitors);
            let endpoint = r.endpoint_display();
            self.rules[i].monitor_key = Some(k.to_string());
            notes.push(format!(
                "已为绑定「{label} → {endpoint}」记录显示器标识 {k}，今后拔插后按此自动恢复"
            ));
        }

        // ---- 4) 音频端点换 ID ----
        let live_ids: HashSet<&str> = endpoints.iter().map(|e| e.id.as_str()).collect();
        for r in &mut self.rules {
            if live_ids.contains(r.endpoint_id.as_str()) || r.endpoint_name.is_empty() {
                continue;
            }
            let same_name: Vec<&AudioEndpoint> =
                endpoints.iter().filter(|e| e.name == r.endpoint_name).collect();
            let [only] = same_name.as_slice() else { continue };
            let old = r.endpoint_id.clone();
            r.endpoint_id = only.id.clone();
            notes.push(format!(
                "音频设备「{}」以新的接口 ID 重新出现（{} → {}），已更新绑定「{}」的指向",
                r.endpoint_name,
                short_id(&old),
                short_id(&only.id),
                r.monitor_display_in(monitors)
            ));
        }

        notes
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

/// 端点 ID 的日志展示：原始 ID 是 `{0.0.0.00000000}.{guid}` 一长串，截出 guid 的
/// 头几位够认人了。
fn short_id(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    let shown: String = tail.chars().take(8).collect();
    format!("…{shown}")
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

    /// 造一条绑定。测试里到处都要，8 行的字面量会把断言淹没。
    fn rule(
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
            live_monitor(r"\\.\DISPLAY1", Some("edid:a"), "H24T7"),
            live_monitor(r"\\.\DISPLAY2", Some("edid:b"), "FHD"),
            live_monitor(r"\\.\DISPLAY3", None, ""), // 没读到型号名 → 无锚点
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
        let odd_mon = vec![live_monitor(r"\\.\DISPLAY1", None, "h24t7")];
        let odd_ep = vec![ep("{x}", "H24T7 (NVIDIA High Definition Audio)")];
        assert_eq!(name_matched_pairs(&odd_mon, &odd_ep).len(), 1);
        // 型号名在所有端点里都找不到 → 这块屏不产生配对
        let lone = vec![live_monitor(r"\\.\DISPLAY1", None, "WEIRD")];
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

    // ------------------------------------------------------------ 记忆治愈 ----

    use crate::win::ffi::Rect;

    fn live_monitor(device: &str, key: Option<&str>, name: &str) -> Monitor {
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

    fn ep(id: &str, name: &str) -> AudioEndpoint {
        AudioEndpoint { id: id.into(), name: name.into(), is_default: false }
    }

    fn cfg_with_rule(monitor_device: &str, key: Option<&str>, endpoint_id: &str) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(ScreenRule {
            monitor_device: monitor_device.into(),
            monitor_name: "FHD".into(),
            monitor_key: key.map(str::to_string),
            endpoint_id: endpoint_id.into(),
            endpoint_name: "FHD".into(),
            enabled: true,
        });
        cfg
    }

    #[test]
    fn heal_rebinds_returned_monitor_by_key() {
        // FHD 屏原来挂在 DISPLAY2，拔掉重插后系统给了 DISPLAY3 —— 键没变，认得出来
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "ep1");
        let live = vec![
            live_monitor(r"\\.\DISPLAY1", Some("edid:main"), "H24T7"),
            live_monitor(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD"),
        ];
        let notes = cfg.heal_against(&live, &[ep("ep1", "FHD")]);
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY3", "应重绑到新设备名");
        assert_eq!(cfg.rules[0].monitor_name, "FHD");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("记忆"), "说明要讲清楚这是记忆恢复：{}", notes[0]);
        // 治愈是幂等的：再来一遍什么都不改
        assert!(cfg.heal_against(&live, &[ep("ep1", "FHD")]).is_empty());
    }

    #[test]
    fn heal_keeps_rule_for_offline_monitor() {
        // 绑定的显示器离线：什么都不改（旧版行为是直接删掉，现在靠记忆留着）
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "ep1");
        let live = vec![live_monitor(r"\\.\DISPLAY1", Some("edid:main"), "H24T7")];
        let notes = cfg.heal_against(&live, &[ep("ep1", "FHD")]);
        assert!(notes.is_empty());
        assert_eq!(cfg.rules.len(), 1, "离线绑定绝不能被删");
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY2");
    }

    #[test]
    fn heal_skips_ambiguous_keys() {
        // 同型号双屏（序列号都是 0）→ 同一个键 → 分不清，不能重绑
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("model:twin"), "ep1");
        let live = vec![
            live_monitor(r"\\.\DISPLAY4", Some("model:twin"), "TWIN"),
            live_monitor(r"\\.\DISPLAY5", Some("model:twin"), "TWIN"),
        ];
        assert!(cfg.heal_against(&live, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY2", "歧义键不能动");
    }

    #[test]
    fn heal_rebinds_endpoint_by_unique_name() {
        // 声卡换了接口：ID 变了，名字没变，且当前只有一个同名端点 → 认得出
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "{old}");
        let live = vec![live_monitor(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
        let eps = vec![ep("{new}", "FHD"), ep("{other}", "USB")];
        let notes = cfg.heal_against(&live, &eps);
        assert_eq!(cfg.rules[0].endpoint_id, "{new}");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("新的接口 ID"), "说明要写明换 ID 的原因：{}", notes[0]);
        assert!(cfg.heal_against(&live, &eps).is_empty(), "幂等");
    }

    #[test]
    fn heal_leaves_endpoint_alone_when_name_ambiguous_or_id_live() {
        let live = vec![live_monitor(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
        // 两个同名端点（两个同型号 USB 声卡）→ 分不清，不动
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "{old}");
        let ambiguous = vec![ep("{a}", "USB"), ep("{b}", "USB")];
        assert!(cfg.heal_against(&live, &ambiguous).is_empty());
        assert_eq!(cfg.rules[0].endpoint_id, "{old}");
        // 原端点还在线 → 根本不触发换 ID
        let mut cfg2 = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "{still-live}");
        let unchanged = vec![ep("{still-live}", "FHD"), ep("{a}", "USB")];
        assert!(cfg2.heal_against(&live, &unchanged).is_empty());
        assert_eq!(cfg2.rules[0].endpoint_id, "{still-live}");
    }

    #[test]
    fn heal_adopts_keyless_rule_by_unique_name() {
        // 存量配置（没存过稳定标识）：FHD 屏的绑定还挂在 DISPLAY2 名下，重插后变成
        // DISPLAY3。键这条路没有，但型号名唯一 → 按名认回，顺手把这枚键也记下来。
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        let live = vec![
            live_monitor(r"\\.\DISPLAY1", Some("edid:main"), "H24T7"),
            live_monitor(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD"),
        ];
        let notes = cfg.heal_against(&live, &[ep("ep1", "FHD")]);
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY3", "应按型号名认回");
        assert_eq!(
            cfg.rules[0].monitor_key.as_deref(),
            Some("edid:fhd"),
            "认回后要补记稳定标识，下次拔插就走键这条路了"
        );
        assert!(notes.iter().any(|n| n.contains("认回")), "实际说明：{notes:?}");
        assert!(notes.iter().any(|n| n.contains("记录显示器标识")));
        // 之后按键治愈照常工作：再拔插换到 DISPLAY4，靠键认回来
        let live2 = vec![
            live_monitor(r"\\.\DISPLAY1", Some("edid:main"), "H24T7"),
            live_monitor(r"\\.\DISPLAY4", Some("edid:fhd"), "FHD"),
        ];
        let notes2 = cfg.heal_against(&live2, &[ep("ep1", "FHD")]);
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY4");
        assert!(notes2.iter().any(|n| n.contains("记忆")));
        assert!(cfg.heal_against(&live2, &[ep("ep1", "FHD")]).is_empty(), "幂等");
    }

    #[test]
    fn heal_backfills_key_for_online_legacy_rule() {
        // 存量绑定、显示器一直在线：只补记标识，别的不动
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        let live = vec![live_monitor(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
        let notes = cfg.heal_against(&live, &[ep("ep1", "FHD")]);
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY2", "设备名没变就不动");
        assert_eq!(cfg.rules[0].monitor_key.as_deref(), Some("edid:fhd"));
        assert_eq!(notes.len(), 1);
        assert!(cfg.heal_against(&live, &[ep("ep1", "FHD")]).is_empty(), "幂等");
    }

    #[test]
    fn heal_does_not_adopt_when_name_unknown_or_ambiguous() {
        // 型号名对不上任何在线屏（真正的陌生屏）→ 什么都不做
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        let stranger = vec![live_monitor(r"\\.\DISPLAY3", Some("edid:x"), "OTHER")];
        assert!(cfg.heal_against(&stranger, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY2");

        // 两块在线屏同名（同型号双屏）→ 分不清，不认
        let twins = vec![
            live_monitor(r"\\.\DISPLAY4", Some("edid:t1"), "TWIN"),
            live_monitor(r"\\.\DISPLAY5", Some("edid:t2"), "TWIN"),
        ];
        let mut cfg2 = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        cfg2.rules[0].monitor_name = "TWIN".into();
        assert!(cfg2.heal_against(&twins, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg2.rules[0].monitor_device, r"\\.\DISPLAY2");

        // 两条同名规则 → 也分不清该谁认，都不动
        // （第二条规则的端点名故意不同，把端点换 ID 的治愈隔离在这个用例之外）
        let mut cfg3 = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        cfg3.upsert_rule(ScreenRule {
            monitor_device: r"\\.\DISPLAY6".into(),
            monitor_name: "FHD".into(),
            monitor_key: None,
            endpoint_id: "ep2".into(),
            endpoint_name: "USB".into(),
            enabled: true,
        });
        let one_fhd = vec![live_monitor(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD")];
        assert!(cfg3.heal_against(&one_fhd, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg3.rules[0].monitor_device, r"\\.\DISPLAY2");
        assert_eq!(cfg3.rules[1].monitor_device, r"\\.\DISPLAY6");
    }
}
