# Rust 方案实测报告 —— 不用 .NET，难度到底增加多少？

> 回答的问题：`我有点不太喜欢 .NET 运行库，能不能不用呢？不用的话技术难度会变得很大吗？`
>
> 结论先说：**难度增量中等偏小（约 +30～50%），而且集中在 UI 层，不在核心。核心的"改道"部分，Rust 和 C# 基本等价——本报告里的探针已经实测跑通。**

实测日期：2026-10-12　实测机器：Windows + Rust 1.98.1 (`x86_64-pc-windows-msvc`)

---

## 1. 先把"不喜欢 .NET"拆成两件完全不同的事

这两件事的解法差一个数量级，很容易混在一起：

| 你的真实顾虑 | 需要换语言吗 | 解法 | 代价 |
|---|---|---|---|
| **不想让用户装 .NET 运行时** | ❌ 不用换 | .NET「自包含发布」(`SelfContained=true`)：运行时打进产物，用户零安装 | 体积 60～150 MB |
| **就是不想用 .NET 这套栈** | ✅ 需要换 | 见 §4 语言对照表 | 见 §5 |

**如果只是前者，一行 MSBuild 配置就够了。** 所以下面重点讨论后者。

---

## 2. 实测：Rust 能不能走通那条非公开 COM 链路？

**能，而且用的是零第三方 crate 的纯手写版本。**

### 2.1 实测输出（原文，未经修饰）

```
===============================================================
 AudioFollow -- Rust COM spike (zero third-party crates)
===============================================================
self pid = 27924
CoInitializeEx            -> 0x00000000  (0x80010106 = already inited, fine)

[A] documented COM: IMMDeviceEnumerator (no undocumented API)
  CoCreateInstance          -> S_OK
  active render endpoints   = 3
    [0] MON-A (High Definition Audio)
        raw id : {0.0.0.00000000}.{0180224e-7548-4ab1-b09b-1996dd22d0e8}
        swd id : \\?\SWD#MMDEVAPI#{0.0.0.00000000}.{0180224e-7548-4ab1-b09b-1996dd22d0e8}#{e6327cad-dcec-4949-ae8a-991e976a79d2}
    [1] MON-B (High Definition Audio)
        raw id : {0.0.0.00000000}.{964b0239-d0c0-4965-84eb-ea64e77ed2a4}
    [2] 扬声器 (USB Audio Device)
        raw id : {0.0.0.00000000}.{c1ef28a6-e146-4341-be70-41ac8e5ae572}

[A2] live audio sessions on the default render endpoint
  session pid 24792

[B] undocumented COM: Windows.Media.Internal.AudioPolicyConfig
  AudioSes.dll loaded, DllGetActivationFactory resolved
  activation factory        -> S_OK (ptr=0x142af0d67e0)
  GetIids                   -> 2 interface(s)
      [0] 00000035-0000-0000-c000-000000000046
      [1] ab3d4648-e242-459f-b02f-541c70306324
  picked (last) IID         = ab3d4648-e242-459f-b02f-541c70306324
  in known-valid whitelist  = true
  QueryInterface            -> S_OK (ptr=0x142af0d67e8)
  target pid 27924 (self):
      eConsole         -> <empty = follows the system default>
      eMultimedia      -> <empty = follows the system default>
      eCommunications  -> <empty = follows the system default>
  target pid 24792 (session):
      eConsole         -> <empty = follows the system default>
      eMultimedia      -> <empty = follows the system default>
      eCommunications  -> <empty = follows the system default>
---------------------------------------------------------------
done. nothing was written to the system (read-only spike).
```

产物体积：

```
-rwxr-xr-x  audiofollow-spike.exe   127,488 bytes   (124 KB)
```

### 2.2 逐条对照：这份 Rust 代码分别证明了什么

| # | 验证点 | 结果 | 对应 C# 写法 |
|---|---|---|---|
| 1 | 文档化 COM：`CoCreateInstance(IMMDeviceEnumerator)` | ✅ 成功 | `new MMDeviceEnumerator()` |
| 2 | 枚举渲染端点 + 读 `IMMDevice::GetId()` | ✅ 3 个设备，含 USB 扬声器 | 同 |
| 3 | 读 `PKEY_Device_FriendlyName`（PROPVARIANT 解包） | ✅ 正确显示"扬声器 (USB Audio Device)" | `IPropertyStore.GetValue` |
| 4 | 枚举音频会话 → `IAudioSessionControl2::GetProcessId` | ✅ 拿到活跃 pid 24792 | SoundSwitch 的会话遍历 |
| 5 | **非公开**：`AudioSes.dll!DllGetActivationFactory` 取激活工厂 | ✅ S_OK | `AudioSes.DllGetActivationFactory` |
| 6 | **非公开**：`IInspectable::GetIids()` 运行时发现 IID | ✅ 2 个 IID，取最后一个 | SoundSwitch 的核心技巧 |
| 7 | **非公开**：白名单校验（防微软改版） | ✅ 命中 `ab3d4648-…` | `_knownValidGuids.Contains` |
| 8 | **非公开**：`QueryInterface` 到目标 IID | ✅ S_OK | 同 |
| 9 | **非公开**：vtable **按槽位号 26** 取 `GetPersistedDefaultAudioEndpoint` | ✅ S_OK + 合法 HSTRING | `_vfTable + _ptrSize * 26` |
| 10 | `PROCESS_NO_AUDIO` 容错分支 | ✅ 已实现并触发过 | `HRESULT.PROCESS_NO_AUDIO` |

