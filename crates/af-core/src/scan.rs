//! 设备扫描的决策逻辑：新旧快照比对 + 记忆治愈 + 自动绑定。
//!
//! 这段逻辑原先长在 `ui.rs::refresh_devices` 里——"何时说话、何时治愈、何时强制
//! 同步"是全程序最容易出回归的编排，却只能活在 GUI 里测不到。提炼成纯函数后，
//! 输入是（配置 + 旧快照 + 新快照 + 模式），输出是"该做的事"，单测直接覆盖；
//! `ui.rs` 只剩 IO：枚举设备、打日志、落盘、触发同步。
//!
//! 自动绑定没有按钮：屏幕的型号名（EDID）出现在某音频端点的友好名里
//! （HDMI/DP 音频的普遍形态，如 `H24T7` 屏 → `H24T7 (NVIDIA …)`）且该屏还没有
//! 绑定时，扫描直接配对生成。设备接入即生效。

use std::collections::{HashMap, HashSet};

use crate::audio::endpoint::AudioEndpoint;
use crate::config::AppConfig;
use crate::win::{monitor, Monitor};

/// 一次扫描的产出。`ui.rs` 照单执行，不做任何决策。
#[derive(Debug, Default)]
pub struct ScanOutcome {
    /// 要写进日志的行，按时间顺序。
    pub log_lines: Vec<String>,
    /// 记忆治愈或自动绑定改动了配置（调用方据此落盘）。
    pub config_changed: bool,
    /// 应立即触发一轮同步（设备集变化、治愈或自动绑定发生——用户刚插上屏，
    /// 不该再等一个完整的轮询周期）。
    pub force_sync: bool,
}

/// 比对一次设备快照并做记忆治愈。
///
/// - `verbose`（启动 / 手动「重新扫描」）：完整列出当前设备；新增项不逐条报
///   （清单里看得到），消失项照报（清单里看不到"少了什么"）。
/// - 非 verbose（周期扫描）：只报变化，否则每几秒刷一遍设备清单。
/// - `endpoints_were_ok` / `endpoints_ok_now`：端点枚举上一轮与这一轮是否成功。
///   失败 → 成功的翻转要报「已恢复」；持续失败不在本函数处理（枚举错误本身
///   在调用方，那里才有错误信息），持续成功不重复报。
///
/// 剖析**不会**因为设备离线而删绑定（见 [`AppConfig::heal_against`]）。
pub fn scan(
    config: &mut AppConfig,
    old_monitors: &[Monitor],
    old_endpoints: &[AudioEndpoint],
    new_monitors: &[Monitor],
    new_endpoints: &[AudioEndpoint],
    verbose: bool,
    endpoints_were_ok: bool,
    endpoints_ok_now: bool,
) -> ScanOutcome {
    let mut out = ScanOutcome::default();

    // ---- 端点枚举从失败中恢复：只在翻转的那一刻说一次 ----
    if endpoints_ok_now && !endpoints_were_ok && !verbose {
        out.log_lines.push("音频设备枚举已恢复".into());
    }

    // ---- 与上一轮比对 ----
    let old_mon: HashSet<&str> = old_monitors.iter().map(|m| m.device.as_str()).collect();
    let new_mon: HashSet<&str> = new_monitors.iter().map(|m| m.device.as_str()).collect();
    let old_ep: HashSet<&str> = old_endpoints.iter().map(|e| e.id.as_str()).collect();
    let new_ep: HashSet<&str> = new_endpoints.iter().map(|e| e.id.as_str()).collect();
    let changed = old_mon != new_mon || old_ep != new_ep;

    // 新增的设备：verbose 模式下面的清单会完整列出，逐条报"已连接"就重复了
    if !verbose {
        for m in new_monitors {
            if !old_mon.contains(m.device.as_str()) {
                out.log_lines
                    .push(format!("显示器已连接：{}", monitor::name_from_device(&m.device)));
            }
        }
        for e in new_endpoints {
            if !old_ep.contains(e.id.as_str()) {
                out.log_lines.push(format!("音频设备已出现：{}", e.name));
            }
        }
    }
    // 消失的设备两种模式都要报：清单里看不到"少了什么"
    for m in old_monitors {
        if !new_mon.contains(m.device.as_str()) {
            out.log_lines.push(format!(
                "显示器已断开：{}（绑定保留，重新接入后自动恢复）",
                monitor::name_from_device(&m.device)
            ));
        }
    }
    for e in old_endpoints {
        if !new_ep.contains(e.id.as_str()) {
            out.log_lines.push(format!("音频设备已移除：{}", e.name));
        }
    }

    if verbose {
        out.log_lines.push(format!("检测到 {} 块显示器", new_monitors.len()));
        for (i, m) in new_monitors.iter().enumerate() {
            out.log_lines.push(format!("  {}", m.label(i)));
        }
        out.log_lines.push(format!("检测到 {} 个音频输出设备", new_endpoints.len()));
        for (i, ep) in new_endpoints.iter().enumerate() {
            out.log_lines.push(format!("  {}", ep.label(i)));
        }
    }

    // ---- 记忆治愈：换编号的显示器、换 ID 的声卡，在这里认回来 ----
    let heal_notes = config.heal_against(new_monitors, new_endpoints);
    let healed = !heal_notes.is_empty();
    out.log_lines.extend(heal_notes);
    out.config_changed = healed;
    out.force_sync = changed || healed;

    // ---- 自动绑定：型号名与端点名匹配、还没有绑定的屏，自动配对 ----
    //
    // 替代旧版的「一键匹配」按钮：设备接入即生效，无需任何操作。保守约束：
    // - 一个端点只被一块屏匹配到时才绑（同型号双屏的音箱重名时分不清谁是谁）；
    // - 稳定标识只在无歧义时记录（与手动添加同一把尺子，见 unique_key_of）；
    // - 已有绑定的屏跳过——周期扫描每几秒跑一次，幂等是硬要求。
    let pairs = crate::config::name_matched_pairs(new_monitors, new_endpoints);
    let mut ep_claims: HashMap<&str, usize> = HashMap::new();
    for (_, _, _, ep) in &pairs {
        *ep_claims.entry(ep.id.as_str()).or_default() += 1;
    }
    for (device, name, _, ep) in &pairs {
        if ep_claims.get(ep.id.as_str()) != Some(&1) {
            continue;
        }
        if config.rule_for_monitor(device, None, name).is_some() {
            continue;
        }
        let key = new_monitors
            .iter()
            .find(|m| m.device.eq_ignore_ascii_case(device))
            .and_then(|m| unique_key_of(m, new_monitors));
        config.upsert_rule(crate::config::ScreenRule::from_pair(device, name, key.as_deref(), ep));
        out.log_lines.push(format!(
            "已自动绑定：{} → {}（按型号名「{name}」匹配）",
            monitor::name_from_device(device),
            ep.name
        ));
        out.config_changed = true;
        out.force_sync = true;
    }

    out
}

