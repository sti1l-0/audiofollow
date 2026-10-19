//! 设备记忆的「治愈」：认回换编号的显示器、换接口 ID 的声卡。
//!
//! 从 `config.rs` 拆出来的原因：`config.rs` 承担序列化 + 匹配判据 + 一键配对三件事
//! 之后再装下这套四阶段状态机就九百多行了。治愈是独立的一块决策逻辑，单独成模块
//! 后，`config.rs` 只剩"数据 + 判据"，这里只有"怎么认人"。

use std::collections::{HashMap, HashSet};

use super::{rule_matches, AppConfig};
use crate::audio::endpoint::AudioEndpoint;
use crate::win::Monitor;

impl AppConfig {
    /// 按当前实际在线的设备**治愈**绑定，实现"设备回来后按记忆恢复"。
    ///
    /// 只在真的改动了什么时返回非空说明（调用方据此落盘和提示）；没改动就返回空，
    /// 这样周期扫描可以放心每几秒调一次而不会刷屏。分四个阶段：
    ///
    /// 1. **按键重绑**：规则的 [`crate::config::ScreenRule::monitor_key`] 命中某块
    ///    在线显示器（键无歧义），但设备名对不上 → 拔插后系统重新编号了，把设备名
    ///    对齐到当前值。这是"记忆认人"的主路径。
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

/// 端点 ID 的日志展示：原始 ID 是 `{0.0.0.00000000}.{guid}` 一长串，截出 guid 的
/// 头几位够认人了。
fn short_id(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    let shown: String = tail.chars().take(8).collect();
    format!("…{shown}")
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
    fn heal_rebinds_returned_monitor_by_key() {
        // FHD 屏原来挂在 DISPLAY2，拔掉重插后系统给了 DISPLAY3 —— 键没变，认得出来
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "ep1");
        let live = vec![
            mon(r"\\.\DISPLAY1", Some("edid:main"), "H24T7"),
            mon(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD"),
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
        let live = vec![mon(r"\\.\DISPLAY1", Some("edid:main"), "H24T7")];
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
            mon(r"\\.\DISPLAY4", Some("model:twin"), "TWIN"),
            mon(r"\\.\DISPLAY5", Some("model:twin"), "TWIN"),
        ];
        assert!(cfg.heal_against(&live, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY2", "歧义键不能动");
    }

    #[test]
    fn heal_rebinds_endpoint_by_unique_name() {
        // 声卡换了接口：ID 变了，名字没变，且当前只有一个同名端点 → 认得出
        let mut cfg = cfg_with_rule(r"\\.\DISPLAY2", Some("edid:fhd"), "{old}");
        let live = vec![mon(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
        let eps = vec![ep("{new}", "FHD"), ep("{other}", "USB")];
        let notes = cfg.heal_against(&live, &eps);
        assert_eq!(cfg.rules[0].endpoint_id, "{new}");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("新的接口 ID"), "说明要写明换 ID 的原因：{}", notes[0]);
        assert!(cfg.heal_against(&live, &eps).is_empty(), "幂等");
    }

    #[test]
    fn heal_leaves_endpoint_alone_when_name_ambiguous_or_id_live() {
        let live = vec![mon(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
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
            mon(r"\\.\DISPLAY1", Some("edid:main"), "H24T7"),
            mon(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD"),
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
            mon(r"\\.\DISPLAY1", Some("edid:main"), "H24T7"),
            mon(r"\\.\DISPLAY4", Some("edid:fhd"), "FHD"),
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
        let live = vec![mon(r"\\.\DISPLAY2", Some("edid:fhd"), "FHD")];
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
        let stranger = vec![mon(r"\\.\DISPLAY3", Some("edid:x"), "OTHER")];
        assert!(cfg.heal_against(&stranger, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg.rules[0].monitor_device, r"\\.\DISPLAY2");

        // 两块在线屏同名（同型号双屏）→ 分不清，不认
        let twins = vec![
            mon(r"\\.\DISPLAY4", Some("edid:t1"), "TWIN"),
            mon(r"\\.\DISPLAY5", Some("edid:t2"), "TWIN"),
        ];
        let mut cfg2 = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        cfg2.rules[0].monitor_name = "TWIN".into();
        assert!(cfg2.heal_against(&twins, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg2.rules[0].monitor_device, r"\\.\DISPLAY2");

        // 两条同名规则 → 也分不清该谁认，都不动
        // （第二条规则的端点名故意不同，把端点换 ID 的治愈隔离在这个用例之外）
        let mut cfg3 = cfg_with_rule(r"\\.\DISPLAY2", None, "ep1");
        cfg3.upsert_rule(rule(r"\\.\DISPLAY6", "FHD", None, "ep2", "USB"));
        let one_fhd = vec![mon(r"\\.\DISPLAY3", Some("edid:fhd"), "FHD")];
        assert!(cfg3.heal_against(&one_fhd, &[ep("ep1", "FHD")]).is_empty());
        assert_eq!(cfg3.rules[0].monitor_device, r"\\.\DISPLAY2");
        assert_eq!(cfg3.rules[1].monitor_device, r"\\.\DISPLAY6");
    }
}
