//! Feasibility spike: per-app audio routing on Windows, in pure Rust, with ZERO
//! third-party crates. Everything here is raw `extern "system"` FFI + hand-rolled
//! COM vtables, to measure how much work the ".NET-free" path actually costs.
//!
//! Read-only: it never writes an audio endpoint. Safe to run.

#![allow(non_snake_case, non_camel_case_types, dead_code, non_upper_case_globals)]

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;

type HRESULT = i32;

// ---------------------------------------------------------------- basics ----

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

impl Guid {
    const fn new(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Self {
        Guid { d1, d2, d3, d4 }
    }
    fn s(&self) -> String {
        format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.d1, self.d2, self.d3,
            self.d4[0], self.d4[1], self.d4[2], self.d4[3],
            self.d4[4], self.d4[5], self.d4[6], self.d4[7]
        )
    }
}

#[repr(C)]
struct PropertyKey {
    fmtid: Guid,
    pid: u32,
}

#[repr(C, align(8))]
struct PropVariant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    data: [u8; 16],
}
const VT_LPWSTR: u16 = 31;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
fn wide_nul_free(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// Raw pointer -> function pointer, by vtable slot index. This is the whole
// "trick" the C# projects wrap in Marshal.GetDelegateForFunctionPointer.
unsafe fn vtbl(obj: *mut c_void) -> *const *const c_void {
    *(obj as *const *const *const c_void)
}
unsafe fn slot<F: Copy>(obj: *mut c_void, idx: usize) -> F {
    let p: *const c_void = *vtbl(obj).add(idx);
    std::mem::transmute_copy::<*const c_void, F>(&p)
}

// ------------------------------------------------------------------ FFI -----

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
    fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    fn GetCurrentProcessId() -> u32;
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(pvReserved: *mut c_void, dwCoInit: u32) -> HRESULT;
    fn CoUninitialize();
    fn CoCreateInstance(
        rclsid: *const Guid,
        pUnkOuter: *mut c_void,
        dwClsContext: u32,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> HRESULT;
    fn CoTaskMemFree(pv: *mut c_void);
    fn PropVariantClear(pvar: *mut c_void) -> HRESULT;
}

// HSTRING is a combase.dll export. Resolved at runtime rather than link time so
// the binary does not depend on which Windows SDK import libs are present.
struct CombaseFns {
    create: unsafe extern "system" fn(*const u16, u32, *mut *mut c_void) -> HRESULT,
    raw: unsafe extern "system" fn(*mut c_void, *mut u32) -> *const u16,
    del: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}
static COMBASE: OnceLock<CombaseFns> = OnceLock::new();

fn combase() -> &'static CombaseFns {
    COMBASE.get_or_init(|| unsafe {
        let dll = wide("combase.dll");
        let m = LoadLibraryW(dll.as_ptr());
        let g = |n: &[u8]| {
            let p = GetProcAddress(m, n.as_ptr());
            assert!(!p.is_null(), "missing export");
            p
        };
        CombaseFns {
            create: std::mem::transmute(g(b"WindowsCreateString\0")),
            raw: std::mem::transmute(g(b"WindowsGetStringRawBuffer\0")),
            del: std::mem::transmute(g(b"WindowsDeleteString\0")),
        }
    })
}

// --------------------------------------------------------------- HSTRING ----

struct HString(*mut c_void);

impl HString {
    fn new(s: &str) -> Result<HString, HRESULT> {
        let w = wide_nul_free(s);
        let mut h: *mut c_void = ptr::null_mut();
        let hr = unsafe { (combase().create)(w.as_ptr(), w.len() as u32, &mut h) };
        if hr < 0 {
            return Err(hr);
        }
        Ok(HString(h))
    }
    fn to_string(&self) -> String {
        if self.0.is_null() {
            return String::new();
        }
        let mut len = 0u32;
        let p = unsafe { (combase().raw)(self.0, &mut len) };
        if p.is_null() {
            return String::new();
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len as usize) })
    }
}
impl Drop for HString {
    fn drop(&mut self) {
        unsafe {
            (combase().del)(self.0);
        }
    }
}

fn hx(hr: HRESULT) -> String {
    format!("0x{:08X}", hr as u32)
}

// ------------------------------------------------------------------ main ----

