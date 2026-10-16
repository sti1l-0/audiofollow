//! 主界面与常驻逻辑。
//!
//! 三个线程各司其职：
//! - **主线程**：slint 的窗口事件循环。界面状态、COM 调用、路由决策都在这里（因此不需要跨线程共享 COM 对象）。
//! - **托盘线程**：`tao` 消息循环 + `tray-icon`，菜单事件通过 channel 发回主线程。
//! - **事件钩子线程**：`SetWinEventHook` 的消息循环，回调只累加一个原子计数。
//!   见 [`af_core::win::hook`] 关于"为什么必须单独一条线程"的说明。
//!
//! # 触发同步的三条路径
//!
//! 1. **窗口事件**（主路径）：钩子线程收到事件 → 置脏 → 主线程 100 ms 醒来时发现脏了，
//!    等手停下来（去抖 250 ms，上限 1 s）再同步。拖动窗口后大约 250 ms 内声音就换了设备。
//! 2. **托盘菜单**"立即重新应用"。
//! 3. **兜底轮询**：事件覆盖不到的情况 —— 进程重启、路由被别的程序改掉、
//!    某个应用刚出声。周期见 [`FALLBACK_POLL_MS`]。
//!
//! 关掉事件驱动（配置或界面）就退化成纯轮询，行为与早期版本一致。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use af_core::audio::com::Apartment;
use af_core::audio::endpoint::AudioEndpoint;
use af_core::audio::AudioEnv;
use af_core::config::{AppConfig, ScreenRule};
use af_core::plan;
use af_core::reconcile::{Action, Change, Reconciler};
use af_core::win::hook::WindowEventHook;
use af_core::win::startup::{self, StartupState};
use af_core::win::{self, dpi::DpiAwareness, window, Monitor, ProcTree};

use crate::tray::{self, TrayEvent};

slint::include_modules!();

/// 定时器醒来间隔。托盘事件与窗口事件都是在这个节奏里被收走的。
const PUMP_MS: u64 = 100;
/// 日志最多保留多少行。
const LOG_LIMIT: usize = 400;
/// 轮询周期下限，防止配置被改成"每秒几百次"。
const MIN_POLL_MS: u64 = 300;
/// 事件到达后等多久再同步。
///
/// 拖动窗口时 `EVENT_OBJECT_LOCATIONCHANGE` 是**每帧**一条，收到就同步的话
/// 一次拖动会触发几百轮。每来一条事件就重新计时，等手停下来再动作。
const DEBOUNCE_MS: u64 = 250;
/// 去抖的等待上限。
///
/// 有些窗口会持续产生事件（动画、标题闪烁）。没有这个上限的话，
/// 只要事件不断，同步就永远等不到"安静下来"的那一刻。
const MAX_DEBOUNCE_MS: u64 = 1000;
/// 事件驱动模式下兜底轮询的周期。
///
/// 事件能覆盖"用户摆窗口"，覆盖不了"应用自己重启后路由丢了"或"某个应用刚开始出声"。
/// 这个周期就是给那几种情况收口的。
const FALLBACK_POLL_MS: u64 = 4000;

/// 一批待处理的窗口事件。
#[derive(Debug, Clone, Copy)]
struct Dirty {
    /// 这批事件里最早的那个时刻。用来算去抖上限。
    first: Instant,
    /// 最新一条事件的时刻。用来算"手停下来多久了"。
    last: Instant,
}

struct AppState {
    config: AppConfig,
    /// 本进程要用的 COM 套间模型。GUI 进程是 STA —— 见 [`run`] 里的说明。
    apartment: Apartment,
    env: Option<AudioEnv>,
    env_error: Option<String>,
    monitors: Vec<Monitor>,
    endpoints: Vec<AudioEndpoint>,
    reconciler: Reconciler,
    log: Vec<LogRow>,
    last_sync: Instant,
    /// 已经收到退出指令：还原做完了，正在等托盘线程收工。
    /// 这之后**不允许**再动任何路由 —— 否则刚还回去的记录会被重新写一遍。
    quitting: bool,
    /// 开机自动启动当前是开是关。
    ///
    /// **真相源在注册表里**（用户可以绕过我们从任务管理器里禁用它），
    /// 这个字段只是启动时读一次的缓存，给界面用。
    startup: bool,
    /// 不直接用，靠 Drop 保证托盘句柄活着。
    _tray: tray::TrayHandle,

