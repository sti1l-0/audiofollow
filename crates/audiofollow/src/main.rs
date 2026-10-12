//! AudioFollow —— 让应用的声音跟随窗口所在的屏幕。
//!
//! 三种运行方式：
//! - 无参数：常驻托盘 + 设置窗口
//! - `--selftest`：只读自检，把整条链路打印出来
//! - `--write-test [pid]`：在单个进程上做可回滚的写入往返验证

mod selftest;
mod tray;
mod ui;

fn main() {
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
  audiofollow                    常驻托盘 + 打开设置窗口
  audiofollow --selftest         只读自检：显示器 / 音频设备 / 窗口归属 / 路由计划
  audiofollow --write-test [pid] 写入往返验证（先备份，写入，读回，再还原）
  audiofollow --clear-all --yes  清空**所有**应用的持久化音频路由（会影响其他程序）
  audiofollow --help             显示本说明

配置文件：%APPDATA%\\AudioFollow\\config.json
"
    );
}
