//! 单实例保护，外加一个"没有窗口可用时"的消息框。
//!
//! 为什么需要它：本程序是托盘常驻的，而且现在**点关闭只是隐藏窗口**（见 `ui::run`）。
//! 用户很容易以为"窗口没了 = 退出了"，于是再双击一次 exe。两份实例同时跑会怎样：
//!
//! - 两个托盘图标，用户分不清哪个是哪个；
//! - 两份 `reconcile` 各自维护自己的"原值备份"，各自往系统里写路由 ——
//!   退出时 A 还原成 A 看到的原值，而 B 已经把值改过了，还原互相打架；
//! - 备份表各记各的，最后留在系统里的记录取决于谁后退出，**不可预测**。
//!
//! 一个命名互斥量就能挡掉，代价是几十行 FFI。第二实例启动时提示一句然后退出，
//! 而不是静静地什么都不做 —— 否则用户点两下没反应，更糊涂。
//!
//! `--startup`（开机自动启动）那条路不一样：开机时绝不该弹框，所以那条路
//! 检测到已有实例就**静默退出**。

use std::ffi::c_void;

use super::ffi::{self, HANDLE};

/// 单实例互斥量的名字。
///
/// `Local\` 前缀把作用域限定在**当前登录会话**：多用户同时登录时各跑各的托盘图标，
/// 互不干扰；同一个用户下面则严格唯一。
const SINGLE_INSTANCE_NAME: &str = r"Local\AudioFollow.SingleInstance";

/// 持有互斥量句柄。活着就代表"我是唯一实例"；`Drop` 时释放。
pub struct SingleInstance {
    handle: HANDLE,
}

/// [`SingleInstance::acquire`] 的结果。
pub enum Acquire {
    /// 拿到了，可以照常启动。
    First(SingleInstance),
    /// 已经有一份在跑了。
    AlreadyRunning,
    /// 互斥量本身创建失败（错误码）。保护没生效，但不该因此拦着用户用程序。
    Unavailable(u32),
}

impl SingleInstance {
    /// 尝试成为唯一实例。
    pub fn acquire() -> Acquire {
        let name = ffi::wide(SINGLE_INSTANCE_NAME);
        // 不开 bInitialOwner：我们不做跨进程同步，只借它的"唯一性"。
        // 拥有权会让持有时的工作线程被当成互斥量属主，反而多一层语义。
        let handle = unsafe { ffi::CreateMutexW(std::ptr::null_mut::<c_void>(), 0, name.as_ptr()) };
        if handle == 0 {
            return Acquire::Unavailable(unsafe { ffi::GetLastError() });
        }
        if unsafe { ffi::GetLastError() } == ffi::ERROR_ALREADY_EXISTS {
            unsafe { ffi::CloseHandle(handle) };
            return Acquire::AlreadyRunning;
        }
        Acquire::First(Self { handle })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe { ffi::CloseHandle(self.handle) };
    }
}

/// 弹一个消息框。
///
/// 只在"什么都还没有显示出来"的时候用——此刻没有窗口可以承载提示，
/// 而这里正是最需要说一句话的时刻。
pub fn message_box(title: &str, text: &str) {
    let t = ffi::wide(title);
    let m = ffi::wide(text);
    let flags = ffi::MB_OK | ffi::MB_ICONINFORMATION | ffi::MB_SETFOREGROUND | ffi::MB_TOPMOST;
    unsafe { ffi::MessageBoxW(0, m.as_ptr(), t.as_ptr(), flags) };
}