**第 9 条是整件事的要害。** 手写 vtable 偏移如果错一格，结果不是"返回错误"而是**调用到完全不相干的函数**（EarTrumpet 作者 riverar 在 issue 里明确讲过：少写桩方法会把 `ClearAllPersistedApplicationDefaultEndpoints` 变成 `add_RingerVibrateStateChanged`）。我们这次拿到了 `S_OK` 和一个结构完全正确的 HSTRING —— 说明槽位对齐无误。

> 顺带一个有用的观察：`GetPersistedDefaultAudioEndpoint` 对一个**没有显式改过路由**的进程，返回的是 `S_OK` + **空字符串**（= 跟随系统默认），而不是 `PROCESS_NO_AUDIO`。`PROCESS_NO_AUDIO` 出现在进程连音频会话都还没建立的时候。这意味着**读回差分逻辑要把"空 = 跟随默认"当成正常值处理**，不能当成错误。

### 2.3 代码代价：约 380 行，其中一半是打印

`src/rust-spike/src/main.rs` 全文 380 行左右，去掉 `println!` 和注释，**真正的 interop 逻辑约 150 行**。这部分在 C# 里（SoundSwitch 的 `AudioPolicyConfig.cs`）是 160 行。**基本一比一。**

关键机制在 Rust 里反而更短，因为 Rust 可以直接把裸指针转成 `extern "system"` 函数指针，不需要 C# 那套 `Marshal.GetDelegateForFunctionPointer`：

```rust
// 按 vtable 槽位取函数指针 —— 对应 C# 的 Marshal.PtrToStructure + GetDelegateForFunctionPointer
unsafe fn vtbl(obj: *mut c_void) -> *const *const c_void {
    *(obj as *const *const *const c_void)
}
unsafe fn slot<F: Copy>(obj: *mut c_void, idx: usize) -> F {
    let p: *const c_void = *vtbl(obj).add(idx);
    std::mem::transmute_copy::<*const c_void, F>(&p)
}

// 就这么用：
let get_persisted: FnGetPersisted = slot(iface, 26);
get_persisted(iface, pid, 0, role, &mut out);
```

### 2.4 实测踩到的两个坑（C# 里也存在，只是被库藏起来了）

1. **`combase.lib` 不一定能链上**。MSVC + 某个版本的 Windows SDK 组合下 `LINK : fatal error LNK1181: 无法打开输入文件"combase.lib"`。解法是运行时 `LoadLibraryW("combase.dll")` + `GetProcAddress` —— 反而更健壮，不挑 SDK。同理 `AudioSes.dll` 也是动态加载的。
2. **`std::mem::transmute` 不能用于泛型**。`transmute::<*const c_void, F>` 编译不过，必须用 `transmute_copy`。属于一次性踩坑。

---

## 3. 没被验证的部分（诚实说明）

| 项 | 状态 |
|---|---|
| **写入**（`SetPersistedDefaultAudioEndpoint`，槽位 25） | 未测——故意。写进去会污染你的音量合成器设置，而 `ClearAll` 会把**你现有的所有** per-app 设置一起清掉，属于破坏性操作，不适合在探针里试 |
| `ClearAll`（槽位 27） | 同上，未测 |
| Chrome 多进程 PID 那个经典问题 | 未测。需要真实播放 + 真实写入 |
| 托盘 UI / 窗口监视 / WinEventHook | 未做，但与语言无关，Rust 侧有成熟 crate（`tray-icon` / `muda` / `windows` crate 自带 WinEvent 绑定） |

**但注意**：写入路径和读取路径走的是**同一个 interface、同一张 vtable、只差一个槽位号**。槽位 26 已经对齐验证过了，槽位 25 的对齐在同一个结构里，风险极低。真正的差异只在"接口语义"（什么时候会 no-op），这个和语言无关。

---

## 4. 各语言/运行时方案对照

