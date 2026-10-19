//! 窗口事件钩子：把"什么时候需要重新判断"从轮询变成事件通知。
//!
//! # 为什么不是纯轮询
//!
//! 纯轮询的代价是延迟的下限就是轮询周期。窗口拖动结束后，用户会看到声音要等
//! 最多一个周期才换设备——听起来像坏掉了。事件钩子能把这个延迟压到几十毫秒。
//!
//! # 为什么回调里什么都不干
//!
//! `SetWinEventHook` 的 `EVENT_OBJECT_LOCATIONCHANGE` 在拖窗口时是**每帧**触发的，
//! 一次拖动轻松上千条。回调里做任何重活（哪怕只是加锁）都会拖垮目标程序的消息处理。
//!
//! 所以这里的回调只做一件事：给一个原子计数 +1。真正的决策在主线程上、由
//! 去抖（debounce）后的定时器来做。这也顺手避开了重入问题——回调在钩子线程上运行，
//! 而钩子线程和主线程之间只有原子变量，没有任何共享可变状态。
//!
//! # 线程模型
//!
//! `WINEVENT_OUTOFCONTEXT` 模式下，事件是通过**安装钩子的那条线程的消息队列**派发的。
//! 所以钩子必须装在一个会泵消息的线程上。这里专门起一条线程跑 `GetMessageW` 循环，
//! 而**不是**装在 slint/winit 的主循环上——那样就等于把钩子的存活绑死在
//! winit 的消息泵实现细节上，还可能和它的内部状态打架。

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::channel;

use super::ffi;

/// 自钩子安装以来收到的、通过了过滤的事件数。`take_events` 会把它清零。
static EVENTS: AtomicUsize = AtomicUsize::new(0);
/// 最近一次通过过滤的事件编号（诊断用）。
static LAST_ID: AtomicU32 = AtomicU32::new(0);
/// 回调被调用过的总次数，含被过滤掉的。用来区分"钩子没装上"和"事件都被滤掉了"。
static CALLS: AtomicUsize = AtomicUsize::new(0);

/// 原始事件环形缓冲的槽数。
///
/// 存在的理由：过滤条件一旦写错，表现就是"钩子明明通着却收不到事件"，
/// 光看计数没法定位。留一小圈原始参数，出问题时有据可查，不用猜。
const RAW_SLOTS: usize = 16;
static RAW: [AtomicU64; RAW_SLOTS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static RAW_IDX: AtomicUsize = AtomicUsize::new(0);

/// 把 `(event, idObject, idChild)` 压成一个 u64 存进环形缓冲。
fn pack(event: u32, id_object: i32, id_child: i32) -> u64 {
    ((event as u64) << 32)
        | (((id_object as u32 as u64) & 0xFFFF) << 16)
        | ((id_child as u32 as u64) & 0xFFFF)
}

fn unpack(v: u64) -> (u32, i32, i32) {
    let event = (v >> 32) as u32;
    // 取回 16 位后按有符号解释：OBJID_CLIENT = -4 这类负值要还原
    let id_object = ((v >> 16) & 0xFFFF) as u16 as i16 as i32;
    let id_child = (v & 0xFFFF) as u16 as i16 as i32;
    (event, id_object, id_child)
}

fn record_raw(event: u32, id_object: i32, id_child: i32) {
    let i = RAW_IDX.fetch_add(1, Ordering::Relaxed) % RAW_SLOTS;
    RAW[i].store(pack(event, id_object, id_child), Ordering::Relaxed);
}

/// 取一小段最近收到的原始事件（**含被过滤掉的**），按收到顺序。
pub fn raw_recent() -> Vec<(u32, i32, i32)> {
    let n = RAW_IDX.load(Ordering::Relaxed);
    let count = n.min(RAW_SLOTS);
    let mut out = Vec::with_capacity(count);
    let start = n.saturating_sub(RAW_SLOTS);
    for k in start..n {
        let v = RAW[k % RAW_SLOTS].load(Ordering::Relaxed);
        if v != 0 {
            out.push(unpack(v));
        }
    }
    out
}

/// 清空原始事件缓冲与计数。测试前调用，好让读到的事件都属于这一次。
pub fn clear_raw() {
    for slot in RAW.iter() {
        slot.store(0, Ordering::Relaxed);
    }
    RAW_IDX.store(0, Ordering::Relaxed);
}

/// `idObject` 的可读名字（按 `winuser.h` 里 `OBJID_*` 的**实际取值**）。
///
/// 这些值是**负数**且从 -1 开始往下排，很容易凭印象写错位。最初这里就写错了两格
/// （把 -2 当成 caret、-9 当成"窗口"），结果诊断输出反而误导人。
pub fn objid_name(id: i32) -> &'static str {
    match id {
        0 => "OBJID_WINDOW（窗口自身）",
        -1 => "OBJID_SYSMENU（系统菜单）",
        -2 => "OBJID_TITLEBAR（标题栏）",
        -3 => "OBJID_MENU（菜单）",
        -4 => "OBJID_CLIENT（客户区）",
        -5 => "OBJID_VSCROLL（垂直滚动条）",
        -6 => "OBJID_HSCROLL（水平滚动条）",
        -7 => "OBJID_SIZEGRIP（尺寸握把）",
        -8 => "OBJID_CARET（文本插入点）",
        -9 => "OBJID_CURSOR（鼠标指针）",
        -10 => "OBJID_ALERT（警告框）",
        -11 => "OBJID_SOUND（声音）",
        -12 => "OBJID_QUERYCLASSNAMEIDX",
        -16 => "OBJID_NATIVEOM",
        _ => "（其它 / 子元素）",
    }
}

