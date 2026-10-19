//! COM 基础设施：GUID、HRESULT、RAII 接口指针、HSTRING、按槽位索引调用 vtable。
//!
//! 这里刻意不引入 `windows` / `windows-sys`：需要的接口总共不到十个，
//! 手写一遍能顺带把"哪些是文档化接口、哪些是系统内部接口"标清楚。

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;

use crate::win::ffi;

pub type HRESULT = i32;

/// `CoInitializeEx` 的 `COINIT_MULTITHREADED`。
pub const COINIT_MULTITHREADED: u32 = 0x0;
/// `CoInitializeEx` 的 `COINIT_APARTMENTTHREADED`。
pub const COINIT_APARTMENTTHREADED: u32 = 0x2;
/// `CLSCTX_ALL`
pub const CLSCTX_ALL: u32 = 0x17;

/// `S_OK`
pub const S_OK: HRESULT = 0;
/// `S_FALSE`
pub const S_FALSE: HRESULT = 1;
/// `E_INVALIDARG`。在 AudioPolicyConfig 的语义里它同时代表 `PROCESS_NO_AUDIO`。
pub const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
/// `RPC_E_CHANGED_MODE`：线程已用另一种套间模型初始化过 COM，不算失败。
pub const RPC_E_CHANGED_MODE: HRESULT = 0x8001_0106u32 as i32;

pub fn succeeded(hr: HRESULT) -> bool {
    hr >= 0
}

pub fn hex(hr: HRESULT) -> String {
    format!("0x{:08X}", hr as u32)
}

// ------------------------------------------------------------------- GUID ----

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

// GUID 是 COM 调用的"类型标识"，布局错了 QI/激活会拿到不相干接口的 vtable。
const _: () = assert!(std::mem::size_of::<Guid>() == 16);

impl Guid {
    pub const fn new(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Self {
        Guid { d1, d2, d3, d4 }
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.d1,
            self.d2,
            self.d3,
            self.d4[0],
            self.d4[1],
            self.d4[2],
            self.d4[3],
            self.d4[4],
            self.d4[5],
            self.d4[6],
            self.d4[7]
        )
    }
}

// ------------------------------------------------------------------- 错误 ----

#[derive(Debug, Clone)]
pub struct ComError {
    pub hr: HRESULT,
    pub context: String,
}

impl ComError {
    pub fn new(context: impl Into<String>, hr: HRESULT) -> Self {
        Self { hr, context: context.into() }
    }
    pub fn msg(context: impl Into<String>) -> Self {
        Self { hr: S_OK, context: context.into() }
    }
}

impl std::fmt::Display for ComError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.hr == S_OK {
            write!(f, "{}", self.context)
        } else {
            write!(f, "{} ({})", self.context, hex(self.hr))
        }
    }
}

impl std::error::Error for ComError {}

// ------------------------------------------------------- vtable 槽位调用 ----

/// 取 COM 对象的 vtable 指针。
///
/// # Safety
/// `obj` 必须是有效的 COM 接口指针。
pub unsafe fn vtable(obj: *mut c_void) -> *const *const c_void {
    *(obj as *const *const *const c_void)
}

/// 按槽位取出函数指针。
///
/// 槽位号是 0 基的：0=QueryInterface、1=AddRef、2=Release，之后按接口声明顺序递增。
/// **槽位错一格不会报错，而是调用到一个签名完全不相干的函数上**——这是这套做法最大的风险，
/// 所以每个槽位号都在调用点写了注释，并且有读回校验兜底。
///
/// # Safety
/// `F` 必须与 `obj` 在该槽位上的真实函数签名一致。
pub unsafe fn slot<F: Copy>(obj: *mut c_void, idx: usize) -> F {
    let p: *const c_void = *vtable(obj).add(idx);
    std::mem::transmute_copy::<*const c_void, F>(&p)
}

// --------------------------------------------------------- RAII 接口指针 ----

