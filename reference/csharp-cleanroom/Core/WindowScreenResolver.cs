// ---------------------------------------------------------------------------
//  AudioFollow - 窗口 / 进程 与 显示器 的归属判定
//
//  这是本项目相对 SoundSwitch 唯一需要自己补的部分：它只关心「前台窗口」，
//  我们需要「每个窗口落在哪块屏」。
//
//  三个必须处理的坑：
//    1. 必须声明 PER_MONITOR_AWARE_V2，否则多屏不同缩放时坐标是虚拟化的，判定会错
//    2. 用 DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) 而非 GetWindowRect
//       （后者含投影边框，跨屏判定会偏）
//    3. 一个进程可能有多个窗口分布在不同屏 —— 需要仲裁
// ---------------------------------------------------------------------------

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;

namespace AudioFollow.Core;

[StructLayout(LayoutKind.Sequential)]
public struct Rect
{
    public int Left;
    public int Top;
    public int Right;
    public int Bottom;

    public int Width => Right - Left;
    public int Height => Bottom - Top;
    public bool IsEmpty => Width <= 0 || Height <= 0;
}

/// <summary>一块显示器。</summary>
public readonly record struct MonitorTarget(IntPtr Handle, string DeviceName, Rect Bounds, Rect WorkArea, bool IsPrimary);

/// <summary>一个顶层窗口。</summary>
public sealed record WindowRef(IntPtr Hwnd, uint ProcessId, string ProcessName, string Title);

/// <summary>某个进程最终归属的显示器。</summary>
public sealed record ProcessPlacement(
    uint ProcessId,
    string ProcessName,
    IntPtr Monitor,
    string MonitorDeviceName,
    string Title,
    double Coverage,
    /// <summary>该进程是否在多个显示器上都有窗口，且没有一个明显占优。</summary>
    bool IsAmbiguous);

public static class WindowScreenResolver
{
    private const int DWMWA_EXTENDED_FRAME_BOUNDS = 9;
    private const int GWL_EXSTYLE = -20;
    private const long WS_EX_TOOLWINDOW = 0x00000080L;
    private const uint MONITORINFOF_PRIMARY = 1;

    // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
    private static readonly IntPtr DpiAwarenessPerMonitorV2 = new(-4);

    /// <summary>
    /// 必须在任何窗口/显示器 API 调用之前执行一次。
    /// 不声明的话，在 100% + 150% 混合缩放的多屏环境下，系统会把坐标虚拟化，
    /// 跨屏判定会得出错误结果。
    /// </summary>
    public static bool InitializePerMonitorDpiAwareness()
    {
        try
        {
            return SetProcessDpiAwarenessContext(DpiAwarenessPerMonitorV2);
        }
        catch (EntryPointNotFoundException)
        {
            // Win10 1607 之前没有这个 API，退一级
            try { return SetProcessDPIAware(); }
            catch { return false; }
        }
    }

    // --- 显示器枚举 ---

    public static IReadOnlyList<MonitorTarget> GetMonitors()
    {
        var list = new List<MonitorTarget>();

        MonitorEnumProc callback = (IntPtr hMonitor, IntPtr hdc, ref Rect rect, IntPtr data) =>
        {
            var info = new MonitorInfoEx();
            info.CbSize = Marshal.SizeOf<MonitorInfoEx>();
            if (GetMonitorInfo(hMonitor, ref info))
            {
                list.Add(new MonitorTarget(
                    hMonitor,
                    info.SzDevice ?? string.Empty,
                    info.RcMonitor,
                    info.RcWork,
                    (info.DwFlags & MONITORINFOF_PRIMARY) != 0));
            }

            return true;
        };

        // 保持委托引用存活到枚举结束，否则可能被 GC 回收导致回调崩溃
        GC.KeepAlive(callback);
        EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, callback, IntPtr.Zero);