/// 该屏的稳定标识，仅当它在线上**唯一**（没有第二块同键的屏）时才可记录。
fn unique_key_of(m: &Monitor, monitors: &[Monitor]) -> Option<String> {
    let k = m.key.as_deref()?;
    let same = monitors.iter().filter(|o| o.key.as_deref() == Some(k)).count();
    (same == 1).then(|| k.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{ep, mon, rule};

    fn cfg_with_rule(monitor_device: &str, key: Option<&str>, endpoint_id: &str) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(rule(monitor_device, "FHD", key, endpoint_id, "FHD"));
        cfg
    }

    #[test]
    fn quiet_scan_reports_changes_only() {
        let mut cfg = AppConfig::default();
        // 旧：D1+D2 / e1；新：D1+D3 / e1+e2
        let old_m = vec![mon(r"\\.\DISPLAY1", None, "A"), mon(r"\\.\DISPLAY2", None, "B")];
        let new_m = vec![mon(r"\\.\DISPLAY1", None, "A"), mon(r"\\.\DISPLAY3", None, "C")];
        let old_e = vec![ep("e1", "X")];
        let new_e = vec![ep("e1", "X"), ep("e2", "Y")];

        let out = scan(&mut cfg, &old_m, &old_e, &new_m, &new_e, false, true, true);

        let joined = out.log_lines.join("\n");
        // 日志行用可读名（name_from_device 把 \\.\DISPLAY3 转成「显示器 3」）
        assert!(joined.contains("显示器 3") && joined.contains("已连接"), "报新增：{joined}");
        assert!(joined.contains("显示器 2") && joined.contains("已断开"), "报消失：{joined}");
        assert!(joined.contains("e2") || joined.contains("Y"), "报新端点：{joined}");
        assert!(joined.contains("已出现"), "报新增：{joined}");
        assert!(!joined.contains("检测到"), "安静模式不该刷清单：{joined}");
        assert!(out.force_sync, "设备集变了要强制同步");
        assert!(!out.config_changed);
    }

    #[test]
    fn verbose_scan_lists_everything_and_suppresses_add_lines() {
        let mut cfg = AppConfig::default();
        let old_m = vec![mon(r"\\.\DISPLAY1", None, "A")];
        let new_m = vec![mon(r"\\.\DISPLAY1", None, "A"), mon(r"\\.\DISPLAY2", None, "B")];
        let old_e: Vec<AudioEndpoint> = Vec::new();
        let new_e = vec![ep("e1", "X")];

        let out = scan(&mut cfg, &old_m, &old_e, &new_m, &new_e, true, true, true);

        let joined = out.log_lines.join("\n");
        assert!(joined.contains("检测到 2 块显示器"), "{joined}");
        assert!(joined.contains("检测到 1 个音频输出设备"), "{joined}");
        assert!(!joined.contains("已连接"), "清单已完整列出，逐条报就重复了：{joined}");
        assert!(!joined.contains("已出现"), "同上：{joined}");
    }

    #[test]
    fn endpoints_recovery_is_reported_only_on_flip() {
        let mut cfg = AppConfig::default();
        let m = vec![mon(r"\\.\DISPLAY1", None, "A")];
        let e = vec![ep("e1", "X")];

        // 失败 → 成功：说一次
        let out = scan(&mut cfg, &m, &e, &m, &e, false, false, true);
        assert!(out.log_lines.iter().any(|l| l.contains("已恢复")));

        // 持续成功：不再说
        let out = scan(&mut cfg, &m, &e, &m, &e, false, true, true);
        assert!(!out.log_lines.iter().any(|l| l.contains("已恢复")));
        // verbose（启动/手动）时即便翻转也不说——完整清单本身就是状态说明
        let out = scan(&mut cfg, &m, &e, &m, &e, true, false, true);
        assert!(!out.log_lines.iter().any(|l| l.contains("已恢复")));
    }

    #[test]
    fn scan_without_change_is_completely_silent() {
        // 幂等性：周期扫描每几秒调一次，没变化就一个字都不说、一个标志都不置
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY1", Some("edid:a"), "e1");
        let m = vec![mon(r"\\.\DISPLAY1", Some("edid:a"), "A")];
        let e = vec![ep("e1", "X")];

        let out = scan(&mut cfg, &m, &e, &m, &e, false, true, true);
        assert!(out.log_lines.is_empty(), "实际：{:?}", out.log_lines);
        assert!(!out.force_sync);
        assert!(!out.config_changed);
    }

    #[test]
    fn scan_auto_binds_matching_screens_without_a_button() {
        // 屏幕型号名出现在端点名里、且还没有绑定 → 扫描时自动生成，无需按钮
        let mut cfg = AppConfig::default();
        let m = vec![mon(r"\\.\DISPLAY1", Some("edid:h"), "H24T7")];
        let e = vec![ep("e1", "H24T7 (NVIDIA High Definition Audio)")];

        let out = scan(&mut cfg, &m, &e, &m, &e, false, true, true);

        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].endpoint_id, "e1");
        assert_eq!(cfg.rules[0].monitor_key.as_deref(), Some("edid:h"), "唯一键要记下来");
        assert!(out.config_changed && out.force_sync);
        assert!(out.log_lines.iter().any(|l| l.contains("已自动绑定")), "{:?}", out.log_lines);

        // 幂等：再扫一遍不重复生成、一个字都不说、不强制同步
        let out2 = scan(&mut cfg, &m, &e, &m, &e, false, true, true);
        assert_eq!(cfg.rules.len(), 1);
        assert!(out2.log_lines.is_empty(), "{:?}", out2.log_lines);
        assert!(!out2.force_sync);
    }

    #[test]
    fn scan_skips_ambiguous_endpoint_claims() {
        // 同型号双屏、音箱同名：两块屏都匹配同一个端点 → 分不清谁是谁，都不自动绑
        let mut cfg = AppConfig::default();
        let m = vec![mon(r"\\.\DISPLAY4", None, "TWIN"), mon(r"\\.\DISPLAY5", None, "TWIN")];
        let e = vec![ep("e1", "TWIN Audio")];

        let out = scan(&mut cfg, &m, &e, &m, &e, false, true, true);

        assert!(cfg.rules.is_empty(), "歧义认领不能自动绑");
        assert!(!out.force_sync);
    }

    #[test]
    fn scan_heals_renamed_monitor_and_flags_save_and_sync() {
        // FHD 屏从 DISPLAY2 换到 DISPLAY3：治愈要发生，配置要落盘，同步要强制
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "e1");
        let old_m = vec![mon(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
        let new_m = vec![mon(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD")];
        let e = vec![ep("e1", "FHD")];

        let out = scan(&mut cfg, &old_m, &e, &new_m, &e, false, true, true);

        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY3", "治愈应重绑");
        assert!(out.config_changed, "治愈改了配置，要落盘");
        assert!(out.force_sync);
        assert!(out.log_lines.iter().any(|l| l.contains("记忆")), "要说明按记忆恢复：{:?}", out.log_lines);
    }
}
