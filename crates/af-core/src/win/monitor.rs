//! 显示器枚举与「窗口归哪块屏」的判定。

use super::dpi;
use super::ffi::{self, Rect, HMONITOR, MONITORINFOEXW, MONITORINFOF_PRIMARY};

/// 一块物理显示器。
#[derive(Debug, Clone)]
pub struct Monitor {
    pub handle: HMONITOR,
    /// 设备名，形如 `\\.\DISPLAY1`。
    ///
    /// 注意它**只在本次连接内稳定**：显示器拔掉重插、或插拔顺序变化后，系统可能给它
    /// 重新编号（`DISPLAY2` 变 `DISPLAY3`）。跨拔插认出"同一块屏"要靠 [`Monitor::key`]。
    pub device: String,
    /// 跨拔插稳定的身份标识，形如 `edid:1a2b3c4d5e6f7081`（EDID 身份段哈希），
    /// **读不到 EDID 时是 `None`**。**记忆功能（拔插后恢复绑定）靠它认人**，见 [`crate::config`]。
    ///
    /// 刻意不做"读不到 EDID 就退回硬件型号段"之类的**弱键**：同一块屏会在
    /// "这次读到了 / 这次没读到"之间摇摆（注册表写入有竞态），弱键和真键是两个
    /// 不同的字符串，一旦绑定里存了弱键，屏正常报真键的那天起这条绑定就永远
    /// 匹配不上——比没有键更糟。`None` 会让匹配退回设备名 + 型号名，
    /// 等 EDID 可读时由补键逻辑（`heal_against`）自愈，不会出现"存了个认不出的键"的死态。
    pub key: Option<String>,
    /// 型号名，如 `MON-A`。**取不到时为空白**。
    ///
    /// 这里刻意不保留系统给的 `"Generic PnP Monitor"` 这种占位串：两块屏都叫这个名字，
    /// 等于没有信息。取不到就留空，由 [`Monitor::display_name`] 退回「显示器 N」。
    pub name: String,
    /// 显示器全景矩形（物理像素）。
    pub rect: Rect,
    /// 工作区矩形（扣掉任务栏）。
    pub work: Rect,
    pub is_primary: bool,
    pub dpi: u32,
}

/// 从 `\\.\DISPLAY2` 里解出系统给的显示器编号 2。
///
/// 注意这和枚举下标不是一回事：枚举顺序会按主屏优先、再按坐标排序，
/// 而 `DISPLAYn` 是系统分配的。两者都可能因为拔插线而变化，所以配置里存的始终是
/// 完整的设备名字符串，这个编号只用于**展示**。
pub fn ordinal_from_device(device: &str) -> Option<u32> {
    let tail = device.rsplit("DISPLAY").next()?;
    if tail.len() == device.len() {
        return None; // 名字里压根没有 DISPLAY 字样
    }
    tail.trim().parse::<u32>().ok()
}

