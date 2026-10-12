# 基座选型：SoundSwitch vs EarTrumpet

> 前提变更：用户确认**愿意开源、不打算商用** → GPL-3.0 不再是障碍，SoundSwitch 正式进入候选。
> 评估角度：**技术改造难度**（不考虑许可证）
> 结论：**都不适合直接 fork，最优解是「引用 SoundSwitch.Audio.Manager + 自己写外壳」**

---

## 一、硬指标对比

| 维度 | **SoundSwitch** | **EarTrumpet** | 胜 |
|---|---|---|---|
| 运行时框架 | **.NET 10**（`net10.0-windows10.0.17763.0`） | .NET Framework **4.6.2** | SoundSwitch |
| 项目格式 | 现代 SDK-style csproj | 老式 csproj + `EarTrumpet.vs15.sln` | SoundSwitch |
| **构建产物** | 直接 build 出 exe，双击即跑 | **必须走 `.wapproj` 打包成 MSIX**（带签名证书） | **SoundSwitch** |
| 构建环境 | 装 .NET 10 SDK 即可 | VS2017+ / .NET 4.6.2 Dev Pack / **Windows 10 SDK 10.0.14393.0** | SoundSwitch |
| **改一行代码的迭代成本** | `dotnet build` → 运行 | 重新打包 → 签名 → 安装 → 运行 | **SoundSwitch** |
| **音频层是否独立库** | ✅ `SoundSwitch.Audio.Manager` 是 `OutputType=Library`，**零 UI 依赖**，仅 2 个 NuGet 包 | ❌ 混在 WPF app 项目里（`EarTrumpet/Interop/...`） | **SoundSwitch** |
| **已有窗口/进程追踪** | ✅ `WindowMonitor`(SetWinEventHook) + `ProcessMonitor`(2s 轮询) | ❌ **零**（grep `MonitorFromWindow` / `SetWinEventHook` / `Screen.AllScreens` 全空） | **SoundSwitch** |
| IID 处理 | 运行时 `GetIids()` 自动发现 + 白名单校验 | 每个 Windows 版本一个 Impl 类（硬编码双分支） | SoundSwitch |
| 测试工程 | ✅ NUnit + FluentAssertions，有独立测试项目 | 无 | SoundSwitch |
| 项目分层 | 11 个 csproj（Audio.Manager / Common / IPC / UI / CLI / Tests） | 2 个 csproj（app + ColorTool） | SoundSwitch |
| 代码规模 | 385 `.cs` | 310 `.cs` | 相当 |
| UI 技术栈 | WinForms | **WPF + XAML** | EarTrumpet |
| per-app 路由能力 | `SwitchProcessTo` | `AudioPolicyConfig.SetDefaultEndPoint` | 平 |
| 许可证 | GPL-3.0 | MIT（排除 3 家公司） | 不考虑 |

**硬指标上 SoundSwitch 拿下 9/11。** 决定性的是三条：**独立类库**、**已有窗口追踪**、**不用打包**。

---

## 二、为什么"必须打包"是个致命伤

EarTrumpet 的官方构建路径（`COMPILING.md`）：

> 1. 装 VS2017 + .NET 桌面开发 + UWP 开发工作负载
> 2. 装 Windows 10 SDK **10.0.14393.0**（2016 年的老版本）
> 3. 装 .NET Framework 4.6.2 Developer Pack
> 4. **把目标平台改成 x86，构建 `EarTrumpet.Package` 项目**

`.wapproj` 里配置了 `PackageCertificateKeyFile` / `PackageCertificateThumbprint`，即产出 MSIX 需要证书签名。

对这个项目来说这是**最要命的一条**：我们的活儿本质是"反复试——拖窗口、听声音、看有没有切过去"，是个高频率调参的过程。如果每次改动都要重新打包+安装，一个下午可能只能试几个版本。SoundSwitch 则是 `dotnet build` 完直接跑。

> 注：EarTrumpet 的 `PackageHelper.cs` 里有 `isPackaged` 判断，理论上能非打包运行，但这不是官方支持路径，需要自己折腾且有踩坑风险。

---

## 三、但真正的答案是：**两个都别 fork**

### 我们需要的三个模块，两个项目的覆盖情况

| 我们要做的 | SoundSwitch | EarTrumpet |
|---|---|---|
| ① 路由层（把应用改到指定设备） | ✅ 有，**且是独立库可直接引用** | ✅ 有，但和 app 项目绑在一起 |
| ② 窗口 → 屏幕判定 | ⚠️ 有窗口钩子骨架，**无屏幕几何** | ❌ 完全没有 |
| ③ 屏幕 ↔ 设备映射配置 UI | ❌ 没有 | ❌ 没有 |