/// 这个事件该被计数吗。
///
/// 只认「事件主体就是窗口本身」的（`idObject == OBJID_WINDOW`，即 0）。
/// `idObject` 非 0 时主体是窗口内部的**部件**：标题栏（-2）、客户区（-4）、
/// 鼠标指针（-9）、文本插入点（-8）等等。鼠标在窗口上一划就会报一批，
/// 频率极高且和窗口在**哪块屏**毫无关系，必须在回调最前面拦掉。
pub fn should_count(id_object: i32, id_child: i32, hwnd: ffi::HWND) -> bool {
    id_object == ffi::OBJID_WINDOW && id_child == 0 && hwnd != 0
}

unsafe extern "system" fn win_event_proc(
    _hook: ffi::HWINEVENTHOOK,
    event: ffi::DWORD,
    hwnd: ffi::HWND,
    id_object: ffi::LONG,
    id_child: ffi::LONG,
    _thread: ffi::DWORD,
    _time: ffi::DWORD,
) {
    CALLS.fetch_add(1, Ordering::Relaxed);
    record_raw(event, id_object, id_child);
    if !should_count(id_object, id_child, hwnd) {
        return;
    }
    LAST_ID.store(event, Ordering::Relaxed);
    EVENTS.fetch_add(1, Ordering::Relaxed);
}

/// 装好的一套钩子。`Drop` 时会停掉钩子线程。
///
/// 全程只允许存在一个实例——计数用的是模块级静态变量，因为 `SetWinEventHook`
/// 的回调签名里没有"用户数据"参数，没法把状态传进去。
pub struct WindowEventHook {
    thread_id: u32,
    /// 两条钩子的句柄，Drop 时解掉。
    handles: Vec<ffi::HWINEVENTHOOK>,
}