/// 仅凭设备名给出一个可读名字。显示器当前不在线（配置里有、但扫不到）时用。
pub fn name_from_device(device: &str) -> String {
    match ordinal_from_device(device) {
        Some(n) => format!("显示器 {n}"),
        None => device.trim_start_matches(r"\\.\").to_string(),
    }
}

impl Monitor {
    pub fn scale_percent(&self) -> u32 {
        dpi::scale_percent(self.dpi)
    }

    /// 用户可见的名字。取不到型号就退回「显示器 N」。
    ///
    /// `ordinal` 从 **1** 开始（人看的编号）。
    pub fn display_name(&self, ordinal: usize) -> String {
        if self.name.is_empty() {
            format!("显示器 {ordinal}")
        } else {
            self.name.clone()
        }
    }

    /// 下拉框 / 列表里的一行。
    ///
    /// 形如 `显示器 1 · MON-A · 2560×1440 @125% · 主屏`；
    /// 型号取不到时是 `显示器 2 · 1080×1920 @125%`——板块编号始终在最前面，
    /// 所以哪怕两块屏都报 `Generic PnP Monitor`，用户也能区分。
    pub fn label(&self, index: usize) -> String {
        let n = index + 1;
        let head = if self.name.is_empty() {
            format!("显示器 {n}")
        } else {
            format!("显示器 {n} · {}", self.name)
        };
        format!(
            "{head} · {}×{}{}{}",
            self.rect.width(),
            self.rect.height(),
            if self.dpi != 96 {
                format!(" @{}%", self.scale_percent())
            } else {
                String::new()
            },
            if self.is_primary { " · 主屏" } else { "" },
        )
    }
}

/// 枚举所有显示器。
pub fn enumerate() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        ffi::EnumDisplayMonitors(
            0, // HDC = isize，0 表示枚举全部显示器
            std::ptr::null(),
            Some(enum_proc),
            &mut out as *mut Vec<Monitor> as isize,
        );
    }
    // 主屏排前面，其余按 x 坐标（左→右），让配置界面里的顺序符合直觉
    out.sort_by_key(|m| (!m.is_primary, m.rect.left, m.rect.top));
    out
}

unsafe extern "system" fn enum_proc(
    hmonitor: HMONITOR,
    _hdc: isize,
    _clip: *mut Rect,
    lparam: isize,
) -> i32 {
    let out = &mut *(lparam as *mut Vec<Monitor>);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if ffi::GetMonitorInfoW(hmonitor, &mut info) != 0 {
        let (name, key) = describe(&info.szDevice);
        out.push(Monitor {
            handle: hmonitor,
            device: ffi::utf16_arr_to_string(&info.szDevice),
            key,
            name,
            rect: info.rcMonitor,
            work: info.rcWork,
            is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
            dpi: dpi::monitor_dpi(hmonitor),
        });
    }
    1 // 继续枚举
}

/// 判断一个系统给的名字是不是"等于没给"的占位串。
///
/// Windows 在拿不到 EDID 里的型号段时会填这些值。它们不是错误，但也不含任何
/// 能区分两块屏的信息，所以一律当成"没有名字"。
fn is_placeholder(name: &str) -> bool {
    let s = name.trim();
    if s.is_empty() {
        return true;
    }
    let low = s.to_ascii_lowercase();
    const NEEDLES: [&str; 6] = [
        "generic",      // Generic PnP Monitor / Generic Monitor
        "default",      // Default Monitor
        "non-pnp",      // Non-PnP Monitor
        "通用",          // 通用即插即用监视器
        "标准监视",       // 标准监视器
        "即插即用监视器",  // 中文版的 Generic PnP Monitor
    ];
    // 中文串走小写化不影响匹配，直接对原文和 ASCII 小写各查一遍
    NEEDLES
        .iter()
        .any(|n| low.contains(n) || s.contains(n))
}