        list.Sort((a, b) => a.IsPrimary ? -1 : b.IsPrimary ? 1 : a.Bounds.Left.CompareTo(b.Bounds.Left));
        return list;
    }

    // --- 窗口几何 ---

    /// <summary>取窗口的真实可见边界（排除投影边框）。最小化或无效窗口返回 false。</summary>
    public static bool TryGetWindowBounds(IntPtr hwnd, out Rect bounds)
    {
        bounds = default;

        if (hwnd == IntPtr.Zero || !IsWindow(hwnd) || IsIconic(hwnd)) return false;

        var size = Marshal.SizeOf<Rect>();
        if (DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, out bounds, size) != 0)
        {
            if (!GetWindowRect(hwnd, out bounds)) return false;
        }

        // 最小化窗口会返回 -32000 附近的值
        if (bounds.Left <= -30000 || bounds.Top <= -30000) return false;

        return !bounds.IsEmpty;
    }

    /// <summary>按面积交集找出窗口归属的显示器。返回 null 表示窗口在所有显示器之外。</summary>
    public static MonitorTarget? ResolveDominantMonitor(Rect window, IReadOnlyList<MonitorTarget> monitors, out double coverage)
    {
        coverage = 0;
        if (window.IsEmpty || monitors.Count == 0) return null;

        var windowArea = (double)window.Width * window.Height;
        MonitorTarget? best = null;
        long bestArea = 0;

        foreach (var m in monitors)
        {
            var area = IntersectionArea(window, m.Bounds);
            if (area > bestArea)
            {
                bestArea = area;
                best = m;
            }
        }

        if (best == null || bestArea == 0) return null;

        coverage = windowArea > 0 ? bestArea / windowArea : 0;
        return best;
    }

    private static long IntersectionArea(in Rect a, in Rect b)
    {
        var left = Math.Max(a.Left, b.Left);
        var top = Math.Max(a.Top, b.Top);
        var right = Math.Min(a.Right, b.Right);
        var bottom = Math.Min(a.Bottom, b.Bottom);

        if (right <= left || bottom <= top) return 0;
        return (long)(right - left) * (bottom - top);
    }

    // --- 窗口枚举 ---

    /// <summary>枚举所有有意义的顶层窗口（可见、有标题、非工具窗口）。</summary>
    public static IReadOnlyList<WindowRef> EnumerateWindows()
    {
        var result = new List<WindowRef>();

        EnumWindowsProc callback = (IntPtr hwnd, IntPtr lParam) =>
        {
            if (!IsWindowVisible(hwnd)) return true;
            if (IsIconic(hwnd)) return true;

            var exStyle = GetWindowLongPtr(hwnd, GWL_EXSTYLE).ToInt64();
            if ((exStyle & WS_EX_TOOLWINDOW) != 0) return true;

            var length = GetWindowTextLength(hwnd);
            if (length <= 0) return true;

            var title = GetWindowText(hwnd);
            if (string.IsNullOrWhiteSpace(title)) return true;

            GetWindowThreadProcessId(hwnd, out var pid);
            if (pid == 0) return true;

            string name;
            try { using var p = Process.GetProcessById((int)pid); name = p.ProcessName; }
            catch { name = "unknown"; }

            result.Add(new WindowRef(hwnd, pid, name, title));
            return true;
        };

        GC.KeepAlive(callback);
        EnumWindows(callback, IntPtr.Zero);
        return result;
    }

    /// <summary>
    /// 把窗口归属汇总到进程级别：一个进程只应有一个目标显示器，因为音频路由接口是
    /// 按「应用身份」生效的，做不到「同一应用的窗口 A 走扬声器 A、窗口 B 走扬声器 B」。
    ///
    /// 仲裁规则：取面积占比最大的窗口所属显示器；若不同显示器间的占比差距小于
    /// <paramref name="ambiguityMargin"/>，标记为 IsAmbiguous，调用方应保持原路由不动，
    /// 避免窗口拖动过程中来回横跳。
    /// </summary>
    public static IReadOnlyDictionary<uint, ProcessPlacement> GetProcessMonitorMap(
        IReadOnlyList<MonitorTarget> monitors,
        double ambiguityMargin = 0.2)
    {
        var byProcess = new Dictionary<uint, ProcessPlacement>();
        var perProcessWindows = new Dictionary<uint, List<(string Title, MonitorTarget Monitor, double Coverage)>>();

        foreach (var w in EnumerateWindows())
        {
            if (!TryGetWindowBounds(w.Hwnd, out var bounds)) continue;

            var monitor = ResolveDominantMonitor(bounds, monitors, out var coverage);
            if (monitor == null) continue;

            if (!perProcessWindows.TryGetValue(w.ProcessId, out var list))
                perProcessWindows[w.ProcessId] = list = new List<(string, MonitorTarget, double)>();

            list.Add((w.Title, monitor.Value, coverage));
        }

        foreach (var (pid, windows) in perProcessWindows)
        {
            windows.Sort((a, b) => b.Coverage.CompareTo(a.Coverage));
            var top = windows[0];

            var ambiguous = false;
            foreach (var other in windows)
            {
                if (other.Monitor.Handle != top.Monitor.Handle && top.Coverage - other.Coverage < ambiguityMargin)
                {
                    ambiguous = true;
                    break;
                }
            }

            byProcess[pid] = new ProcessPlacement(
                pid,
                windows.Count > 0 ? GetProcessNameSafe(pid) : "unknown",
                top.Monitor.Handle,
                top.Monitor.DeviceName,
                top.Title,
                top.Coverage,
                ambiguous);
        }

        return byProcess;
    }

    private static string GetProcessNameSafe(uint pid)
    {
        try { using var p = Process.GetProcessById((int)pid); return p.ProcessName; }
        catch { return "unknown"; }
    }

    // --- P/Invoke ---

    private delegate bool MonitorEnumProc(IntPtr hMonitor, IntPtr hdc, ref Rect rect, IntPtr data);

    private delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct MonitorInfoEx
    {
        public int CbSize;
        public Rect RcMonitor;
        public Rect RcWork;
        public uint DwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string SzDevice;
    }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SetProcessDpiAwarenessContext(IntPtr value);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SetProcessDPIAware();

    [DllImport("user32.dll")]
    private static extern bool EnumDisplayMonitors(IntPtr hdc, IntPtr clip, MonitorEnumProc proc, IntPtr data);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern bool GetMonitorInfo(IntPtr hMonitor, ref MonitorInfoEx info);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsProc proc, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hwnd);

    [DllImport("user32.dll")]
    private static extern bool IsIconic(IntPtr hwnd);

    [DllImport("user32.dll")]
    private static extern bool IsWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    private static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowTextLength(IntPtr hwnd);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int maxCount);

    private static string GetWindowText(IntPtr hwnd)
    {
        var length = GetWindowTextLength(hwnd);
        if (length <= 0) return string.Empty;
        var sb = new StringBuilder(length + 2);
        GetWindowText(hwnd, sb, sb.Capacity);
        return sb.ToString();
    }

    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW", SetLastError = true)]
    private static extern IntPtr GetWindowLongPtr64(IntPtr hwnd, int index);

    [DllImport("user32.dll", EntryPoint = "GetWindowLongW", SetLastError = true)]
    private static extern int GetWindowLong32(IntPtr hwnd, int index);

    private static IntPtr GetWindowLongPtr(IntPtr hwnd, int index)
        => IntPtr.Size == 8 ? GetWindowLongPtr64(hwnd, index) : new IntPtr(GetWindowLong32(hwnd, index));

    [DllImport("dwmapi.dll")]
    private static extern int DwmGetWindowAttribute(IntPtr hwnd, int attribute, out Rect value, int size);
}
