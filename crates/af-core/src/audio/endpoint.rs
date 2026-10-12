//! Core Audio 的**文档化**部分：枚举音频端点、读设备友好名、取默认端点。
//!
//! 这段全部是公开接口（`IMMDeviceEnumerator` 系列），没有任何未公开成分。

// 同 audio/mod.rs：COM 接口常量保留系统头文件里的原始大小写
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::ptr;

use super::com::{succeeded, ComError, ComPtr, Guid, HRESULT};
use crate::win::ffi;

// ------------------------------------------------------------------ 常量 ----

pub const CLSID_MMDeviceEnumerator: Guid =
    Guid::new(0xBCDE_0395, 0xE52F, 0x467C, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
pub const IID_IMMDeviceEnumerator: Guid =
    Guid::new(0xA956_64D2, 0x9614, 0x4F35, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);
pub const IID_IMMDeviceCollection: Guid =
    Guid::new(0x0BD7_A1BE, 0x7A1A, 0x44DB, [0x83, 0x97, 0xCC, 0x53, 0x92, 0x38, 0x7B, 0x5E]);
pub const IID_IMMDevice: Guid =
    Guid::new(0xD666_063F, 0x1587, 0x4E43, [0x81, 0xF1, 0xB9, 0x48, 0xE8, 0x07, 0x36, 0x3F]);
pub const IID_IPropertyStore: Guid =
    Guid::new(0x886D_8EEB, 0x8CF2, 0x4446, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);

/// 设备接口类别。渲染设备与采集设备的 GUID 后缀不同，拼接时不能混。
pub const DEVINTERFACE_AUDIO_RENDER: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
pub const DEVINTERFACE_AUDIO_CAPTURE: &str = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";

/// 设备 ID 必须加的前缀。
///
/// `IMMDevice::GetId()` 返回的是**不带前缀**的原始值（形如 `{0.0.0.00000000}.{guid}`），
/// 而 `SetPersistedDefaultAudioEndpoint` 只认 SWD 形式。格式不对不会报错，而是静默无效——
/// 这是整套实现里埋得最深的一个坑。
pub const MMDEVAPI_TOKEN: &str = r"\\?\SWD#MMDEVAPI#";

/// `EDataFlow`
pub const E_RENDER: i32 = 0;
pub const E_CAPTURE: i32 = 1;
pub const E_ALL: i32 = 2;

/// `ERole`
pub const ROLE_CONSOLE: i32 = 0;
pub const ROLE_MULTIMEDIA: i32 = 1;
pub const ROLE_COMMUNICATIONS: i32 = 2;

/// `DEVICE_STATE_ACTIVE`
pub const DEVICE_STATE_ACTIVE: u32 = 0x0000_0001;

const STGM_READ: u32 = 0x0000_0000;

// ------------------------------------------------------------ PROPVARIANT ----

const VT_LPWSTR: u16 = 31;

#[repr(C, align(8))]
struct PropVariant {
    vt: u16,
    _r1: u16,
    _r2: u16,
    _r3: u16,
    data: [u8; 16],
}

impl PropVariant {
    fn zeroed() -> Self {
        Self { vt: 0, _r1: 0, _r2: 0, _r3: 0, data: [0; 16] }
    }

    /// `VT_LPWSTR` 时取出内部字符串指针。
    fn as_lpwstr(&self) -> Option<*const u16> {
        if self.vt != VT_LPWSTR {
            return None;
        }
        let p = unsafe { *(self.data.as_ptr() as *const *const u16) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }
}

#[repr(C)]
struct PropertyKey {
    fmtid: Guid,
    pid: u32,
}

/// `PKEY_Device_FriendlyName`
const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
    fmtid: Guid::new(0xA45C_254E, 0xDF1C, 0x4EFD, [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0]),
    pid: 14,
};

// ------------------------------------------------------------- 端点模型 ----

/// 一个音频输出端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEndpoint {
    /// `IMMDevice::GetId()` 的原始值，**不带** SWD 前缀。配置里存这个。
    pub id: String,
    /// 友好名，形如 `扬声器 (Realtek(R) Audio)`。
    pub name: String,
    /// 该端点是否就是当前系统默认渲染设备。
    pub is_default: bool,
}