/// 取显示器的型号名与稳定标识。
///
/// 型号名三步走，从最权威到最将就：
///
/// 1. **EDID 里的 `0xFC` 描述符**。这是显示器**自己报上来的**名字（EDID 是显示器固件里的
///    一段数据，型号名就在其中），不受驱动影响。本机实测：`MON-A`、`MON-B`。
///    拿它的路径是设备接口路径 → 注册表实例键 → `Device Parameters\EDID`。
/// 2. `EnumDisplayDevicesW` 的 `DeviceString`。有些机器的驱动会在这里填真型号，
///    但更多时候给的是 `Generic PnP Monitor` 这种占位串。
/// 3. 设备接口路径中间的**硬件型号段**，形如 `ACM2428`（= EDID 厂商码 `ACM` + 产品码 `2428`）。
///    能区分两块屏，但可读性不如 1。
///
/// 三条都拿不到就返回空串，由调用方退回「显示器 N」。
///
/// 稳定标识（[`Monitor::key`]）只来自 EDID 身份段哈希（见 [`edid_identity`]）；
/// EDID 读不到就是 `None`，**不做弱键兜底**——理由见 [`Monitor::key`] 的文档：
/// 弱键会造成"绑定里存了一个屏正常时报不出的键"的死态，`None` 反而能自愈。
///
/// > 为什么不用 `SetupDiGetDeviceRegistryProperty(SPDRP_FRIENDLY_NAME)`：它给的是
/// > `Generic Monitor (MON-A)`，要从括号里抠出来，且需要多一整套 SetupAPI 声明；
/// > 而 EDID 那条路只需要注册表四个 API，还能顺带拿到厂商码与物理尺寸。
fn describe(device: &[u16; ffi::CCHDEVICENAME]) -> (String, Option<String>) {
    unsafe {
        let mut dd: ffi::DISPLAY_DEVICEW = std::mem::zeroed();
        dd.cb = std::mem::size_of::<ffi::DISPLAY_DEVICEW>() as u32;
        if ffi::EnumDisplayDevicesW(device.as_ptr(), 0, &mut dd, 0) == 0 {
            return (String::new(), None);
        }
        let device_string = ffi::utf16_arr_to_string(&dd.DeviceString);

        // 再问一次要设备接口路径。它带的信息比 DeviceString 有用得多：
        // 中间两段正好是注册表实例键的位置，顺着就能读到 EDID。
        let mut di: ffi::DISPLAY_DEVICEW = std::mem::zeroed();
        di.cb = std::mem::size_of::<ffi::DISPLAY_DEVICEW>() as u32;
        let iface_path = if ffi::EnumDisplayDevicesW(
            device.as_ptr(),
            0,
            &mut di,
            ffi::EDD_GET_DEVICE_INTERFACE_NAME,
        ) != 0
        {
            ffi::utf16_arr_to_string(&di.DeviceID)
        } else {
            String::new()
        };

        let edid = edid_from_interface_path(&iface_path);

        // ---- 稳定标识：只认 EDID，读不到就是 None（不做弱键兜底，见 Monitor::key 文档） ----
        let key = edid.as_deref().and_then(edid_identity);

        // ---- 1) EDID 自报的型号名 ----
        if let Some(edid) = edid.as_deref() {
            if let Some(name) = monitor_name_from_edid(edid) {
                return (name, key);
            }
        }

        // ---- 2) 驱动填的 DeviceString ----
        if !is_placeholder(&device_string) {
            return (device_string, key);
        }

        // ---- 3) 接口路径里的硬件型号段 ----
        if let Some(model) = iface_path.split('#').nth(1) {
            if !is_placeholder(model) {
                return (model.to_string(), key);
            }
        }

        // 实在没有型号。留空，让上层显示「显示器 N」。
        (String::new(), key)
    }
}

// ---------------------------------------------------------------- EDID 读取 --

/// 从设备接口路径解出注册表实例键的三段：(枚举器, 硬件 ID, 实例 ID)。
///
/// 输入形如
/// `\\?\DISPLAY#ACM2428#7&1a2b3c4d&0&UID0001#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}`，
/// 对应注册表 `HKLM\SYSTEM\CurrentControlSet\Enum\DISPLAY\ACM2428\7&1a2b3c4d&0&UID0001`。
///
/// 传进来的不是接口路径（例如 `EnumDisplayDevicesW` 没带 flag 时给的
/// `MONITOR\ACM2428\{4d36e96e-...}\0004`）时返回 `None`——那条路径不是实例键，
/// 顺着它读不到 EDID。
fn registry_key_of_interface_path(path: &str) -> Option<(&str, &str, &str)> {
    // `\\?\DISPLAY#ACM2428#7&1a2b3c4d&0&UID0001#{...}`
    //   → ["\\?\DISPLAY", "ACM2428", "7&1a2b3c4d&0&UID0001", "{...}"]
    let parts: Vec<&str> = path.split('#').collect();
    if parts.len() < 4 {
        return None;
    }
    // `\\?\DISPLAY` → "DISPLAY"
    let enumerator = parts[0].rsplit('\\').next().unwrap_or("");
    if enumerator.is_empty() {
        return None;
    }
    if parts[1].is_empty() || parts[2].is_empty() {
        return None;
    }
    Some((enumerator, parts[1], parts[2]))
}

