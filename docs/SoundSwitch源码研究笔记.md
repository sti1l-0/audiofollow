# SoundSwitch 源码研究笔记

> 研究对象：`https://github.com/Belphemur/SoundSwitch`（默认分支 `dev`，2026-10 快照，约 1075 个文件）
> 目的：确认"按应用改道音频"的可靠实现方式，评估能否复用其代码
> 结论速览：**技术可完全借鉴；代码不可直接复制（GPL-3.0）**

---

## 一、许可证判定（先看这个）

```
LICENSE.txt → GNU GENERAL PUBLIC LICENSE Version 3, 29 June 2007
```

| 项 | 结论 |
|---|---|
| 许可证 | **GPL-3.0**（强传染 / copyleft） |
| 直接复制代码 | ❌ 会导致你的整个项目必须以 GPL-3.0 开源 |
| 借鉴思路与接口事实 | ✅ 不受版权保护，完全可用 |
| 对照参考 | EarTrumpet = **MIT**（含 3 家排除实体，与个人项目无关），是唯一可放心复制代码的成熟参考 |

**取舍**：SoundSwitch 的 interop 层总共约 200 行，自己重写成本约 1～2 小时，却能保留自由授权。**建议自己写**。

---

## 二、关键源码位置索引

| 功能 | 文件 |
|---|---|
| COM 接口调用核心（最值得读） | `SoundSwitch.Audio.Manager/Interop/Client/Extended/AudioPolicyConfig.cs` |
| 工厂创建 + OS 版本门控 | `SoundSwitch.Audio.Manager/Interop/Client/Extended/Factory/AudioPolicyConfigFactory.cs` |
| 降级实现 | `SoundSwitch.Audio.Manager/Interop/Client/Extended/UnsupportedAudioPolicyConfig.cs` |
| 设备 ID 拼装 + 上层封装 | `SoundSwitch.Audio.Manager/Interop/Client/ExtendedPolicyClient.cs` |
| 路由公开 API + 差分判断 | `SoundSwitch.Audio.Manager/AudioSwitcher.cs`（`SwitchProcessTo` / `SwitchForegroundProcessTo`） |
| 前台窗口监视 | `SoundSwitch.Audio.Manager/WindowMonitor.cs` |
| **新进程轮询** | `SoundSwitch.Audio.Manager/ProcessMonitor.cs` |
| 应用规则引擎 | `SoundSwitch/Services/AppSoundLockManager.cs` |
| 仅定义用到的 IInspectable | `Interop/Interface/Policy/Extended/IInspectableSlim.cs` |
| HRESULT 常量 | `Interop/Enum/HRESULT.cs` |

---

## 三、五个关键技术细节（v1 方案全部写错）

### 1. 激活工厂：用 `AudioSes.dll` 的 `DllGetActivationFactory`，不是 `RoGetActivationFactory`

```csharp
// SoundSwitch.Audio.Manager/Interop/Com/Base/AudioSes.cs
[DllImport("AudioSes.dll", PreserveSig = false)]
public static extern void DllGetActivationFactory([In] HSTRING iid, [Out] out IntPtr factory);
```

调用：`AudioSes.DllGetActivationFactory(HSTRING.FromString("Windows.Media.Internal.AudioPolicyConfig"), out _factory)`

**价值**：不需要初始化 WinRT 运行时，普通桌面进程直接可用，更少的依赖和失败点。

### 2. IID 运行时发现，不硬编码

```csharp
var iinspectable = (IInspectableSlim)Marshal.GetObjectForIUnknown(_factory);
var result = iinspectable.GetIids(count, ref iids);   // IInspectable 第 3 槽
var totalGuids = Marshal.ReadInt32(count);
var audioPolicyConfigGuidPtr = iids + GUID_SIZE * (totalGuids - 1);   // 取最后一个
var audioPolicyConfigGuid = GuidPtrToString(audioPolicyConfigGuidPtr);
if (!_knownValidGuids.Contains(audioPolicyConfigGuid))
    throw new InvalidComObjectException($"Unknown AudioPolicyConfig GUID: {audioPolicyConfigGuid}");
// 再用这个 GUID 做 QueryInterface
```

白名单里的 3 个已知 GUID（对应不同 Windows 版本）：