impl AudioEndpoint {
    /// 拼成接口要求的 SWD 形式。
    pub fn swd_id(&self) -> String {
        to_swd(&self.id)
    }

    pub fn label(&self, index: usize) -> String {
        format!(
            "{}. {}{}",
            index + 1,
            self.name,
            if self.is_default { "（系统默认）" } else { "" }
        )
    }
}

/// 由原始 ID 拼 SWD 形式。还原备份时手上只有字符串、没有 [`AudioEndpoint`]，所以单独抽出来。
/// 已经是 SWD 形式的输入会被原样返回，避免拼出双层前缀。
pub fn to_swd(raw_id: &str) -> String {
    if raw_id.starts_with(MMDEVAPI_TOKEN) {
        raw_id.to_string()
    } else {
        format!("{MMDEVAPI_TOKEN}{raw_id}{DEVINTERFACE_AUDIO_RENDER}")
    }
}

/// 把读回来的 SWD 形式还原成原始 ID。读回校验、备份比较都要用。
pub fn strip_swd(raw: &str) -> String {
    let mut s = raw;
    if let Some(rest) = s.strip_prefix(MMDEVAPI_TOKEN) {
        s = rest;
    }
    for suffix in [DEVINTERFACE_AUDIO_RENDER, DEVINTERFACE_AUDIO_CAPTURE] {
        if let Some(rest) = s.strip_suffix(suffix) {
            s = rest;
        }
    }
    s.to_string()
}

// ------------------------------------------------------------ 枚举实现 ----

type FnEnumEndpoints =
    unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> HRESULT;
type FnGetDefault =
    unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> HRESULT;
type FnGetCount = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;
type FnItem = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
type FnGetId = unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT;
type FnOpenStore = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
type FnGetValue =
    unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT;

/// 创建 `IMMDeviceEnumerator`（文档化接口，`CoCreateInstance` 即可）。
pub fn device_enumerator() -> Result<ComPtr, ComError> {
    let mut raw: *mut c_void = ptr::null_mut();
    let hr = unsafe {
        ffi::CoCreateInstance(
            &CLSID_MMDeviceEnumerator,
            ptr::null_mut(),
            super::com::CLSCTX_ALL,
            &IID_IMMDeviceEnumerator,
            &mut raw,
        )
    };
    if !succeeded(hr) {
        return Err(ComError::new("CoCreateInstance(MMDeviceEnumerator)", hr));
    }
    unsafe { ComPtr::from_raw(raw) }
        .ok_or_else(|| ComError::msg("CoCreateInstance(MMDeviceEnumerator) 返回空指针"))
}

/// 取当前系统默认渲染设备。用来在界面上标注"这是系统默认"。
pub fn default_render_endpoint(enumerator: &ComPtr) -> Option<String> {
    unsafe {
        let get_default: FnGetDefault = enumerator.call(4);
        let mut raw: *mut c_void = ptr::null_mut();
        let hr = get_default(enumerator.as_ptr(), E_RENDER, ROLE_CONSOLE, &mut raw);
        if !succeeded(hr) {
            return None;
        }
        let dev = ComPtr::from_raw(raw)?;
        raw_device_id(&dev)
    }
}

