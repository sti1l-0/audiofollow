//! 主界面与常驻逻辑。
//!
//! 三个线程各司其职：
//! - **主线程**：slint 的窗口事件循环。界面状态、COM 调用、路由决策都在这里（因此不需要跨线程共享 COM 对象）。
//! - **托盘线程**：`tao` 消息循环 + `tray-icon`，菜单事件通过 channel 发回主线程。
//! - 定时器：跑在 slint 的事件循环里，每 100 ms 醒一次，收托盘事件 + 按配置周期做一次差量同步。
//!
//! 用轮询而不是窗口移动事件，是刻意的取舍：轮询天然兜住"应用重启后路由丢失"，
//! 也避开了 `SetWinEventHook` 的消息风暴与去抖问题。事件钩子留作后续优化。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use af_core::audio::com::Apartment;
use af_core::audio::endpoint::AudioEndpoint;
use af_core::audio::AudioEnv;
use af_core::config::{AppConfig, ScreenRule};
use af_core::plan;
use af_core::reconcile::{Action, Change, Reconciler};
use af_core::win::{self, dpi::DpiAwareness, window, Monitor};

use crate::tray::{self, TrayEvent};

slint::include_modules!();

/// 定时器醒来间隔。托盘事件也是在这个节奏里被收走的。
const PUMP_MS: u64 = 100;
/// 日志最多保留多少行。
const LOG_LIMIT: usize = 400;
/// 轮询周期下限，防止配置被改成"每秒几百次"。
const MIN_POLL_MS: u64 = 300;

struct AppState {
    config: AppConfig,
    /// 本进程要用的 COM 套间模型。GUI 进程是 STA —— 见 [`run`] 里的说明。
    apartment: Apartment,
    env: Option<AudioEnv>,
    env_error: Option<String>,
    monitors: Vec<Monitor>,
    endpoints: Vec<AudioEndpoint>,
    reconciler: Reconciler,
    log: Vec<String>,
    last_sync: Instant,
    /// 不直接用，靠 Drop 保证托盘句柄活着。
    _tray: tray::TrayHandle,

