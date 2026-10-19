//! 主界面与常驻逻辑。
//!
//! 三个线程各司其职：
//! - **主线程**：slint 的窗口事件循环。界面状态、COM 调用、路由决策都在这里（因此不需要跨线程共享 COM 对象）。
//! - **托盘线程**：`tao` 消息循环 + `tray-icon`，菜单事件通过 channel 发回主线程。
//! - **事件钩子线程**：`SetWinEventHook` 的消息循环，回调只累加一个原子计数。
//!   见 [`af_core::win::hook`] 关于"为什么必须单独一条线程"的说明。
//!
//! # 触发同步的四条路径
//!
//! 1. **窗口事件**（主路径）：钩子线程收到事件 → 置脏 → 主线程 100 ms 醒来时发现脏了，
//!    等手停下来（去抖 250 ms，上限 1 s）再同步。拖动窗口后大约 250 ms 内声音就换了设备。
//! 2. **托盘菜单**"立即重新应用"。
//! 3. **兜底轮询**：事件覆盖不到的情况 —— 进程重启、路由被别的程序改掉、
//!    某个应用刚出声。周期见 [`FALLBACK_POLL_MS`]。
//! 4. **设备扫描**：显示器拔插、声卡进出（周期 [`DEVICE_SCAN_MS`]）。扫到设备集变化
//!    或记忆治愈修好了绑定，就强制下一轮同步立刻执行。
//!
//! # 设备记忆
//!
//! 绑定**从不因设备离线而删除**。显示器拔掉重插后系统可能重新编号（`DISPLAY2` →
//! `DISPLAY3`），声卡换接口后端点 ID 会变——每轮设备扫描都会按稳定标识把这类绑定
//! "治愈"回可用状态（见 [`af_core::config::AppConfig::heal_against`]），用户无感。
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
use af_core::config::{self, AppConfig, ScreenRule};
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
/// 设备自动扫描的周期（显示器拔插、声卡进出）。
///
/// Windows 的显示器与音频设备变化没有走 `SetWinEventHook` 的通知，这里用轮询盯。
/// 枚举本身很便宜（`EnumDisplayMonitors` + 少量注册表/COM 读取），几秒一次无感。
/// 显示器重新接入后，它挂的 HDMI 音频端点可能晚几秒才出现——这个周期顺带把
/// "屏回来了但声音设备还没回来"的窗口期也收掉了。
const DEVICE_SCAN_MS: u64 = 4000;

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
    /// 托盘图标是否还活着。
    ///
    /// 托盘注册失败时（见 `tray::TrayEvent::TrayFailed`）置 false：那时「退出并还原」
    /// 的唯一入口没了，关窗的含义要从「藏进托盘」改成「还原并退出」，否则用户只能
    /// 用任务管理器杀进程——那会跳过还原，把改过的路由留在系统里。
    tray_alive: bool,
    /// 不直接用，靠 Drop 保证托盘句柄活着。
    _tray: tray::TrayHandle,

    // ---- 事件驱动 ----
    /// 窗口事件钩子。`None` 表示不可用（退回轮询）。
    hook: Option<WindowEventHook>,
    /// 待处理的窗口事件（去抖用）。
    dirty: Option<Dirty>,
    /// 累计处理过的事件数，只用于展示。
    events_total: usize,

    // ---- 设备自动扫描 ----
    /// 上次设备扫描的时刻。
    last_device_scan: Instant,
    /// 上次音频端点枚举是否成功。
    ///
    /// 只在**状态翻转**时记日志：音频服务偶尔抖一下不该每 4 秒刷一行"枚举失败"。
    endpoints_ok: bool,

    // ---- 计划里的说明，用来在变化时提示一次 ----
    plan_notes: Vec<String>,
    logged_notes: Vec<String>,
    /// 每个进程上一轮的动作。只在动作**发生变化**时打印，避免每轮刷屏。
    last_action: HashMap<u32, Action>,

    // 下面几个是为了**只在内容变化时**才把模型推给界面。
    // 如果每 100ms 重建一次下拉框的 model，用户正在选的那一项会被冲掉。
    pushed_monitors: Vec<String>,
    pushed_endpoints: Vec<String>,
    /// 绑定行缓存，直接整表比较（slint 结构体自带 PartialEq）：以后给 RuleRow
    /// 加字段会自动参与比较，不会出现"忘了把新字段加进签名 → 界面不刷新"的沉默 bug。
    pushed_rules: Vec<RuleRow>,
    /// 日志序号：每写一行 +1。
    ///
    /// **不能按长度判断**：到达上限后长度恒等于 `LOG_LIMIT`（推一条、挤掉一条），
    /// 按长度比较的话界面日志会从第 401 行起永远不再刷新——真实存在的坑。
    log_seq: u64,
    pushed_log_seq: u64,
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
            tray_alive: true,
            _tray: tray_handle,
            hook: None,
            dirty: None,
            events_total: 0,
            last_device_scan: Instant::now(),
            endpoints_ok: true,
            plan_notes: Vec::new(),
            logged_notes: Vec::new(),
            last_action: HashMap::new(),
            pushed_monitors: Vec::new(),
            pushed_endpoints: Vec::new(),
            pushed_rules: Vec::new(),
            log_seq: 0,
            pushed_log_seq: u64::MAX,
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
        self.log_seq += 1;
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

    /// 启动 / 手动「重新扫描」：完整列出设备清单，再做记忆治愈。
    fn rescan(&mut self) {
        self.refresh_devices(true);
    }

    /// 周期性的设备自动扫描。到了 [`DEVICE_SCAN_MS`] 的节奏就扫一遍。
    fn maybe_refresh_devices(&mut self) {
        if self.last_device_scan.elapsed() < Duration::from_millis(DEVICE_SCAN_MS) {
            return;
        }
        self.last_device_scan = Instant::now();
        self.refresh_devices(false);
    }

    /// 扫一遍显示器与音频设备，交给 [`af_core::scan`] 决策，然后照单执行。
    ///
    /// 这是「设备记忆」的入口。显示器拔掉重插后系统可能给它换设备名（`DISPLAY2`
    /// 变 `DISPLAY3`）、声卡换接口后端点 ID 会变——两者都会让绑定"看起来失效"，
    /// 由扫描决策层按稳定标识认回来。决策逻辑（变化怎么报、何时治愈、何时强制
    /// 同步）全部在 `af_core::scan` 里单测覆盖；这里只剩 IO：枚举、打日志、落盘、
    /// 触发同步。
    ///
    /// `verbose` 为真时完整列出当前设备（启动与手动扫描用）；周期扫描只报变化，
    /// 否则每 4 秒刷一遍设备清单。
    fn refresh_devices(&mut self, verbose: bool) {
        // 音频接口迟到自愈：开机时音频服务可能比本程序晚起来，第一次 init 失败
        // 不代表永远失败，每次扫描都再试。只在成功那一刻记日志。
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
                    // 失败只在 verbose（启动/手动）或"上次还是好的"翻转时说一次，
                    // 不然音频服务晚起的几分钟里每 4 秒刷一行一样的错
                    if verbose || self.endpoints_ok {
                        self.log(format!("音频接口初始化失败：{e}"));
                    }
                    self.endpoints_ok = false;
                    self.env_error = Some(format!("音频接口初始化失败：{e}"));
                }
            }
        }

        let monitors = win::monitor::enumerate();
        let (endpoints, endpoints_ok_now) = match self.env.as_ref().map(|e| e.endpoints()) {
            Some(Ok(eps)) => (eps, true),
            Some(Err(e)) => {
                if verbose || self.endpoints_ok {
                    self.log(format!("枚举音频设备失败：{e}"));
                }
                self.endpoints_ok = false;
                return; // 端点列表不可信，本轮不更新、不治愈
            }
            None => (Vec::new(), false),
        };

        let outcome = af_core::scan::scan(
            &mut self.config,
            &self.monitors,
            &self.endpoints,
            &monitors,
            &endpoints,
            verbose,
            self.endpoints_ok,
            endpoints_ok_now,
        );
        self.monitors = monitors;
        self.endpoints = endpoints;
        self.endpoints_ok = endpoints_ok_now;

        for l in outcome.log_lines {
            self.log(l);
        }
        if outcome.config_changed {
            if let Err(e) = self.config.save() {
                self.log(format!("保存配置失败：{e}"));
            }
        }
        if outcome.force_sync {
            // 设备集变了或绑定恢复了：下一轮同步不等轮询周期，立刻执行
            self.last_sync = Instant::now() - Duration::from_secs(3600);
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

    /// 这块屏的稳定标识，仅当它在当前在线的屏里**唯一**时才返回。
    ///
    /// 同型号双屏（EDID 序列号都是 0）会共用同一个标识，写进规则反而会引起误认
    /// （治愈时分不清谁是谁）——这种时候宁可不记，退回设备名匹配。
    fn unique_live_key(&self, device: &str) -> Option<String> {
        let m = self.monitors.iter().find(|m| m.device.eq_ignore_ascii_case(device))?;
        let k = m.key.as_deref()?;
        let same = self.monitors.iter().filter(|o| o.key.as_deref() == Some(k)).count();
        (same == 1).then(|| k.to_string())
    }

    /// 添加（或替换）一条绑定。`Err` 的情况就地显示在按钮旁（见 `add-hint`）。
    fn add_rule(&mut self, monitor_index: usize, endpoint_index: usize) -> Result<String, String> {
        let Some(m) = self.monitors.get(monitor_index) else {
            return Err("没有可用的显示器".into());
        };
        let Some(ep) = self.endpoints.get(endpoint_index) else {
            return Err("没有可用的音频设备".into());
        };
        // 把显示器的稳定标识记进规则：拔插后设备名会变，靠它才能认出"同一块屏"
        let key = self.unique_live_key(&m.device);
        // upsert 会顶掉「同设备名或同键」的旧规则。文案必须如实区分新增和替换，
        // 否则用户以为只是又加了一条，没意识到旧指向已经被换掉了。
        let existing = self
            .config
            .rules
            .iter()
            .find(|r| {
                r.monitor_device.eq_ignore_ascii_case(&m.device)
                    || (key.is_some() && r.monitor_key.as_deref() == key.as_deref())
            })
            .cloned();
        let rule = ScreenRule::from_pair(&m.device, &m.name, key.as_deref(), ep);
        let desc = rule.describe();
        let action = match &existing {
            Some(old) if old.endpoint_id == ep.id => "绑定无变化：".to_string(),
            Some(old) => format!("已更新绑定（原指向 {}）：", old.endpoint_display()),
            None => "已添加绑定：".to_string(),
        };
        self.config.upsert_rule(rule);
        self.last_sync = Instant::now() - Duration::from_secs(3600); // 立刻生效
        match self.config.save() {
            Ok(_) => Ok(format!("{action}{desc}")),
            Err(e) => Ok(format!("{action}{desc}，但保存失败：{e}")),
        }
    }

    /// 停用/启用一条绑定。不删：停用期间那块屏上的应用恢复原路由，
    /// 绑定本身（含设备记忆）原样保留。
    fn toggle_rule(&mut self, index: usize) -> String {
        let Some(rule) = self.config.rules.get_mut(index) else {
            return "没有选中任何绑定".into();
        };
        rule.enabled = !rule.enabled;
        let state = if rule.enabled { "启用" } else { "停用" };
        let desc = format!(
            "{} → {}",
            rule.monitor_display_in(&self.monitors),
            rule.endpoint_display()
        );
        let saved = self.config.save().is_ok();
        // 两个方向都需要立即同步：停用 → 那块屏上的进程要还原；重新启用 → 要改道。
        // 清掉动作去重，让变化如实上报。
        self.last_sync = Instant::now() - Duration::from_secs(3600);
        self.last_action.clear();
        if saved {
            format!("已{state}绑定：{desc}")
        } else {
            format!("已{state}绑定：{desc}，但保存失败")
        }
    }

    /// 「一键匹配」：按型号名把还没绑定的在线屏配到同名音频设备
    /// （HDMI/DP 音频的端点名里通常就含显示器型号，见 [`config::name_matched_pairs`]）。
    fn auto_match_rules(&mut self) -> String {
        let pairs = config::name_matched_pairs(&self.monitors, &self.endpoints);
        let matched = pairs.len();
        let total = self.monitors.len();
        let mut added = 0usize;
        let mut already_bound = 0usize;
        for (device, name, _raw_key, ep) in pairs {
            // 歧义键（同型号双屏）在这里同样不能记，与手动添加保持一把尺子
            let key = self.unique_live_key(&device);
            if self.config.rule_for_monitor(&device, key.as_deref(), &name).is_some() {
                already_bound += 1;
                continue;
            }
            self.config
                .upsert_rule(ScreenRule::from_pair(&device, &name, key.as_deref(), &ep));
            added += 1;
        }
        if added == 0 {
            return if matched == 0 {
                format!("没有可按型号名匹配的屏幕（{total} 块在线，都没有型号名对得上的音频设备）")
            } else {
                "所有能按型号名匹配的屏幕都已有绑定".into()
            };
        }
        let mut msg = format!("已按型号名生成 {added} 条绑定");
        if already_bound > 0 {
            msg.push_str(&format!("（{already_bound} 块已有绑定，跳过）"));
        }
        let unmatched = total.saturating_sub(matched);
        if unmatched > 0 {
            msg.push_str(&format!("；另有 {unmatched} 块屏没有同名音频设备，未处理"));
        }
        if let Err(e) = self.config.save() {
            self.log(format!("保存配置失败：{e}"));
        }
        self.last_sync = Instant::now() - Duration::from_secs(3600);
        msg
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

    /// 记住当前窗口尺寸（逻辑像素），下次启动恢复，免得每次都是默认 880×640。
    ///
    /// 存**逻辑**像素：物理像素随所在屏的缩放比变，跨 DPI 迁移后会失真。
    fn remember_window(&mut self, ui: &AppWindow) {
        let scale = ui.window().scale_factor();
        let sz = ui.window().size();
        if scale <= 0.0 || sz.width == 0 || sz.height == 0 {
            return;
        }
        let w = (sz.width as f32 / scale).round() as u32;
        let h = (sz.height as f32 / scale).round() as u32;
        if self.config.window_width == Some(w) && self.config.window_height == Some(h) {
            return;
        }
        self.config.window_width = Some(w);
        self.config.window_height = Some(h);
        if let Err(e) = self.config.save() {
            self.log(format!("保存窗口尺寸失败：{e}"));
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
                self.remember_window(ui);
                self.shutdown();
                // 还原已经落地，让托盘线程收工 —— 它退出事件循环时 tao 会直接
                // 以 0 结束进程。不走 slint 的退出信号：那条路是异步的，而此刻
                // 我们还在定时器回调里，只要它没被处理就退不掉。
                self._tray.request_quit();
            }
            TrayEvent::TrayFailed(reason) => {
                self.tray_alive = false;
                self.log(format!("托盘不可用：{reason}"));
                self.log(
                    "退出方式已切换：关闭设置窗口 = 先还原全部路由再退出。\
                     也可以先点「还原全部改动」核对一遍再关窗",
                );
                // 窗口必须顶出来：托盘菜单没了，这里可能是用户接触本程序的唯一入口
                // （开机自动启动那条路原本不弹窗）。
                let _ = ui.show();
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

    /// 模型内容变化后，把下拉框的选中项按**标签**重新定位。
    ///
    /// slint 的 `current-index` 是纯位置：列表一变（设备插入/移除/重排），原来的
    /// 序号会指向**另一个条目**——用户以为还选着「屏幕 1」，实际已经换人了，再点
    /// 「添加绑定」就会绑到错的设备上。按值找回来：标签还在就跟随；不在了
    /// （选中的设备被拔掉、或标签变了）就清空选择，让占位文案提示用户重选。
    /// 诚实地丢选择，也好过沉默地换目标。
    fn carry_selection(old: &[String], old_index: i32, new: &[String]) -> i32 {
        if old_index < 0 {
            return -1;
        }
        match old.get(old_index as usize) {
            Some(label) => new.iter().position(|l| l == label).map_or(-1, |i| i as i32),
            None => -1,
        }
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
            let kept = Self::carry_selection(&self.pushed_monitors, ui.get_monitor_index(), &ml);
            self.pushed_monitors = ml.clone();
            ui.set_monitor_lines(to_model(ml));
            ui.set_monitor_index(kept);
        }

        let el: Vec<String> = self
            .endpoints
            .iter()
            .enumerate()
            .map(|(i, e)| e.label(i))
            .collect();
        if el != self.pushed_endpoints {
            let kept = Self::carry_selection(&self.pushed_endpoints, ui.get_endpoint_index(), &el);
            self.pushed_endpoints = el.clone();
            ui.set_endpoint_lines(to_model(el));
            ui.set_endpoint_index(kept);
        }

        // 绑定行按**当前实际扫到的显示器**解析展示名，这样「显示器 N」的编号
        // 和上面的下拉框始终一致。屏幕不在线时如实标注，而不是悄悄消失。
        // 设备名虽然对上、但键或型号名对不上（位置被别的显示器顶了）也要说清楚，
        // 不然用户会以为绑定还生效着。
        let rows: Vec<RuleRow> = self
            .config
            .rules
            .iter()
            .map(|r| {
                let at_slot = self
                    .monitors
                    .iter()
                    .enumerate()
                    .find(|(_, m)| m.device.eq_ignore_ascii_case(&r.monitor_device));
                let monitor = match at_slot {
                    // label(i) 是完整信息（编号 · 分辨率 · 主屏标记），和下拉框一致
                    Some((i, m)) if r.matches_monitor(m) => m.label(i),
                    Some((_, _)) => format!(
                        "{}（该位置现在是别的显示器，等待原屏接入后自动恢复）",
                        win::monitor::name_from_device(&r.monitor_device)
                    ),
                    None => format!(
                        "{}（不在线，接入后自动恢复）",
                        win::monitor::name_from_device(&r.monitor_device)
                    ),
                };
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
        if rows != self.pushed_rules {
            self.pushed_rules = rows.clone();
            ui.set_rules(slint::ModelRc::new(slint::VecModel::from(rows)));
        }

        if self.log_seq != self.pushed_log_seq {
            self.pushed_log_seq = self.log_seq;
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

    // 恢复上次的窗口尺寸（逻辑像素）。不低于界面声明的最小值，也不放大到
    // 离谱的程度（防御手改配置文件塞进奇怪数字）。
    if let (Some(w), Some(h)) = (state.config.window_width, state.config.window_height) {
        let (w, h) = (w.clamp(700, 8192), h.clamp(560, 8192));
        let scale = ui.window().scale_factor();
        ui.window().set_size(slint::PhysicalSize::new(
            (w as f32 * scale).round() as u32,
            (h as f32 * scale).round() as u32,
        ));
    }

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
                // 失败原因就地显示：只沉进日志区的话，注意力在按钮上的用户看不见
                ui.set_add_hint("请先选择屏幕和音频设备".into());
            } else {
                match s.add_rule(mi as usize, ei as usize) {
                    Ok(msg) => {
                        s.log(msg);
                        ui.set_add_hint("".into());
                    }
                    Err(e) => {
                        s.log(e.clone());
                        ui.set_add_hint(e.into());
                    }
                }
            }
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_toggle_rule(move |idx| {
            let Some(ui) = w.upgrade() else { return };
            if idx < 0 {
                return;
            }
            let mut s = st.borrow_mut();
            let msg = s.toggle_rule(idx as usize);
            s.log(msg);
            s.sync_to_ui(&ui);
        });
    }

    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.on_auto_match(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut s = st.borrow_mut();
            let msg = s.auto_match_rules();
            s.log(msg);
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
    //
    // 例外：托盘不可用时（`tray::TrayEvent::TrayFailed`）关窗 = 还原并退出。
    // 「退出并还原」只在托盘菜单里，托盘没了再藏窗口，程序就成了一个只能靠
    // 任务管理器杀掉的僵尸——那会跳过还原。所以这里直接在主线程上还原完路由，
    // 然后 exit(0)。不走 slint 的退出信号（异步，在回调里叫不醒循环），
    // 也不走托盘线程（它已经死了）；还原是 `shutdown()` 里同步做完的，
    // exit 前没有未落地的副作用。
    {
        let st = state.clone();
        let w = ui.as_weak();
        ui.window().on_close_requested(move || {
            let Some(ui) = w.upgrade() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            if let Ok(mut s) = st.try_borrow_mut() {
                s.remember_window(&ui);
                if s.tray_alive {
                    s.note_window_hidden();
                    return slint::CloseRequestResponse::HideWindow;
                }
                println!("---- 托盘不可用，关窗即退出：先还原 ----");
                s.shutdown();
            }
            std::process::exit(0);
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
                // panic 防护：这个回调浓缩了全部运行逻辑，任何一处越界/unwrap 在
                // panic=abort 的配置下都会直接杀进程、跳过还原，把改过的路由留在
                // 系统里。接住 panic、留痕、下一轮继续——「退出必还原」的承诺比
                // 一次崩溃的体面重要（release 因此不再设 panic=abort，见 Cargo.toml）。
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut s = st.borrow_mut();
                    while let Ok(ev) = rx.try_recv() {
                        s.handle_tray(ev, &ui);
                    }
                    if s.quitting {
                        // 收到退出指令了：还原已经做完，剩下的交给托盘线程收工。
                        // 这时候再去 pump/maybe_sync 会把刚还回去的路由又改一遍。
                        return;
                    }
                    // 设备扫描放在最前面：拔插显示器 / 声卡回来的那一刻，先让
                    // 记忆治愈修好绑定，紧接着的 maybe_sync 就能按新状态路由
                    s.maybe_refresh_devices();
                    s.pump_events();
                    s.maybe_sync();
                    s.sync_to_ui(&ui);
                }));
                if ok.is_err() {
                    // RefCell 的借用在栈展开时已释放，这里能重新借到
                    if let Ok(mut s) = st.try_borrow_mut() {
                        s.log("内部错误：本轮处理发生 panic，已跳过并继续运行；若反复出现请反馈");
                    }
                }
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
