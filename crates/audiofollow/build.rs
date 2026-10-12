fn main() {
    // 把 .slint 界面编译进二进制。界面语法错误会在这里直接报出来。
    slint_build::compile("ui/app-window.slint").expect("slint 界面编译失败");
    println!("cargo:rerun-if-changed=ui/app-window.slint");
}
