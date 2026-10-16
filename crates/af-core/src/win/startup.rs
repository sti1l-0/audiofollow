//! 「开机自动启动」开关 —— 读写注册表里的启动项。
//!
//! 位置是 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下一个名为 `AudioFollow`
//! 的字符串值。选它而不是别的手段的理由：
//!
//! - **不需要管理员权限**。写 `HKLM\...\Run` 要提权，而这个程序是个托盘小工具，
//!   为了开机启动弹一次 UAC 完全不划算。
//! - **不需要生成 .lnk**。快捷方式要走 COM 的 `IShellLink`，而本项目的约定是
//!   "除了 serde 之外零第三方依赖、COM 全部手写"，为一个开关手搓 .lnk 不值得。
//! - **用户能在系统里看见并自行禁用**。「任务管理器 → 启动应用」列的就是这一项，
//!   用户想关掉它不必回到本程序，这是系统给用户的正当入口。
//!
//! 代价是记录里存的是 exe 的**绝对路径**。程序被移动、或升级到另一个版本目录之后，
//! 这条记录会静默失效（系统找不到文件就不启动，不会报错）。
//! 所以每次启动都拿当前路径和记录比对一次，不一致就改写 —— 见 [`sync_to_current_exe`]。
//! scoop 安装尤其依赖这个自愈：`current` 只是符号链接，而 `current_exe()` 给的是
//! **解析之后**的版本化目录（`apps\audiofollow\0.1.1\audiofollow.exe`），
//! 每次 `scoop update` 之后这个路径都会变。

use std::path::Path;

use super::ffi::{self, HKEY};

/// 启动项所在的位置（`HKEY_CURRENT_USER` 下）。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 值的名字。用户在任务管理器里看到的就是它。
const VALUE_NAME: &str = "AudioFollow";

/// 开机启动时附加的参数：只驻留托盘，不弹设置窗口。
pub const STARTUP_FLAG: &str = "--startup";

/// 开机启动开关的当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupState {
    /// 注册表里没有这一项。
    Disabled,
    /// 开着，且记录的路径就是当前这个 exe。
    Enabled,
    /// 开着，但记录的路径是**另一个** exe（换目录/升级过还没自愈）。
    /// 带上记录里的内容，好让日志能把它原样打出来。
    Stale(String),
}

impl StartupState {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// 要写进注册表的命令行。
///
/// **必须带引号**：路径里可能有空格（`C:\Program Files\...`），
/// 不加引号的话 `CreateProcess` 会把空格前面那段当成程序名。
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" {STARTUP_FLAG}", exe.display())
}

/// 把一个「程序路径 + 参数」形式的命令行拆成程序路径。
///
/// 有引号就取引号之间的内容；没有引号就取第一个空格之前的部分。
/// 只用来和当前 exe 比对，所以不需要处理 `\"` 转义这类边角。
pub fn exe_from_command_line(line: &str) -> &str {
    let line = line.trim();
    match line.strip_prefix('"') {
        Some(rest) => match rest.find('"') {
            Some(end) => &rest[..end],
            // 引号只有开头这一个（记录被谁改坏了）：退回按空格切
            None => rest,
        },
        None => line.split(' ').next().unwrap_or(line),
    }
}

/// 读当前状态。注册表访问失败时返回 `Err`，调用方照"读不到"处理即可。
pub fn state() -> Result<StartupState, String> {
    let Some(recorded) = read_record()? else {
        return Ok(StartupState::Disabled);
    };
    let exe = current_exe()?;
    let recorded_exe = exe_from_command_line(&recorded);
    if recorded_exe.eq_ignore_ascii_case(&exe) {
        Ok(StartupState::Enabled)
    } else {
        Ok(StartupState::Stale(recorded))
    }
}

/// 打开/关闭开机启动。
pub fn set_enabled(on: bool) -> Result<(), String> {
    if on {
        let exe = current_exe()?;
        write_record(&command_line(Path::new(&exe)))
    } else {
        delete_record()
    }
}

/// 启动时调用：如果开着开机启动，但记录里的路径不是当前 exe，就用当前路径重写。
///
/// 只在**开着**的时候动它 —— 用户没打开这个开关，就不该在我们不知情的情况下
/// 往注册表里写东西。
pub fn sync_to_current_exe() -> Result<StartupState, String> {
    match state()? {
        StartupState::Disabled => Ok(StartupState::Disabled),
        StartupState::Enabled => Ok(StartupState::Enabled),
        StartupState::Stale(old) => {
            // 记录还在，只是指向别处。按当前路径重写。
            set_enabled(true)?;
            Ok(StartupState::Stale(old))
        }
    }
}

fn current_exe() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("取当前程序路径失败：{e}"))
}

