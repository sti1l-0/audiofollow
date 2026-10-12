// ---------------------------------------------------------------------------
//  AudioFollow - 按应用改道音频输出的互操作层
//
//  本文件为独立实现（clean-room），不包含 SoundSwitch 的任何代码。
//  SoundSwitch 为 GPL-3.0，复制其代码会导致整个项目必须 GPL-3.0 开源；
//  本实现只使用其验证过的「接口事实与实现技巧」，这些不受版权保护。
//
//  关键技巧来源（均为接口事实）：
//    1. 用 AudioSes.dll!DllGetActivationFactory 取激活工厂（无需 WinRT 初始化）
//    2. 用 IInspectable::GetIids() 运行时发现 IID，不再硬编码版本分支
//    3. vtable 直接按槽位偏移取，Set=25 / Get=26 / ClearAll=27
//    4. 设备 ID 必须包成 \\?\SWD#MMDEVAPI#{id}#{设备接口类GUID}
//    5. PROCESS_NO_AUDIO(0x80070057) 表示「目标进程没有音频会话」，是静默 no-op
//
//  该接口为 Windows 未公开接口，微软未承诺兼容性。所有调用都必须容忍失败。
// ---------------------------------------------------------------------------

using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

namespace AudioFollow.Interop;

public enum EDataFlow
{
    eRender = 0,
    eCapture = 1,
    eAll = 2,
}

public enum ERole
{
    eConsole = 0,
    eMultimedia = 1,
    eCommunications = 2,
}

public static class HResult
{
    public const int S_OK = 0;

    /// <summary>
    /// E_INVALIDARG。此接口用它表示「目标进程当前没有音频会话」，
    /// 此时调用被忽略，且不会写入任何设置 —— 必须当作「稍后重试」，不是致命错误。
    /// </summary>
    public const int PROCESS_NO_AUDIO = unchecked((int)0x80070057);

    public static bool Failed(int hr) => hr < 0;
}

public enum RoutingResult
{
    /// <summary>写入成功。</summary>
    Applied,

    /// <summary>目标进程还没有音频会话，本次未写入。下次触发时应重试。</summary>
    NoAudioSession,

    /// <summary>当前系统不支持该接口（版本过低或接口被移除）。</summary>
    Unsupported,

    /// <summary>其它失败，详见日志。</summary>
    Failed,
}

/// <summary>
/// 按进程设置音频输出端点。实现应保证任何失败都不会抛异常穿透到调用方。
/// </summary>
public interface IAudioPolicyConfig : IDisposable
{
    RoutingResult SetProcessEndpoint(uint processId, EDataFlow flow, ERole role, string endpointId);

    /// <summary>读取当前生效的端点；返回 null 表示未设置（跟随系统默认）。</summary>
    string? GetProcessEndpoint(uint processId, EDataFlow flow, ERole role);

    /// <summary>清空本程序设置过的全部映射。慎用：会一并清掉用户的其它 per-app 设置。</summary>
    void ClearAll();
}

/// <summary>
/// 不支持该接口的系统上的降级实现：全部返回空操作，让上层无需到处判断版本。
/// </summary>
public sealed class UnsupportedAudioPolicyConfig : IAudioPolicyConfig
{
    private readonly string _reason;

    public UnsupportedAudioPolicyConfig(string reason) => _reason = reason;

    public string Reason => _reason;

    public RoutingResult SetProcessEndpoint(uint processId, EDataFlow flow, ERole role, string endpointId)
        => RoutingResult.Unsupported;

    public string? GetProcessEndpoint(uint processId, EDataFlow flow, ERole role) => null;

    public void ClearAll()
    {
    }

    public void Dispose()
    {
    }
}

public sealed class AudioPolicyConfig : IAudioPolicyConfig
{
    // ResourceReader 里的 Windows.Media.Internal.AudioPolicyConfig 类名
    private const string ActivatableClassId = "Windows.Media.Internal.AudioPolicyConfig";

    // 该接口只在这 3 个 IID 之一上出现过。运行时发现 IID 后再用它做白名单校验，
    // 这样微软改 IID 时我们能第一时间发现（而不是静默调用到错误的槽位而崩溃）。
    private static readonly HashSet<string> KnownIids = new(StringComparer.OrdinalIgnoreCase)
    {
        "2a59116d-6c4f-45e0-a74f-707e3fef9258", // Windows 10 (pre-21H2)
        "ab3d4648-e242-459f-b02f-541c70306324", // Windows 11 21H2+
        "32aa8e18-6496-4e24-9f94-b800e7eccc45", // Windows 10 16299
    };