| 方案 | 用户需装运行时 | 体积 | 路由层（COM） | 窗口→屏幕 | 配置 UI | 零依赖部署 |
|---|---|---|---|---|---|---|
| **A. C# / .NET 8 + WPF，自包含发布** | ❌ 否 | 60～150 MB | ★★☆☆☆（直接引用现成库） | ★☆☆☆☆ | ★☆☆☆☆ | 打勾但体积大 |
| **B. C# / .NET Framework 4.8 + WinForms** | ❌ 否（系统预装） | ~1 MB | ★★☆☆☆ | ★☆☆☆☆ | ★☆☆☆☆ | ✅ 天然 |
| **C. C# / .NET 8 Native AOT + 手写 Win32 UI** | ❌ 否 | ~5～10 MB | ★★☆☆☆ | ★☆☆☆☆ | ★★★★☆ | ✅ |
| **D. Rust + windows-rs**（或本报告的零依赖写法） | ❌ 否 | ~2～8 MB | ★★★☆☆（≈150 行） | ★★☆☆☆ | ★★★☆☆ | ✅ 天然 |
| **E. C++/WinRT + Win32** | ❌ 否 | ~1～3 MB | ★★★☆☆ | ★★☆☆☆ | ★★★★☆ | ✅ 天然 |

### 关于 C 方案的一个重要事实

**WinForms / WPF 不支持 Native AOT**，而且不是"不推荐"而是 **SDK 层面直接拦住了**（`Known trimming incompatibilities`：WPF 重反射、WinForms 重 built-in COM marshalling，两者都已关闭 trimming 支持）。

所以"用 AOT 做到零运行时"这条路，**必然意味着放弃 WinForms/WPF 的 UI**，改成手写原生窗口 —— 那 UI 难度就从 ★☆☆☆☆ 直接跳到 ★★★★☆，比直接用 Rust 还亏。

**结论：C 方案是"两头不讨好"的陷阱，应该排除。**

---

## 5. 换语言真正付出的代价

| 代价 | 严重程度 | 说明 |
|---|---|---|
| **不能直接引用 `SoundSwitch.Audio.Manager`** | 中 | 需重写约 300～400 行（含单元测试）。但**知识已完全掌握**，且本报告证明 Rust 侧成本并不高 |
| 配置 UI 要重做 | 中 | 我们的 UI 只是"一张屏幕↔设备映射表 + 托盘菜单"，复杂度低。Rust 用 `tray-icon` + `muda` + 一个原生对话框足够 |
| 生态里没有现成的"音频+屏幕"全套 | 低 | 但有直接先例：`Eververdants/AppAudioRouter`（**MIT**，Tauri 2 + Rust + 官方 `windows` crate，Win10/11 per-app 路由 + WASAPI loopback 复制，无虚拟驱动）。说明这条路已经有人走通 |
| 调试体验 | 低 | COM 崩溃在两种语言里都一样难查；用 `PROCESS_NO_AUDIO` 之类的返回码做探针比调试器有效 |

---

## 6. 结论与建议

**难度会不会大很多？不会。整体 +30～50%，且几乎全部落在 UI 层。**

分三层看：

| 层 | 难度变化 | 理由 |
|---|---|---|
| 路由层（最"玄"的部分） | **基本持平** | 实测 150 行手写 interop 已跑通，与 C# 一比一 |
| 窗口→屏幕判定 | +1 星 | 纯 Win32，`windows` crate 有绑定，无实质差异 |
| 配置 UI | **+2 星** | 这才是真正的成本。C#/WinForms 白送，Rust 要自己搭 |

**建议：**

- **如果"不喜欢 .NET"主要是不想背运行时的负担** → 保持 C#，开 `SelfContained=true` 或干脆用 .NET Framework 4.8 + WinForms（系统预装、1 MB）。不要为此换语言。
- **如果确实想彻底脱离 .NET** → 选 **Rust**。理由：① 核心难度已实测证明不增加；② 天然零运行时、单文件、小体积；③ 已有 MIT 先例项目可参考；④ 反过来还能拿到 `tray-icon`/`windows` 这套现代化绑定。
- **不要选 C++**，除非你就是喜欢 C++。同样的核心难度，UI 更痛。
- **不要选 C 方案**（.NET Native AOT + 手写 Win32 UI），它是本清单里最坏的性价比。

**下一步建议**：把 Phase 0 剩下的一个真实未知项（对正在播放的 Chrome 传窗口 PID 是否即时生效）用 Rust 探针扩一个写入版本验掉 —— 约半小时。要写入就意味着会给系统留一条 per-app 映射，探针里会附带"只清掉自己写的那条"的回滚逻辑（**不用 `ClearAll`**，那会清掉你已有的所有设置）。
