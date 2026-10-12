# audiofollow-spike

可行性探针：**零第三方 crate** 的 Rust 实现，验证"不用 .NET"能不能走通 per-app 音频改道。

## 跑

```bash
cargo run --release
```

或直接跑已编译产物：`target/release/audiofollow-spike.exe`

## 它做什么

| 段 | 内容 | 用的是 |
|---|---|---|
| `[A]` | 枚举渲染端点，读设备 ID + 友好名 | 文档化 Core Audio COM |
| `[A2]` | 枚举默认端点上的活跃音频会话，拿到 PID | `IAudioSessionManager2` |
| `[B]` | 取非公开激活工厂 → 运行时发现 IID → QI → 读 per-app 持久化端点 | `AudioSes.dll!DllGetActivationFactory` |

**只读。** 不写任何音频端点设置。

## 关键实现点

- `vtbl()` / `slot()`：按槽位号从裸 vtable 取函数指针（对应 C# 的 `Marshal.GetDelegateForFunctionPointer`）
- 激活工厂：`LoadLibraryW("AudioSes.dll")` + `GetProcAddress("DllGetActivationFactory")`
- `combase.dll` 的函数也走运行时解析，避免 `LNK1181: 无法打开输入文件"combase.lib"`
- IID：`GetIids()` 取最后一个 + 白名单校验，不硬编码
- vtable 槽位：`25` = Set / `26` = Get / `27` = ClearAll
- `PROCESS_NO_AUDIO`（`0x80070057`）= 目标进程还没有音频会话，调用为空操作

## 结论

见上级目录《Rust方案实测报告.md》。
