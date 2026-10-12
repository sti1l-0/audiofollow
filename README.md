# AudioFollow —— 让应用音频跟随窗口所在的屏幕

多显示器场景下，每块屏幕各挂一个音频设备。系统的默认音频设备是全局的，所有声音都从同一个设备出来。
AudioFollow 在托盘常驻，监视窗口落在哪块屏幕，然后把**该应用的声音**自动改到**那块屏幕对应的音频设备**。

不移除系统默认设备、不动其他应用、纯用户态、无内核驱动。拖动窗口后约 **250 ms** 声音就换设备。

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

其它几块：

| 能力 | 手段 |
|---|---|
| 枚举显示器 | `EnumDisplayMonitors` + `GetMonitorInfoW` |
| 窗口落在哪块屏 | `EnumWindows` + `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` + 面积交集 |
| **多进程应用** | `CreateToolhelp32Snapshot` 建进程树，把窗口进程展开成「根 + 同名可执行文件的后代」 |
| **实时触发** | `SetWinEventHook`（窗口移动/显隐/创建销毁/前台切换）+ 250 ms 去抖 |
| 进程 → 音频设备 | 上面的 `IAudioPolicyConfig` |

### 多进程应用：本项目最容易踩空的地方

Chrome / Edge / Electron / WebView2 的**窗口在主进程、音频在子进程**：

```
msedge.exe (pid 1836)  ← 窗口在这里，归属判定用它
  ├─ msedge.exe  --type=renderer
  ├─ msedge.exe  --type=utility --utility-sub-type=audio.mojom.AudioService   ← 真正出声的
  └─ msedge.exe  --type=gpu-process
```

只写窗口进程的路由，这类应用**完全不生效**（窗口进程压根没有音频会话，写入还会被 `PROCESS_NO_AUDIO` 挡掉）。
所以计划里区分「应用根」（做归属仲裁的那个进程）和「写入目标」（根 + 同名后代，逐个写）。

**后代必须限定「同名可执行文件」**，这是本机的实测依据：

| 应用 | 后代总数 | 同名后代 | 异名后代 |
|---|---|---|---|
| `msedge.exe` (1836) | 19 | **19** | 0 |
| `msedgewebview2.exe` (11048) | 5 | **5** | 0 |
| `imapp.exe` (3544) | 15 | 4 | `imapp_helper`×10、`crashpad_handler`×1 |
| `launcher.exe` (8128) | 29 | 8 | `cmd`、`conhost`×9、`node`×3、`pwsh`、`sandbox-cli`×2 |

最后一行是关键反例：launcher 的子树里挂着一堆**毫不相干**的 `cmd` / `node` 进程。
无脑展开全部后代就会把这些工具的音频一起改道。限定同名之后，浏览器系 100% 覆盖、启动器型零误伤。
（本机 `svchost.exe` 有 87 个实例——一旦某个 svchost 有窗口，无脑展开会卷进大半个系统。）

---

## 目录结构

```
crates/af-core/        纯逻辑库（无 GUI 依赖）
  src/win/ffi.rs        手写 Win32 FFI 声明
  src/win/dpi.rs        Per-Monitor V2 DPI 感知
  src/win/monitor.rs    显示器枚举 + 命名（型号缺失时退回「显示器 N」）
  src/win/window.rs     顶层窗口枚举 + 归属判定
  src/win/proc.rs       进程树快照 + 父 PID 复用校验
  src/win/hook.rs       SetWinEventHook 窗口事件钩子
  src/audio/            Core Audio COM 互操作 + 未公开的 AudioPolicyConfig
  src/config.rs         配置持久化（%APPDATA%\AudioFollow\config.json）
  src/plan.rs           窗口→屏幕→设备的映射计划（多窗口仲裁 + 多进程展开）
  src/reconcile.rs      差分执行 + 原值备份 + 还原
crates/audiofollow/    托盘 + 设置界面（slint）
  ui/app-window.slint   界面定义
  src/tray.rs           tao + tray-icon 常驻托盘
  src/ui.rs             slint 事件循环 + 事件去抖 + 路由决策
  src/selftest.rs       无界面自检（含可回滚的写入往返与钩子自证）
spikes/                可行性验证程序，独立小工程，不参与主构建
reference/             C# 参考实现（早期探索阶段留档）
docs/                  技术方案、研究笔记与实测报告
research/              第三方源码（SoundSwitch / EarTrumpet），已 gitignore
```

---

## 构建与运行

```bash
# 无界面自检：显示器 / 音频设备 / 窗口归属 / 进程树 / 路由计划 / 事件钩子（只读，不写系统）
cargo run -p audiofollow -- --selftest

# 单独验证事件钩子：合成事件 + 真实挪动一个窗口 2px 再复原
cargo run -p audiofollow -- --hook-test

# 可回滚的写入验证：备份原值 → 写入 → 读回校验 → 还原
# 不指定 pid 时自动挑一个有音频会话的进程；也可以指定某个子进程，
# 例如 --write-test 19232（Edge 的音频子进程）
cargo run -p audiofollow -- --write-test [pid]

# 正常启动：托盘 + 设置窗口
cargo run -p audiofollow
```

需要 Rust 1.75+ 与 MSVC 工具链。若依赖拉取超时，套一层本地代理：

```bash
env https_proxy=http://127.0.0.1:10612 http_proxy=http://127.0.0.1:10612 cargo build
```

---

## 已知边界

