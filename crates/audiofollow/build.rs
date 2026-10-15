//! 构建脚本：编译 slint 界面 + 把 Windows 资源（exe 图标、版本信息）嵌进 PE。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // 把 .slint 界面编译进二进制。界面语法错误会在这里直接报出来。
    slint_build::compile("ui/app-window.slint").expect("slint 界面编译失败");
    println!("cargo:rerun-if-changed=ui/app-window.slint");

    embed_windows_resources();
}

/// 把 `win/audiofollow.rc` 编成 .res 并交给链接器。
///
/// 不做这一步的话，exe 在资源管理器里是空白图标 —— slint 的 `Window.icon`
/// 只影响运行时的窗口，编不进 PE 资源节。
///
/// 找不到 rc.exe 时只告警、不中断构建：图标属于锦上添花，
/// 不该让一个缺了 Windows SDK 的机器连程序都编不出来。
#[cfg(windows)]
fn embed_windows_resources() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = manifest.join("win").join("audiofollow.rc");

    println!("cargo:rerun-if-changed=win/audiofollow.rc");
    println!("cargo:rerun-if-changed=ui/icon.ico");

    let Some(rc_exe) = find_rc() else {
        println!(
            "cargo:warning=找不到 rc.exe（Windows SDK），本次不嵌入 exe 图标与版本信息"
        );
        return;
    };

    // 版本号只从 Cargo.toml 取一份，生成成 C 头文件给 .rc include，
    // 避免两处各写一遍对不上。
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<String> =
        version.split('.').map(|s| s.to_string()).collect();
    while parts.len() < 4 {
        parts.push("0".into());
    }
    std::fs::write(
        out_dir.join("version_gen.h"),
        format!(
            "// 由 build.rs 生成，不要手改\n#define AF_VERSION_COMMA {}\n#define AF_VERSION_STR \"{}\"\n",
            parts.join(","),
            version
        ),
    )
    .expect("写 version_gen.h 失败");

    let res = out_dir.join("audiofollow.res");
    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/I")
        .arg(&out_dir) // 给 version_gen.h
        .arg("/fo")
        .arg(&res)
        .arg(&rc_file)
        .current_dir(&manifest)
        .status();

    match status {
        Ok(s) if s.success() => {
            // link.exe 认识 .res，直接作为输入文件即可。
            println!("cargo:rustc-link-arg={}", res.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe 退出码 {s}，未嵌入 exe 图标"),
        Err(e) => println!("cargo:warning=调用 rc.exe 失败：{e}，未嵌入 exe 图标"),
    }
}

#[cfg(not(windows))]
fn embed_windows_resources() {}

/// 找 rc.exe：先看 `RC` 环境变量，再扫 Windows SDK 的安装目录（取版本最高的）。
fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(var) {
            roots.push(Path::new(&base).join("Windows Kits").join("10").join("bin"));
        }
    }

    let mut candidates: Vec<(Vec<u32>, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let exe = entry.path().join(arch).join("rc.exe");
            if !exe.is_file() {
                continue;
            }
            // 目录名形如 10.0.26100.0，按数字段排序取最新
            let key: Vec<u32> = entry
                .file_name()
                .to_string_lossy()
                .split('.')
                .filter_map(|s| s.parse().ok())
                .collect();
            candidates.push((key, exe));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.pop().map(|(_, p)| p)
}
