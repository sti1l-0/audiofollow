//! AudioFollow —— 让应用的声音跟随窗口所在的屏幕。
//!
//! 运行方式：
//! - 无参数：常驻托盘 + 设置窗口（窗口事件驱动，毫秒级响应）
//! - `--selftest`：只读自检，把整条链路打印出来
//! - `--hook-test`：单独验证窗口事件钩子通不通（必要时会挪动一个窗口 2px 再复原）
//! - `--write-test [pid]`：在单个进程上做可回滚的写入往返验证

// 这是个托盘常驻的 GUI 程序：双击时不该弹出一个控制台黑框，所以编译成 WINDOWS 子系统。
// 代价是进程默认没有控制台，命令行子命令必须自己把父进程的控制台接回来（见 `main`
// 开头），否则所有 `println!` 会被静默丢弃——不报错，只是什么都没有。
#![windows_subsystem = "windows"]

mod icon;
mod selftest;
mod tray;
mod ui;

fn main() {
    // 编译成 GUI 子系统后本进程不带控制台，先把父进程的控制台接回来：
    // 从终端启动时，这一步让下面所有 println! 的输出能被看见；双击启动时父进程是
    // explorer、它没有控制台，调用失败但没有副作用——也就不会冒出黑框。
    // 必须在任何 println! 之前调用，Rust 的 stdout 句柄只取一次。
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe { af_core::win::ffi::AttachConsole(ATTACH_PARENT_PROCESS) };

    // 控制台切 UTF-8，免得中文日志在 GBK 代码页的窗口里变乱码
    unsafe { af_core::win::ffi::SetConsoleOutputCP(65001) };

    // 关键顺序：DPI 感知必须在**创建任何窗口之前**声明。
    // 一个 DPI 不感知的进程拿到的是被系统虚拟化过的坐标，在缩放比不同的两块屏交界处
    // 会让窗口归属判错。
    let dpi = af_core::win::dpi::enable();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");

    let code = match cmd {
        "--selftest" | "selftest" => selftest::run(false, &args),
        "--write-test" | "write-test" => selftest::run(true, &args),
        "--hook-test" | "hook-test" => match selftest::hook_probe(true) {
            Ok(()) => 0,
            Err(e) => {
                println!("事件钩子不可用：{e}");
                2
            }
        },
        "--clear-all" => selftest::clear_all(&args),
        "-h" | "--help" => {
            print_help();
            0
        }
        "" => match ui::run(dpi) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("界面启动失败：{e}");
                2
            }
        },
        other => {
            eprintln!("未知参数：{other}");
            print_help();
            1
        }
    };

    std::process::exit(code);
}

fn print_help() {
    println!(
        "\
AudioFollow —— 让应用的声音跟随窗口所在的屏幕

用法：
  audiofollow                    常驻托盘 + 打开设置窗口（窗口事件驱动）
  audiofollow --selftest         只读自检：显示器 / 音频设备 / 窗口归属 / 进程树 / 路由计划
  audiofollow --hook-test        单独验证窗口事件钩子能否收到事件
  audiofollow --write-test [pid] 写入往返验证（先备份，写入，读回，再还原）
  audiofollow --clear-all --yes  清空**所有**应用的持久化音频路由（会影响其他程序）
  audiofollow --help             显示本说明

配置文件：%APPDATA%\\AudioFollow\\config.json
"
    );
}