1. **粒度只到「应用」，做不到「窗口」。** 同一个 Chrome 开两个窗口分别放两块屏时，Windows 层面无法让它们走不同设备。多窗口跨屏时按面积占比最大者归属，并在日志里标注发生了取舍。
2. **同一进程只有一条持久化记录。** 这是 Windows 的限制，不是本项目的取舍。
3. **部分应用不即时生效。** 官方音量合成器也是"通常立即生效，个别应用需暂停重播或重启"；自带设备选择器的程序（游戏、DAW、Teams/Zoom/Discord）会覆盖本程序的设置。
4. **目标进程必须已有音频会话**，否则写入返回 `PROCESS_NO_AUDIO`（`0x80070057`）被静默忽略。所以某个应用**开始出声**时才真正写进去；这也是兜底轮询存在的理由（事件只覆盖"用户摆窗口"）。
5. **写入会持久化到系统里**（重启仍在，且出现在用户的音量合成器 UI 中）。所以必须提供备份与还原——本程序退出时会把改过的条目还原干净（见下）。
6. **依赖未公开接口**，微软可能在任何一次系统更新里改坏它。IID 运行时发现能挡住大部分情况，但不是万无一失。
7. **多进程展开限定同名可执行文件，因此有一类场景不覆盖**：宿主程序 + 独立的音频子进程。例如某个 App 用 WebView2 播放音频，声音实际出在 `msedgewebview2.exe` 里，而它和宿主程序不同名，不会被一起改道。计划会在「说明」里明确列出这类"正在出声但未被接管"的进程，不假装覆盖到了。
8. **显示器型号名可能取不到。** 虚拟显示器、无 EDID 型号段的设备只报 `Generic PnP Monitor`。此时一律显示为「显示器 N」（N 为枚举序号，主屏在前、其余按坐标从左到右），绝不把 `Generic PnP Monitor` 这种两块屏完全一样的串摆给用户看。

## 已验证的行为（本机实测，非推测）

### 音频路由写入

用 `audiofollow --write-test` 在真实进程上做过完整的「备份 → 写入 → 读回 → 还原」往返：

| 行为 | 实测结果 |
|---|---|
| IID 运行时发现 | `GetIids()` 返回 2 个接口，取到 `ab3d4648-e242-459f-b02f-541c70306324`，命中白名单 |
| 写入（槽位 25，HSTRING 按值传） | 返回 `S_OK` |
| 写后读回（槽位 26） | 与写入值完全一致 |
| 「跟随系统默认」的读回值 | `S_OK` + **空字符串**（不是 `PROCESS_NO_AUDIO`） |
| **恢复为跟随系统默认** | 写一个**空 HSTRING** 就返回 `S_OK`，读回变回"跟随系统默认"。**这条路可行**，所以退出时能完整还原、不留残余 |
| 还原后复检 | 与改动前逐项一致，遗留影响：无 |

在三个不同的目标进程上都复现了同一结论，**包括一个真正的多进程子进程**：

| 目标进程 | 性质 | 结果 |
|---|---|---|
| `imapp.exe` (3544) | 单进程应用 | 写入 `S_OK`、读回一致、还原成功、遗留影响：无 |
| `app.exe` (436) | 未知应用 | 同上 |
| **`msedge.exe` (19232)** | **Edge 的音频子进程**（窗口进程是 1836） | 同上；且**原值本来就是「已固定到 MON-B」**——说明用户此前手动设过这个进程的按应用路由 |

> 关于"写空值"这一条：SoundSwitch 的代码里有 `if (string.IsNullOrEmpty(deviceId)) return false;` 的保护，说明它的作者没有试过这条路，因此它的实现里留下了无法清除的残余记录。本程序实测确认可以写空值，并把它用在了还原路径上。

### 多进程覆盖（真实数据）

`--selftest` 的 `[5]` 段用真实快照验证「哪些出声的进程会被覆盖到」：

```
正在出声的进程，是否落在某个窗口进程的同名子树里：
     436  app.exe             ⚠️ 不会被接管（与任何窗口进程都不同名，或不在其树下）
    3544  imapp.exe             ✅ 会被改道（挂在窗口进程 pid 3544 的树下）
   19232  msedge.exe             ✅ 会被改道（挂在窗口进程 pid 1836 的树下）
   35336  game.exe  ✅ 会被改道（挂在窗口进程 pid 35336 的树下）
```

`19232 msedge.exe` 这一行是决定性的：Edge 的音频**确实在子进程里输出**，而进程树把它和窗口进程 1836 关联起来了。计划里 1836 的写入目标因此是 **20 个进程**（自身 + 19 个同名后代）。

### 事件钩子（实时性）

`audiofollow --hook-test` 分两级取证，连续 3 次运行结果一致：

| 级别 | 手段 | 结果 |
|---|---|---|
| ① 合成事件 | `NotifyWinEvent` 造一个事件 | ✅ 回调被调用且通过过滤 |
| ② 真实位移 | 把某窗口平移 2px 再复原（不激活、不改 z 序） | ✅ 收到 2 条事件，最近一条是 `EVENT_OBJECT_LOCATIONCHANGE` |

回调里只累加一个原子计数，真正的同步由主线程去抖后执行，因此拖动窗口时的每帧事件风暴不会造成反复写入。

## 许可证

`MIT OR Apache-2.0`。本仓库代码为独立实现，未复制 SoundSwitch（GPL-3.0）或 EarTrumpet 的源码；
仅参考了公开可查的接口事实（DLL 名、IID、vtable 槽位号、设备 ID 格式）。`research/` 下的第三方源码不随仓库分发。
