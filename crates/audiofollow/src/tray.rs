//! 托盘图标。
//!
//! 跑在**独立线程**上：slint 的窗口事件循环占着主线程，而 `tray-icon` 要求图标所属线程
//! 有消息循环才能收到菜单事件。两个循环各占一条线程，用 channel 通信。
//!
//! 菜单事件通过 `MenuEvent::set_event_handler` 取得 —— 回调在托盘线程的窗口过程里
//! **同步**执行，不在事件循环里轮询 channel。这里有一条实测教训：
//! `MenuEvent::receiver()`（crossbeam 通道）与 `set_event_handler` 是**互斥**的两条路，
//! 一旦挂了 handler，事件就不再进通道，轮询 `try_recv()` 永远是空的。

use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::EventLoopBuilderExtWindows;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 托盘上发生的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    OpenSettings,
    /// 用户点了「自动跟随」。**不带目标状态**：菜单勾选框的状态由托盘线程维护，
    /// 而真相源是主线程的配置。主线程收到后按自己的配置取反，再回写勾选框。
    ToggleAutoFollow,
    Reapply,
    Quit,
}

/// 主线程 → 托盘线程的指令。
#[derive(Debug, Clone)]
pub enum TrayCommand {
    SetAutoFollow(bool),
    /// 让托盘线程退出自己的事件循环。
    ///
    /// tao 的 `EventLoop::run` 在循环结束后直接 `process::exit(0)`，托盘图标也在
    /// 这一过程中被正常注销。走这条路退出比让 slint 结束事件循环更硬：
    /// 后者要等 slint 的消息泵自己反应过来，而且那时主线程还在定时器回调里。
    Quit,
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

    /// 要求托盘线程收工：它退出事件循环后，tao 会直接以 0 结束进程。
    ///
    /// 调用方必须**先做完还原**再调这个 —— 进程不会再有第二次机会。
    pub fn request_quit(&self) {
        let _ = self.proxy.send_event(TrayCommand::Quit);
    }
}

/// 起托盘线程。`on_event` 会在**托盘线程**上被调用 —— 也就是菜单消息的窗口过程里，
/// 所以它必须立刻返回，只做「把动作转回主线程」这一件事。
pub fn spawn(on_event: impl Fn(TrayEvent) + Send + Sync + 'static) -> TrayHandle {
    let (tx, rx) = std::sync::mpsc::channel::<EventLoopProxy<TrayCommand>>();

    std::thread::Builder::new()
        .name("audiofollow-tray".into())
        .spawn(move || {
            // tao 默认禁止在主线程之外创建事件循环（跨平台兼容性保护）。
            // 但这里**必须**放在独立线程：主线程被 slint 的窗口事件循环占着，
            // 而托盘图标要求所属线程有消息循环。显式声明 any_thread 表明这是有意为之。
            let event_loop = EventLoopBuilder::<TrayCommand>::with_user_event()
                .with_any_thread(true)
                .build();
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

            // 菜单事件必须走 handler，不能靠 channel 轮询。
            //
            // 原来的写法是让 tao 的循环每 100 ms `MenuEvent::receiver().try_recv()`
            // 去 channel 里取，实测**一个都取不到**：WM_COMMAND 确实送到了托盘窗口，
            // muda 也确实在它的窗口过程里产生了事件（挂 handler 探针当场能打印出
            // 事件 id），但 channel 那一路永远是空的 —— 于是菜单里所有项点了都没反应，
            // 包括「退出并还原」。
            //
            // handler 是 muda 为这种场景提供的入口：事件产生的那一刻直接在**本条线程**
            // 回调，不存在「谁来取、什么时候取」的时序问题。
            //
            // 回调在窗口过程里同步执行，所以这里只允许做转发（`on_event` 内部就是
            // 往 channel 里塞一个枚举），不能碰主线程的状态。
            {
                let on_event = on_event;
                let ids = ids.clone();
                MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
                    if ev.id == ids.open {
                        on_event(TrayEvent::OpenSettings);
                    } else if ev.id == ids.auto {
                        on_event(TrayEvent::ToggleAutoFollow);
                    } else if ev.id == ids.reapply {
                        on_event(TrayEvent::Reapply);
                    } else if ev.id == ids.quit {
                        on_event(TrayEvent::Quit);
                    }
                }));
            }

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
                Ok(tray) => run_loop(event_loop, auto, tray),
                Err(e) => eprintln!("[托盘] 注册失败：{e}"),
            }
        })
        .expect("无法创建托盘线程");

    let proxy = rx.recv().expect("托盘线程启动失败");
    TrayHandle { proxy }
}

#[derive(Clone)]
struct Ids {
    open: MenuId,
    auto: MenuId,
    reapply: MenuId,
    quit: MenuId,
}

/// 托盘线程的事件循环。
///
/// 用 `ControlFlow::Wait`：循环完全由消息驱动，不需要任何定时唤醒 ——
/// 菜单事件是 muda 在托盘窗口的窗口过程里同步回调出来的，本来就在这条线程上。
fn run_loop(event_loop: tao::event_loop::EventLoop<TrayCommand>, auto: CheckMenuItem, tray: TrayIcon) {
    // 图标要活到循环结束
    let _keep_alive = tray;

    event_loop.run(move |event, _, control_flow| {
        match event {
            // 主线程要求同步勾选状态
            tao::event::Event::UserEvent(TrayCommand::SetAutoFollow(on)) => {
                if auto.is_checked() != on {
                    auto.set_checked(on);
                }
            }
            // 主线程已经还原完毕，要求收工。tao 的 run 在循环结束后 process::exit(0)。
            tao::event::Event::UserEvent(TrayCommand::Quit) => {
                *control_flow = ControlFlow::Exit;
                return;
            }
            _ => {}
        }

        *control_flow = ControlFlow::Wait;
    });
}

/// 托盘图标来自 `icon.rs` 的统一源图（与窗口/任务栏同一份几何）。
fn tray_icon() -> Result<Icon, String> {
    let rgba = crate::icon::tray_rgba()?;
    Icon::from_rgba(rgba, crate::icon::TRAY_PX, crate::icon::TRAY_PX).map_err(|e| e.to_string())
}