    // ---- 事件驱动 ----
    /// 窗口事件钩子。`None` 表示不可用（退回轮询）。
    hook: Option<WindowEventHook>,
    /// 待处理的窗口事件（去抖用）。
    dirty: Option<Dirty>,
    /// 累计处理过的事件数，只用于展示。
    events_total: usize,

    // ---- 计划里的说明，用来在变化时提示一次 ----
    plan_notes: Vec<String>,
    logged_notes: Vec<String>,
    /// 每个进程上一轮的动作。只在动作**发生变化**时打印，避免每轮刷屏。
    last_action: HashMap<u32, Action>,

    // 下面几个是为了**只在内容变化时**才把模型推给界面。
    // 如果每 100ms 重建一次下拉框的 model，用户正在选的那一项会被冲掉。
    pushed_monitors: Vec<String>,
    pushed_endpoints: Vec<String>,
    /// 绑定行的签名（monitor, endpoint, enabled）。比较用，避免重建 model。
    pushed_rules: Vec<(String, String, bool)>,
    pushed_log_len: usize,
}

impl AppState {
    fn new(tray_handle: tray::TrayHandle, apartment: Apartment) -> Self {
        let (config, warn) = AppConfig::load();
        let mut me = Self {
            config,
            apartment,
            env: None,
            env_error: None,
            monitors: Vec::new(),
            endpoints: Vec::new(),
            reconciler: Reconciler::new(),
            log: Vec::new(),
            last_sync: Instant::now() - Duration::from_secs(3600),
            quitting: false,
            startup: false,
            _tray: tray_handle,
            hook: None,
            dirty: None,
            events_total: 0,
            plan_notes: Vec::new(),
            logged_notes: Vec::new(),
            last_action: HashMap::new(),
            pushed_monitors: Vec::new(),
            pushed_endpoints: Vec::new(),
            pushed_rules: Vec::new(),
            pushed_log_len: usize::MAX,
        };
        if let Some(w) = warn {
            me.log(w);
        }
        me.log(format!("配置文件：{}", AppConfig::path().display()));
        me.init_startup();
        me.rescan();
        me.install_hook();
        me
    }

    /// 读一次开机启动状态，顺手把失效的路径修好。
    ///
    /// 注册表里存的是绝对路径，程序换目录（尤其是 scoop 升级到新的版本化目录）之后
    /// 这条记录会静默失效。每次启动都对齐一次，这个问题就不会攒到用户发现。
    fn init_startup(&mut self) {
        match startup::sync_to_current_exe() {
            Ok(StartupState::Disabled) => {}
            Ok(StartupState::Enabled) => {
                self.startup = true;
                self.log("开机自动启动：已开启");
            }
            Ok(StartupState::Stale(old)) => {
                self.startup = true;
                self.log(format!(
                    "开机自动启动：已开启。记录里的程序路径已失效（{old}），\
                     已按当前路径更新"
                ));
            }
            Err(e) => self.log(format!("读取开机启动状态失败：{e}")),
        }
    }

    fn log(&mut self, msg: impl Into<String>) {
        let time = win::now_hms();
        let msg = msg.into();
        println!("{time}  {msg}");
        self.log.push(LogRow { time: time.into(), msg: msg.into() });
        if self.log.len() > LOG_LIMIT {
            let drop = self.log.len() - LOG_LIMIT;
            self.log.drain(0..drop);
        }
    }

    // ---------------------------------------------------------- 事件驱动 ----

    fn install_hook(&mut self) {
        if !self.config.event_driven {
            self.log(format!(
                "事件驱动已在配置里关闭，使用轮询：{} ms",
                self.config.poll_ms.max(MIN_POLL_MS)
            ));
            return;
        }
        match WindowEventHook::install() {
            Ok(h) => {
                self.log(format!(
                    "窗口事件钩子已启用（{} 条），窗口移动后约 {} ms 内响应；\
                     另有 {} ms 兜底轮询",
                    h.hooked_count(),
                    DEBOUNCE_MS,
                    FALLBACK_POLL_MS
                ));
                self.hook = Some(h);
            }
            Err(e) => {
                self.log(format!(
                    "窗口事件钩子不可用（{e}），退回轮询：{} ms",
                    self.config.poll_ms.max(MIN_POLL_MS)
                ));
            }
        }
    }