    // vtable 槽位（指针索引）
    private const int SlotGetIids = 3;   // IInspectable::GetIids
    private const int SlotSetEndpoint = 25;
    private const int SlotGetEndpoint = 26;
    private const int SlotClearAll = 27;

    private const int GuidSize = 16;

    // SWD 设备接口路径拼装
    private const string SwdPrefix = @"\\?\SWD#MMDEVAPI#";
    private const string DevInterfaceRender = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
    private const string DevInterfaceCapture = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";

    // 低于此 build（Windows 10 1803）接口不可靠
    private const int MinSupportedBuild = 17134;

    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int GetIidsFn(IntPtr self, out int count, out IntPtr iids);

    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int QueryInterfaceFn(IntPtr self, IntPtr iid, out IntPtr obj);

    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int SetEndpointFn(IntPtr self, uint processId, EDataFlow flow, ERole role, IntPtr deviceIdHString);

    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int GetEndpointFn(IntPtr self, uint processId, EDataFlow flow, ERole role, out IntPtr deviceIdHString);

    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    private delegate int ClearAllFn(IntPtr self);

    [DllImport("AudioSes.dll", ExactSpelling = true, PreserveSig = false)]
    private static extern void DllGetActivationFactory(IntPtr activatableClassId, out IntPtr factory);

    [DllImport("api-ms-win-core-winrt-string-l1-1-0.dll", ExactSpelling = true)]
    private static extern int WindowsCreateString([MarshalAs(UnmanagedType.LPWStr)] string source, int length, out IntPtr hstring);

    [DllImport("api-ms-win-core-winrt-string-l1-1-0.dll", ExactSpelling = true)]
    private static extern int WindowsDeleteString(IntPtr hstring);

    [DllImport("api-ms-win-core-winrt-string-l1-1-0.dll", ExactSpelling = true)]
    private static extern IntPtr WindowsGetStringRawBuffer(IntPtr hstring, out int length);

    [DllImport("ole32.dll", ExactSpelling = true)]
    private static extern void CoTaskMemFree(IntPtr ptr);

    private readonly IntPtr _obj;
    private readonly int _ptrSize = IntPtr.Size;
    private bool _disposed;

    private AudioPolicyConfig(IntPtr obj) => _obj = obj;

    /// <summary>当前系统是否可能支持该接口（快速门控，真正判定仍以创建结果为准）。</summary>
    public static bool IsSupportedOs()
    {
        var v = GetRealOsVersion();
        return v.Major >= 10 && v.Build >= MinSupportedBuild;
    }

    /// <summary>
    /// 尝试创建。失败时返回 false 并给出原因，调用方应改用 <see cref="UnsupportedAudioPolicyConfig"/>。
    /// </summary>
    public static bool TryCreate(out IAudioPolicyConfig config, out string? error)
    {
        config = new UnsupportedAudioPolicyConfig("not initialized");
        error = null;

        if (!IsSupportedOs())
        {
            var v = GetRealOsVersion();
            error = $"系统版本过低（{v.Major}.{v.Minor}.{v.Build} < {MinSupportedBuild}）";
            config = new UnsupportedAudioPolicyConfig(error);
            return false;
        }

        IntPtr classIdHString = IntPtr.Zero;
        IntPtr factory = IntPtr.Zero;
        try
        {
            int hr = WindowsCreateString(ActivatableClassId, ActivatableClassId.Length, out classIdHString);
            if (HResult.Failed(hr))
            {
                error = $"WindowsCreateString 失败: 0x{hr:X8}";
                config = new UnsupportedAudioPolicyConfig(error);
                return false;
            }

            DllGetActivationFactory(classIdHString, out factory);
            if (factory == IntPtr.Zero)
            {
                error = "DllGetActivationFactory 返回空指针";
                config = new UnsupportedAudioPolicyConfig(error);
                return false;
            }

            var obj = QueryAudioPolicyConfigInterface(factory, out error);
            if (obj == IntPtr.Zero)
            {
                config = new UnsupportedAudioPolicyConfig(error ?? "QueryInterface 失败");
                return false;
            }

            config = new AudioPolicyConfig(obj);
            return true;
        }
        catch (Exception ex)
        {
            error = ex.Message;
            config = new UnsupportedAudioPolicyConfig(ex.Message);
            return false;
        }
        finally
        {
            if (factory != IntPtr.Zero) Marshal.Release(factory);
            if (classIdHString != IntPtr.Zero) WindowsDeleteString(classIdHString);
        }
    }

