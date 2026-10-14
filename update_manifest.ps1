# Embed manifest into exe via Win32 UpdateResource (RT_MANIFEST=24, ID=1)
# Paths derive from this script's own location: the project can be moved or
# cloned onto another machine without editing it.
$root = $PSScriptRoot
$exe = Join-Path $root 'target\release\fan-tune.exe'
$manifestPath = Join-Path $root 'src\app.manifest'
if (-not (Test-Path $exe)) { Write-Host "exe not found: $exe"; exit 1 }
if (-not (Test-Path $manifestPath)) { Write-Host "manifest not found: $manifestPath"; exit 1 }

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class ResUpdater {
    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    public static extern IntPtr BeginUpdateResource(string pFileName, bool bDeleteExistingResources);
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern bool UpdateResource(IntPtr hUpdate, IntPtr lpType, IntPtr lpName, ushort wLanguage, byte[] lpData, uint cbData);
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern bool EndUpdateResource(IntPtr hUpdate, bool fDiscard);
}
"@

$data = [System.IO.File]::ReadAllBytes($manifestPath)
$h = [ResUpdater]::BeginUpdateResource($exe, $false)
if ($h -eq [IntPtr]::Zero) { Write-Host "BeginUpdateResource failed"; exit 1 }
# RT_MANIFEST = 24, name = 1 (MAKEINTRESOURCE), LANG_NEUTRAL
$ok = [ResUpdater]::UpdateResource($h, [IntPtr]24, [IntPtr]1, 0, $data, [uint32]$data.Length)
if (-not $ok) { Write-Host "UpdateResource failed"; [ResUpdater]::EndUpdateResource($h, $true) | Out-Null; exit 1 }
$ok2 = [ResUpdater]::EndUpdateResource($h, $false)
if ($ok2) { Write-Host "MANIFEST EMBEDDED OK ($($data.Length) bytes)" } else { Write-Host "EndUpdateResource failed"; exit 1 }