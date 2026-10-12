//! 差量执行：把 [`crate::plan::Plan`] 落到系统上，并且**记得原来的值**。
//!
//! # 为什么必须有备份
//!
//! `SetPersistedDefaultAudioEndpoint` 写进去的东西是**持久化**的：重启仍在，
//! 并且会出现在用户自己的"音量合成器"界面里。也就是说，本程序改的是一个属于用户的设置。
//! 所以每个进程的原值必须在第一次改动前记下来，退出时还原。
//!
//! # 还原
//!
//! 这个接口没有"清除某个进程的设置"的操作，只有一个全局的
//! `ClearAllPersistedApplicationDefaultEndpoints`（会连别人的一起清掉，本程序绝不调用）。
//! 但**实测发现**：写入一个空 HSTRING 就能把单个进程恢复成"跟随系统默认"，且返回 `S_OK`。
//! （SoundSwitch 的代码里有 `if (string.IsNullOrEmpty(deviceId)) return false;` 的保护，
//! 说明它的作者没试过这条路。）所以：
//!
//! - 原值是"固定到某设备" → 写回原设备；
//! - 原值是"跟随系统默认" → 写空值，同样能完整还原。
//!
//! 因为这是未文档化行为，仍有失败可能，所以还原失败时会记账并在报告里如实说明，
//! 而不是假装已经清理干净。
//!
//! # 多进程应用
//!
//! 一次 `apply` 遍历的是计划的**写入清单**（应用根 + 同名后代，见 [`crate::plan`]），
//! 而不是"一个应用一条"。每个目标进程都有**自己独立的**持久化设置，
//! 所以备份和记账也必须按目标进程分别做 —— 浏览器的 audio utility 子进程和主进程
//! 是两条互不相干的记录。

use std::collections::{HashMap, HashSet};

use crate::audio::policy::PersistedRoute;
use crate::audio::{AudioEnv, Role};
use crate::plan::{Plan, PlannedRoute};
use crate::win::{proc, window};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 已改到目标设备。
    Routed,
    /// 已还原。
    Unrouted,
    /// 本来就在目标设备上，未做改动。
    Unchanged,
    /// 本轮没做成，下次再试（通常是进程还没有音频会话）。
    Deferred,
    /// 出错。
    Failed,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Routed => "已改道",
            Action::Unrouted => "已还原",
            Action::Unchanged => "无需改动",
            Action::Deferred => "待重试",
            Action::Failed => "失败",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Change {
    pub pid: u32,
    pub who: String,
    pub action: Action,
    pub detail: String,
}

impl Change {
    pub fn line(&self) -> String {
        format!("[{}] {} —— {}", self.action.label(), self.who, self.detail)
    }
}

/// 记录"本程序改过哪些进程、每个进程原来的值是什么"。
#[derive(Debug, Default)]
pub struct Reconciler {
    /// pid → 本程序最后成功写入的端点原始 ID。
    applied: HashMap<u32, String>,
    /// pid → 改动前的原始 ID。`None` 表示原来就是"跟随系统默认"。
    backups: HashMap<u32, Option<String>>,
    /// 因为原值是"跟随系统默认"而无法还原的进程。
    leftovers: Vec<u32>,
    /// 上次 restore_all 第二阶段清扫掉的遗留记录数。
    last_swept: usize,
}

