//! 路由计划：把「窗口落在哪块屏」翻译成「哪些进程应该走哪个音频设备」。
//!
//! 计划是**纯计算**，不碰系统。真正的写入在 [`crate::reconcile`]。
//!
//! # 归属规则
//!
//! 1. 只看**非最小化、非本程序、且所在屏幕配了规则**的窗口。
//! 2. 同一应用可能有多块屏幕上配了规则的窗口 —— 按**总交集面积**择大。
//!    Windows 层面每个进程只能有一条持久化路由，所以这里必然要做取舍，取舍原则要可解释。
//! 3. 一个应用若在任何已配置屏幕上都没有窗口，计划里就不出现它 —— 上层会把之前改过的还原回去。
//!
//! # 多进程应用（本模块最容易出错的地方）
//!
//! Chrome / Edge / Electron 的**窗口在主进程、音频在子进程**。只写窗口进程的路由，
//! 这类应用完全不生效。所以计划里区分两个概念：
//!
//! - `PlannedRoute::pid`：**应用根**，即窗口所属的那个进程。归属仲裁用它。
//! - `PlannedRoute::targets`：**真正要写路由的进程集合** = 根 + 同名可执行文件的后代。
//!
//! ## 为什么后代必须限定「同名可执行文件」
//!
//! 本机实测（296 个进程）：
//!
//! ```text
//! msedge.exe          pid=1836   后代=19   同名=19   异名=0      ← 全部同名，规则完美覆盖
//! msedgewebview2.exe  pid=11048  后代=5    同名=5    异名=0
//! imapp.exe          pid=3544   后代=15   同名=4    异名=crashpad_handler,imapp_helper×10
//! launcher.exe       pid=8128   后代=29   同名=8    异名=cmd,conhost×9,node×3,pwsh,…
//! ```
//!
//! 最后一行是关键反例：launcher 的子树里挂着一堆**毫不相干**的 cmd / node / pwsh
//! 进程。如果无脑展开全部后代，就会把这些工具进程的音频一起改道。
//! 同理 `svchost.exe` 在本机有 87 个实例，一旦某个 svchost 有了窗口，
//! 无脑展开会把大半个系统卷进来。
//!
//! 限定同名之后：真正的多进程应用（浏览器系、WebView2 系）100% 被覆盖，
//! 而"启动器型"进程零误伤。
//!
//! **已知不覆盖**：宿主程序 + 独立的音频子进程（比如某 App 配 WebView2，
//! 音频实际在 `msedgewebview2.exe` 里）。这类进程与本程序判定的根不同名，
//! 不会被一起改道。计划会用 `notes` 把它们列出来，不假装覆盖到了。

use std::collections::{HashMap, HashSet};

use crate::audio::endpoint::AudioEndpoint;
use crate::config::AppConfig;
use crate::win::{monitor, window, Monitor, ProcTree, WindowInfo};

/// 一个应用的子树最多展开成多少个写入目标。
///
/// 纯防御：正常的浏览器实例几十个进程，但没有上限的话，某些会自我复制的程序
/// 可能让一次同步变成几百次 COM 调用。
const MAX_TARGETS_PER_APP: usize = 64;

/// [`build`] 的输入。字段多，用结构体传，免得参数错位。
pub struct Inputs<'a> {
    pub config: &'a AppConfig,
    pub monitors: &'a [Monitor],
    pub windows: &'a [WindowInfo],
    pub endpoints: &'a [AudioEndpoint],
    /// 进程树。空树（拿不到快照）时退化成"每个窗口进程各自为战"。
    pub tree: &'a ProcTree,
    /// 当前持有音频会话的进程。用来诊断"哪些出声的进程没被接管"。
    pub sessions: &'a [u32],
    pub self_pid: u32,
}

/// 一个应用的目标路由。
#[derive(Debug, Clone)]
pub struct PlannedRoute {
    /// **应用根**：窗口所属的那个进程。归属仲裁与展示都用它。
    pub pid: u32,
    pub exe: String,
    /// 目标显示器在 `monitors` 中的下标。
    pub monitor_index: usize,
    pub monitor_device: String,
    pub monitor_label: String,
    /// 目标音频设备（取自当前活跃端点，因此名称与"是否系统默认"都是实时的）。
    pub endpoint: AudioEndpoint,
    /// 该应用落在目标屏幕上的窗口标题。
    pub windows: Vec<String>,
    /// 该应用落在目标屏幕上的总交集面积。
    pub overlap: i64,
    /// 该应用在其他**已配置**屏幕上也有窗口时，列在这里（表示发生过取舍）。
    pub contested_monitors: Vec<String>,
    /// 真正要写路由的进程集合：根 + 同名可执行文件的后代。至少含根自己。
    pub targets: Vec<u32>,
}