    /// <summary>
    /// 运行时发现正确的 IID 并 QI 到目标接口。
    /// 不硬编码 IID 是这套实现最关键的稳健性来源：微软在 Win11 21H2 换过 IID，
    /// 用 GetIids 枚举后取最后一个即可自动适配。
    /// </summary>
    private static IntPtr QueryAudioPolicyConfigInterface(IntPtr factory, out string? error)
    {
        error = null;

        var vtbl = Marshal.ReadIntPtr(factory);
        var getIidsPtr = Marshal.ReadIntPtr(vtbl, SlotGetIids * IntPtr.Size);
        var getIids = Marshal.GetDelegateForFunctionPointer<GetIidsFn>(getIidsPtr);

        int hr = getIids(factory, out int count, out IntPtr iids);
        if (HResult.Failed(hr) || count <= 0 || iids == IntPtr.Zero)
        {
            error = $"GetIids 失败: hr=0x{hr:X8} count={count}";
            return IntPtr.Zero;
        }

        try
        {
            var iidPtr = iids + GuidSize * (count - 1);
            var iid = Marshal.PtrToStructure<Guid>(iidPtr);
            var iidKey = iid.ToString("D").ToLowerInvariant();

            if (!KnownIids.Contains(iidKey))
            {
                // 微软换了 IID。不是崩溃，而是明确报出来，便于快速跟进。
                error = $"未识别的 AudioPolicyConfig IID: {iidKey}（可能是系统更新改动了接口）";
                return IntPtr.Zero;
            }

            var qiPtr = Marshal.ReadIntPtr(vtbl, 0); // QueryInterface 固定在第 0 槽
            var qi = Marshal.GetDelegateForFunctionPointer<QueryInterfaceFn>(qiPtr);

            hr = qi(factory, iidPtr, out var obj);
            if (HResult.Failed(hr) || obj == IntPtr.Zero)
            {
                error = $"QueryInterface({iidKey}) 失败: 0x{hr:X8}";
                return IntPtr.Zero;
            }

            return obj;
        }
        finally
        {
            CoTaskMemFree(iids);
        }
    }

    public RoutingResult SetProcessEndpoint(uint processId, EDataFlow flow, ERole role, string endpointId)
    {
        if (_disposed || string.IsNullOrEmpty(endpointId)) return RoutingResult.Failed;

        var fn = GetSlot<SetEndpointFn>(SlotSetEndpoint);
        var swdPath = ToSwdPath(endpointId, flow);

        int hr = WindowsCreateString(swdPath, swdPath.Length, out var deviceIdHString);
        if (HResult.Failed(hr)) return RoutingResult.Failed;

        try
        {
            hr = fn(_obj, processId, flow, role, deviceIdHString);
            if (hr == HResult.S_OK) return RoutingResult.Applied;
            if (hr == HResult.PROCESS_NO_AUDIO) return RoutingResult.NoAudioSession;
            return RoutingResult.Failed;
        }
        finally
        {
            WindowsDeleteString(deviceIdHString);
        }
    }

    public string? GetProcessEndpoint(uint processId, EDataFlow flow, ERole role)
    {
        if (_disposed) return null;

        var fn = GetSlot<GetEndpointFn>(SlotGetEndpoint);

        int hr = fn(_obj, processId, flow, role, out var hstring);
        // 未设置过时这个接口可能返回 S_OK + 空，也可能返回错误，两种都按「无映射」处理。
        if (HResult.Failed(hr) || hstring == IntPtr.Zero) return null;

        try
        {
            var buf = WindowsGetStringRawBuffer(hstring, out int length);
            if (buf == IntPtr.Zero || length <= 0) return null;
            return FromSwdPath(Marshal.PtrToStringUni(buf, length));
        }
        finally
        {
            WindowsDeleteString(hstring);
        }
    }

    public void ClearAll()
    {
        if (_disposed) return;
        var fn = GetSlot<ClearAllFn>(SlotClearAll);
        fn(_obj);
    }

    private T GetSlot<T>(int slot) where T : Delegate
    {
        var vtbl = Marshal.ReadIntPtr(_obj);
        var fnPtr = Marshal.ReadIntPtr(vtbl, slot * _ptrSize);
        return Marshal.GetDelegateForFunctionPointer<T>(fnPtr);
    }