    /// 收走钩子线程攒下的事件，标记"有事情发生了"。
    fn pump_events(&mut self) {
        let Some(hook) = self.hook.as_ref() else { return };
        let n = hook.take_events();
        if n == 0 {
            return;
        }
        self.events_total += n;
        let now = Instant::now();
        match &mut self.dirty {
            Some(d) => d.last = now,
            None => self.dirty = Some(Dirty { first: now, last: now }),
        }
    }

    /// 该不该现在同步一次。
    fn maybe_sync(&mut self) {
        if !self.config.auto_follow || self.env.is_none() {
            return;
        }

        if let Some(d) = self.dirty {
            let quiet = d.last.elapsed() >= Duration::from_millis(DEBOUNCE_MS);
            let capped = d.first.elapsed() >= Duration::from_millis(MAX_DEBOUNCE_MS);
            if quiet || capped {
                self.dirty = None;
                self.tick();
            }
            // 事件驱动模式下不再叠加兜底轮询：刚同步过，没必要紧接着再来一次
            return;
        }

        if self.due_for_fallback() {
            self.tick();
        }
    }

    fn due_for_fallback(&self) -> bool {
        let period = if self.config.event_driven && self.hook.is_some() {
            FALLBACK_POLL_MS
        } else {
            self.config.poll_ms.max(MIN_POLL_MS)
        };
        self.last_sync.elapsed() >= Duration::from_millis(period)
    }

    // ---------------------------------------------------------- 设备扫描 ----

    fn rescan(&mut self) {
        self.monitors = win::monitor::enumerate();
        self.log(format!("检测到 {} 块显示器", self.monitors.len()));
        let labels: Vec<String> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| m.label(i))
            .collect();
        for l in labels {
            self.log(format!("  {l}"));
        }

        if self.env.is_none() {
            match AudioEnv::init_in(self.apartment) {
                Ok(env) => {
                    self.log(format!(
                        "音频接口就绪（{}，运行时发现的 IID = {}）",
                        self.apartment.label(),
                        env.policy().picked_iid()
                    ));
                    self.env = Some(env);
                    self.env_error = None;
                }
                Err(e) => {
                    let msg = format!("音频接口初始化失败：{e}");
                    self.env_error = Some(msg.clone());
                    self.log(msg);
                }
            }
        }

        if let Some(env) = self.env.as_ref() {
            match env.endpoints() {
                Ok(eps) => {
                    self.log(format!("检测到 {} 个音频输出设备", eps.len()));
                    for (i, ep) in eps.iter().enumerate() {
                        self.log(format!("  {}", ep.label(i)));
                    }
                    self.endpoints = eps;
                }
                Err(e) => self.log(format!("枚举音频设备失败：{e}")),
            }
        }