| GUID | 对应系统 |
|---|---|
| `2a59116d-6c4f-45e0-a74f-707e3fef9258` | Windows 10（pre-21H2） |
| `ab3d4648-e242-459f-b02f-541c70306324` | Windows 11 21H2+ |
| `32aa8e18-6496-4e24-9f94-b800e7eccc45` | Windows 10.0.16299 |

**价值**：一劳永逸地解决了"微软改 IID"的问题，也不需要靠 HRESULT 试错。**这是整个研究里最有价值的一条。**

> 注意：`2a59116d` 名字里虽写着 "Pre-H1H2"（笔误，应为 Pre-21H2），但从工厂门控 `Build <= 16299` 直接降级看，实际支持的起点是 Windows 10 1803。

### 3. vtable 直接按字节偏移取

```csharp
// 方法槽位（指针大小 × 索引）
var setPtr   = Marshal.PtrToStructure<IntPtr>(_vfTable + (_ptrSize * 25));  // Set
var getPtr   = Marshal.PtrToStructure<IntPtr>(_vfTable + (_ptrSize * 26));  // Get
var clearPtr = Marshal.PtrToStructure<IntPtr>(_vfTable + (_ptrSize * 27));  // ClearAll
```

**价值**：不用声明 0～24 号那一堆用不到的方法桩，代码从"必须完整对齐否则崩溃"变成"只取需要的槽位"。

### 4. 设备 ID 必须包成 SWD 设备接口路径

```csharp
private const string DEVINTERFACE_AUDIO_RENDER  = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
private const string DEVINTERFACE_AUDIO_CAPTURE = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";
private const string MMDEVAPI_TOKEN = @"\\?\SWD#MMDEVAPI#";

// 写入时拼装
deviceIdStr = MMDEVAPI_TOKEN + IMMDeviceId + (flow == eRender ? DEVINTERFACE_AUDIO_RENDER : DEVINTERFACE_AUDIO_CAPTURE);
// 读回时剥离
```

其中 `IMMDeviceId` 是 `IMMDevice::GetId()` 的原始返回，形如 `{0.0.0.00000000}.{GUID}`。

**最终形态**：
```
\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{GUID}#{e6327cad-dcec-4949-ae8a-991e976a79d2}
```

**注意**：`IMMDevice::GetId()` 返回的是**不带前缀的**原始值（见 `AudioDevice.cs:ReadId`），必须自己拼。这是最容易踩的坑——格式不对会静默失败。

### 5. `PROCESS_NO_AUDIO` 是静默失败，必须容错

```csharp
// Interop/Enum/HRESULT.cs
PROCESS_NO_AUDIO = 0x80070057     // 即 E_INVALIDARG

// AudioPolicyConfig.SetPersistedDefaultAudioEndpoint 内
if (result == HRESULT.PROCESS_NO_AUDIO)
{
    // PROCESS_NO_AUDIO (E_INVALIDARG) means the call was a no-op and the
    // endpoint was not applied. Treat that as a failure so callers don't
    // report a successful routing when nothing changed.
    return false;
}
```

**含义**：目标 PID **没有音频会话**时，接口返回 `E_INVALIDARG`，**且什么都不写**。

**这直接决定了"跳过音频检测"这个简化的代价**：不能只在窗口移动时设一次就完事，必须给它重试机会。

---

## 四、它到底有没有检测"谁在播声音"？—— 没有

这是本次研究最关键的结论，直接支持你的判断。

在 `SoundSwitch.Audio.Manager` 全库搜索音频会话 API 的使用：

```
AudioDevice.cs:241   Activate(iid, "IAudioSessionManager2")
AudioDevice.cs:246   manager.GetSessionEnumerator(out enumerator)
AudioDevice.cs:253   enumerator.GetCount
```

**只有一处，且用途是读取设备音量/会话信息，从未参与路由决策。**

`AppSoundLockManager`（应用声音锁定，功能上最接近我们的需求）的完整流程：

```
WindowMonitor.ForegroundChanged  ─┐
                                  ├─→ ApplyRulesToProcess(pid, name, path, title)
ProcessMonitor.ProcessesDetected ─┘         ↓
                                     IsMatch(rule, name, path, title)
                                            ↓
                                     SwitchProcessTo(device.Id, ERole_enum_count, flow, pid)
                                            ↓
                                     直接设路由，不看是否在播放
```

`SwitchProcessTo` 的内部逻辑（`AudioSwitcher.cs:118`）：

