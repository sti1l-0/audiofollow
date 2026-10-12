//! `Windows.Media.Internal.AudioPolicyConfig` —— 按应用分流音频的接口。
//!
//! # 这是一个未公开接口
//!
//! 微软没有为它发布任何文档。它由 `AudioSes.dll` 导出，被系统自己的"音量合成器"使用。
//! 已知事实（来自对系统行为的观测与多个开源项目的独立实现，接口事实本身不受版权保护）：
//!
//! - 激活方式不是 `RoGetActivationFactory`，而是直接调 `AudioSes.dll!DllGetActivationFactory`，
//!   好处是不需要先初始化 WinRT。
//! - 类名 `Windows.Media.Internal.AudioPolicyConfig`。
//! - 接口 IID 随系统版本变过，所以**运行时用 `IInspectable::GetIids()` 发现**，
//!   再用已知白名单校验，而不是硬编码一个版本。
//! - vtable 槽位：25 = `SetPersistedDefaultAudioEndpoint`，
//!   26 = `GetPersistedDefaultAudioEndpoint`，27 = `ClearAllPersistedApplicationDefaultEndpoints`。
//! - 设备 ID 必须是 SWD 形式（见 [`super::endpoint::MMDEVAPI_TOKEN`]）。
//! - 目标进程若尚未建立音频会话，写入返回 `PROCESS_NO_AUDIO`（即 `E_INVALIDARG`）并且**什么都不做**。
//!
//! # 本实现的防护
//!
//! 1. IID 运行时发现 + 白名单校验；不在白名单里就**拒绝构造**，绝不拿未知 vtable 去按槽位调用。
//! 2. 写入后由调用方读回校验（见 [`crate::reconcile`]）。
//! 3. 库的常规路径**不会**碰槽位 27；`clear_all_persisted` 需要显式调用。

use std::ffi::c_void;
use std::ptr;

use super::com::{succeeded, ComError, ComPtr, Guid, HString, HRESULT, E_INVALIDARG};
use super::endpoint::{strip_swd, MMDEVAPI_TOKEN};
use crate::win::ffi;

/// `PROCESS_NO_AUDIO`：目标进程还没有音频会话，调用被静默忽略。
///
/// 它的数值与 `E_INVALIDARG` 相同（`0x80070057`），这是接口自己的编码方式。
pub const PROCESS_NO_AUDIO: HRESULT = E_INVALIDARG;

/// 历史上出现过的合法 IID。进程内不做版本判断，而是运行时发现 + 对着这张表校验。
pub const KNOWN_VALID_IIDS: [&str; 3] = [
    "2a59116d-6c4f-45e0-a74f-707e3fef9258", // Windows 10 21H2 之前
    "ab3d4648-e242-459f-b02f-541c70306324", // 21H2 / Windows 11
    "32aa8e18-6496-4e24-9f94-b800e7eccc45", // Windows 10 10.0.16299
];

pub const CLASS_NAME: &str = "Windows.Media.Internal.AudioPolicyConfig";
const DLL_NAME: &str = "AudioSes.dll";

/// 某个进程当前的持久化音频路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedRoute {
    /// 该进程此刻没有音频会话，接口拒绝作答。不是错误，是需要稍后重试。
    NoAudioSession,
    /// 从未单独设置过 → 跟随系统默认设备。
    FollowSystemDefault,
    /// 被固定到某个端点上，值为**去掉 SWD 前缀**的原始设备 ID。
    Pinned(String),
}

impl PersistedRoute {
    pub fn device_id(&self) -> Option<&str> {
        match self {
            PersistedRoute::Pinned(id) => Some(id),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            PersistedRoute::NoAudioSession => "无音频会话（接口暂不作答）".into(),
            PersistedRoute::FollowSystemDefault => "跟随系统默认".into(),
            PersistedRoute::Pinned(id) => format!("已固定到 {id}"),
        }
    }
}

fn is_whitelisted(g: &Guid) -> bool {
    let s = g.to_string();
    KNOWN_VALID_IIDS.contains(&s.as_str())
}