        self.prune_rules();
    }

    /// 清掉指向已不存在设备的绑定。
    ///
    /// **必须加保护**：音频设备枚举可能因为服务未启动、接口初始化失败而瞬时返回空列表，
    /// 那种情况下按"设备不见了"去删绑定，会把用户配置一次性清光。所以只在
    /// "确实扫到了设备"时才清理。
    fn prune_rules(&mut self) {
        if self.monitors.is_empty() || self.endpoints.is_empty() {
            return;
        }
        let mdevs: Vec<&str> = self.monitors.iter().map(|m| m.device.as_str()).collect();
        let eids: Vec<&str> = self.endpoints.iter().map(|e| e.id.as_str()).collect();
        let dropped = self.config.prune_missing(&mdevs, &eids);
        if !dropped.is_empty() {
            self.log(format!(
                "已移除 {} 条失效绑定：{}",
                dropped.len(),
                dropped.join("；")
            ));
            if let Err(e) = self.config.save() {
                self.log(format!("保存配置失败：{e}"));
            }
        }
    }

    // ---------------------------------------------------------- 差量同步 ----

    fn reconcile_now(&mut self) -> Vec<Change> {
        // 进程树与会话列表每轮都要重新拍：窗口一拖、进程一起一落就变了。
        let windows = window::enumerate_app_windows();
        let tree = ProcTree::snapshot();

        let (notes, changes) = {
            let Some(env) = self.env.as_ref() else {
                return Vec::new();
            };
            let sessions = env.session_pids();
            let held: HashSet<u32> = sessions.iter().copied().collect();
            let p = plan::build(&plan::Inputs {
                config: &self.config,
                monitors: &self.monitors,
                windows: &windows,
                endpoints: &self.endpoints,
                tree: &tree,
                sessions: &sessions,
                self_pid: std::process::id(),
            });
            let roles = self.config.roles.clone();
            let changes = self.reconciler.apply(env, &p, &roles, &held);
            (p.notes, changes)
        };

        self.plan_notes = notes;
        changes
    }

    /// 跑一轮同步。
    ///
    /// 只把"动作发生了变化"的行写进日志：每次同步都会对每个目标进程产生一行
    /// `Unchanged`，全打出来就是每几秒刷屏一次。
    fn tick(&mut self) {
        self.last_sync = Instant::now();
        let changes = self.reconcile_now();

        for c in changes {
            let prev = self.last_action.insert(c.pid, c.action);
            if c.action == Action::Unchanged || prev == Some(c.action) {
                continue;
            }
            self.log(c.line());
        }

        // 计划里的说明只在"出现新说明"时提示一次
        let notes = self.plan_notes.clone();
        if notes != self.logged_notes {
            for n in &notes {
                if !self.logged_notes.contains(n) {
                    self.log(format!("说明：{n}"));
                }
            }
            self.logged_notes = notes;
        }
    }

    // ---------------------------------------------------------- 用户动作 ----

    fn add_rule(&mut self, monitor_index: usize, endpoint_index: usize) -> String {
        let Some(m) = self.monitors.get(monitor_index) else {
            return "没有可用的显示器".into();
        };
        let Some(ep) = self.endpoints.get(endpoint_index) else {
            return "没有可用的音频设备".into();
        };
        let rule = ScreenRule::from_pair(&m.device, &m.name, ep);
        let desc = rule.describe();
        self.config.upsert_rule(rule);
        match self.config.save() {
            Ok(_) => {
                self.last_sync = Instant::now() - Duration::from_secs(3600); // 立刻生效
                format!("已添加绑定：{desc}")
            }
            Err(e) => format!("已添加绑定：{desc}，但保存失败：{e}"),
        }
    }

    fn remove_rule(&mut self, index: usize) -> String {
        let Some(rule) = self.config.rules.get(index) else {
            return "没有选中任何绑定".into();
        };
        let desc = rule.describe();
        let device = rule.monitor_device.clone();
        self.config.remove_rule_for_monitor(&device);
        let saved = self.config.save().map(|_| ()).map_err(|e| e.to_string());
        self.last_sync = Instant::now() - Duration::from_secs(3600);
        // 绑定少了之后，原先被改道的进程需要还原。清掉去重记录，
        // 让下一轮如实报告它们的新动作。
        self.last_action.clear();
        match saved {
            Ok(()) => format!("已删除绑定：{desc}"),
            Err(e) => format!("已删除绑定：{desc}，但保存失败：{e}"),
        }
    }

    fn set_auto_follow(&mut self, on: bool) -> String {
        self.config.auto_follow = on;
        if on {
            self.last_sync = Instant::now() - Duration::from_secs(3600);
        }
        let _ = self._tray.set_auto_follow(on);
        match self.config.save() {
            Ok(_) => format!("自动跟随已{}", if on { "开启" } else { "关闭" }),
            Err(e) => format!("自动跟随已{}，但保存失败：{e}", if on { "开启" } else { "关闭" }),
        }
    }

    /// 开关开机自动启动。
    ///
    /// 不做"乐观更新"：`self.startup` 只在注册表真的写成功之后才改。
    /// 写失败时界面会被 [`Self::sync_to_ui`] 拨回原状 —— 开关的可信度比手感重要。
    fn set_startup(&mut self, on: bool) -> String {
        if self.startup == on {
            return format!("开机自动启动已经是{}状态", if on { "开启" } else { "关闭" });
        }
        match startup::set_enabled(on) {
            Ok(()) => {
                self.startup = on;
                if on {
                    let line = std::env::current_exe()
                        .map(|p| startup::command_line(&p))
                        .unwrap_or_else(|_| "（取当前程序路径失败）".into());
                    format!("已设为开机自动启动：{line}（登录后只驻留托盘，不弹设置窗口）")
                } else {
                    "已取消开机自动启动".into()
                }
            }
            Err(e) => format!("设置开机自动启动失败：{e}（开关已拨回）"),
        }
    }

    fn set_event_driven(&mut self, on: bool) -> String {
        if self.config.event_driven == on {
            return format!("事件驱动已经是{}状态", if on { "开启" } else { "关闭" });
        }
        self.config.event_driven = on;
        if on {
            if self.hook.is_none() {
                self.install_hook();
            }
        } else {
            // Drop 会 PostThreadMessage(WM_QUIT) 停掉钩子线程
            self.hook = None;
            self.dirty = None;
            self.log(format!(
                "已切换到纯轮询：{} ms",
                self.config.poll_ms.max(MIN_POLL_MS)
            ));
        }
        self.last_sync = Instant::now() - Duration::from_secs(3600);
        match self.config.save() {
            Ok(_) => String::new(), // install_hook 已经打过日志了
            Err(e) => format!("保存配置失败：{e}"),
        }
    }

    fn restore_all(&mut self) -> String {
        let Some(env) = self.env.as_ref() else {
            return "音频接口不可用，无法还原".into();
        };
        let roles = self.config.roles.clone();
        // 第二阶段（遗留清扫）的目标设备集合 = 所有绑定里配置过的端点。
        let rule_endpoints: Vec<String> = self
            .config
            .rules
            .iter()
            .map(|r| r.endpoint_id.clone())
            .collect();
        let changes = self.reconciler.restore_all(env, &roles, &rule_endpoints);
        let done = changes.len();
        for c in &changes {
            self.log(c.line());
        }
        self.last_action.clear();
        let swept = self.reconciler.last_swept();
        let leftovers = self.reconciler.leftovers().to_vec();
        if done == 0 && swept == 0 {
            let has_rules = !rule_endpoints.is_empty();
            return if has_rules {
                "没有需要还原的改动：本会话没有改道任何进程，\
                 也没有发现指向绑定设备的遗留记录"
                    .into()
            } else {
                "没有需要还原的改动：本会话没有改道任何进程（也还没有绑定）".into()
            };
        }
        let mut msg = format!("已还原 {done} 个进程的音频路由");
        if swept > 0 {
            msg.push_str(&format!("，另清除 {swept} 条历史遗留记录"));
        }
        if !leftovers.is_empty() {
            msg.push_str(&format!(
                "；另有 {} 个进程原是「跟随系统默认」，接口无法写回空值，\
                 其固定记录会留在系统里（可在系统音量合成器里手动改回）",
                leftovers.len()
            ));
        }
        msg
    }

    fn shutdown(&mut self) {
        // 从这里起进入退出流程：定时器那边不再同步任何路由，
        // 否则刚还回去的记录会被下一轮 maybe_sync 重新写一遍。
        self.quitting = true;
        println!("---- 退出，开始还原 ----");
        let msg = self.restore_all();
        println!("{msg}");
    }

    /// 用户点了窗口的关闭按钮。
    ///
    /// 这只是把窗口藏起来，**不是退出** —— 退出要走托盘菜单的「退出并还原」，
    /// 那条路会先把改过的音频路由还回去。藏窗口时什么都不用做：路由照旧跟着走，
    /// 托盘图标也还在。唯一要做的是留一句话，免得用户以为程序已经退了
    /// （下次打开窗口时能在日志里看到）。
    fn note_window_hidden(&mut self) {
        if self.quitting {
            return;
        }
        self.log("窗口已关闭，程序仍在后台运行：右键托盘图标可再次打开设置，或退出并还原");
    }

    /// 处理来自托盘线程的事件。
    ///
    /// 这里在**主线程**上跑（由下面的定时器把 channel 里的事件取出来），
    /// 所以可以放心碰界面状态和 COM。
    fn handle_tray(&mut self, ev: TrayEvent, ui: &AppWindow) {
        match ev {
            TrayEvent::OpenSettings => {
                let _ = ui.show();
            }
            TrayEvent::ToggleAutoFollow => {
                // 以主线程的配置为唯一真相源取反；托盘上那个勾选框只是显示。
                let on = !self.config.auto_follow;
                let msg = self.set_auto_follow(on);
                self.log(msg);
            }
            TrayEvent::Reapply => {
                self.log("托盘触发：立即重新应用");
                self.tick();
            }
            TrayEvent::Quit => {
                self.shutdown();
                // 还原已经落地，让托盘线程收工 —— 它退出事件循环时 tao 会直接
                // 以 0 结束进程。不走 slint 的退出信号：那条路是异步的，而此刻
                // 我们还在定时器回调里，只要它没被处理就退不掉。
                self._tray.request_quit();
            }
        }
    }

    // ---------------------------------------------------------- 界面同步 ----

    fn headline(&self) -> String {
        if let Some(err) = &self.env_error {
            return format!("音频接口不可用：{err}");
        }
        let trigger = if self.hook.is_some() {
            "实时事件驱动".to_string()
        } else if self.config.event_driven {
            format!("事件钩子不可用，轮询 {} ms", self.config.poll_ms.max(MIN_POLL_MS))
        } else {
            format!("轮询 {} ms", self.config.poll_ms.max(MIN_POLL_MS))
        };
        format!(
            "{} 块显示器 · {} 个音频输出设备 · {} 条绑定 · 已改道 {} 个进程 · {}{}",
            self.monitors.len(),
            self.endpoints.len(),
            self.config.rules.len(),
            self.reconciler.tracked(),
            trigger,
            if self.config.auto_follow {
                ""
            } else {
                "（自动跟随已关闭）"
            }
        )
    }

    fn sync_to_ui(&mut self, ui: &AppWindow) {
        // slint 的 `[string]` 对应 SharedString，不是 String，这里统一转一次
        let to_model = |lines: Vec<String>| -> slint::ModelRc<slint::SharedString> {
            let v: Vec<slint::SharedString> = lines.into_iter().map(Into::into).collect();
            slint::ModelRc::new(slint::VecModel::from(v))
        };

        let ml: Vec<String> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| m.label(i))
            .collect();
        if ml != self.pushed_monitors {
            self.pushed_monitors = ml.clone();
            ui.set_monitor_lines(to_model(ml));
        }

        let el: Vec<String> = self
            .endpoints
            .iter()
            .enumerate()
            .map(|(i, e)| e.label(i))
            .collect();
        if el != self.pushed_endpoints {
            self.pushed_endpoints = el.clone();
            ui.set_endpoint_lines(to_model(el));
        }

        // 绑定行按**当前实际扫到的显示器**解析展示名，这样「显示器 N」的编号
        // 和上面的下拉框始终一致。屏幕不在线时如实标注，而不是悄悄消失。
        let rows: Vec<RuleRow> = self
            .config
            .rules
            .iter()
            .map(|r| {
                let monitor = self
                    .monitors
                    .iter()
                    .enumerate()
                    .find(|(_, m)| m.device == r.monitor_device)
                    // label(i) 是完整信息（编号 · 分辨率 · 主屏标记），和下拉框一致
                    .map(|(i, m)| m.label(i))
                    .unwrap_or_else(|| {
                        format!(
                            "{}（不在线）",
                            win::monitor::name_from_device(&r.monitor_device)
                        )
                    });
                let endpoint = if r.endpoint_name.is_empty() {
                    r.endpoint_id.clone()
                } else {
                    r.endpoint_name.clone()
                };
                RuleRow {
                    monitor: monitor.into(),
                    endpoint: endpoint.into(),
                    enabled: r.enabled,
                }
            })
            .collect();
        let sig: Vec<(String, String, bool)> = rows
            .iter()
            .map(|r| (r.monitor.to_string(), r.endpoint.to_string(), r.enabled))
            .collect();
        if sig != self.pushed_rules {
            self.pushed_rules = sig;
            ui.set_rules(slint::ModelRc::new(slint::VecModel::from(rows)));
        }

        if self.log.len() != self.pushed_log_len {
            self.pushed_log_len = self.log.len();
            ui.set_log_entries(slint::ModelRc::new(slint::VecModel::from(self.log.clone())));
        }

        ui.set_headline(self.headline().into());
        ui.set_auto_follow(self.config.auto_follow);
        ui.set_event_driven(self.config.event_driven);
        ui.set_hook_live(self.hook.is_some());
        ui.set_startup(self.startup);
    }
}