impl WindowEventHook {
    /// 装上三段钩子：创建/销毁/显隐（`0x8000..=0x8003`）、位置变化（`0x800B`）、
    /// 系统级前台切换/最小化（`0x0003..=0x0017`）。范围怎么取舍、砍掉了哪些
    /// 噪声事件，见线程闭包里的注释。
    pub fn install() -> Result<Self, String> {
        EVENTS.store(0, Ordering::Relaxed);
        CALLS.store(0, Ordering::Relaxed);
        LAST_ID.store(0, Ordering::Relaxed);
        clear_raw();

        let (tx, rx) = channel::<Result<(u32, Vec<ffi::HWINEVENTHOOK>), String>>();

        std::thread::Builder::new()
            .name("audiofollow-winevent".into())
            .spawn(move || unsafe {
                let mut handles: Vec<ffi::HWINEVENTHOOK> = Vec::new();

                // 对象级·其一：窗口创建/销毁/显示/隐藏（0x8000..=0x8003）。
                //
                // 刻意**不**把范围一口气拉到 0x800C：中间夹着的 REORDER / FOCUS /
                // SELECTION×4 / STATECHANGE 与"窗口在哪块屏"毫无关系，却同样通过
                // OBJID_WINDOW 过滤、同样置脏。持续变标题的应用（音乐播放器滚动
                // 歌名）叠加 1s 去抖上限，会造成每秒一次全量同步（进程快照 + 会话
                // 枚举）白做。
                let h1 = ffi::SetWinEventHook(
                    ffi::EVENT_OBJECT_CREATE,
                    ffi::EVENT_OBJECT_HIDE,
                    0, // hmod：进程内钩子传 0
                    Some(win_event_proc),
                    0, // 全部进程
                    0, // 全部线程
                    ffi::WINEVENT_OUTOFCONTEXT,
                );
                if h1 != 0 {
                    handles.push(h1);
                }

                // 对象级·其二：位置变化（0x800B，单点）。归属判定只看窗口矩形，
                // 矩形不动（标题变、焦点变、状态变）就不需要重新判定。
                let h2 = ffi::SetWinEventHook(
                    ffi::EVENT_OBJECT_LOCATIONCHANGE,
                    ffi::EVENT_OBJECT_LOCATIONCHANGE,
                    0,
                    Some(win_event_proc),
                    0,
                    0,
                    ffi::WINEVENT_OUTOFCONTEXT,
                );
                if h2 != 0 {
                    handles.push(h2);
                }

                // 系统级：前台切换、最小化/还原（0x0003..=0x0017）。
                // 前台切换是另一条主路径：焦点从 A 屏的窗口换到 B 屏的窗口，
                // 两块屏的规则可能不同，必须重新判定。
                let h3 = ffi::SetWinEventHook(
                    ffi::EVENT_SYSTEM_FOREGROUND,
                    ffi::EVENT_SYSTEM_MINIMIZEEND,
                    0,
                    Some(win_event_proc),
                    0,
                    0,
                    ffi::WINEVENT_OUTOFCONTEXT,
                );
                if h3 != 0 {
                    handles.push(h3);
                }

                if handles.is_empty() {
                    let _ = tx.send(Err(format!(
                        "SetWinEventHook 失败（GetLastError={}）",
                        ffi::GetLastError()
                    )));
                    return;
                }

                let tid = ffi::GetCurrentThreadId();
                // 只把句柄副本报给主线程用来计数，线程里这份留着给自己收尾。
                if tx.send(Ok((tid, handles.clone()))).is_err() {
                    return;
                }

                // 钩子装好后必须有人泵消息，事件才会被派发过来。
                // GetMessageW 在收到 WM_QUIT 时返回 0，出错返回 -1，两者都该退出。
                let mut msg: ffi::MSG = std::mem::zeroed();
                while ffi::GetMessageW(&mut msg, 0, 0, 0) > 0 {
                    ffi::TranslateMessage(&msg);
                    ffi::DispatchMessageW(&msg);
                }

                for h in handles {
                    ffi::UnhookWinEvent(h);
                }
            })
            .map_err(|e| format!("无法创建事件钩子线程：{e}"))?;

        match rx.recv() {
            Ok(Ok((thread_id, handles))) => Ok(Self { thread_id, handles }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("事件钩子线程在装好之前就退出了".into()),
        }
    }

    /// 取走自上次调用以来累计的事件数。返回 0 表示没有新事件。
    pub fn take_events(&self) -> usize {
        EVENTS.swap(0, Ordering::AcqRel)
    }

    /// 回调被调用的总次数（含被过滤的）。用来区分"没装上"和"被滤掉了"。
    pub fn total_calls(&self) -> usize {
        CALLS.load(Ordering::Relaxed)
    }

    /// 最近一次通过过滤的事件编号。
    pub fn last_event_id(&self) -> u32 {
        LAST_ID.load(Ordering::Relaxed)
    }

    /// 某事件编号的可读名字。覆盖我们关心的那些（含已不挂钩的 NAMECHANGE，
    /// 供诊断输出对照原始事件缓冲时认得出来）。
    pub fn event_name(id: u32) -> &'static str {
        match id {
            ffi::EVENT_SYSTEM_FOREGROUND => "EVENT_SYSTEM_FOREGROUND（前台窗口切换）",
            ffi::EVENT_SYSTEM_MINIMIZESTART => "EVENT_SYSTEM_MINIMIZESTART（开始最小化）",
            ffi::EVENT_SYSTEM_MINIMIZEEND => "EVENT_SYSTEM_MINIMIZEEND（结束最小化）",
            ffi::EVENT_OBJECT_CREATE => "EVENT_OBJECT_CREATE（窗口创建）",
            ffi::EVENT_OBJECT_DESTROY => "EVENT_OBJECT_DESTROY（窗口销毁）",
            ffi::EVENT_OBJECT_SHOW => "EVENT_OBJECT_SHOW（窗口显示）",
            ffi::EVENT_OBJECT_HIDE => "EVENT_OBJECT_HIDE（窗口隐藏）",
            ffi::EVENT_OBJECT_LOCATIONCHANGE => "EVENT_OBJECT_LOCATIONCHANGE（窗口移动/缩放）",
            ffi::EVENT_OBJECT_NAMECHANGE => "EVENT_OBJECT_NAMECHANGE（标题变化）",
            _ => "（未知）",
        }
    }

