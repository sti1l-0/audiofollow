//! 路由计划：把「窗口落在哪块屏」翻译成「哪些进程应该走哪个音频设备」。
//!
//! 计划是**纯计算**，不碰系统。真正的写入在 [`crate::reconcile`]。
//!
//! # 归属规则
//!
//! 1. 只看**非最小化、非本程序、且所在屏幕配了规则**的窗口。
//! 2. 同一进程可能有多块屏幕上配了规则的窗口 —— 按**总交集面积**择大。
//!    Windows 层面每个进程只能有一条持久化路由，所以这里必然要做取舍，取舍原则要可解释。
//! 3. 一个进程若在任何已配置屏幕上都没有窗口，计划里就不出现它 —— 上层会把之前改过的还原回去。

use std::collections::HashMap;

use crate::audio::endpoint::AudioEndpoint;
use crate::config::AppConfig;
use crate::win::{window, Monitor, WindowInfo};

/// 一个进程的目标路由。
#[derive(Debug, Clone)]
pub struct PlannedRoute {
    pub pid: u32,
    pub exe: String,
    /// 目标显示器在 `monitors` 中的下标。
    pub monitor_index: usize,
    pub monitor_device: String,
    pub monitor_label: String,
    /// 目标音频设备（取自当前活跃端点，因此名称与"是否系统默认"都是实时的）。
    pub endpoint: AudioEndpoint,
    /// 该进程落在目标屏幕上的窗口标题。
    pub windows: Vec<String>,
    /// 该进程落在目标屏幕上的总交集面积。
    pub overlap: i64,
    /// 该进程在其他**已配置**屏幕上也有窗口时，列在这里（表示发生过取舍）。
    pub contested_monitors: Vec<String>,
}

impl PlannedRoute {
    pub fn label(&self) -> String {
        if self.exe.is_empty() {
            format!("pid {}", self.pid)
        } else {
            format!("{} (pid {})", self.exe, self.pid)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub entries: Vec<PlannedRoute>,
    /// 影响理解但不构成错误的说明（例如"某块屏配的设备当前不可用"）。
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 计算计划。
///
/// `windows` 传全部顶层窗口即可，本函数会自己排除 `self_pid`。
pub fn build(
    config: &AppConfig,
    monitors: &[Monitor],
    windows: &[WindowInfo],
    endpoints: &[AudioEndpoint],
    self_pid: u32,
) -> Plan {
    let mut plan = Plan::default();

    if config.rules.iter().all(|r| !r.enabled) {
        plan.notes.push("没有任何启用的屏幕规则，不会改动任何应用".into());
        return plan;
    }
    if monitors.is_empty() {
        plan.notes.push("没有检测到显示器".into());
        return plan;
    }

    // pid -> 显示器下标 -> (总交集面积, 窗口标题)
    let mut per_pid: HashMap<u32, HashMap<usize, (i64, Vec<String>)>> = HashMap::new();
    let mut pid_exe: HashMap<u32, String> = HashMap::new();
    let mut minimized_skipped = 0usize;

    for w in windows {
        if w.pid == self_pid {
            continue;
        }
        // 最小化窗口的位置没有意义（坐标会跑到 -32000），
        // 让它保持原路由即可 —— 用户没在"摆放"它。
        if w.minimized {
            minimized_skipped += 1;
            continue;
        }
        let Some(mi) = window::monitor_index_for(monitors, w) else { continue };
        if config.rule_for_monitor(&monitors[mi].device).is_none() {
            continue;
        }
        pid_exe.entry(w.pid).or_insert_with(|| w.exe.clone());
        let slot = per_pid
            .entry(w.pid)
            .or_default()
            .entry(mi)
            .or_insert_with(|| (0, Vec::new()));
        slot.0 += monitors[mi].rect.intersection_area(&w.rect);
        if !w.title.is_empty() && slot.1.len() < 4 {
            slot.1.push(w.title.clone());
        }
    }

    for (pid, by_monitor) in per_pid {
        // 面积择大：交集面积相同则取下标小的（结果稳定，便于测试）
        let mut best: Option<(usize, i64)> = None;
        for (mi, (area, _)) in &by_monitor {
            let cand = (*mi, *area);
            let better = match best {
                None => true,
                Some((bmi, barea)) => (cand.1, std::cmp::Reverse(cand.0)) > (barea, std::cmp::Reverse(bmi)),
            };
            if better {
                best = Some(cand);
            }
        }
        let Some((best_mi, _)) = best else { continue };

        let monitor = &monitors[best_mi];
        let Some(rule) = config.rule_for_monitor(&monitor.device) else { continue };

        // 配置里的设备可能已经拔掉了，用当前活跃端点校验一次
        let Some(endpoint) = endpoints.iter().find(|e| e.id == rule.endpoint_id) else {
            let note = format!(
                "规则「{}」指向的音频设备当前不在线，已跳过（设备 ID {}）",
                monitor.device, rule.endpoint_id
            );
            if !plan.notes.contains(&note) {
                plan.notes.push(note);
            }
            continue;
        };

        let (overlap, titles) = by_monitor[&best_mi].clone();
        let contested = by_monitor
            .keys()
            .filter(|&&mi| mi != best_mi)
            .map(|&mi| monitors[mi].device.clone())
            .collect();

        plan.entries.push(PlannedRoute {
            pid,
            exe: pid_exe.get(&pid).cloned().unwrap_or_default(),
            monitor_index: best_mi,
            monitor_device: monitor.device.clone(),
            monitor_label: monitor.label(best_mi),
            endpoint: endpoint.clone(),
            windows: titles,
            overlap,
            contested_monitors: contested,
        });
    }

    plan.entries.sort_by_key(|e| e.pid);
    if minimized_skipped > 0 {
        plan.notes.push(format!(
            "{minimized_skipped} 个最小化窗口未参与判定（保持其原有路由）"
        ));
    }
    plan
}
