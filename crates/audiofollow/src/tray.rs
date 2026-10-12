//! 托盘图标。
//!
//! 跑在**独立线程**上：slint 的窗口事件循环占着主线程，而 `tray-icon` 要求图标所属线程
//! 有消息循环才能收到菜单事件。两个循环各占一条线程，用 channel 通信。
//!
//! 菜单事件通过 `MenuEvent::receiver()`（全局通道）取得。事件循环每 100 ms 醒一次去收，
//! 不依赖"托盘消息会唤醒事件循环"这种实现细节。

use std::time::{Duration, Instant};

use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const PUMP_MS: u64 = 100;

/// 托盘上发生的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    OpenSettings,
    ToggleAutoFollow(bool),
    Reapply,
    Quit,
}

/// 主线程 → 托盘线程的指令。
#[derive(Debug, Clone)]
pub enum TrayCommand {
    SetAutoFollow(bool),
}

/// 主线程持有的托盘句柄。
pub struct TrayHandle {
    proxy: EventLoopProxy<TrayCommand>,
}

impl TrayHandle {
    /// 同步"自动跟随"的勾选状态（用户在设置窗口里改动时调用）。
    pub fn set_auto_follow(&self, on: bool) {
        let _ = self.proxy.send_event(TrayCommand::SetAutoFollow(on));
    }
}

/// 起托盘线程。`handler` 在**托盘线程**上被调用，它必须自己把动作转回主线程。
pub fn spawn(handler: impl Fn(TrayEvent) + Send + 'static) -> TrayHandle {
    let (tx, rx) = std::sync::mpsc::channel::<EventLoopProxy<TrayCommand>>();

    std::thread::Builder::new()
        .name("audiofollow-tray".into())
        .spawn(move || {
            let event_loop = EventLoopBuilder::<TrayCommand>::with_user_event().build();
            let proxy = event_loop.create_proxy();
            if tx.send(proxy).is_err() {
                return;
            }

            let menu = Menu::new();
            let open = MenuItem::new("打开设置…", true, None);
            let auto = CheckMenuItem::new("自动跟随", true, true, None);
            let reapply = MenuItem::new("立即重新应用", true, None);
            let quit = MenuItem::new("退出并还原", true, None);

            let _ = menu.append(&open);
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&auto);
            let _ = menu.append(&reapply);
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&quit);

            let ids = Ids {
                open: open.id().clone(),
                auto: auto.id().clone(),
                reapply: reapply.id().clone(),
                quit: quit.id().clone(),
            };

            let icon = match tray_icon() {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("[托盘] 生成图标失败：{e}");
                    return;
                }
            };

            // TrayIcon 必须跑在拥有消息循环的这条线程上。
            match TrayIconBuilder::new()
                .with_tooltip("音频跟随 —— 应用声音跟随窗口所在的屏幕")
                .with_icon(icon)
                .with_menu(Box::new(menu))
                .build()
            {
                Ok(tray) => run_loop(event_loop, handler, ids, auto, tray),
                Err(e) => eprintln!("[托盘] 注册失败：{e}"),
            }
        })
        .expect("无法创建托盘线程");

    let proxy = rx.recv().expect("托盘线程启动失败");
    TrayHandle { proxy }
}

struct Ids {
    open: MenuId,
    auto: MenuId,
    reapply: MenuId,
    quit: MenuId,
}

fn run_loop(
    event_loop: tao::event_loop::EventLoop<TrayCommand>,
    handler: impl Fn(TrayEvent) + Send + 'static,
    ids: Ids,
    auto: CheckMenuItem,
    tray: TrayIcon,
) {
    // 图标要活到循环结束
    let _keep_alive = tray;
    let mut next_tick = Instant::now() + Duration::from_millis(PUMP_MS);

    event_loop.run(move |event, _, control_flow| {
        // 主线程要求同步勾选状态
        if let tao::event::Event::UserEvent(TrayCommand::SetAutoFollow(on)) = event {
            if auto.is_checked() != on {
                auto.set_checked(on);
            }
        }

        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == ids.open {
                handler(TrayEvent::OpenSettings);
            } else if ev.id == ids.auto {
                // CheckMenuItem 被点击时自己已经切换了状态，这里读新的即可
                handler(TrayEvent::ToggleAutoFollow(auto.is_checked()));
            } else if ev.id == ids.reapply {
                handler(TrayEvent::Reapply);
            } else if ev.id == ids.quit {
                handler(TrayEvent::Quit);
            }
        }

        if Instant::now() >= next_tick {
            next_tick = Instant::now() + Duration::from_millis(PUMP_MS);
        }
        *control_flow = ControlFlow::WaitUntil(next_tick);
    });
}

/// 画一个 32×32 的托盘图标：蓝色圆角底 + 白色扬声器 + 两道声波。
fn tray_icon() -> Result<Icon, String> {
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];

    let bg = [0x2F, 0x6F, 0xED, 0xFF];
    let fg = [0xFF, 0xFF, 0xFF, 0xFF];

    // 圆角矩形底
    let r = 7i32;
    for y in 0..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let cx = x.clamp(r, SIZE as i32 - 1 - r);
            let cy = y.clamp(r, SIZE as i32 - 1 - r);
            let (dx, dy) = (x - cx, y - cy);
            if dx * dx + dy * dy <= r * r {
                put(&mut rgba, SIZE, x, y, bg);
            }
        }
    }

    // 扬声器：左侧箱体
    for y in 12..20 {
        for x in 7..10 {
            put(&mut rgba, SIZE, x, y, fg);
        }
    }
    // 喇叭口（向右张开的三角形）
    for x in 10..17 {
        let half = (x - 10) as i32 + 2;
        for y in (16 - half)..(16 + half) {
            put(&mut rgba, SIZE, x, y, fg);
        }
    }

    // 声波弧
    for radius in [6.0f64, 9.0f64] {
        for deg in -50..=50 {
            let a = (deg as f64).to_radians();
            let x = 16.0 + radius * a.cos();
            let y = 16.0 + radius * a.sin();
            put(&mut rgba, SIZE, x.round() as i32, y.round() as i32, fg);
        }
    }

    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).map_err(|e| e.to_string())
}

fn put(rgba: &mut [u8], size: usize, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
        return;
    }
    let i = (y as usize * size + x as usize) * 4;
    rgba[i..i + 4].copy_from_slice(&color);
}