**两个项目都只覆盖了 1/3，而且都没覆盖我们最核心的那个 1/3（屏幕维度）。**

所以 fork 任何一个都要额外做两件事，而 fork 的代价是吃下它们整个 UI 层——SoundSwitch 的 385 个 `.cs` 里，大部分是更新器、快捷键、多语言、托盘菜单、IPC、蓝牙管理……和我们无关。

### 最优解：把路由层当库引用，外壳自己写

```
AudioFollow.sln
├── AudioFollow.Tray          ← 自写：托盘外壳 + 屏幕↔设备配置界面
├── AudioFollow.Core          ← 自写：屏幕判定 + 决策引擎（我们的核心增量）
│     ├── WindowScreenResolver.cs    ✅ 已完成（DPI / 显示器枚举 / DWM 边框 / 面积交集 / 多窗口仲裁）
│     └── RouterService.cs           ⬜ 待写（WinEventHook + 2s 轮询 + 差分 + 重试）
└── 引用 SoundSwitch.Audio.Manager    ← GPL-3.0，直接拿经过生产验证的路由层
```

**收益**：
- 白拿：经生产验证的路由层（含 `UnsupportedAudioPolicyConfig` 降级、`ComThread` 线程处理、单元测试）
- 白拿思路：`WindowMonitor` + `ProcessMonitor` 的双通道触发设计（可以照抄，也可以直接用它的库）
- 不吃：数千行与我们无关的 UI / 更新器 / 快捷键 / 多语言
- **改造面从"上千文件"缩到"3 个自己的模块 + 1 个库引用"**

### 如果非要 fork 一个

| | fork SoundSwitch | fork EarTrumpet |
|---|---|---|
| 白拿 | `WindowMonitor` + `ProcessMonitor`（省 Phase 3 一半） | 好看的 WPF UI 框架 |
| 痛点 | 要在 **WinForms** 里加屏幕配置界面（做屏幕缩略图选择会很痛苦） | **零窗口追踪**，全部自己写；且改一行要打包 |
| 判定 | 勉强可行 | **不推荐** |

---

## 四、一个需要你决定的小分歧

`SoundSwitch.Audio.Manager` 是现成的、经过生产验证的、有测试覆盖的库；我之前给你写的那份 clean-room 实现（`src/AudioFollow/Interop/AudioPolicyConfig.cs`）是零依赖、约 400 行、但**未经任何验证**的。

既然你接受 GPL-3.0：

| | 用 `SoundSwitch.Audio.Manager` | 用我写的 clean-room 版 |
|---|---|---|
| 成熟度 | ✅ 生产验证 + 单元测试 | ⚠️ 未验证 |
| 依赖 | CsWinRT + Serilog + .NET 10 | **零依赖** |
| 可控性 | 一般（要跟着它升级） | 完全可控，代码短 |
| 建议 | **首选** | 备选；或作为交叉验证的对照实现 |

**我的建议：首选引用它的库。** 把有限的时间花在真正的增量上（屏幕判定 + 配置 UI），而不是重写已经别人踩过坑的部分。我那份可以留着当对照——如果它的库在某个场景失败，两份实现对比能快速定位问题。

⚠️ 注意：它 target `net10.0-windows`。你的项目得跟上 .NET 10；若想用 .NET 8，需要自己降到 `net8.0-windows` 试（大概率能编过，只是没验证过）。

---

## 五、许可证落地注意

你接受开源，所以按 GPL-3.0 走即可。两点实操提醒：

1. **C# 引用 GPL 程序集在法律上构成"组合作品"**，你的整个程序需以 GPL-3.0 发布。这正是你接受的前提，没问题。
2. **发布时记得带上 LICENSE 和完整源码**。另外 `research/SoundSwitch-dev/` 这个研究目录**不要提交进你的仓库**——它会让仓库体积暴涨且来源混乱，需要时让对方自己去 clone。

---

## 六、一句话总结

**SoundSwitch 更适合**（9/11 项胜出，决定性的是独立类库 + 已有窗口追踪 + 不用打包），**但最好的做法是既不 fork SoundSwitch 也不 fork EarTrumpet，而是引它的音频库、自己写外壳**——因为两者都只覆盖了我们 1/3 的需求，而我们最核心的"屏幕维度"两边都没有。