    // 下面三个是为了**只在内容变化时**才把模型推给界面。
    // 如果每 100ms 重建一次 ComboBox 的 model，用户正在选的那一项会被冲掉。
    pushed_monitors: Vec<String>,
    pushed_endpoints: Vec<String>,
    pushed_rules: Vec<String>,
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
            _tray: tray_handle,
            pushed_monitors: Vec::new(),
            pushed_endpoints: Vec::new(),
            pushed_rules: Vec::new(),
            pushed_log_len: usize::MAX,
        };
        if let Some(w) = warn {
            me.log(w);
        }
        me.log(format!("配置文件：{}", AppConfig::path().display()));
        me.rescan();
        me
    }

    fn log(&mut self, msg: impl Into<String>) {
        let line = format!("{}  {}", win::now_hms(), msg.into());
        println!("{line}");
        self.log.push(line);
        if self.log.len() > LOG_LIMIT {
            let drop = self.log.len() - LOG_LIMIT;
            self.log.drain(0..drop);
        }
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

    fn due_for_sync(&self) -> bool {
        self.config.auto_follow
            && self.env.is_some()
            && self.last_sync.elapsed() >= Duration::from_millis(self.config.poll_ms.max(MIN_POLL_MS))
    }

    fn reconcile_now(&mut self) -> Vec<Change> {
        let Some(env) = self.env.as_ref() else {
            return Vec::new();
        };
        let windows = window::enumerate_app_windows();
        let p = plan::build(
            &self.config,
            &self.monitors,
            &windows,
            &self.endpoints,
            std::process::id(),
        );
        let roles = self.config.roles.clone();
        self.reconciler.apply(env, &p, &roles)
    }

    /// 跑一轮同步，只把"有实质动作"的变化写进日志（否则每 1.5 秒刷屏一次）。
    fn tick(&mut self) {
        self.last_sync = Instant::now();
        let changes = self.reconcile_now();
        for c in changes {
            if c.action != Action::Unchanged {
                self.log(c.line());
            }
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
        match saved {
            Ok(()) => format!("已删除绑定：{desc}（该屏上的应用会被还原）"),
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

    fn restore_all(&mut self) -> String {
        let Some(env) = self.env.as_ref() else {
            return "音频接口不可用，无法还原".into();
        };
        let roles = self.config.roles.clone();
        let changes = self.reconciler.restore_all(env, &roles);
        let done = changes.len();
        for c in &changes {
            println!("[还原] {}", c.line());
        }
        let leftovers = self.reconciler.leftovers().to_vec();
        if leftovers.is_empty() {
            format!("已还原 {done} 个进程的音频路由")
        } else {
            format!(
                "已还原 {done} 个进程；另有 {} 个进程原是「跟随系统默认」，接口无法写回空值，\
                 其固定记录会留在系统里（可在系统音量合成器里手动改回）",
                leftovers.len()
            )
        }
    }

    fn shutdown(&mut self) {
        println!("---- 退出，开始还原 ----");
        let msg = self.restore_all();
        println!("{msg}");
    }

    /// 处理来自托盘线程的事件。
    fn handle_tray(&mut self, ev: TrayEvent, ui: &AppWindow) {
        match ev {
            TrayEvent::OpenSettings => {
                let _ = ui.show();
            }
            TrayEvent::ToggleAutoFollow(on) => {
                let msg = self.set_auto_follow(on);
                self.log(msg);
            }
            TrayEvent::Reapply => {
                self.log("托盘触发：立即重新应用");
                self.tick();
            }
            TrayEvent::Quit => {
                self.shutdown();
                let _ = slint::quit_event_loop();
            }
        }
    }

    // ---------------------------------------------------------- 界面同步 ----

    fn headline(&self) -> String {
        if let Some(err) = &self.env_error {
            return format!("音频接口不可用：{err}");
        }
        format!(
            "{} 块显示器 · {} 个音频输出设备 · {} 条绑定 · 当前已改道 {} 个进程{}",
            self.monitors.len(),
            self.endpoints.len(),
            self.config.rules.len(),
            self.reconciler.tracked(),
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

        let rl: Vec<String> = self
            .config
            .rules
            .iter()
            .enumerate()
            .map(|(i, r)| {
                format!(
                    "{}. {}  →  {}",
                    i + 1,
                    if r.monitor_name.is_empty() {
                        r.monitor_device.trim_start_matches(r"\\.\").to_string()
                    } else {
                        r.monitor_name.clone()
                    },
                    if r.endpoint_name.is_empty() {
                        r.endpoint_id.clone()
                    } else {
                        r.endpoint_name.clone()
                    }
                )
            })
            .collect();
        if rl != self.pushed_rules {
            self.pushed_rules = rl.clone();
            ui.set_rule_lines(to_model(rl));
        }

        if self.log.len() != self.pushed_log_len {
            self.pushed_log_len = self.log.len();
            ui.set_log_lines(to_model(self.log.clone()));
        }

        ui.set_headline(self.headline().into());
        ui.set_auto_follow(self.config.auto_follow);
    }
}

/// 启动界面与常驻逻辑。`dpi` 只用于在日志里如实报告生效的 DPI 模式。
pub fn run(dpi: DpiAwareness) -> Result<(), slint::PlatformError> {
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
            let mi = ui.get_monitor_index().max(0) as usize;
            let ei = ui.get_endpoint_index().max(0) as usize;
            let mut s = st.borrow_mut();
            let msg = s.add_rule(mi, ei);
            s.log(msg);
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_remove_selected(move || {
            let Some(ui) = w.upgrade() else { return };
            let idx = ui.get_selected_rule();
            let mut s = st.borrow_mut();
            if idx < 0 {
                s.log("请先在列表里点一条绑定再删除");
            } else {
                let msg = s.remove_rule(idx as usize);
                s.log(msg);
                ui.set_selected_rule(-1);
            }
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
        ui.on_restore_all(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            let msg = s.restore_all();
            s.log(msg);
            s.sync_to_ui(&ui);
        });
    }

    state.borrow_mut().sync_to_ui(&ui);

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
                if s.due_for_sync() {
                    s.tick();
                }
                s.sync_to_ui(&ui);
            },
        );
    }

    let result = ui.run();
    // 无论正常退出还是出错退出，都要把改过的路由还回去
    state.borrow_mut().shutdown();
    result
}