/// 已就绪的 AudioPolicyConfig 接口。
pub struct AudioPolicyConfig {
    /// 激活工厂本体。必须持有，否则被 QI 出来的接口会一起失效。
    _factory: ComPtr,
    iface: ComPtr,
    picked_iid: Guid,
    all_iids: Vec<Guid>,
}

impl AudioPolicyConfig {
    /// 激活并锁定接口。IID 不在白名单里时返回错误（而不是硬着头皮调用）。
    pub fn create() -> Result<Self, ComError> {
        unsafe {
            let dll = ffi::wide(DLL_NAME);
            let module = ffi::LoadLibraryW(dll.as_ptr());
            if module.is_null() {
                return Err(ComError::msg(format!("LoadLibraryW({DLL_NAME}) 失败")));
            }

            type FnDllGetActivationFactory =
                unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
            let proc = ffi::GetProcAddress(module, b"DllGetActivationFactory\0".as_ptr());
            if proc.is_null() {
                return Err(ComError::msg(format!("{DLL_NAME} 未导出 DllGetActivationFactory")));
            }
            let dll_get_activation_factory: FnDllGetActivationFactory = std::mem::transmute(proc);

            let class_name = HString::new(CLASS_NAME)?;
            let mut factory_raw: *mut c_void = ptr::null_mut();
            let hr = dll_get_activation_factory(class_name.as_raw(), &mut factory_raw);
            if !succeeded(hr) {
                return Err(ComError::new(
                    format!("DllGetActivationFactory({CLASS_NAME})"),
                    hr,
                ));
            }
            let factory = ComPtr::from_raw(factory_raw)
                .ok_or_else(|| ComError::msg("DllGetActivationFactory 返回空指针"))?;

            // ---- IID 运行时发现：不硬编码版本 ----
            type FnGetIids =
                unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut Guid) -> HRESULT;
            let get_iids: FnGetIids = factory.call(3); // IInspectable::GetIids
            let mut count = 0u32;
            let mut iids: *mut Guid = ptr::null_mut();
            let hr = get_iids(factory.as_ptr(), &mut count, &mut iids);
            if !succeeded(hr) {
                return Err(ComError::new("IInspectable::GetIids", hr));
            }
            if count == 0 || iids.is_null() {
                return Err(ComError::msg("GetIids 返回空列表"));
            }
            let all: Vec<Guid> = (0..count as usize).map(|i| *iids.add(i)).collect();
            ffi::CoTaskMemFree(iids as *mut c_void);

            // 从后往前找白名单里的 IID：内部接口 historically 排在最后，
            // 但万一将来系统在它后面又追加了新接口，"取最后一个"就会错位。
            let picked = all
                .iter()
                .rev()
                .copied()
                .find(is_whitelisted)
                .ok_or_else(|| {
                    let list = all
                        .iter()
                        .map(|g| g.to_string())
                        .collect::<Vec<_>>()
                        .join("\n    ");
                    ComError::msg(format!(
                        "GetIids 返回的 {count} 个接口中没有已知可用的 IID，拒绝按槽位调用。\n\
                         实际拿到：\n    {list}\n\
                         如果系统刚更新过，需要把新的 IID 加进 KNOWN_VALID_IIDS。"
                    ))
                })?;

            let iface = factory.query_interface(&picked)?;

            Ok(Self { _factory: factory, iface, picked_iid: picked, all_iids: all })
        }
    }

    pub fn picked_iid(&self) -> Guid {
        self.picked_iid
    }

    pub fn all_iids(&self) -> &[Guid] {
        &self.all_iids
    }

    /// 读取某个进程在指定数据流 / 角色上的持久化路由。
    pub fn get_persisted(
        &self,
        pid: u32,
        flow: i32,
        role: i32,
    ) -> Result<PersistedRoute, ComError> {
        type FnGet =
            unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut *mut c_void) -> HRESULT;
        let get: FnGet = unsafe { self.iface.call(26) };

        let mut out: *mut c_void = ptr::null_mut();
        let hr = unsafe { get(self.iface.as_ptr(), pid, flow, role, &mut out) };

        if hr == PROCESS_NO_AUDIO {
            return Ok(PersistedRoute::NoAudioSession);
        }
        if !succeeded(hr) {
            return Err(ComError::new(
                format!("GetPersistedDefaultAudioEndpoint(pid={pid}, flow={flow}, role={role})"),
                hr,
            ));
        }

        let h = unsafe { HString::from_raw(out) };
        let raw = h.to_string();
        if raw.is_empty() {
            // 注意：没有单独设置过的进程返回的是 **S_OK + 空字符串**，而不是 PROCESS_NO_AUDIO。
            // 差分逻辑必须把空值当成合法值处理，否则会反复重复写入。
            return Ok(PersistedRoute::FollowSystemDefault);
        }
        Ok(PersistedRoute::Pinned(strip_swd(&raw)))
    }

    /// 把某个进程固定到指定端点。
    ///
    /// `swd_device_id` 必须是完整的 SWD 形式（用 [`super::endpoint::AudioEndpoint::swd_id`] 生成）。
    /// 返回 `Ok(false)` 表示进程还没有音频会话、系统没接受这次写入——调用方应当稍后重试。
    pub fn set_persisted(
        &self,
        pid: u32,
        flow: i32,
        role: i32,
        swd_device_id: &str,
    ) -> Result<bool, ComError> {
        debug_assert!(
            swd_device_id.starts_with(MMDEVAPI_TOKEN),
            "设备 ID 必须以 SWD 前缀开头，否则写入会静默失效"
        );
        self.set_persisted_raw(pid, flow, role, swd_device_id)
    }

    /// 不做任何格式校验的写入。只给自检程序用——它的用途之一恰恰是探测
    /// 「写空字符串能不能把应用恢复成跟随系统默认」，而空字符串是故意不符合格式的。
    pub fn set_persisted_raw(
        &self,
        pid: u32,
        flow: i32,
        role: i32,
        device_id: &str,
    ) -> Result<bool, ComError> {
        // HSTRING 在这里是**按值**传的（不是指针的指针）。
        type FnSet =
            unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> HRESULT;
        let set: FnSet = unsafe { self.iface.call(25) };

        let id = HString::new(device_id)?;
        let hr = unsafe { set(self.iface.as_ptr(), pid, flow, role, id.as_raw()) };

        if hr == PROCESS_NO_AUDIO {
            return Ok(false);
        }
        if !succeeded(hr) {
            return Err(ComError::new(
                format!("SetPersistedDefaultAudioEndpoint(pid={pid}, flow={flow}, role={role})"),
                hr,
            ));
        }
        Ok(true)
    }

    /// 把某个进程恢复成"跟随系统默认"，做法是写入一个**空字符串**。
    ///
    /// # 这是实测出来的行为，不是文档
    ///
    /// 接口没提供"清除单个进程"的操作（槽位 27 是全局清除，会连别人的一起清掉）。
    /// 直觉上会认为写入空值会被拒绝——SoundSwitch 的代码里就有 `if (string.IsNullOrEmpty(deviceId)) return false;`
    /// 的保护，说明它的作者也没试过。实测（Windows 11 26100，`ab3d4648-…` 接口）：
    /// **写入空 HSTRING 返回 `S_OK`，且读回变成"跟随系统默认"**。
    ///
    /// 因此本程序退出时可以完整还原，不留残余。但因为是未文档化行为，
    /// 老版本系统上是否同样成立未经验证，所以上层仍要按"可能失败"处理。
    pub fn clear_persisted(&self, pid: u32, flow: i32, role: i32) -> Result<bool, ComError> {
        self.set_persisted_raw(pid, flow, role, "")
    }

    /// **危险**：清空**所有**应用的持久化路由，不只是本程序设置的那些。
    ///
    /// 这是接口唯一提供的"清除"操作，粒度是全局的——它没有"只清某个进程"的版本。
    /// 因此常规代码路径绝不调用它，只有用户显式要求重置时才用。
    pub fn clear_all_persisted(&self) -> Result<(), ComError> {
        type FnClearAll = unsafe extern "system" fn(*mut c_void) -> HRESULT;
        let clear: FnClearAll = unsafe { self.iface.call(27) };
        let hr = unsafe { clear(self.iface.as_ptr()) };
        if !succeeded(hr) {
            return Err(ComError::new("ClearAllPersistedApplicationDefaultEndpoints", hr));
        }
        Ok(())
    }
}
