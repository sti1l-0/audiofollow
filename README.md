# AudioFollow —— 让应用音频跟随窗口所在的屏幕

多显示器场景下，每块屏幕各挂一个音频设备。系统的默认音频设备是全局的，所有声音都从同一个设备出来。
AudioFollow 在托盘常驻，监视窗口落在哪块屏幕，然后把**该窗口所属进程**的声音自动改到**那块屏幕对应的音频设备**。

不改系统默认设备，不动其他应用，纯用户态，无内核驱动。

---

## 实现原理

Windows 有个按应用分流音频的接口（系统"音量合成器"自己用的那套）：

```
AudioSes.dll!DllGetActivationFactory
    → Windows.Media.Internal.AudioPolicyConfig   (激活工厂)
    → IInspectable::GetIids()                     (运行时发现 IID，规避版本变更)
    → IAudioPolicyConfig  (vtable 槽位 25=Set / 26=Get / 27=ClearAll)
    → SetPersistedDefaultAudioEndpoint(pid, eRender, role, "\?\SWD#MMDEVAPI#{...}#{e6327cad-...}")
```

**该接口微软未公开文档**，属于系统内部实现。行为、IID、vtable 布局都可能随系统更新变化。
本项目对此的对策：运行时发现 IID + 白名单校验 + 读回校验 + 绝不调用 `ClearAll`。

其它三块：

| 能力 | 手段 |
|---|---|
| 枚举显示器 | `EnumDisplayMonitors` + `GetMonitorInfoW` |
| 窗口落在哪块屏 | `EnumWindows` + `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` + 面积交集 |
| 进程 → 音频设备 | 上面的 `IAudioPolicyConfig` |

---

## 目录结构

```
crates/af-core/        纯逻辑库（无 GUI 依赖）
  src/win/             Win32：DPI 感知、显示器枚举、窗口枚举与归属判定
  src/audio/           Core Audio COM 互操作 + 未公开的 AudioPolicyConfig
  src/config.rs        配置持久化（%APPDATA%\AudioFollow\config.json）
  src/plan.rs          窗口→屏幕→设备的映射计划（含多窗口仲裁）
  src/reconcile.rs     差分执行 + 原值备份 + 还原
crates/audiofollow/    托盘 + 设置界面（slint）
spikes/                可行性验证程序，独立小工程，不参与主构建
reference/             C# 参考实现（早期探索阶段留档）
docs/                  技术方案、研究笔记与实测报告
research/              第三方源码（SoundSwitch / EarTrumpet），已 gitignore
```

---

## 构建与运行

```bash
# 无界面自检：打印显示器、音频设备、窗口归属和将要执行的路由计划（只读，不写系统）
cargo run -p audiofollow -- --selftest

# 可回滚的写入验证：备份原值 → 写入 → 读回校验 → 还原
cargo run -p audiofollow -- --write-test

# 正常启动：托盘 + 设置窗口
cargo run -p audiofollow
```

需要 Rust 1.75+ 与 MSVC 工具链。若依赖拉取超时，套一层本地代理：

```bash
env https_proxy=http://127.0.0.1:10612 http_proxy=http://127.0.0.1:10612 cargo build
```

---

## 已知边界

1. **粒度只到「应用/进程」，做不到「窗口」。** 同一个 Chrome 开两个窗口分别放两块屏时，Windows 层面无法让它们走不同设备。多窗口跨屏时按面积占比最大者归属。
2. **同一进程只有一条持久化记录。** 这是 Windows 的限制，不是本项目的取舍。
3. **部分应用不即时生效。** 官方音量合成器也是"通常立即生效，个别应用需暂停重播或重启"；自带设备选择器的程序（游戏、DAW、Teams/Zoom/Discord）会覆盖本程序的设置。
4. **目标进程必须已有音频会话**，否则写入返回 `PROCESS_NO_AUDIO`（`0x80070057`）被静默忽略。因此需要轮询重试，不能设一次就完事。
5. **写入会持久化到系统里**（重启仍在，且出现在用户的音量合成器 UI 中）。所以必须提供备份与还原——本程序退出时会把改过的条目还原干净（见下）。
6. **依赖未公开接口**，微软可能在任何一次系统更新里改坏它。IID 运行时发现能挡住大部分情况，但不是万无一失。

## 已验证的行为（本机实测，非推测）

用 `audiofollow --write-test` 在一个真实进程上做过完整的「备份 → 写入 → 读回 → 还原」往返：

| 行为 | 实测结果 |
|---|---|
| IID 运行时发现 | `GetIids()` 返回 2 个接口，取到 `ab3d4648-e242-459f-b02f-541c70306324`，命中白名单 |
| 写入（槽位 25，HSTRING 按值传） | 返回 `S_OK` |
| 写后读回（槽位 26） | 与写入值完全一致 |
| 「跟随系统默认」的读回值 | `S_OK` + **空字符串**（不是 `PROCESS_NO_AUDIO`） |
| **恢复为跟随系统默认** | 写一个**空 HSTRING** 就返回 `S_OK`，读回变回"跟随系统默认"。**这条路可行**，所以退出时能完整还原、不留残余 |
| 还原后复检 | 与改动前逐项一致，遗留影响：无 |

> 关于"写空值"这一条：SoundSwitch 的代码里有 `if (string.IsNullOrEmpty(deviceId)) return false;` 的保护，说明它的作者没有试过这条路，因此它的实现里留下了无法清除的残余记录。本程序实测确认可以写空值，并把它用在了还原路径上。

## 许可证

`MIT OR Apache-2.0`。本仓库代码为独立实现，未复制 SoundSwitch（GPL-3.0）或 EarTrumpet 的源码；
仅参考了公开可查的接口事实（DLL 名、IID、vtable 槽位号、设备 ID 格式）。`research/` 下的第三方源码不随仓库分发。
