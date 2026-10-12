//! # af-core
//!
//! AudioFollow 的全部逻辑层，不含任何界面代码。
//!
//! ## 分层
//!
//! ```text
//! config      配置持久化（%APPDATA%\AudioFollow\config.json）
//! win         显示器枚举 + 顶层窗口枚举 + 「窗口属于哪块屏」
//! audio       Core Audio 互操作；其中 policy 模块是未公开的按应用路由接口
//! plan        纯计算：窗口 → 屏幕 → 设备 的映射计划
//! reconcile   差量执行计划，并负责备份与还原
//! ```
//!
//! ## 典型用法
//!
//! ```text
//! // 必须在创建任何窗口之前声明 DPI 感知
//! af_core::win::dpi::enable();
//!
//! let (config, _warn) = af_core::config::AppConfig::load();
//! let env = af_core::audio::AudioEnv::init()?;          // 初始化 COM + 音频接口
//! let monitors = af_core::win::monitor::enumerate();     // 显示器
//! let windows  = af_core::win::window::enumerate_app_windows();
//! let endpoints = env.endpoints()?;                     // 音频输出设备
//!
//! let plan = af_core::plan::build(&config, &monitors, &windows, &endpoints, std::process::id());
//! let mut reconciler = af_core::reconcile::Reconciler::new();
//! for change in reconciler.apply(&env, &plan, &config.roles) {
//!     println!("{}", change.line());
//! }
//! // 退出前还原
//! reconciler.restore_all(&env, &config.roles);
//! ```
//!
//! 上面这段未经 doc test 编译校验（本机 rustdoc 起子进程会因管道耗尽失败），
//! 但它的每一步都在 `crates/audiofollow/src/selftest.rs` 里以真实代码形式跑过。
//!
//! ## 这套实现有没有风险
//!
//! 有。`audio::policy` 用的接口微软没有公开，随时可能改。缓解手段（IID 运行时发现、
//! 白名单校验、写后读回、绝不调用全局清除）都在对应模块里写明了，但**不能消除风险**。

#![cfg(windows)]

pub mod audio;
pub mod config;
pub mod plan;
pub mod reconcile;
pub mod win;

pub use win::Rect;