```csharp
var process = Process.GetProcessById((int)processId);   // 只确认进程存在
// roles = [eConsole, eCommunications, eMultimedia]
var allRolesAlreadyMatch = roles.All(eRole => {
    var current = ExtendPolicyClient.GetDefaultEndPoint(flow, eRole, processId);
    return !string.IsNullOrEmpty(current) && deviceId.Equals(current);
});
if (allRolesAlreadyMatch) return false;                  // 差分：已一致就不写
return ExtendPolicyClient.SetDefaultEndPoint(deviceId, flow, roles, processId);
```

**没有任何"是否在播放"的判断。** 所以你的简化方案成立。

---

## 五、真正值得照抄的是它的"双通道触发"

SoundSwitch 用**事件 + 轮询**两条通道，这是成熟设计：

| 通道 | 实现 | 解决的问题 |
|---|---|---|
| 事件 | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND)` | 前台切换即时响应 |
| **轮询** | `ProcessMonitor`，默认 **2 秒**一轮 | ① 应用重启后策略丢失；② 应用启动晚于窗口出现；③ 事件丢失兜底 |

`ProcessMonitor.PollProcesses()` 有两个精巧设计：

1. **延迟一轮上报**：新发现的 PID 先放进 `_pendingProcesses`，**下一轮**（2 秒后）才上报。给进程留出初始化时间——这同时也让"音频已初始化"的概率大大提高，间接缓解了 `PROCESS_NO_AUDIO` 问题。
2. **进程退出清理**：每轮用 `currentPids` 反向清理 `_processedProcessIds` / `_pendingProcesses`，避免集合无限增长。

```csharp
foreach (var process in Process.GetProcesses())
{
    var pid = process.Id;
    if (pid <= 4) continue;                        // 跳过系统进程
    currentPids.Add(pid);
    if (_processedProcessIds.Contains(pid)) continue;
    if (_pendingProcesses.Contains(pid)) toProcess.Add(pid);   // 延迟一轮
    else _pendingProcesses.Add(pid);
}
_processedProcessIds.RemoveWhere(pid => !currentPids.Contains(pid));
_pendingProcesses.RemoveWhere(pid => !currentPids.Contains(pid));
```

**我们要做的替换**：把"前台窗口变化"改成"窗口位置变化"，其余节奏照抄。

---

## 六、它没做的、我们要补的

| 能力 | SoundSwitch | 我们要补 |
|---|---|---|
| 触发条件 | 前台窗口切换 / 快捷键 | **窗口所在显示器** |
| 屏幕枚举与几何判定 | 无 | 需要（`EnumDisplayMonitors` + DWM 扩展边框） |
| 屏幕 ↔ 设备映射配置 | 无（规则是应用↔设备） | 需要 |
| 多窗口仲裁 | 无（只有前台一个窗口） | 需要（一个进程可能多个窗口跨屏） |
| DPI 感知 | 未处理 | 需要（`PER_MONITOR_AWARE_V2`） |

---

## 七、可直接复用的设计模式（不是代码）

1. **`IAudioPolicyConfig` 抽象 + `UnsupportedAudioPolicyConfig` 降级类**：不支持的环境返回 `false` / `null` / no-op，而不是抛异常。上层逻辑无需到处判断系统版本。
2. **能力探测前置**：工厂创建失败 → 降级实例；调用失败 → 返回 `false` 而非崩溃。
3. **写前读回差分**：避免重复写入打断音频流。
4. **全角色一次性设置**：`eConsole` + `eMultimedia` + `eCommunications` 三个都设，避免"通话音量走了旧设备"。
5. **COM 调用集中到专用线程**（`ComThread.Invoke`）：避免 STA/MTA 线程模型问题。
6. **进程存在性预校验**：`Process.GetProcessById` 先确认，避免对已退出进程空调用。

---

## 八、Phase 0 验证清单

拿到下面的代码骨架后，**半天内**应该能回答：

- [ ] 对正在播放的 Chrome 设端点，是否即时生效？（传窗口 PID）
- [ ] 对**没有播放**的应用（打开记事本）设端点，是否返回 `PROCESS_NO_AUDIO`？
- [ ] 对 Chrome 传 renderer 子进程 PID vs 窗口进程 PID，结果是否不同？
- [ ] 写入后，Windows 设置 → 音量合成器里是否出现该应用的输出设备下拉项？
- [ ] 是否真的不需要管理员权限？
- [ ] `GetPersistedDefaultAudioEndpoint` 读回的值是否与写入的 SWD 形式一致？