/// 遍历所有活跃渲染端点的 `IMMDevice`。
///
/// 抽出来是因为音频会话是**按端点**枚举的：想拿到全局的"谁在出声"，必须先把所有端点的
/// 会话管理器都逛一遍再合并，只查默认设备会漏掉已经被路由到别处的应用。
pub fn with_render_devices<F>(enumerator: &ComPtr, mut f: F) -> Result<(), ComError>
where
    F: FnMut(&ComPtr),
{
    unsafe {
        let enum_endpoints: FnEnumEndpoints = enumerator.call(3);
        let mut coll_raw: *mut c_void = ptr::null_mut();
        let hr = enum_endpoints(
            enumerator.as_ptr(),
            E_RENDER,
            DEVICE_STATE_ACTIVE,
            &mut coll_raw,
        );
        if !succeeded(hr) {
            return Err(ComError::new("IMMDeviceEnumerator::EnumAudioEndpoints", hr));
        }
        let coll = ComPtr::from_raw(coll_raw)
            .ok_or_else(|| ComError::msg("EnumAudioEndpoints 返回空集合"))?;

        let get_count: FnGetCount = coll.call(3);
        let item: FnItem = coll.call(4);
        let mut count = 0u32;
        let hr = get_count(coll.as_ptr(), &mut count);
        if !succeeded(hr) {
            return Err(ComError::new("IMMDeviceCollection::GetCount", hr));
        }

        for i in 0..count {
            let mut dev_raw: *mut c_void = ptr::null_mut();
            if !succeeded(item(coll.as_ptr(), i, &mut dev_raw)) {
                continue;
            }
            if let Some(dev) = ComPtr::from_raw(dev_raw) {
                f(&dev);
            }
        }
        Ok(())
    }
}

/// 枚举所有**活跃**的渲染端点。
pub fn list_render_endpoints(enumerator: &ComPtr) -> Result<Vec<AudioEndpoint>, ComError> {
    let default_id = default_render_endpoint(enumerator);
    let mut out: Vec<AudioEndpoint> = Vec::new();
    with_render_devices(enumerator, |dev| unsafe {
        let Some(id) = raw_device_id(dev) else { return };
        let name = friendly_name(dev).unwrap_or_else(|| "<未知设备>".to_string());
        let is_default = default_id.as_deref() == Some(id.as_str());
        out.push(AudioEndpoint { id, name, is_default });
    })?;
    Ok(out)
}

unsafe fn raw_device_id(device: &ComPtr) -> Option<String> {
    let get_id: FnGetId = device.call(5); // IMMDevice::GetId
    let mut p: *mut u16 = ptr::null_mut();
    if !succeeded(get_id(device.as_ptr(), &mut p)) || p.is_null() {
        return None;
    }
    let s = ffi::pwsz(p, 512);
    ffi::CoTaskMemFree(p as *mut c_void);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

unsafe fn friendly_name(device: &ComPtr) -> Option<String> {
    let open_store: FnOpenStore = device.call(4); // IMMDevice::OpenPropertyStore
    let mut store_raw: *mut c_void = ptr::null_mut();
    if !succeeded(open_store(device.as_ptr(), STGM_READ, &mut store_raw)) {
        return None;
    }
    let store = ComPtr::from_raw(store_raw)?;
    let get_value: FnGetValue = store.call(5); // IPropertyStore::GetValue
    let mut pv = PropVariant::zeroed();
    let hr = get_value(store.as_ptr(), &PKEY_DEVICE_FRIENDLY_NAME, &mut pv);
    let name = if succeeded(hr) {
        pv.as_lpwstr().map(|p| ffi::pwsz(p, 512))
    } else {
        None
    };
    ffi::PropVariantClear(&mut pv as *mut PropVariant as *mut c_void);
    name.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swd_roundtrip() {
        let ep = AudioEndpoint {
            id: "{0.0.0.00000000}.{abc-123}".into(),
            name: "扬声器".into(),
            is_default: true,
        };
        let swd = ep.swd_id();
        assert!(swd.starts_with(MMDEVAPI_TOKEN));
        assert!(swd.ends_with(DEVINTERFACE_AUDIO_RENDER));
        assert_eq!(strip_swd(&swd), ep.id);
        // 原始 ID 传进去也不会被弄坏
        assert_eq!(strip_swd(&ep.id), ep.id);
    }
}