const CLSID_MMDeviceEnumerator: Guid =
    Guid::new(0xBCDE0395, 0xE52F, 0x467C, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
const IID_IMMDeviceEnumerator: Guid =
    Guid::new(0xA95664D2, 0x9614, 0x4F35, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);
const IID_IMMDeviceCollection: Guid =
    Guid::new(0x0BD7A1BE, 0x7A1A, 0x44DB, [0x83, 0x97, 0xCC, 0x53, 0x92, 0x38, 0x7B, 0x5E]);
const IID_IMMDevice: Guid =
    Guid::new(0xD666063F, 0x1587, 0x4E43, [0x81, 0xF1, 0xB9, 0x48, 0xE8, 0x07, 0x36, 0x3F]);
const IID_IPropertyStore: Guid =
    Guid::new(0x886D8EEB, 0x8CF2, 0x4446, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);
const PKEY_Device_FriendlyName: PropertyKey = PropertyKey {
    fmtid: Guid::new(0xA45C254E, 0xDF1C, 0x4EFD, [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0]),
    pid: 14,
};
const IID_IAudioSessionManager2: Guid =
    Guid::new(0x77AA99A0, 0x1BD6, 0x484F, [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F]);
const IID_IAudioSessionControl2: Guid =
    Guid::new(0xBFB7FF88, 0x7239, 0x4FC9, [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D]);

/// IIDs the AudioPolicyConfig factory is known to expose, across OS builds.
const KNOWN_VALID_IIDS: [&str; 3] = [
    "2a59116d-6c4f-45e0-a74f-707e3fef9258", // pre-21H2 / downlevel
    "ab3d4648-e242-459f-b02f-541c70306324", // 21H2 / Win11
    "32aa8e18-6496-4e24-9f94-b800e7eccc45", // 10.0.16299
];

const DEVINTERFACE_AUDIO_RENDER: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const MMDEVAPI_TOKEN: &str = r"\\?\SWD#MMDEVAPI#";

fn main() {
    unsafe { run() }
}

unsafe fn run() {
    let pid = GetCurrentProcessId();
    println!("===============================================================");
    println!(" AudioFollow -- Rust COM spike (zero third-party crates)");
    println!("===============================================================");
    println!("self pid = {pid}");

    let hr = CoInitializeEx(ptr::null_mut(), 0x0); // COINIT_MULTITHREADED
    println!("CoInitializeEx            -> {}  (0x80010106 = already inited, fine)", hx(hr));

    part_a_enumerate_devices();
    part_a2_enumerate_sessions();
    part_b_internal_policy_config(pid);

    CoUninitialize();
    println!("---------------------------------------------------------------");
    println!("done. nothing was written to the system (read-only spike).");
}

// -------- Part A: fully documented Core Audio, via CoCreateInstance --------

unsafe fn part_a_enumerate_devices() {
    println!();
    println!("[A] documented COM: IMMDeviceEnumerator (no undocumented API)");

    type FnEnum = unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> HRESULT;
    type FnCount = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;
    type FnItem = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
    type FnGetId = unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT;
    type FnOpenStore = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
    type FnGetValue =
        unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT;
    type FnRelease = unsafe extern "system" fn(*mut c_void) -> u32;

    let mut enumerator: *mut c_void = ptr::null_mut();
    let hr = CoCreateInstance(
        &CLSID_MMDeviceEnumerator,
        ptr::null_mut(),
        0x17, // CLSCTX_ALL
        &IID_IMMDeviceEnumerator,
        &mut enumerator,
    );
    if hr < 0 {
        println!("  CoCreateInstance(MMDeviceEnumerator) FAILED {}", hx(hr));
        return;
    }
    println!("  CoCreateInstance          -> S_OK");

    let enum_endpoints: FnEnum = slot(enumerator, 3);
    let mut coll: *mut c_void = ptr::null_mut();
    let hr = enum_endpoints(enumerator, 0 /* eRender */, 0x1 /* ACTIVE */, &mut coll);
    if hr < 0 {
        println!("  EnumAudioEndpoints FAILED {}", hx(hr));
        return;
    }

    let get_count: FnCount = slot(coll, 3);
    let item: FnItem = slot(coll, 4);
    let mut n = 0u32;
    get_count(coll, &mut n);
    println!("  active render endpoints   = {n}");

    for i in 0..n {
        let mut dev: *mut c_void = ptr::null_mut();
        if item(coll, i, &mut dev) < 0 {
            continue;
        }
        let get_id: FnGetId = slot(dev, 5);
        let mut id_ptr: *mut u16 = ptr::null_mut();
        let id = if get_id(dev, &mut id_ptr) >= 0 && !id_ptr.is_null() {
            let s = pwsz(id_ptr);
            CoTaskMemFree(id_ptr as *mut c_void);
            s
        } else {
            String::from("<no id>")
        };

        // friendly name lives in the property store
        let open_store: FnOpenStore = slot(dev, 4);
        let mut store: *mut c_void = ptr::null_mut();
        let mut name = String::from("<no name>");
        if open_store(dev, 0 /* STGM_READ */, &mut store) >= 0 {
            let get_value: FnGetValue = slot(store, 5);
            let mut pv = PropVariant { vt: 0, r1: 0, r2: 0, r3: 0, data: [0; 16] };
            if get_value(store, &PKEY_Device_FriendlyName, &mut pv) >= 0 && pv.vt == VT_LPWSTR {
                let p = *(pv.data.as_ptr() as *const *const u16);
                if !p.is_null() {
                    name = pwsz(p);
                }
            }
            PropVariantClear(&mut pv as *mut _ as *mut c_void);
            let rel: FnRelease = slot(store, 2);
            rel(store);
        }
        let rel: FnRelease = slot(dev, 2);
        rel(dev);

        println!("    [{i}] {name}");
        println!("        raw id : {id}");
        println!("        swd id : {MMDEVAPI_TOKEN}{id}{DEVINTERFACE_AUDIO_RENDER}");
    }

    let rel: FnRelease = slot(coll, 2);
    rel(coll);
    let rel: FnRelease = slot(enumerator, 2);
    rel(enumerator);
}

unsafe fn pwsz(mut p: *const u16) -> String {
    let mut v: Vec<u16> = Vec::new();
    while *p != 0 && v.len() < 512 {
        v.push(*p);
        p = p.add(1);
    }
    String::from_utf16_lossy(&v)
}

// -------- Part A2: who actually has a live audio session right now --------
// Needed because Set/GetPersistedDefaultAudioEndpoint silently no-op for a PID
// that owns no audio session (PROCESS_NO_AUDIO). Same trap SoundSwitch hit.

type FnHR1 = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type FnRelease = unsafe extern "system" fn(*mut c_void) -> u32;

unsafe fn pids_with_sessions() -> Vec<u32> {
    type FnGetDefault = unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> HRESULT;
    type FnActivate =
        unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> HRESULT;
    type FnCount = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
    type FnSession = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT;
    type FnQI = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HRESULT;
    type FnGetPid = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;

    let mut out = Vec::new();
    let mut en: *mut c_void = ptr::null_mut();
    if CoCreateInstance(
        &CLSID_MMDeviceEnumerator, ptr::null_mut(), 0x17, &IID_IMMDeviceEnumerator, &mut en,
    ) < 0
    {
        return out;
    }
    let get_default: FnGetDefault = slot(en, 4);
    let mut dev: *mut c_void = ptr::null_mut();
    if get_default(en, 0, 0, &mut dev) < 0 {
        return out;
    }
    let activate: FnActivate = slot(dev, 3);
    let mut mgr: *mut c_void = ptr::null_mut();
    if activate(dev, &IID_IAudioSessionManager2, 0x17, ptr::null_mut(), &mut mgr) < 0 {
        return out;
    }
    let get_sessions: FnHR1 = slot(mgr, 5);
    let mut it: *mut c_void = ptr::null_mut();
    if get_sessions(mgr, &mut it) < 0 {
        return out;
    }
    let get_count: FnCount = slot(it, 3);
    let get_session: FnSession = slot(it, 4);
    let mut n = 0i32;
    get_count(it, &mut n);
    for i in 0..n {
        let mut ctl: *mut c_void = ptr::null_mut();
        if get_session(it, i, &mut ctl) < 0 {
            continue;
        }
        let qi: FnQI = slot(ctl, 0);
        let mut c2: *mut c_void = ptr::null_mut();
        if qi(ctl, &IID_IAudioSessionControl2, &mut c2) >= 0 {
            let get_pid: FnGetPid = slot(c2, 14); // IAudioSessionControl2::GetProcessId
            let mut p = 0u32;
            if get_pid(c2, &mut p) >= 0 && p != 0 && !out.contains(&p) {
                out.push(p);
            }
            let rel: FnRelease = slot(c2, 2);
            rel(c2);
        }
        let rel: FnRelease = slot(ctl, 2);
        rel(ctl);
    }
    let rel: FnRelease = slot(it, 2);
    rel(it);
    let rel: FnRelease = slot(mgr, 2);
    rel(mgr);
    let rel: FnRelease = slot(dev, 2);
    rel(dev);
    let rel: FnRelease = slot(en, 2);
    rel(en);
    out
}

unsafe fn part_a2_enumerate_sessions() {
    println!();
    println!("[A2] live audio sessions on the default render endpoint");
    let pids = pids_with_sessions();
    if pids.is_empty() {
        println!("  (no active session -- start some audio and re-run)");
        return;
    }
    for p in &pids {
        println!("  session pid {p}");
    }
}

// ---------- Part B: the undocumented internal AudioPolicyConfig ------------

unsafe fn part_b_internal_policy_config(pid: u32) {
    println!();
    println!("[B] undocumented COM: Windows.Media.Internal.AudioPolicyConfig");

    type FnDllGetActivationFactory =
        unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
    type FnGetIids = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut Guid) -> HRESULT;
    type FnQI = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HRESULT;
    type FnGetPersisted =
        unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut *mut c_void) -> HRESULT;

    let dll_name = wide("AudioSes.dll");
    let mod_audioses = LoadLibraryW(dll_name.as_ptr());
    if mod_audioses.is_null() {
        println!("  LoadLibraryW(AudioSes.dll) FAILED");
        return;
    }
    println!("  AudioSes.dll loaded, DllGetActivationFactory resolved");

    let proc = GetProcAddress(mod_audioses, b"DllGetActivationFactory\0".as_ptr());
    if proc.is_null() {
        println!("  DllGetActivationFactory NOT FOUND");
        return;
    }
    let dll_get_activation_factory: FnDllGetActivationFactory = std::mem::transmute(proc);

    let class_name = match HString::new("Windows.Media.Internal.AudioPolicyConfig") {
        Ok(h) => h,
        Err(hr) => {
            println!("  WindowsCreateString FAILED {}", hx(hr));
            return;
        }
    };

    let mut factory: *mut c_void = ptr::null_mut();
    let hr = dll_get_activation_factory(class_name.0, &mut factory);
    if hr < 0 {
        println!("  DllGetActivationFactory FAILED {}", hx(hr));
        return;
    }
    println!("  activation factory        -> S_OK (ptr={factory:p})");

    // --- IID discovery instead of hardcoding a version-specific IID ---
    let get_iids: FnGetIids = slot(factory, 3); // IInspectable::GetIids
    let mut count: u32 = 0;
    let mut iids: *mut Guid = ptr::null_mut();
    let hr = get_iids(factory, &mut count, &mut iids);
    if hr < 0 || count == 0 {
        println!("  GetIids FAILED {} (count={count})", hx(hr));
        return;
    }
    println!("  GetIids                   -> {count} interface(s)");
    for i in 0..count as usize {
        let g = *iids.add(i);
        println!("      [{i}] {}", g.s());
    }

    let picked = *iids.add(count as usize - 1);
    let picked_s = picked.s();
    let known = KNOWN_VALID_IIDS.contains(&picked_s.as_str());
    println!("  picked (last) IID         = {picked_s}");
    println!("  in known-valid whitelist  = {known}");
    CoTaskMemFree(iids as *mut c_void);

    // --- QI for the picked IID, then index the vtable by slot number ---
    let qi: FnQI = slot(factory, 0);
    let mut iface: *mut c_void = ptr::null_mut();
    let hr = qi(factory, &picked, &mut iface);
    if hr < 0 {
        println!("  QueryInterface FAILED {}", hx(hr));
        return;
    }
    println!("  QueryInterface            -> S_OK (ptr={iface:p})");

    // slots 25 / 26 / 27 == Set / Get / ClearAll on this vtable
    let get_persisted: FnGetPersisted = slot(iface, 26);

    let mut targets: Vec<u32> = vec![pid];
    for p in pids_with_sessions() {
        if !targets.contains(&p) {
            targets.push(p);
        }
    }

    for t in &targets {
        let tag = if *t == pid { "self" } else { "session" };
        let mut lines = Vec::new();
        for (role, label) in [
            (0i32, "eConsole"),
            (1i32, "eMultimedia"),
            (2i32, "eCommunications"),
        ] {
            let mut out: *mut c_void = ptr::null_mut();
            let hr = get_persisted(iface, *t, 0 /* eRender */, role, &mut out);
            if hr == 0x8007_0057u32 as i32 {
                lines.push(format!("      {label:<16} -> PROCESS_NO_AUDIO"));
                continue;
            }
            if hr < 0 {
                lines.push(format!("      {label:<16} -> FAILED {}", hx(hr)));
                continue;
            }
            let h = HString(out);
            let s = h.to_string();
            let shown = if s.is_empty() {
                "<empty = follows the system default>".to_string()
            } else {
                s
            };
            lines.push(format!("      {label:<16} -> {shown}"));
        }
        println!("  target pid {t} ({tag}):");
        for l in lines {
            println!("{l}");
        }
    }
}
