//! 音频会话枚举。
//!
//! **本模块不参与路由决策。** 路由只看窗口在哪块屏，不看应用有没有在出声
//! （这也是 SoundSwitch 的做法：拿到窗口 PID 直接设路由）。
//!
//! 它存在的唯一理由是诊断：`PROCESS_NO_AUDIO` 是因为"进程还没有音频会话"才会出现的，
//! 想解释一次写入为什么没生效，就得能看出哪些进程现在有会话。

use std::ffi::c_void;
use std::ptr;

use super::com::{succeeded, ComPtr, Guid};
use super::endpoint::{with_render_devices, E_RENDER, ROLE_CONSOLE};
use super::IID_IAudioSessionControl2;
use super::IID_IAudioSessionManager2;
use super::com::CLSCTX_ALL;

type FnActivate =
    unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> i32;
type FnGetSessions = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32;
type FnCount = unsafe extern "system" fn(*mut c_void, *mut i32) -> i32;
type FnSession = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32;
type FnQI = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
type FnGetPid = unsafe extern "system" fn(*mut c_void, *mut u32) -> i32;
type FnGetDefault = unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> i32;

/// 收集所有当前持有音频会话的进程 ID（跨**全部**渲染端点合并）。
pub fn session_pids(enumerator: &ComPtr) -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();

    let _ = with_render_devices(enumerator, |device| {
        unsafe {
            let activate: FnActivate = device.call(3);
            let mut mgr: *mut c_void = ptr::null_mut();
            if !succeeded(activate(
                device.as_ptr(),
                &IID_IAudioSessionManager2,
                CLSCTX_ALL,
                ptr::null_mut(),
                &mut mgr,
            )) {
                return;
            }
            let Some(mgr) = ComPtr::from_raw(mgr) else { return };

            let get_sessions: FnGetSessions = mgr.call(5); // IAudioSessionManager2::GetSessionEnumerator
            let mut it: *mut c_void = ptr::null_mut();
            if !succeeded(get_sessions(mgr.as_ptr(), &mut it)) {
                return;
            }
            let Some(it) = ComPtr::from_raw(it) else { return };

            let count: FnCount = it.call(3);
            let get: FnSession = it.call(4);
            let mut n = 0i32;
            if !succeeded(count(it.as_ptr(), &mut n)) {
                return;
            }

            for i in 0..n {
                let mut ctl: *mut c_void = ptr::null_mut();
                if !succeeded(get(it.as_ptr(), i, &mut ctl)) {
                    continue;
                }
                let Some(ctl) = ComPtr::from_raw(ctl) else { continue };

                let qi: FnQI = ctl.call(0);
                let mut c2: *mut c_void = ptr::null_mut();
                if succeeded(qi(ctl.as_ptr(), &IID_IAudioSessionControl2, &mut c2)) {
                    if let Some(c2) = ComPtr::from_raw(c2) {
                        // IAudioSessionControl2::GetProcessId —— 槽位 14。
                        // 一旦错位会读到完全不相干的函数，所以拿 0 和自身 PID 做合理性过滤。
                        let get_pid: FnGetPid = c2.call(14);
                        let mut pid = 0u32;
                        if succeeded(get_pid(c2.as_ptr(), &mut pid))
                            && pid != 0
                            && !pids.contains(&pid)
                        {
                            pids.push(pid);
                        }
                    }
                }
            }
        }
    });

    pids.sort_unstable();
    pids
}

/// 只取系统默认渲染端点上的会话 PID。用来判断"默认设备上有没有人在出声"。
pub fn session_pids_on_default(enumerator: &ComPtr) -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        let get_default: FnGetDefault = enumerator.call(4);
        let mut dev: *mut c_void = ptr::null_mut();
        if !succeeded(get_default(enumerator.as_ptr(), E_RENDER, ROLE_CONSOLE, &mut dev)) {
            return pids;
        }
        let Some(dev) = ComPtr::from_raw(dev) else { return pids };

        let activate: FnActivate = dev.call(3);
        let mut mgr: *mut c_void = ptr::null_mut();
        if !succeeded(activate(
            dev.as_ptr(),
            &IID_IAudioSessionManager2,
            CLSCTX_ALL,
            ptr::null_mut(),
            &mut mgr,
        )) {
            return pids;
        }
        let Some(mgr) = ComPtr::from_raw(mgr) else { return pids };

        let get_sessions: FnGetSessions = mgr.call(5);
        let mut it: *mut c_void = ptr::null_mut();
        if !succeeded(get_sessions(mgr.as_ptr(), &mut it)) {
            return pids;
        }
        let Some(it) = ComPtr::from_raw(it) else { return pids };

        let count: FnCount = it.call(3);
        let get: FnSession = it.call(4);
        let mut n = 0i32;
        if count(it.as_ptr(), &mut n) < 0 {
            return pids;
        }
        for i in 0..n {
            let mut ctl: *mut c_void = ptr::null_mut();
            if get(it.as_ptr(), i, &mut ctl) < 0 {
                continue;
            }
            let Some(ctl) = ComPtr::from_raw(ctl) else { continue };
            let qi: FnQI = ctl.call(0);
            let mut c2: *mut c_void = ptr::null_mut();
            if qi(ctl.as_ptr(), &IID_IAudioSessionControl2, &mut c2) >= 0 {
                if let Some(c2) = ComPtr::from_raw(c2) {
                    let get_pid: FnGetPid = c2.call(14);
                    let mut pid = 0u32;
                    if get_pid(c2.as_ptr(), &mut pid) >= 0 && pid != 0 && !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    pids.sort_unstable();
    pids
}