/// 读该显示器在注册表里的 EDID。
fn edid_from_interface_path(path: &str) -> Option<Vec<u8>> {
    let (enumerator, hwid, instance) = registry_key_of_interface_path(path)?;
    let subkey = format!(
        r"SYSTEM\CurrentControlSet\Enum\{enumerator}\{hwid}\{instance}\Device Parameters"
    );
    reg_read_binary(&subkey, "EDID")
}

/// 读一个 `REG_BINARY` 值。任何一步失败都返回 `None`，绝不 panic。
fn reg_read_binary(subkey: &str, value: &str) -> Option<Vec<u8>> {
    unsafe {
        let sub = ffi::wide(subkey);
        let mut hkey: ffi::HKEY = 0;
        if ffi::RegOpenKeyExW(
            ffi::HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            0,
            ffi::KEY_READ,
            &mut hkey,
        ) != ffi::ERROR_SUCCESS
        {
            return None;
        }

        let name = ffi::wide(value);
        let mut ty: u32 = 0;
        let mut len: u32 = 0;
        // 先问长度（lpData 传 null）
        let rc = ffi::RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        );
        if rc != ffi::ERROR_SUCCESS || ty != ffi::REG_BINARY || len == 0 || len > ffi::EDID_MAX_LEN
        {
            ffi::RegCloseKey(hkey);
            return None;
        }

        let mut buf = vec![0u8; len as usize];
        let rc = ffi::RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr(),
            &mut len,
        );
        ffi::RegCloseKey(hkey);
        if rc != ffi::ERROR_SUCCESS {
            return None;
        }
        buf.truncate((len as usize).min(buf.len()));
        Some(buf)
    }
}

/// EDID 基础块的 4 个 18 字节描述符槽位。
const EDID_DESCRIPTOR_OFFSETS: [usize; 4] = [0x36, 0x48, 0x5A, 0x6C];
/// 描述符 tag：显示器名。
const EDID_TAG_MONITOR_NAME: u8 = 0xFC;

/// 从 EDID 里抠出显示器名。
///
/// EDID 基础块 128 字节，第 54 字节起有 4 个 18 字节的"描述符"槽。如果某个槽的头三字节
/// 是 0、第 4 字节是 tag，它就装的是一条文本：tag `0xFC` 是显示器名（13 字节 ASCII，
/// 以 `0x0A` 或 `0x00` 收尾）。其余 tag（`0xFF` 序列号、`0xFE` 文本、`0xFB` 色域）
/// 不是我们要的。
///
/// 槽位里也可能是"详细时序数据"而不是文本描述符——判据就是头三字节非零。这时跳过。
fn monitor_name_from_edid(edid: &[u8]) -> Option<String> {
    for &off in &EDID_DESCRIPTOR_OFFSETS {
        if edid.len() < off + 18 {
            break;
        }
        if edid[off] != 0 || edid[off + 1] != 0 || edid[off + 2] != 0 {
            continue; // 详细时序数据，不是文本描述符
        }
        if edid[off + 3] != EDID_TAG_MONITOR_NAME {
            continue;
        }
        // 第 4 字节保留，第 5..18 是 13 字节文本
        let raw = &edid[off + 5..off + 18];
        // 只取可打印 ASCII：厂商偶尔会填进非 ASCII 垃圾，直接丢字符比乱码强
        let s: String = raw
            .iter()
            .take_while(|&&b| b != 0x0A && b != 0x00)
            .filter(|&&b| (0x20..0x7F).contains(&b))
            .map(|&b| b as char)
            .collect();
        let s = s.trim().to_string();
        if !s.is_empty() && !is_placeholder(&s) {
            return Some(s);
        }
    }
    None
}