/// 持有 COM 接口指针，析构时 `Release`。
pub struct ComPtr {
    p: *mut c_void,
}

// 原始指针默认不是 Send，导致持有 ComPtr 的结构体无法放进需要 Send 的回调里。
// 这里显式放开，但调用仍然必须发生在 COM 已初始化的线程——由上层保证。
unsafe impl Send for ComPtr {}

impl ComPtr {
    /// # Safety
    /// `p` 必须是已 AddRef 的接口指针，或为空。
    pub unsafe fn from_raw(p: *mut c_void) -> Option<Self> {
        if p.is_null() {
            None
        } else {
            Some(ComPtr { p })
        }
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.p
    }

    /// 取某个槽位的函数指针。
    ///
    /// # Safety
    /// 调用者需保证槽位号与类型签名匹配。
    pub unsafe fn call<F: Copy>(&self, idx: usize) -> F {
        slot(self.p, idx)
    }

    /// # Safety
    /// 无额外前置条件（QueryInterface 本身安全）。
    pub unsafe fn query_interface(&self, iid: &Guid) -> Result<ComPtr, ComError> {
        type FnQI = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HRESULT;
        let qi: FnQI = self.call(0);
        let mut out: *mut c_void = ptr::null_mut();
        let hr = qi(self.p, iid, &mut out);
        if !succeeded(hr) {
            return Err(ComError::new(format!("QueryInterface({iid})"), hr));
        }
        ComPtr::from_raw(out)
            .ok_or_else(|| ComError::msg(format!("QueryInterface({iid}) 返回空指针")))
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.p.is_null() {
            unsafe {
                type FnRelease = unsafe extern "system" fn(*mut c_void) -> u32;
                let release: FnRelease = slot(self.p, 2);
                release(self.p);
            }
        }
    }
}

// ------------------------------------------------------------ COM 生命周期 --

/// COM 套间模型。
///
/// 对 Core Audio 的调用本身来说两种都能用，但**必须和同一线程上其它 COM 使用者一致**：
/// winit（slint 的 Windows 后端）在建窗时会调用 `OleInitialize`，而它在 COM 已被初始化成
/// MTA 的线程上会直接 panic（`RPC_E_CHANGED_MODE`）。所以任何会开窗的进程都得用 STA。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apartment {
    SingleThreaded,
    Multithreaded,
}

impl Apartment {
    fn coinit(self) -> u32 {
        match self {
            Apartment::SingleThreaded => COINIT_APARTMENTTHREADED,
            Apartment::Multithreaded => COINIT_MULTITHREADED,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Apartment::SingleThreaded => "STA（单线程套间）",
            Apartment::Multithreaded => "MTA（多线程套间）",
        }
    }
}

/// COM 套间初始化守卫。析构时按需 `CoUninitialize`。
pub struct ComGuard {
    /// 只有 `S_OK` / `S_FALSE` 两种情况下才需要配对的 `CoUninitialize`；
    /// `RPC_E_CHANGED_MODE` 表示别的套间模型已经被占用，此时**不能**反初始化。
    needs_uninit: bool,
    hr: HRESULT,
    apartment: Apartment,
}

impl ComGuard {
    /// 按指定套间模型初始化当前线程的 COM。
    pub fn new(apartment: Apartment) -> Self {
        let hr = unsafe { ffi::CoInitializeEx(ptr::null_mut(), apartment.coinit()) };
        Self {
            needs_uninit: hr == S_OK || hr == S_FALSE,
            hr,
            apartment,
        }
    }

    /// 多线程套间。命令行工具用这个（不涉及任何窗口）。
    pub fn mta() -> Self {
        Self::new(Apartment::Multithreaded)
    }

    pub fn apartment(&self) -> Apartment {
        self.apartment
    }

    pub fn hresult(&self) -> HRESULT {
        self.hr
    }

