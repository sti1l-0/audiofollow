// 验证：preferred-width/height 才允许缩放，固定 width/height 会锁死窗口。
slint::slint! {
    export component App inherits Window {
        preferred-width: 600px;
        preferred-height: 400px;
        min-width: 400px;
        min-height: 300px;
        Text {
            text: Math.round(root.width / 1px) + " x " + Math.round(root.height / 1px);
            font-size: 48px;
        }
    }
}

fn main() {
    let app = App::new().unwrap();
    app.show().unwrap();
    slint::run_event_loop().unwrap();
}