/// EDID 第 8-9 字节里的厂商码（3 个字母，各占 5 位、偏移 64）。
///
/// 本机的 `ACM`（MON-A）、`ACN`（MON-B）、以及注册表里留着的 `ACO`（Z2719Q）。
/// 型号名取不到时，用它至少能告诉用户"这是哪家的屏"。
pub fn edid_manufacturer(edid: &[u8]) -> Option<String> {
    if edid.len() < 10 {
        return None;
    }
    let v = ((edid[8] as u16) << 8) | edid[9] as u16;
    let mfg: String = [(v >> 10) & 0x1F, (v >> 5) & 0x1F, v & 0x1F]
        .iter()
        .map(|&c| if c == 0 { '?' } else { (c as u8 + 64) as char })
        .collect();
    if mfg.contains('?') {
        return None;
    }
    Some(mfg)
}

// ------------------------------------------------------------ 稳定标识 --

/// EDID 身份段的字节范围：第 8-17 字节，共 10 字节。
///
/// 内容依次是：厂商码(2) + 产品码(2) + 序列号(4) + 生产周(1) + 生产年(1)。
/// 这一段是显示器**出厂就烧死**的身份——同型号的两台机器序列号也不同（除非固件填 0），
/// 而且不受显示模式、描述符变化影响。
///
/// 刻意**不**哈希整个 EDID 基础块：部分显示器在不同信号格式下会报出不同的
/// 时序/描述符（4K@60 与 144Hz 模式的 EDID 有差异），整块哈希会把"同一块屏"
/// 误判成两块。身份段不随模式变。
const EDID_IDENTITY_RANGE: std::ops::Range<usize> = 8..18;

/// 由 EDID 算出跨拔插稳定的身份标识。
///
/// 返回 `edid:<16 位十六进制>`（身份段的 FNV-1a 64 哈希）。EDID 不足 128 字节
/// （不完整、不该出现在正常机器上）时返回 `None`，由调用方退回名字/设备名匹配。
///
/// 为什么不直接存明文"厂商码+产品码+序列号"：这段里序列号是裸数字，日志和
/// 配置文件里出现一串无意义数字不如短哈希可读；哈希还顺带把比较统一成字符串相等。
fn edid_identity(edid: &[u8]) -> Option<String> {
    if edid.len() < 128 {
        return None;
    }
    Some(format!("edid:{:016x}", fnv1a64(&edid[EDID_IDENTITY_RANGE])))
}

/// FNV-1a 64 位哈希。只为生成稳定短标识，无密码学要求，不引依赖。
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 读某块显示器当前的 EDID（诊断用，`--selftest` 会打印厂商码）。
pub fn edid_of(device: &str) -> Option<Vec<u8>> {
    let dev = ffi::wide(device);
    unsafe {
        let mut di: ffi::DISPLAY_DEVICEW = std::mem::zeroed();
        di.cb = std::mem::size_of::<ffi::DISPLAY_DEVICEW>() as u32;
        if ffi::EnumDisplayDevicesW(
            dev.as_ptr(),
            0,
            &mut di,
            ffi::EDD_GET_DEVICE_INTERFACE_NAME,
        ) == 0
        {
            return None;
        }
        edid_from_interface_path(&ffi::utf16_arr_to_string(&di.DeviceID))
    }
}