impl Reconciler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前被本程序改动过的进程数（多进程应用会占多个）。
    pub fn tracked(&self) -> usize {
        self.applied.len()
    }

    /// 被改动过的进程列表，按 pid 升序。
    pub fn tracked_pids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.applied.keys().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn applied_endpoint(&self, pid: u32) -> Option<&str> {
        self.applied.get(&pid).map(String::as_str)
    }

    pub fn backup_of(&self, pid: u32) -> Option<&Option<String>> {
        self.backups.get(&pid)
    }

    /// 因为接口限制而无法还原的进程。
    pub fn leftovers(&self) -> &[u32] {
        &self.leftovers
    }

    /// 上次 restore_all 第二阶段清扫掉的遗留记录数。
    pub fn last_swept(&self) -> usize {
        self.last_swept
    }

    /// 按计划执行一次差量同步。
    ///
    /// `sessions` 是当前持有音频会话的进程集合。它只用来**避免无效写入**：
    /// 系统对没有音频会话的进程会静默忽略写入（返回 `PROCESS_NO_AUDIO`），
    /// 所以对它们不调用接口，也不报告——否则一个 20 进程的浏览器每轮都会刷屏。
    /// 一旦某个进程真的出声了，下一轮就会被写进去。
    pub fn apply(
        &mut self,
        env: &AudioEnv,
        plan: &Plan,
        roles: &[Role],
        sessions: &HashSet<u32>,
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        if roles.is_empty() {
            changes.push(Change {
                pid: 0,
                who: "配置".into(),
                action: Action::Failed,
                detail: "没有配置任何音频角色，无法写入".into(),
            });
            return changes;
        }

        let write_list = plan.write_list();
        let planned: HashSet<u32> = write_list.iter().map(|(p, _, _)| *p).collect();

        // ---- 1) 计划里不再出现的进程：把原值还回去 ----
        let mut stale: Vec<u32> = self
            .applied
            .keys()
            .copied()
            .filter(|p| !planned.contains(p))
            .collect();
        stale.sort_unstable();

        for pid in stale {
            // 进程已经退出：它那条持久化设置随进程一起没了，不需要（也没法）还原。
            // 直接把账销掉，否则会一直重试到天荒地老。
            if !proc::exists(pid) {
                self.applied.remove(&pid);
                self.backups.remove(&pid);
                changes.push(Change {
                    pid,
                    who: format!("pid {pid}"),
                    action: Action::Unrouted,
                    detail: "进程已退出，无需还原（记录已清除）".into(),
                });
                continue;
            }

            let who = self.who(pid);
            let original = self.backups.get(&pid).cloned().flatten();
            match original {
                Some(id) => {
                    let outcome = env.route_to_id(pid, &id, roles);
                    if outcome.applied() {
                        self.applied.remove(&pid);
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Unrouted,
                            detail: format!("窗口已离开已配置的屏幕，还原为原设备 {id}"),
                        });
                    } else {
                        // 进程可能暂时拿不到音频会话。保留记录，下轮再试。
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Deferred,
                            detail: format!("还原为 {id} 暂未生效：{}", outcome.summary()),
                        });
                    }
                }
                None => {
                    let outcome = env.clear_route(pid, roles);
                    if outcome.applied() {
                        self.applied.remove(&pid);
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Unrouted,
                            detail: format!(
                                "窗口已离开已配置的屏幕，已恢复为跟随系统默认（{}）",
                                outcome.summary()
                            ),
                        });
                    } else if outcome.no_audio_session {
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Deferred,
                            detail: "恢复为跟随系统默认暂未生效（进程没有音频会话），下轮重试目前不影响出声".into(),
                        });
                    } else {
                        // 写空值这条路在这个系统版本上没走通。如实记账，不假装修好了。
                        self.applied.remove(&pid);
                        if !self.leftovers.contains(&pid) {
                            self.leftovers.push(pid);
                        }
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Unrouted,
                            detail: format!(
                                "无法恢复为跟随系统默认（{}）；这条固定记录会留在系统里，\
                                 可在系统「音量合成器」中手动改回",
                                outcome.summary()
                            ),
                        });
                    }
                }
            }
        }

        // ---- 2) 按计划下发（逐目标进程） ----
        for entry in &plan.entries {
            // 整个应用（含全部后代）现在都没出声 → 写进去也会被系统忽略。
            let any_session = entry.targets.iter().any(|p| sessions.contains(p));
            let already_tracked = entry.targets.iter().any(|p| self.applied.contains_key(p));

            if !any_session && !already_tracked {
                changes.push(Change {
                    pid: entry.pid,
                    who: entry.label(),
                    action: Action::Deferred,
                    detail: format!(
                        "当前没有音频会话，暂未写入；等它出声时自动改道到 {}",
                        entry.endpoint.name
                    ),
                });
                continue;
            }

            for &target in &entry.targets {
                let is_child = target != entry.pid;
                // 没出声的后代：跳过。它的路由等它出声那一轮再写。
                if is_child && !sessions.contains(&target) {
                    continue;
                }
                if let Some(c) = self.write_one(env, target, entry, is_child, roles) {
                    changes.push(c);
                }
            }
        }

        changes
    }

    /// 把一个目标进程写到计划指定的设备。
    fn write_one(
        &mut self,
        env: &AudioEnv,
        pid: u32,
        entry: &PlannedRoute,
        is_child: bool,
        roles: &[Role],
    ) -> Option<Change> {
        let who = self.who_of(entry, pid, is_child);

        let already = self
            .applied_endpoint(pid)
            .map(|id| id == entry.endpoint.id.as_str())
            .unwrap_or(false);
        if already {
            return Some(Change {
                pid,
                who,
                action: Action::Unchanged,
                detail: format!("已在 {}", entry.endpoint.name),
            });
        }

        // 第一次改动这个进程之前，把原值记下来
        if !self.backups.contains_key(&pid) {
            let original = match env.get_route(pid, roles[0]) {
                Ok(PersistedRoute::Pinned(id)) => Some(id),
                // NoAudioSession / FollowSystemDefault 都归为"没有单独设置过"
                _ => None,
            };
            self.backups.insert(pid, original);
        }

        let outcome = env.route(pid, &entry.endpoint, roles);
        if outcome.applied() {
            self.applied.insert(pid, entry.endpoint.id.clone());
            let mut detail = format!(
                "{} → {}（{}）",
                entry.monitor_device,
                entry.endpoint.name,
                outcome.summary()
            );
            if is_child {
                detail.push_str("；多进程应用的后代进程，音频实际由它输出");
            }
            if !is_child && !entry.contested_monitors.is_empty() {
                detail.push_str(&format!(
                    "；该应用在 {:?} 上也有窗口，按面积择大",
                    entry.contested_monitors
                ));
            }
            Some(Change {
                pid,
                who,
                action: Action::Routed,
                detail,
            })
        } else if outcome.no_audio_session {
            Some(Change {
                pid,
                who,
                action: Action::Deferred,
                detail: format!(
                    "该进程还没有音频会话，系统未接受写入；下一轮重试（{}）",
                    entry.endpoint.name
                ),
            })
        } else {
            Some(Change {
                pid,
                who,
                action: Action::Failed,
                detail: outcome.summary(),
            })
        }
    }

    /// 退出前把所有改动还原。
    ///
    /// 两阶段：
    /// 1. 精确还原 —— 遍历本会话记录在 `applied` 里的进程，按备份还原。
    /// 2. 遗留清扫 —— 遍历当前所有有音频会话的进程，凡持久化记录指向
    ///    `rule_endpoints`（本工具各绑定配置的目标设备）的，一律写空清除。
    ///
    /// 为什么要第二阶段：`SetPersistedDefaultAudioEndpoint` 的记录是**持久化**
    /// 的，进程退出后仍留在系统里，pid 甚至会被新进程继承。程序重启后
    /// `applied` 是空的，单靠第一阶段永远清不掉历史会话留下的记录。
    /// 只清"指向绑定目标设备"的记录是安全的：那些设备正是本工具会写入的
    /// 目标；用户手动固定到其他设备的记录不受影响。
    pub fn restore_all(
        &mut self,
        env: &AudioEnv,
        roles: &[Role],
        rule_endpoints: &[String],
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut pids: Vec<u32> = self.applied.keys().copied().collect();
        pids.sort_unstable();

        let mut restored: Vec<u32> = Vec::new();
        for pid in pids {
            let who = self.who(pid);
            let original = self.backups.get(&pid).cloned().flatten();
            match original {
                Some(id) => {
                    let outcome = env.route_to_id(pid, &id, roles);
                    if outcome.applied() {
                        self.applied.remove(&pid);
                        restored.push(pid);
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Unrouted,
                            detail: format!("已还原为原设备 {id}"),
                        });
                    } else {
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Failed,
                            detail: format!(
                                "还原失败（进程可能已退出）：{}；记录会保留，下次启动继续尝试",
                                outcome.summary()
                            ),
                        });
                    }
                }
                None => {
                    let outcome = env.clear_route(pid, roles);
                    if outcome.applied() {
                        self.applied.remove(&pid);
                        restored.push(pid);
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Unrouted,
                            detail: "已恢复为跟随系统默认".into(),
                        });
                    } else {
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Failed,
                            detail: format!(
                                "恢复为跟随系统默认失败（进程可能已退出）：{}；记录会保留，下次启动继续尝试",
                                outcome.summary()
                            ),
                        });
                    }
                }
            }
        }

        // ---- 第二阶段：遗留清扫 ----
        let mut swept = 0usize;
        if !rule_endpoints.is_empty() {
            for pid in env.session_pids() {
                if restored.contains(&pid) || self.applied.contains_key(&pid) {
                    continue; // 第一阶段刚处理过，或还在记账里（还原失败的）
                }
                // 任一角色上的固定记录命中绑定目标设备 → 清掉
                let hit = roles.iter().any(|role| {
                    env.get_route(pid, *role)
                        .ok()
                        .and_then(|r| r.device_id().map(String::from))
                        .is_some_and(|id| rule_endpoints.iter().any(|e| *e == id))
                });
                if !hit {
                    continue;
                }
                let outcome = env.clear_route(pid, roles);
                if outcome.applied() {
                    swept += 1;
                    changes.push(Change {
                        pid,
                        who: self.who(pid),
                        action: Action::Unrouted,
                        detail: "已清除遗留记录（历史会话写入，恢复跟随系统默认）".into(),
                    });
                }
            }
        }
        self.last_swept = swept;

        changes
    }

    /// 变更行里显示的"这是谁"。后代进程要标清楚归属，否则用户看到一串
    /// 同名 chrome.exe 会不知道哪条对应哪个应用。
    fn who_of(&self, entry: &PlannedRoute, pid: u32, is_child: bool) -> String {
        if !is_child {
            return entry.label();
        }
        let name = window::process_name(pid).unwrap_or_else(|| "?".into());
        format!("{name} (pid {pid}，{} 的子进程)", entry.label())
    }

    fn who(&self, pid: u32) -> String {
        match crate::win::window::process_name(pid) {
            Some(name) => format!("{name} (pid {pid})"),
            None => format!("pid {pid}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_labels_are_stable() {
        assert_eq!(Action::Routed.label(), "已改道");
        assert_eq!(Action::Deferred.label(), "待重试");
        assert_eq!(Action::Failed.label(), "失败");
    }

    #[test]
    fn change_line_reads_naturally() {
        let c = Change {
            pid: 42,
            who: "chrome.exe (pid 42)".into(),
            action: Action::Routed,
            detail: r"\\.\DISPLAY2 → 扬声器 B".into(),
        };
        assert_eq!(
            c.line(),
            r"[已改道] chrome.exe (pid 42) —— \\.\DISPLAY2 → 扬声器 B"
        );
    }
}