impl PlannedRoute {
    pub fn label(&self) -> String {
        if self.exe.is_empty() {
            format!("pid {}", self.pid)
        } else {
            format!("{} (pid {})", self.exe, self.pid)
        }
    }

    /// 除根之外的目标进程（多进程应用的后代）。
    pub fn child_targets(&self) -> Vec<u32> {
        self.targets.iter().copied().filter(|p| *p != self.pid).collect()
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

    /// 展平成写入清单：`(要写的 pid, 它属于哪个应用, 是不是后代)`。
    ///
    /// 应用根之间不会重叠（[`app_root`] 已经把同名前代折叠进去了），
    /// 所以这里不需要处理冲突。
    pub fn write_list(&self) -> Vec<(u32, &PlannedRoute, bool)> {
        let mut out = Vec::new();
        for e in &self.entries {
            for &t in &e.targets {
                out.push((t, e, t != e.pid));
            }
        }
        out.sort_by_key(|(pid, _, _)| *pid);
        out
    }

    /// 会被改道的全部进程。
    pub fn all_targets(&self) -> HashSet<u32> {
        self.write_list().into_iter().map(|(p, _, _)| p).collect()
    }
}

/// 找一个窗口进程所属的**应用根**。
///
/// 沿祖先链上溯，取"最靠上的、同样是窗口进程、且可执行文件同名"的那个。
/// 同名前代才是同一个应用的不同进程（浏览器的 renderer / audio utility）；
/// 不同名的祖先（启动器、explorer）不是同一个应用，不能合并。
fn app_root(tree: &ProcTree, owner: u32, owners: &HashSet<u32>) -> u32 {
    let Some(exe) = tree.exe_of(owner) else {
        return owner;
    };
    let mut root = owner;
    for a in tree.ancestors(owner) {
        if owners.contains(&a) && tree.exe_of(a) == Some(exe) {
            root = a; // 继续上溯，取最高的那个
        }
    }
    root
}

/// 计算计划。
pub fn build(input: &Inputs) -> Plan {
    let Inputs { config, monitors, windows, endpoints, tree, sessions, self_pid } = *input;
    let mut plan = Plan::default();

    if config.rules.iter().all(|r| !r.enabled) {
        plan.notes.push("没有任何启用的屏幕规则，不会改动任何应用".into());
        return plan;
    }
    if monitors.is_empty() {
        plan.notes.push("没有检测到显示器".into());
        return plan;
    }

    // ---- 1) 哪些进程"拥有"参与判定的窗口 ----
    //
    // 先只收窗口，不聚合面积：要先知道完整的 owner 集合，才能算应用根。
    struct Win {
        monitor_index: usize,
        overlap: i64,
        title: String,
    }
    let mut owner_of_window: Vec<(u32, Win)> = Vec::new();
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
        owner_of_window.push((
            w.pid,
            Win {
                monitor_index: mi,
                overlap: monitors[mi].rect.intersection_area(&w.rect),
                title: w.title.clone(),
            },
        ));
    }

    if owner_of_window.is_empty() {
        if minimized_skipped > 0 {
            plan.notes.push(format!(
                "{minimized_skipped} 个最小化窗口未参与判定（保持其原有路由）"
            ));
        }
        return plan;
    }

    let owners: HashSet<u32> = owner_of_window.iter().map(|(p, _)| *p).collect();

    // ---- 2) 把窗口归到应用根，并按「根 → 显示器」聚合面积 ----
    let mut per_root: HashMap<u32, HashMap<usize, (i64, Vec<String>)>> = HashMap::new();
    let mut root_exe: HashMap<u32, String> = HashMap::new();
    let mut owner_exe: HashMap<u32, String> = HashMap::new();
    for w in windows {
        if owners.contains(&w.pid) {
            owner_exe.entry(w.pid).or_insert_with(|| w.exe.clone());
        }
    }

    for (owner, win) in &owner_of_window {
        let root = app_root(tree, *owner, &owners);
        if let Some(exe) = owner_exe.get(owner) {
            root_exe.entry(root).or_insert_with(|| exe.clone());
        }
        let slot = per_root
            .entry(root)
            .or_default()
            .entry(win.monitor_index)
            .or_insert_with(|| (0, Vec::new()));
        slot.0 += win.overlap;
        if !win.title.is_empty() && slot.1.len() < 4 {
            slot.1.push(win.title.clone());
        }
    }