    /// 这个 HRESULT 是否代表"COM 可用"。`RPC_E_CHANGED_MODE` 也算可用。
    pub fn usable(&self) -> bool {
        self.hr == S_OK || self.hr == S_FALSE || self.hr == RPC_E_CHANGED_MODE
    }

    pub fn describe(&self) -> String {
        match self.hr {
            S_OK => "S_OK（本线程首次初始化）".into(),
            S_FALSE => "S_FALSE（本线程已初始化过）".into(),
            RPC_E_CHANGED_MODE => "RPC_E_CHANGED_MODE（已由其他套间模型初始化，可继续使用）".into(),
            other => format!("{}", hex(other)),
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.needs_uninit {
            unsafe { ffi::CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------- HSTRING ----

type FnWindowsCreateString =
    unsafe extern "system" fn(*const u16, u32, *mut *mut c_void) -> HRESULT;
type FnWindowsGetStringRawBuffer = unsafe extern "system" fn(*mut c_void, *mut u32) -> *const u16;
type FnWindowsDeleteString = unsafe extern "system" fn(*mut c_void) -> HRESULT;

struct CombaseFns {
    create: FnWindowsCreateString,
    raw: FnWindowsGetStringRawBuffer,
    delete: FnWindowsDeleteString,
}

fn combase() -> Option<&'static CombaseFns> {
    static FNS: OnceLock<Option<CombaseFns>> = OnceLock::new();
    FNS.get_or_init(|| unsafe {
        let dll = ffi::wide("combase.dll");
        let module = ffi::LoadLibraryW(dll.as_ptr());
        if module.is_null() {
            return None;
        }
        let resolve = |sym: &[u8]| -> Option<*mut c_void> {
            let p = ffi::GetProcAddress(module, sym.as_ptr());
            if p.is_null() {
                None
            } else {
                Some(p)
            }
        };
        Some(CombaseFns {
            create: std::mem::transmute::<*mut c_void, FnWindowsCreateString>(resolve(
                b"WindowsCreateString\0",
            )?),
            raw: std::mem::transmute::<*mut c_void, FnWindowsGetStringRawBuffer>(resolve(
                b"WindowsGetStringRawBuffer\0",
            )?),
            delete: std::mem::transmute::<*mut c_void, FnWindowsDeleteString>(resolve(
                b"WindowsDeleteString\0",
            )?),
        })
    })
    .as_ref()
}

/// WinRT `HSTRING`。`AudioPolicyConfig` 的设备 ID 参数就是用它传的。
pub struct HString {
    h: *mut c_void,
}

impl HString {
    pub fn new(s: &str) -> Result<Self, ComError> {
        let fns = combase().ok_or_else(|| ComError::msg("无法从 combase.dll 解析 HSTRING 函数"))?;
        let w = ffi::wide_raw(s);
        let mut h: *mut c_void = ptr::null_mut();
        let hr = unsafe { (fns.create)(w.as_ptr(), w.len() as u32, &mut h) };
        if !succeeded(hr) {
            return Err(ComError::new("WindowsCreateString", hr));
        }
        Ok(HString { h })
    }

    /// 接管一个由系统返回的 HSTRING，析构时负责释放。
    ///
    /// # Safety
    /// `h` 必须是本进程持有所有权的 HSTRING（例如作为出参由 API 写回来的那个）。
    pub unsafe fn from_raw(h: *mut c_void) -> Self {
        HString { h }
    }

    pub fn as_raw(&self) -> *mut c_void {
        self.h
    }

    pub fn to_string(&self) -> String {
        let Some(fns) = combase() else {
            return String::new();
        };
        if self.h.is_null() {
            return String::new();
        }
        let mut len = 0u32;
        let p = unsafe { (fns.raw)(self.h, &mut len) };
        if p.is_null() || len == 0 {
            return String::new();
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len as usize) })
    }
}

impl Drop for HString {
    fn drop(&mut self) {
        if let Some(fns) = combase() {
            if !self.h.is_null() {
                unsafe { (fns.delete)(self.h) };
            }
        }
    }
}