    pub fn hooked_count(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for WindowEventHook {
    fn drop(&mut self) {
        // 让钩子线程的 GetMessageW 收到 WM_QUIT 退出，线程里会自己 UnhookWinEvent。
        unsafe { ffi::PostThreadMessageW(self.thread_id, ffi::WM_QUIT, 0, 0) };
    }
}

/// 自检用：人为制造一个事件，看钩子能不能收到。
///
/// `NotifyWinEvent` 是公开 API，语义就是"假装发生了这个事件"。
/// 返回 `false` 表示调用失败（一般来说不会）。
pub fn synthesize(event: u32) -> bool {
    unsafe {
        let desk = ffi::GetDesktopWindow();
        if desk == 0 {
            return false;
        }
        ffi::NotifyWinEvent(event, desk, ffi::OBJID_WINDOW, 0);
    }
    true
}

/// 自检用：把某个**别人的**窗口平移 2 px 再挪回来，制造一个真实的
/// `EVENT_OBJECT_LOCATIONCHANGE`。
///
/// 只在 `NotifyWinEvent` 证实不了钩子通着的时候才用。挑选目标时刻意保守：
/// 只动我们自己窗口以外的、尺寸正常、非工具窗口的可见顶层窗口，且不激活、不改 z 序。
pub fn jiggle_some_window() -> Result<String, String> {
    let self_pid = unsafe { ffi::GetCurrentProcessId() };
    let mut candidates: Vec<ffi::HWND> = Vec::new();
    unsafe {
        ffi::EnumWindows(
            Some(collect_candidate),
            &mut candidates as *mut Vec<ffi::HWND> as isize,
        );
    }

    for hwnd in candidates {
        let mut pid = 0u32;
        unsafe { ffi::GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == self_pid || pid == 0 {
            continue;
        }
        let mut r = ffi::Rect::default();
        if unsafe { ffi::GetWindowRect(hwnd, &mut r) } == 0 || r.is_empty() {
            continue;
        }
        // 只碰尺寸正常的窗口：太小的可能是浮层，太大的可能是全屏应用/游戏。
        if r.width() < 200 || r.height() < 120 || r.width() > 3000 || r.height() > 2000 {
            continue;
        }

        let moved = unsafe {
            ffi::SetWindowPos(
                hwnd,
                0,
                r.left + 2,
                r.top,
                0,
                0,
                ffi::SWP_NOSIZE | ffi::SWP_NOZORDER | ffi::SWP_NOACTIVATE,
            )
        };
        if moved == 0 {
            continue;
        }
        // 立刻挪回去。就算后面哪一步出错，位移也只有 2px。
        unsafe {
            ffi::SetWindowPos(
                hwnd,
                0,
                r.left,
                r.top,
                0,
                0,
                ffi::SWP_NOSIZE | ffi::SWP_NOZORDER | ffi::SWP_NOACTIVATE,
            )
        };
        let title = unsafe {
            let mut buf = vec![0u16; 128];
            let n = ffi::GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                String::new()
            }
        };
        return Ok(format!("hwnd {hwnd} 「{title}」（已挪回原位）"));
    }
    Err("找不到可以用来测试的窗口".into())
}

unsafe extern "system" fn collect_candidate(hwnd: ffi::HWND, lparam: isize) -> i32 {
    let out = &mut *(lparam as *mut Vec<ffi::HWND>);
    if ffi::IsWindowVisible(hwnd) != 0 {
        let ex = ffi::GetWindowLongW(hwnd, ffi::GWL_EXSTYLE) as i64;
        if ex & ffi::WS_EX_TOOLWINDOW == 0 {
            out.push(hwnd);
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_window_level_events_are_counted() {
        // 正常的窗口事件
        assert!(should_count(0, 0, 12345));
        // 窗口内部的部件：标题栏、客户区、鼠标指针、插入点 —— 高频噪声，必须滤掉
        assert!(!should_count(-2, 0, 12345)); // OBJID_TITLEBAR
        assert!(!should_count(-4, 0, 12345)); // OBJID_CLIENT
        assert!(!should_count(-8, 0, 12345)); // OBJID_CARET
        assert!(!should_count(-9, 0, 12345)); // OBJID_CURSOR
        // 有 idChild 说明是子元素
        assert!(!should_count(0, 7, 12345));
        // 没有窗口句柄的事件对我们没意义
        assert!(!should_count(0, 0, 0));
    }

    #[test]
    fn event_names_are_mapped() {
        assert!(WindowEventHook::event_name(ffi::EVENT_OBJECT_LOCATIONCHANGE).contains("移动"));
        assert_eq!(WindowEventHook::event_name(0x7FFF), "（未知）");
    }

    #[test]
    fn raw_event_packing_roundtrips_including_negatives() {
        // OBJID_CLIENT = -4 这类负值必须能原样还原，否则诊断输出会骗人
        for (ev, obj, child) in [
            (ffi::EVENT_OBJECT_LOCATIONCHANGE, 0, 0),
            (ffi::EVENT_OBJECT_LOCATIONCHANGE, -4, 0),
            (ffi::EVENT_SYSTEM_FOREGROUND, 0, 0),
            (ffi::EVENT_OBJECT_NAMECHANGE, -2, 7),
            (ffi::EVENT_OBJECT_SHOW, -9, 0),
        ] {
            assert_eq!(unpack(pack(ev, obj, child)), (ev, obj, child));
        }
    }

    #[test]
    fn objid_names_match_the_real_constants() {
        // 这几个值来自 winuser.h，写错位会让诊断输出反过来误导人
        assert_eq!(ffi::OBJID_WINDOW, 0);
        assert!(objid_name(0).contains("窗口自身"));
        assert!(objid_name(-2).contains("标题栏"));
        assert!(objid_name(-4).contains("客户区"));
        assert!(objid_name(-8).contains("插入点"));
        assert!(objid_name(-9).contains("鼠标指针"));
    }
}