/// 启动界面与常驻逻辑。
///
/// `dpi` 只用于在日志里如实报告生效的 DPI 模式。
/// `show_window` 为 false 时窗口保持隐藏（开机自动启动用它，避免登录时弹窗打扰），
/// 用户仍可从托盘菜单打开。
pub fn run(dpi: DpiAwareness, show_window: bool) -> Result<(), slint::PlatformError> {
    // 必须在创建窗口之前就把 COM 初始化成 **STA**。
    //
    // slint 的 Windows 后端是 winit，而 winit 在建窗时会调用 `OleInitialize`。
    // `OleInitialize` 内部就是 `CoInitializeEx(STA)`，它在"本线程已被初始化成 MTA"的情况下
    // 会返回 `RPC_E_CHANGED_MODE`，而 winit 对这个返回值是**直接 panic** 的。
    // 我们自己先以 STA 初始化，`OleInitialize` 随后就会拿到 `S_FALSE`（已初始化过），
    // 顺利通过。ComGuard 会在进程退出时做配对的反初始化。
    //
    // Core Audio 在 STA 下完全可用 —— SoundSwitch 本身就是一个 STA 的 WinForms 程序。
    let apartment = Apartment::SingleThreaded;
    let _com = af_core::audio::com::ComGuard::new(apartment);

    let (tx, rx) = channel::<TrayEvent>();
    let tray_handle = tray::spawn(move |ev| {
        let _ = tx.send(ev);
    });

    let ui = AppWindow::new()?;
    let mut state = AppState::new(tray_handle, apartment);
    state.log(format!("DPI 感知：{}", dpi.as_str()));
    let state = Rc::new(RefCell::new(state));

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_add_rule(move || {
            let Some(ui) = w.upgrade() else { return };
            let mi = ui.get_monitor_index();
            let ei = ui.get_endpoint_index();
            let mut s = st.borrow_mut();
            if mi < 0 || ei < 0 {
                s.log("请先在上方选择屏幕和音频设备");
            } else {
                let msg = s.add_rule(mi as usize, ei as usize);
                s.log(msg);
            }
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_remove_rule(move |idx| {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            if idx < 0 {
                return;
            }
            let msg = s.remove_rule(idx as usize);
            s.log(msg);
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_rescan(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            s.rescan();
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_apply_now(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            s.log("手动触发：立即应用");
            s.tick();
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_toggle_auto_follow(move |on| {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            let msg = s.set_auto_follow(on);
            s.log(msg);
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_toggle_event_driven(move |on| {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            let msg = s.set_event_driven(on);
            if !msg.is_empty() {
                s.log(msg);
            }
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_toggle_startup(move |on| {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            let msg = s.set_startup(on);
            s.log(msg);
            // set_startup 只在自己成功时才改状态，所以这里推一次就会把
            // 写注册表失败的情况拨回开关原状。
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_restore_all(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            let msg = s.restore_all();
            s.log(msg);
            s.sync_to_ui(&ui);
        });
    }

    state.borrow_mut().sync_to_ui(&ui);

    // 窗口的关闭按钮 = 隐藏到托盘，不是退出。
    //
    // slint 的默认动作**本来就**是隐藏（`CloseRequestResponse::HideWindow` 是默认值），
    // 坑在事件循环上 —— 见下面 `run_event_loop_until_quit` 那段。这里只留一句话，
    // 免得用户以为程序已经退了。
    {
        let st = state.clone();
        ui.window().on_close_requested(move || {
            if let Ok(mut s) = st.try_borrow_mut() {
                s.note_window_hidden();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    if show_window {
        ui.show()?;
    }

    let timer = slint::Timer::default();
    {
        let st = state.clone();
        let w = ui.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(PUMP_MS),
            move || {
                let Some(ui) = w.upgrade() else { return };
                let mut s = st.borrow_mut();
                while let Ok(ev) = rx.try_recv() {
                    s.handle_tray(ev, &ui);
                }
                if s.quitting {
                    // 收到退出指令了：还原已经做完，剩下的交给托盘线程收工。
                    // 这时候再去 pump/maybe_sync 会把刚还回去的路由又改一遍。
                    return;
                }
                s.pump_events();
                s.maybe_sync();
                s.sync_to_ui(&ui);
            },
        );
    }

    // 不用 `ComponentHandle::run()`：它内部是 `run_event_loop()`，语义是
    // **"最后一个窗口关闭后返回"——而且 `hide()` 也算关闭**。窗口一藏进托盘，
    // 事件循环立刻返回，紧接着下面的 `shutdown()` 把路由还原、进程退出，
    // 表现成"点关闭 = 程序直接退出"（这正是之前那个 bug 的根因）。
    //
    // `run_event_loop_until_quit()` 则在没有任何可见窗口时也继续跑，
    // 直到 `quit_event_loop()` 被调用。我们的退出不走 slint ——
    // 托盘菜单还原完会让托盘线程直接 `process::exit`。
    let result = slint::run_event_loop_until_quit();
    // 无论正常退出还是出错退出，都要把改过的路由还回去
    state.borrow_mut().shutdown();
    result
}