/// 读注册表里那条记录的原始字符串。`Ok(None)` = 值不存在（= 关着）。
fn read_record() -> Result<Option<String>, String> {
    unsafe {
        let mut hkey: HKEY = 0;
        let key = ffi::wide(RUN_KEY);
        let rc = ffi::RegOpenKeyExW(
            ffi::HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            ffi::KEY_READ,
            &mut hkey,
        );
        if rc != ffi::ERROR_SUCCESS {
            // 键都打不开：不是"关着"，而是读不到。如实报错，不去猜。
            return Err(format!("打不开启动项注册表键（错误码 {rc}）"));
        }

        let name = ffi::wide(VALUE_NAME);
        let mut kind = 0u32;
        let mut bytes = 0u32;
        // 先问长度。值不存在时 RegQueryValueExW 返回 ERROR_FILE_NOT_FOUND。
        let rc = ffi::RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        );
        if rc == ffi::ERROR_FILE_NOT_FOUND {
            ffi::RegCloseKey(hkey);
            return Ok(None);
        }
        if rc != ffi::ERROR_SUCCESS {
            ffi::RegCloseKey(hkey);
            return Err(format!("读启动项失败（错误码 {rc}）"));
        }

        let mut buf = vec![0u8; bytes.max(2) as usize];
        let mut got = bytes;
        let rc = ffi::RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            buf.as_mut_ptr(),
            &mut got,
        );
        ffi::RegCloseKey(hkey);
        if rc != ffi::ERROR_SUCCESS {
            return Err(format!("读启动项内容失败（错误码 {rc}）"));
        }

        // 存的是 REG_SZ（UTF-16，带结尾 NUL）。按 UTF-16 解释。
        let units: Vec<u16> = buf[..got as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(Some(ffi::utf16_arr_to_string(&units)))
    }
}

fn write_record(line: &str) -> Result<(), String> {
    unsafe {
        let mut hkey: HKEY = 0;
        let key = ffi::wide(RUN_KEY);
        let rc = ffi::RegOpenKeyExW(
            ffi::HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            ffi::KEY_SET_VALUE,
            &mut hkey,
        );
        if rc != ffi::ERROR_SUCCESS {
            return Err(format!("打不开启动项注册表键（错误码 {rc}）"));
        }

        let name = ffi::wide(VALUE_NAME);
        let mut data = ffi::wide(line); // 末尾带 NUL —— REG_SZ 的长度要含它
        let cb = (data.len() * 2) as u32;
        let rc = ffi::RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            ffi::REG_SZ,
            data.as_mut_ptr() as *const u8,
            cb,
        );
        ffi::RegCloseKey(hkey);
        if rc != ffi::ERROR_SUCCESS {
            return Err(format!("写启动项失败（错误码 {rc}）"));
        }
        Ok(())
    }
}

fn delete_record() -> Result<(), String> {
    unsafe {
        let mut hkey: HKEY = 0;
        let key = ffi::wide(RUN_KEY);
        let rc = ffi::RegOpenKeyExW(
            ffi::HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            ffi::KEY_SET_VALUE,
            &mut hkey,
        );
        if rc != ffi::ERROR_SUCCESS {
            return Err(format!("打不开启动项注册表键（错误码 {rc}）"));
        }

        let name = ffi::wide(VALUE_NAME);
        let rc = ffi::RegDeleteValueW(hkey, name.as_ptr());
        ffi::RegCloseKey(hkey);
        // 值不存在 = 已经是"关着"的目标状态
        if rc != ffi::ERROR_SUCCESS && rc != ffi::ERROR_FILE_NOT_FOUND {
            return Err(format!("删除启动项失败（错误码 {rc}）"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_the_path_and_adds_flag() {
        let line = command_line(Path::new(r"C:\Program Files\AudioFollow\audiofollow.exe"));
        assert_eq!(
            line,
            r#""C:\Program Files\AudioFollow\audiofollow.exe" --startup"#
        );
    }

    #[test]
    fn parses_back_quoted_path() {
        let line = r#""C:\Users\me\scoop\apps\audiofollow\current\audiofollow.exe" --startup"#;
        assert_eq!(
            exe_from_command_line(line),
            r"C:\Users\me\scoop\apps\audiofollow\current\audiofollow.exe"
        );
    }

    #[test]
    fn parses_back_path_without_quotes() {
        // 手改过注册表、或者别的工具写进去的形式
        assert_eq!(exe_from_command_line(r"C:\tools\audiofollow.exe --startup"), r"C:\tools\audiofollow.exe");
        assert_eq!(exe_from_command_line(r"C:\tools\audiofollow.exe"), r"C:\tools\audiofollow.exe");
    }

    #[test]
    fn roundtrips_through_a_stale_comparison() {
        // 记录指向别的路径 → 应该被判成 Stale（用来触发自愈）
        let exe = Path::new(r"C:\new\audiofollow.exe");
        let line = command_line(exe);
        assert_eq!(exe_from_command_line(&line), r"C:\new\audiofollow.exe");
        assert!(!exe_from_command_line(&line).eq_ignore_ascii_case(r"C:\old\audiofollow.exe"));
    }

    #[test]
    fn stale_detection_is_case_insensitive() {
        // Windows 路径大小写不敏感，大小写差异不该被当成"换了路径"
        assert!(r"C:\Tools\AF.EXE".eq_ignore_ascii_case(r"c:\tools\af.exe"));
    }
}
