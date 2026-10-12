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

use std::collections::HashMap;

use crate::audio::policy::PersistedRoute;
use crate::audio::{AudioEnv, Role};
use crate::plan::Plan;

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
    /// pid → 改动前的原始 ID。`None` 表示原来就是"跟随系统默认"（无法还原）。
    backups: HashMap<u32, Option<String>>,
    /// 因为原值是"跟随系统默认"而无法还原的进程。
    leftovers: Vec<u32>,
}

impl Reconciler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前被本程序改动过的进程数。
    pub fn tracked(&self) -> usize {
        self.applied.len()
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

    /// 按计划执行一次差量同步。
    pub fn apply(&mut self, env: &AudioEnv, plan: &Plan, roles: &[Role]) -> Vec<Change> {
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

        let planned: HashMap<u32, &crate::plan::PlannedRoute> =
            plan.entries.iter().map(|e| (e.pid, e)).collect();

        // ---- 1) 计划里不再出现的进程：把原值还回去 ----
        let mut stale: Vec<u32> = self
            .applied
            .keys()
            .copied()
            .filter(|p| !planned.contains_key(p))
            .collect();
        stale.sort_unstable();

        for pid in stale {
            let original = self.backups.get(&pid).cloned().flatten();
            let who = self.who(pid);
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
                        // 进程可能已经退出（拿不到音频会话）。保留记录，下轮再试。
                        changes.push(Change {
                            pid,
                            who,
                            action: Action::Deferred,
                            detail: format!("还原为 {id} 暂未生效：{}", outcome.summary()),
                        });
                    }
                }
                None => {
                    // 原值是"跟随系统默认"。写空字符串即可恢复（实测可用），所以能真正还原干净。
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
                            detail: "恢复为跟随系统默认暂未生效（进程没有音频会话），下轮重试".into(),
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

        // ---- 2) 按计划下发 ----
        for entry in &plan.entries {
            let already = self
                .applied_endpoint(entry.pid)
                .map(|id| id == entry.endpoint.id.as_str())
                .unwrap_or(false);
            if already {
                changes.push(Change {
                    pid: entry.pid,
                    who: entry.label(),
                    action: Action::Unchanged,
                    detail: format!("已在 {}", entry.endpoint.name),
                });
                continue;
            }

            // 第一次改动这个进程之前，把原值记下来
            if !self.backups.contains_key(&entry.pid) {
                let original = match env.get_route(entry.pid, roles[0]) {
                    Ok(PersistedRoute::Pinned(id)) => Some(id),
                    // NoAudioSession / FollowSystemDefault 都归为"没有单独设置过"
                    _ => None,
                };
                self.backups.insert(entry.pid, original);
            }

            let outcome = env.route(entry.pid, &entry.endpoint, roles);
            if outcome.applied() {
                self.applied.insert(entry.pid, entry.endpoint.id.clone());
                let mut detail = format!(
                    "{} → {}（{}）",
                    entry.monitor_device, entry.endpoint.name, outcome.summary()
                );
                if !entry.contested_monitors.is_empty() {
                    detail.push_str(&format!(
                        "；该进程在 {:?} 上也有窗口，按面积择大",
                        entry.contested_monitors
                    ));
                }
                changes.push(Change {
                    pid: entry.pid,
                    who: entry.label(),
                    action: Action::Routed,
                    detail,
                });
            } else if outcome.no_audio_session {
                changes.push(Change {
                    pid: entry.pid,
                    who: entry.label(),
                    action: Action::Deferred,
                    detail: format!(
                        "该进程还没有音频会话，系统未接受写入；下一轮重试（{}）",
                        entry.endpoint.name
                    ),
                });
            } else {
                changes.push(Change {
                    pid: entry.pid,
                    who: entry.label(),
                    action: Action::Failed,
                    detail: outcome.summary(),
                });
            }
        }

        changes
    }

    /// 退出前把所有改动还原。
    pub fn restore_all(&mut self, env: &AudioEnv, roles: &[Role]) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut pids: Vec<u32> = self.applied.keys().copied().collect();
        pids.sort_unstable();

        for pid in pids {
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
        changes
    }

    fn who(&self, pid: u32) -> String {
        match crate::win::window::process_name(pid) {
            Some(name) => format!("{name} (pid {pid})"),
            None => format!("pid {pid}"),
        }
    }
}