/// 判定一个矩形属于哪块显示器：按**交集面积**取最大者。
///
/// 为什么不用窗口中心点或 `MonitorFromWindow`：
/// - 中心点法在窗口被拖到屏幕交界处时会跳变；
/// - `MonitorFromWindow` 只看窗口最大可见区域的归属，对"一半在 A 一半在 B"没有可控的偏好。
///   面积法能给出可解释、可调的结果，多窗口仲裁也用同一把尺子。
pub fn best_for_rect(monitors: &[Monitor], rect: &Rect) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }

    let mut best: Option<(usize, i64)> = None;
    for (i, m) in monitors.iter().enumerate() {
        let area = m.rect.intersection_area(rect);
        if area > 0 && best.map_or(true, |(_, a)| area > a) {
            best = Some((i, area));
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }

    // 完全没交集（窗口被拖到所有屏幕之外，或最小化到 -32000）：退回到"包含中心点"，
    // 再退回到"距离最近的屏"。
    let (cx, cy) = rect.center();
    if let Some(i) = monitors.iter().position(|m| m.rect.contains_point(cx, cy)) {
        return Some(i);
    }
    monitors
        .iter()
        .enumerate()
        .min_by_key(|(_, m)| {
            let (mx, my) = m.rect.center();
            let dx = (mx - cx) as i64;
            let dy = (my - cy) as i64;
            dx * dx + dy * dy
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(left: i32, top: i32, right: i32, bottom: i32) -> Monitor {
        Monitor {
            handle: 0,
            device: format!(r"\\.\DISPLAY@{left}"),
            key: None,
            name: String::new(),
            rect: Rect::new(left, top, right, bottom),
            work: Rect::new(left, top, right, bottom),
            is_primary: left == 0,
            dpi: 96,
        }
    }

    #[test]
    fn rect_intersection() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 200, 200);
        assert_eq!(a.intersection_area(&b), 50 * 50);
        assert_eq!(a.intersection_area(&Rect::new(200, 200, 300, 300)), 0);
    }

    #[test]
    fn picks_larger_overlap() {
        // 左屏 0..1920，右屏 1920..3840
        let mons = [mon(0, 0, 1920, 1080), mon(1920, 0, 3840, 1080)];
        // 窗口横跨交界，2/3 在左屏
        let w = Rect::new(1200, 100, 2400, 900);
        assert_eq!(best_for_rect(&mons, &w), Some(0));
        // 反过来 2/3 在右屏
        let w = Rect::new(1500, 100, 2700, 900);
        assert_eq!(best_for_rect(&mons, &w), Some(1));
    }

    #[test]
    fn falls_back_when_offscreen() {
        let mons = [mon(0, 0, 1920, 1080), mon(1920, 0, 3840, 1080)];
        // 完全在屏幕外：按最近中心点归到右屏
        let w = Rect::new(5000, 5000, 5100, 5100);
        assert_eq!(best_for_rect(&mons, &w), Some(1));
    }

    // ------------------------------------------------------------ 命名回退 ----

    #[test]
    fn placeholder_names_are_treated_as_missing() {
        for s in [
            "",
            "   ",
            "Generic PnP Monitor",
            "generic monitor",
            "Default Monitor",
            "Non-PnP Monitor",
            "通用即插即用监视器",
            "标准监视器",
        ] {
            assert!(is_placeholder(s), "「{s}」应被判为无信息");
        }
        for s in ["MON-A", "ACME-27Q", "ACME-24F", "ACME-24G"] {
            assert!(!is_placeholder(s), "「{s}」是真型号，不该被丢掉");
        }
    }

    #[test]
    fn device_ordinal_is_parsed() {
        assert_eq!(ordinal_from_device(r"\\.\DISPLAY1"), Some(1));
        assert_eq!(ordinal_from_device(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(ordinal_from_device("DISPLAY3"), Some(3));
        // 没有编号可用
        assert_eq!(ordinal_from_device(r"\\.\DISPLAYX"), None);
        assert_eq!(ordinal_from_device("随便什么"), None);
    }

    #[test]
    fn name_from_device_falls_back_to_device() {
        assert_eq!(name_from_device(r"\\.\DISPLAY2"), "显示器 2");
        assert_eq!(name_from_device(r"\\.\WEIRD"), "WEIRD");
    }

    #[test]
    fn label_keeps_two_identical_monitors_distinguishable() {
        // 这是用户实际遇到的情况：两块屏都报 Generic PnP Monitor，
        // 系统型号名为空。旧实现里两行的中间段一模一样，分不清谁是谁。
        let mut a = mon(0, 0, 2560, 1440);
        let mut b = mon(2560, 0, 3640, 1920);
        a.dpi = 120;
        b.dpi = 120;
        a.name = String::new();
        b.name = String::new();

        let la = a.label(0);
        let lb = b.label(1);
        assert_ne!(la, lb, "两块屏的标签必须能区分开");
        assert!(la.starts_with("显示器 1 · 2560×1440"), "实际是：{la}");
        assert!(lb.starts_with("显示器 2 · 1080×1920"), "实际是：{lb}");
        assert!(la.contains("主屏"));
    }

    #[test]
    fn label_uses_model_name_when_available() {
        let mut a = mon(0, 0, 2560, 1440);
        a.name = "MON-A".into();
        assert_eq!(a.label(0), "显示器 1 · MON-A · 2560×1440 · 主屏");
        assert_eq!(a.display_name(1), "MON-A");
    }

    // ------------------------------------------------------------ EDID 读取 ----

    /// 造一个只有型号描述符的 EDID 基础块。
    fn fake_edid(name: &str) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        let off = EDID_DESCRIPTOR_OFFSETS[0];
        e[off + 3] = EDID_TAG_MONITOR_NAME;
        let b = name.as_bytes();
        let n = b.len().min(13);
        e[off + 5..off + 5 + n].copy_from_slice(&b[..n]);
        if n < 13 {
            e[off + 5 + n] = 0x0A;
        }
        e
    }

    #[test]
    fn interface_path_is_split_into_registry_key() {
        // 本机 DISPLAY1 的实际值
        let p = r"\\?\DISPLAY#ACM2428#7&1a2b3c4d&0&UID0001#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(
            registry_key_of_interface_path(p),
            Some(("DISPLAY", "ACM2428", "7&1a2b3c4d&0&UID0001"))
        );
        // 不带 EDD flag 时 `EnumDisplayDevicesW` 给的是 class key 路径，不是实例键。
        // 顺着它读 EDID 会读到别的东西（或读不到），必须拒绝。
        assert!(registry_key_of_interface_path(
            r"MONITOR\ACM2428\{4d36e96e-e325-11ce-bfc1-08002be10318}\0004"
        )
        .is_none());
        assert!(registry_key_of_interface_path("").is_none());
        assert!(registry_key_of_interface_path(r"\\?\DISPLAY#ACM2428#").is_none());
    }

    #[test]
    fn edid_monitor_name_is_extracted() {
        assert_eq!(
            monitor_name_from_edid(&fake_edid("MON-A")).as_deref(),
            Some("MON-A")
        );
        assert_eq!(
            monitor_name_from_edid(&fake_edid("Z2719Q")).as_deref(),
            Some("Z2719Q")
        );
        // 满 13 字节、没有终止符也要能取全
        assert_eq!(
            monitor_name_from_edid(&fake_edid("ABCDEFGHIJKLM")).as_deref(),
            Some("ABCDEFGHIJKLM")
        );
    }

    #[test]
    fn edid_name_in_any_descriptor_slot() {
        // 名字落在最后一个槽（0x6C）也要找得到——本机 ACN0039 的 MON-B 就不在第一个槽
        let mut e = vec![0u8; 128];
        let off = EDID_DESCRIPTOR_OFFSETS[3];
        e[off + 3] = EDID_TAG_MONITOR_NAME;
        e[off + 5..off + 10].copy_from_slice(b"MON-B");
        e[off + 10] = 0x0A;
        assert_eq!(monitor_name_from_edid(&e).as_deref(), Some("MON-B"));
    }

    #[test]
    fn gibberish_edid_yields_no_name() {
        // 全零：头三字节为 0 但 tag 是 0，不是 0xFC，不该被当成名字
        assert_eq!(monitor_name_from_edid(&vec![0u8; 128]), None);
        // 头三字节非零 = 详细时序数据，跳过
        let mut e = vec![0u8; 128];
        for off in EDID_DESCRIPTOR_OFFSETS {
            e[off] = 0x12;
        }
        assert_eq!(monitor_name_from_edid(&e), None);
        // 太短，读不了
        assert_eq!(monitor_name_from_edid(&[0u8; 16]), None);
        // 描述符里只有占位串
        assert_eq!(monitor_name_from_edid(&fake_edid("Generic PnP")), None);
        // 非 ASCII 垃圾要滤掉，不能变成乱码
        let mut e2 = fake_edid("A");
        let off = EDID_DESCRIPTOR_OFFSETS[0];
        e2[off + 6] = 0xC4; // 一个非法 UTF-8/ASCII 字节
        e2[off + 7] = b'B';
        e2[off + 8] = 0x0A;
        assert_eq!(monitor_name_from_edid(&e2).as_deref(), Some("AB"));
    }

    #[test]
    fn edid_manufacturer_is_decoded() {
        // "ACM" → A=1, C=3, M=13 → (1<<10)|(3<<5)|13 = 1024+96+13 = 1133 = 0x046D
        let mut e = vec![0u8; 16];
        e[8] = 0x04;
        e[9] = 0x6D;
        assert_eq!(edid_manufacturer(&e).as_deref(), Some("ACM"));

        // "ACO" → A=1, C=3, O=15 → (1<<10)|(3<<5)|15 = 1024+96+15 = 1135 = 0x046F
        let mut d = vec![0u8; 16];
        d[8] = 0x04;
        d[9] = 0x6F;
        assert_eq!(edid_manufacturer(&d).as_deref(), Some("ACO"));

        // 全零不是合法厂商码
        assert_eq!(edid_manufacturer(&vec![0u8; 16]), None);
        assert_eq!(edid_manufacturer(&[0u8; 4]), None);
    }

    // ------------------------------------------------------------ 稳定标识 ----

    /// 身份段不同、其余全同的两份 EDID（模拟同型号、不同序列号的两台机器）。
    fn edid_with_serial(serial: u32) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[8..18].copy_from_slice(&[0x04, 0x6D, 0x24, 0x28, 0, 0, 0, 0, 1, 30]);
        e[12..16].copy_from_slice(&serial.to_le_bytes());
        e
    }

    #[test]
    fn edid_identity_separates_units_of_same_model() {
        let a = edid_identity(&edid_with_serial(100)).unwrap();
        let b = edid_identity(&edid_with_serial(200)).unwrap();
        assert_ne!(a, b, "同型号不同序列号必须是两个身份");
        assert!(a.starts_with("edid:"), "实际是 {a}");
        // 序列号为 0 的两台同型号机器（固件没填）会撞键 —— 上层的歧义保护负责兜住
        assert_eq!(
            edid_identity(&edid_with_serial(0)),
            edid_identity(&edid_with_serial(0))
        );
    }

    #[test]
    fn edid_identity_ignores_non_identity_bytes() {
        // 时序/描述符区域（模式切换可能变化）不影响身份：
        // 模拟同一块屏在两种信号格式下报出不同描述符
        let mut x = edid_with_serial(100);
        let mut y = x.clone();
        x[0x36] = 0xAA; // 详细时序槽
        y[0x48] = 0xBB;
        assert_eq!(edid_identity(&x), edid_identity(&y));
        // 身份段一动就不是同一块屏了
        y[15] ^= 1;
        assert_ne!(edid_identity(&x), edid_identity(&y));
    }

    #[test]
    fn edid_identity_requires_full_base_block() {
        assert_eq!(edid_identity(&edid_with_serial(1)[..127]), None);
    }

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        // FNV-1a 64 的公开参考向量，防手写常数笔误
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }
}