    /// <summary>
    /// 原始端点 ID（IMMDevice::GetId() 的返回值，形如 {0.0.0.00000000}.{GUID}）
    /// 必须包成 SWD 设备接口路径，否则写入会失败。
    /// </summary>
    private static string ToSwdPath(string endpointId, EDataFlow flow)
    {
        if (endpointId.StartsWith(SwdPrefix, StringComparison.OrdinalIgnoreCase)) return endpointId;
        var suffix = flow == EDataFlow.eCapture ? DevInterfaceCapture : DevInterfaceRender;
        return SwdPrefix + endpointId + suffix;
    }

    /// <summary>读回的路径同样是 SWD 形式，需要剥回原始端点 ID 才能与本机枚举结果比对。</summary>
    private static string FromSwdPath(string swdPath)
    {
        var s = swdPath;
        if (s.StartsWith(SwdPrefix, StringComparison.OrdinalIgnoreCase))
            s = s.Substring(SwdPrefix.Length);
        if (s.EndsWith(DevInterfaceRender, StringComparison.Ordinal))
            s = s.Substring(0, s.Length - DevInterfaceRender.Length);
        if (s.EndsWith(DevInterfaceCapture, StringComparison.Ordinal))
            s = s.Substring(0, s.Length - DevInterfaceCapture.Length);
        return s;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        if (_obj != IntPtr.Zero) Marshal.Release(_obj);
    }

    // --- OS 版本 ---
    // 注意：无 manifest 的进程里 Environment.OSVersion 会撒谎（报 6.2），
    // 所以这里直接问 ntdll。

    [StructLayout(LayoutKind.Sequential)]
    private struct OsVersionInfo
    {
        public int Size;
        public int Major;
        public int Minor;
        public int Build;
        public int PlatformId;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string CsdVersion;
        public ushort ServicePackMajor;
        public ushort ServicePackMinor;
        public ushort SuiteMask;
        public byte ProductType;
        public byte Reserved;
    }

    [DllImport("ntdll.dll", ExactSpelling = true)]
    private static extern int RtlGetVersion(ref OsVersionInfo versionInfo);

    private static (int Major, int Minor, int Build) GetRealOsVersion()
    {
        var info = new OsVersionInfo();
        info.Size = Marshal.SizeOf<OsVersionInfo>();
        info.CsdVersion = string.Empty;
        try
        {
            if (RtlGetVersion(ref info) == 0)
                return (info.Major, info.Minor, info.Build);
        }
        catch
        {
            // 忽略，退回首选方案
        }

        var v = Environment.OSVersion.Version;
        return (v.Major, v.Minor, v.Build);
    }
}

/// <summary>
/// 上层封装：对单个进程设置全部 3 个角色，并做「写前读回差分」。
/// </summary>
public sealed class ProcessRouter
{
    private static readonly ERole[] AllRoles = { ERole.eConsole, ERole.eMultimedia, ERole.eCommunications };

    private readonly IAudioPolicyConfig _policy;

    public ProcessRouter(IAudioPolicyConfig policy) => _policy = policy;

    /// <summary>
    /// 把指定进程的全部音频角色指向 endpointId。
    /// 返回 true 表示确实写入了；false 表示已一致或写入失败（详见 out 参数）。
    /// </summary>
    public bool Route(uint processId, string endpointId, out RoutingResult result)
    {
        // 差分：三个角色都已经指向目标设备时直接跳过，避免重复写入打断音频流、造成爆音。
        var alreadyMatch = true;
        foreach (var role in AllRoles)
        {
            var current = _policy.GetProcessEndpoint(processId, EDataFlow.eRender, role);
            if (current == null || !string.Equals(current, endpointId, StringComparison.OrdinalIgnoreCase))
            {
                alreadyMatch = false;
                break;
            }
        }

        if (alreadyMatch)
        {
            result = RoutingResult.Applied;
            return false;
        }

        var anyApplied = false;
        var best = RoutingResult.Failed;
        foreach (var role in AllRoles)
        {
            var r = _policy.SetProcessEndpoint(processId, EDataFlow.eRender, role, endpointId);
            if (r == RoutingResult.Applied) anyApplied = true;
            // 保留「最有信息量」的结果：NoAudioSession 比 Failed 更值得上层知道
            if (r == RoutingResult.NoAudioSession || (best == RoutingResult.Failed && r != RoutingResult.Failed))
                best = r;
        }

        result = anyApplied ? RoutingResult.Applied : best;
        return anyApplied;
    }
}
