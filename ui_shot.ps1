# Screenshot the fan-tune window to _ui_shot.png.
#
# Hard-won knowledge (2026-10-12):
# - The process has several windows; "largest area" picks an INVISIBLE NVIDIA
#   GL helper (2475x1288). The real UI is the window titled "fan-tune · ...".
# - This script must stay DPI-aware or every rect comes back scaled by the
#   OS zoom factor (1.75x here) and the crop lands in the wrong place.
# - fan-tune runs ELEVATED (ring0): from a non-elevated shell ShowWindow /
#   SetForegroundWindow on its window silently fail (UIPI). If the window is
#   minimized this script cannot restore it — run it from an elevated shell,
#   or click the window open first. PrintWindow(PW_RENDERFULLCONTENT) is also
#   blocked, so there is no minimized-capture fallback.
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinPick {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
  public delegate bool EnumProc(IntPtr h, IntPtr lp);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
[WinPick]::SetProcessDPIAware() | Out-Null

$targetPid = (Get-Process -Name fan-tune -ErrorAction SilentlyContinue | Select-Object -First 1).Id
if (-not $targetPid) { throw "fan-tune is not running" }
$ft = [IntPtr]::Zero
$cb = [WinPick+EnumProc]{ param($h, $lp)
  $pid2 = 0
  [WinPick]::GetWindowThreadProcessId($h, [ref]$pid2) | Out-Null
  if ($pid2 -eq [UInt32]$lp.ToInt64()) {
    $sb = New-Object System.Text.StringBuilder 256
    [WinPick]::GetWindowText($h, $sb, 256) | Out-Null
    if ($sb.ToString() -like 'fan-tune *') { $script:ft = $h }
  }
  return $true
}
[WinPick]::EnumWindows($cb, [IntPtr]$targetPid) | Out-Null
if ($ft -eq [IntPtr]::Zero) { throw "titled fan-tune window not found" }

[WinPick]::BringWindowToTop($ft) | Out-Null
[WinPick]::SetForegroundWindow($ft) | Out-Null
Start-Sleep -Milliseconds 800
$r = New-Object WinPick+RECT
[WinPick]::GetWindowRect($ft, [ref]$r) | Out-Null
if ($r.L -le -30000) { Write-Host "window is minimized and cannot be restored from a non-elevated shell; capture anyway (may show other apps)" }

$vb = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap($vb.Width, $vb.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($vb.Left, $vb.Top, 0, 0, $bmp.Size)
$g.Dispose()

$w = $r.R - $r.L; $hh = $r.B - $r.T
if ($r.L -gt -30000 -and $w -gt 0) {
  $crop = New-Object System.Drawing.Bitmap($w, $hh)
  $g2 = [System.Drawing.Graphics]::FromImage($crop)
  $g2.DrawImage($bmp, (New-Object System.Drawing.Rectangle(0,0,$w,$hh)), (New-Object System.Drawing.Rectangle($r.L, $r.T, $w, $hh)), [System.Drawing.GraphicsUnit]::Pixel)
  $g2.Dispose()
  $crop.Save('C:\Users\67217\Desktop\fan-tune\_ui_shot.png', [System.Drawing.Imaging.ImageFormat]::Png)
  $crop.Dispose()
  Write-Host ("saved _ui_shot.png ({0}x{1})" -f $w, $hh)
} else {
  $bmp.Save('C:\Users\67217\Desktop\fan-tune\_ui_shot.png', [System.Drawing.Imaging.ImageFormat]::Png)
  Write-Host "window not restorable; saved full virtual screen instead"
}
$bmp.Dispose()