    // ---- 3) 每个根：面积择大 → 查设备 → 展开写入目标 ----
    for (root, by_monitor) in per_root {
        let mut best: Option<(usize, i64)> = None;
        for (mi, (area, _)) in &by_monitor {
            let cand = (*mi, *area);
            let better = match best {
                None => true,
                Some((bmi, barea)) => {
                    (cand.1, std::cmp::Reverse(cand.0)) > (barea, std::cmp::Reverse(bmi))
                }
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

        let exe = root_exe.get(&root).cloned().unwrap_or_default();

        // 展开写入目标：根本身 + 同名可执行文件的后代
        let mut targets = vec![root];
        let mut truncated = false;
        for d in tree.descendants(root) {
            // 同名才收：见模块头部关于 launcher.exe / svchost.exe 的说明
            if tree.exe_of(d) != Some(exe.as_str()) {
                continue;
            }
            if targets.len() >= MAX_TARGETS_PER_APP {
                truncated = true;
                break;
            }
            targets.push(d);
        }
        targets.sort_unstable();
        targets.dedup();

        let children = targets.iter().filter(|p| **p != root).count();
        if truncated {
            let note = format!(
                "「{}」的后代进程超过 {MAX_TARGETS_PER_APP} 个，只接管了前一部分",
                if exe.is_empty() { format!("pid {root}") } else { exe.clone() }
            );
            if !plan.notes.contains(&note) {
                plan.notes.push(note);
            }
        }
        let _ = children;

        let (overlap, titles) = by_monitor[&best_mi].clone();
        let contested = by_monitor
            .keys()
            .filter(|&&mi| mi != best_mi)
            .map(|&mi| monitor::name_from_device(&monitors[mi].device))
            .collect();

        plan.entries.push(PlannedRoute {
            pid: root,
            exe,
            monitor_index: best_mi,
            monitor_device: monitor.device.clone(),
            monitor_label: monitor.label(best_mi),
            endpoint: endpoint.clone(),
            windows: titles,
            overlap,
            contested_monitors: contested,
            targets,
        });
    }

    plan.entries.sort_by_key(|e| e.pid);

    // ---- 4) 诊断：有没有"在出声但没被接管"的进程 ----
    //
    // 按**可执行文件名**聚合上报，不带 pid：pid 每轮都在变，带着它会让这条说明
    // 每次都不同，上层"只在说明变化时打印"的去重就失效了，日志会被刷屏。
    if !sessions.is_empty() && !plan.entries.is_empty() {
        let covered = plan.all_targets();
        let mut uncovered: HashMap<String, usize> = HashMap::new();
        for &pid in sessions {
            if pid == self_pid || covered.contains(&pid) {
                continue;
            }
            let name = tree
                .exe_of(pid)
                .map(str::to_string)
                .or_else(|| window::process_name(pid))
                .unwrap_or_else(|| "?".into());
            *uncovered.entry(name).or_insert(0) += 1;
        }
        if !uncovered.is_empty() {
            let mut listed: Vec<(String, usize)> = uncovered.into_iter().collect();
            listed.sort();
            let shown = listed
                .iter()
                .take(4)
                .map(|(n, c)| if *c > 1 { format!("{n}×{c}") } else { n.clone() })
                .collect::<Vec<_>>()
                .join("、");
            let more = if listed.len() > 4 {
                format!(" 等 {} 种", listed.len())
            } else {
                String::new()
            };
            plan.notes.push(format!(
                "{shown}{more} 正在出声但未被接管：与任何已改道的应用都不同名，\
                 也没有参与判定的窗口（本程序只按窗口归属改道）"
            ));
        }
    }

    if minimized_skipped > 0 {
        plan.notes.push(format!(
            "{minimized_skipped} 个最小化窗口未参与判定（保持其原有路由）"
        ));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::ffi::Rect;
    use crate::win::proc::ProcEntry;
    use crate::config::ScreenRule;

    fn mon(left: i32, right: i32, name: &str) -> Monitor {
        Monitor {
            handle: 0,
            device: format!(r"\\.\DISPLAY{}", if left == 0 { 1 } else { 2 }),
            name: name.to_string(),
            rect: Rect::new(left, 0, right, 1080),
            work: Rect::new(left, 0, right, 1080),
            is_primary: left == 0,
            dpi: 96,
        }
    }

    fn win(pid: u32, exe: &str, left: i32, right: i32) -> WindowInfo {
        WindowInfo {
            hwnd: pid as isize,
            pid,
            title: format!("{exe} 的窗口"),
            exe: exe.to_string(),
            rect: Rect::new(left, 0, right, 800),
            minimized: false,
        }
    }

    fn ep(id: &str, name: &str) -> AudioEndpoint {
        AudioEndpoint {
            id: id.into(),
            name: name.into(),
            is_default: false,
        }
    }

    fn e(pid: u32, ppid: u32, exe: &str) -> ProcEntry {
        ProcEntry { pid, ppid, exe: exe.to_string() }
    }

    /// 两块屏，各自绑一个设备；左屏规则指向 SPEAKER-A，右屏指向 SPEAKER-B。
    fn config() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(ScreenRule {
            monitor_device: r"\\.\DISPLAY1".into(),
            monitor_name: String::new(),
            endpoint_id: "A".into(),
            endpoint_name: "扬声器 A".into(),
            enabled: true,
        });
        cfg.upsert_rule(ScreenRule {
            monitor_device: r"\\.\DISPLAY2".into(),
            monitor_name: String::new(),
            endpoint_id: "B".into(),
            endpoint_name: "扬声器 B".into(),
            enabled: true,
        });
        cfg
    }

    /// 一棵浏览器树：主进程 200 持有窗口，音频在子进程 202。
    fn browser_tree() -> ProcTree {
        ProcTree::from_entries(vec![
            e(1, 0, "explorer.exe"),
            e(200, 1, "chrome.exe"),
            e(201, 200, "chrome.exe"), // renderer
            e(202, 200, "chrome.exe"), // audio utility ← 真正出声的
            e(203, 200, "chrome.exe"), // gpu
        ])
    }

    fn build_with(
        tree: &ProcTree,
        windows: Vec<WindowInfo>,
        sessions: &[u32],
    ) -> Plan {
        let monitors = vec![mon(0, 1920, ""), mon(1920, 3840, "")];
        let endpoints = vec![ep("A", "扬声器 A"), ep("B", "扬声器 B")];
        let cfg = config();
        build(&Inputs {
            config: &cfg,
            monitors: &monitors,
            windows: &windows,
            endpoints: &endpoints,
            tree,
            sessions,
            self_pid: 9999,
        })
    }

    #[test]
    fn multi_process_app_expands_to_all_same_exe_children() {
        // 窗口在左屏，但音频子在 202
        let p = build_with(&browser_tree(), vec![win(200, "chrome.exe", 100, 900)], &[202]);
        assert_eq!(p.entries.len(), 1);
        let e = &p.entries[0];
        assert_eq!(e.pid, 200, "根应是窗口进程");
        assert_eq!(e.endpoint.id, "A");
        // 关键：三个 chrome.exe 后代都要写，否则音频（202）不会被改道
        assert_eq!(e.targets, vec![200, 201, 202, 203]);
        assert_eq!(e.child_targets(), vec![201, 202, 203]);
    }

    #[test]
    fn unrelated_children_are_not_dragged_in() {
        // launcher 式结构：子树里混着同名和异名进程。
        // 异名的 cmd / node 绝不能被改道——它们和这个应用毫无关系。
        let tree = ProcTree::from_entries(vec![
            e(10, 1, "launcher.exe"),
            e(11, 10, "launcher.exe"), // 同名，收
            e(12, 10, "cmd.exe"),       // 异名，不收
            e(13, 10, "node.exe"),      // 异名，不收
            e(14, 12, "conhost.exe"),   // 异名的孙，更不收
        ]);
        let p = build_with(&tree, vec![win(10, "launcher.exe", 100, 900)], &[11, 12, 13]);
        let e = &p.entries[0];
        assert_eq!(e.targets, vec![10, 11]);
        // 而且要如实说明有出声的进程没被接管
        assert!(
            p.notes.iter().any(|n| n.contains("未被接管")),
            "应给出未覆盖诊断，实际 notes = {:?}",
            p.notes
        );
    }

    #[test]
    fn same_exe_child_window_is_collapsed_into_the_root() {
        // 子进程自己也有窗口、也在另一块屏上。同名 → 视为同一个应用，
        // 按总面积择大，只产生一条路由（Windows 一个进程只能有一条）。
        let tree = ProcTree::from_entries(vec![
            e(200, 1, "chrome.exe"),
            e(201, 200, "chrome.exe"),
        ]);
        let p = build_with(
            &tree,
            vec![
                win(200, "chrome.exe", 100, 900),   // 左屏小窗
                win(201, "chrome.exe", 2000, 3700), // 右屏大窗
            ],
            &[200, 201],
        );
        assert_eq!(p.entries.len(), 1, "同名应折叠成一个应用");
        let e = &p.entries[0];
        assert_eq!(e.pid, 200, "根取最高层的同名窗口进程");
        assert_eq!(e.endpoint.id, "B", "右屏面积更大，应选右屏设备");
        // 取舍要如实标注
        assert_eq!(e.contested_monitors, vec!["显示器 1"]);
        assert_eq!(e.targets, vec![200, 201]);
    }

    #[test]
    fn different_exe_windows_are_independent_apps() {
        // explorer 启动了 notepad。两者都有窗口、在不同屏 —— 必须各自为战，
        // 绝不能因为"notepad 是 explorer 的子进程"就把它们合并。
        let tree = ProcTree::from_entries(vec![
            e(1, 0, "explorer.exe"),
            e(50, 1, "notepad.exe"),
        ]);
        let p = build_with(
            &tree,
            vec![
                win(1, "explorer.exe", 100, 900),
                win(50, "notepad.exe", 2000, 3700),
            ],
            &[1, 50],
        );
        assert_eq!(p.entries.len(), 2, "不同可执行文件是两个独立应用");
        let by_pid: HashMap<u32, &PlannedRoute> =
            p.entries.iter().map(|e| (e.pid, e)).collect();
        assert_eq!(by_pid[&1].endpoint.id, "A");
        assert_eq!(by_pid[&50].endpoint.id, "B");
        // 各自的 targets 不能跨界
        assert_eq!(by_pid[&1].targets, vec![1]);
        assert_eq!(by_pid[&50].targets, vec![50]);
    }

    #[test]
    fn no_tree_degrades_to_single_process_per_window() {
        // 拿不到进程树时（空树），退化成每个窗口进程只改自己 —— 多进程应用会不生效，
        // 但绝不能因此改错别的进程。
        let p = build_with(&ProcTree::empty(), vec![win(200, "chrome.exe", 100, 900)], &[200]);
        assert_eq!(p.entries[0].targets, vec![200]);
    }

    #[test]
    fn window_on_unconfigured_monitor_yields_nothing() {
        let monitors = vec![mon(0, 1920, "")];
        let endpoints = vec![ep("A", "扬声器 A")];
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(ScreenRule {
            // 规则只配了 DISPLAY2，但只有一块 DISPLAY1
            monitor_device: r"\\.\DISPLAY2".into(),
            monitor_name: String::new(),
            endpoint_id: "A".into(),
            endpoint_name: "扬声器 A".into(),
            enabled: true,
        });
        let p = build(&Inputs {
            config: &cfg,
            monitors: &monitors,
            windows: &[win(200, "chrome.exe", 100, 900)],
            endpoints: &endpoints,
            tree: &browser_tree(),
            sessions: &[],
            self_pid: 9999,
        });
        assert!(p.is_empty(), "窗口所在屏幕没有规则，不该有任何改动");
        assert!(p.notes.is_empty(), "不该编造说明，实际 {:?}", p.notes);
    }

    #[test]
    fn offline_endpoint_is_reported_not_silently_dropped() {
        // 规则指向的声卡被拔了：必须出说明，不能静默什么都不做
        let monitors = vec![mon(0, 1920, "")];
        let endpoints = vec![ep("A", "扬声器 A")]; // 只有 A 在线
        let mut cfg = AppConfig::default();
        cfg.upsert_rule(ScreenRule {
            monitor_device: r"\\.\DISPLAY1".into(),
            monitor_name: String::new(),
            endpoint_id: "B-已拔掉".into(),
            endpoint_name: "蓝牙耳机".into(),
            enabled: true,
        });
        let p = build(&Inputs {
            config: &cfg,
            monitors: &monitors,
            windows: &[win(200, "chrome.exe", 100, 900)],
            endpoints: &endpoints,
            tree: &browser_tree(),
            sessions: &[],
            self_pid: 9999,
        });
        assert!(p.is_empty());
        assert!(
            p.notes.iter().any(|n| n.contains("不在线")),
            "应说明设备不在线，实际 {:?}",
            p.notes
        );
    }

    #[test]
    fn disabled_rules_disable_everything() {
        let monitors = vec![mon(0, 1920, "")];
        let endpoints = vec![ep("A", "扬声器 A")];
        let mut cfg = config();
        for r in cfg.rules.iter_mut() {
            r.enabled = false;
        }
        let p = build(&Inputs {
            config: &cfg,
            monitors: &monitors,
            windows: &[win(200, "chrome.exe", 100, 900)],
            endpoints: &endpoints,
            tree: &browser_tree(),
            sessions: &[],
            self_pid: 9999,
        });
        assert!(p.is_empty());
        assert!(p.notes.iter().any(|n| n.contains("没有任何启用")));
    }
}
