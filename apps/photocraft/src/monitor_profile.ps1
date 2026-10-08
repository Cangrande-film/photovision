# The ICC profile of the display under the screen point ($x, $y) in physical pixels, printed as
# `PROFILE=<path>`. Run by monitor_profile.rs (Windows) with `$x = ..; $y = ..` prepended; prints
# nothing when there is no answer. Win32 calls go through Add-Type P/Invoke because the
# workspace forbids `unsafe`.
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class PhotocraftMonitorProfile {
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct MONITORINFOEX {
    public int cbSize; public int l, t, r, b; public int wl, wt, wr, wb; public uint dwFlags;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string szDevice; }
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct DISPLAY_DEVICE {
    public int cb;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string DeviceName;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceString;
    public uint StateFlags;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceID;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceKey; }
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr ctx);
  [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] static extern IntPtr MonitorFromPoint(POINT pt, uint flags);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool GetMonitorInfoW(IntPtr mon, ref MONITORINFOEX mi);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool EnumDisplayDevicesW(string dev, uint i, ref DISPLAY_DEVICE dd, uint flags);
  [DllImport("mscms.dll", CharSet = CharSet.Unicode)] static extern bool WcsGetUsePerUserProfiles(string dev, uint cls, out bool perUser);
  [DllImport("mscms.dll", CharSet = CharSet.Unicode)] static extern bool WcsGetDefaultColorProfile(int scope, string dev, int type, int sub, uint id, uint cb, StringBuilder name);
  [DllImport("mscms.dll", CharSet = CharSet.Unicode)] static extern bool GetColorDirectoryW(string machine, StringBuilder dir, ref uint cb);
  [DllImport("gdi32.dll", CharSet = CharSet.Unicode)] static extern IntPtr CreateDCW(string drv, string dev, string port, IntPtr mode);
  [DllImport("gdi32.dll")] static extern bool DeleteDC(IntPtr dc);
  [DllImport("gdi32.dll", CharSet = CharSet.Unicode)] static extern bool GetICMProfileW(IntPtr dc, ref uint cb, StringBuilder name);
  const uint MONITOR_DEFAULTTONEAREST = 2, EDD_GET_DEVICE_INTERFACE_NAME = 1, DISPLAY_DEVICE_ACTIVE = 1;
  const uint CLASS_MONITOR = 0x6d6e7472; // 'mntr'
  const int SCOPE_SYSTEM_WIDE = 0, SCOPE_CURRENT_USER = 1, CPT_ICC = 0, CPST_NONE = 4;
  static string ColorDir() {
    uint cb = 1040; var sb = new StringBuilder(520);
    return GetColorDirectoryW(null, sb, ref cb) ? sb.ToString() : null;
  }
  static string Resolve(string name, string dir) {
    return (System.IO.Path.IsPathRooted(name) || dir == null) ? name : System.IO.Path.Combine(dir, name);
  }
  public static string Find(int x, int y) {
    // Physical pixels, like winit: per-monitor DPI aware (Windows 10 1703+), else system aware.
    try { SetProcessDpiAwarenessContext(new IntPtr(-4)); } catch (EntryPointNotFoundException) { SetProcessDPIAware(); }
    var pt = new POINT { X = x, Y = y };
    IntPtr mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    if (mon == IntPtr.Zero) return null;
    var mi = new MONITORINFOEX(); mi.cbSize = Marshal.SizeOf(typeof(MONITORINFOEX));
    if (!GetMonitorInfoW(mon, ref mi)) return null;
    string dir = ColorDir();
    // WCS keys profile associations by the monitor's device interface name.
    for (uint i = 0; ; i++) {
      var dd = new DISPLAY_DEVICE(); dd.cb = Marshal.SizeOf(typeof(DISPLAY_DEVICE));
      if (!EnumDisplayDevicesW(mi.szDevice, i, ref dd, EDD_GET_DEVICE_INTERFACE_NAME)) break;
      if ((dd.StateFlags & DISPLAY_DEVICE_ACTIVE) == 0 || string.IsNullOrEmpty(dd.DeviceID)) continue;
      bool perUser = false;
      WcsGetUsePerUserProfiles(dd.DeviceID, CLASS_MONITOR, out perUser);
      foreach (int scope in perUser ? new[] { SCOPE_CURRENT_USER, SCOPE_SYSTEM_WIDE } : new[] { SCOPE_SYSTEM_WIDE, SCOPE_CURRENT_USER }) {
        var sb = new StringBuilder(520);
        if (WcsGetDefaultColorProfile(scope, dd.DeviceID, CPT_ICC, CPST_NONE, 0, 1040, sb) && sb.Length > 0) return Resolve(sb.ToString(), dir);
      }
    }
    // Fallback: the GDI display DC's profile (Windows' default sRGB when none is associated).
    IntPtr dc = CreateDCW("DISPLAY", mi.szDevice, null, IntPtr.Zero);
    if (dc == IntPtr.Zero) return null;
    try {
      uint cb = 520; var sb = new StringBuilder(520);
      return (GetICMProfileW(dc, ref cb, sb) && sb.Length > 0) ? Resolve(sb.ToString(), dir) : null;
    } finally { DeleteDC(dc); }
  }
}
'@
$p = [PhotocraftMonitorProfile]::Find($x, $y)
if ($p) { 'PROFILE=' + $p }
