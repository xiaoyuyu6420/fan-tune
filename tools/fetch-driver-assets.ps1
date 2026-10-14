# Fetch the third-party runtime assets that are deliberately not tracked in git.
#
# Run from anywhere:  powershell -ExecutionPolicy Bypass -File tools/fetch-driver-assets.ps1
#
# Why they are not in the repo: WinRing0x64.sys and the PawnIO modules are
# third-party binaries with their own licences (LGPL-2.1 for the modules, see
# driver/PAWNIO-MODULES-LICENSE.txt). Redistributing binaries we did not build
# is a poor default; fetching them is one command.
#
# What each is for:
#   LpcIO.bin / IntelMSR.bin  PawnIO modules (raw port I/O + MSR reads).
#                             Required. PawnIO is the preferred backend.
#   WinRing0x64.sys           OpenLibSys 1.2.0 driver. Only needed for the
#                             WinRing0 fallback backend, which Windows may
#                             blocklist (HVCI / Smart App Control).
#
# Hashes are recorded so a fetch is verifiable, not just convenient, and the\n# URLs pin the v0.9.5 tag: master has since rebuilt these modules, and a\n# silent swap is exactly the kind of change that should be a deliberate commit.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'driver'
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$assets = @(
    @{
        Name = 'LpcIO.bin'
        Url  = 'https://raw.githubusercontent.com/LibreHardwareMonitor/LibreHardwareMonitor/v0.9.5/LibreHardwareMonitorLib/Resources/PawnIo/LpcIO.bin'
        Sha  = '3dcf8b2bc80ff642d97c4608511a818642b5bf315ff53df3df393d043e71d101'
        Required = $true
    }
    @{
        Name = 'IntelMSR.bin'
        Url  = 'https://raw.githubusercontent.com/LibreHardwareMonitor/LibreHardwareMonitor/v0.9.5/LibreHardwareMonitorLib/Resources/PawnIo/IntelMSR.bin'
        Sha  = 'f3ab69f0a2686813de2f47efcda6271e176c4ee50fab5f107254f7d6a4e77361'
        Required = $true
    }
)

foreach ($a in $assets) {
    $out = Join-Path $dest $a.Name
    if (Test-Path $out) {
        $cur = (Get-FileHash $out -Algorithm SHA256).Hash
        if ($cur -ieq $a.Sha) { Write-Host "[ok]      $($a.Name) (already present, hash matches)"; continue }
        Write-Host "[replace] $($a.Name) (hash mismatch: $cur)"
    }
    Write-Host "[fetch]   $($a.Name) <- $($a.Url)"
    try {
        Invoke-WebRequest -Uri $a.Url -OutFile $out -UseBasicParsing
    } catch {
        if ($a.Required) { throw "Failed to fetch $($a.Name): $($_.Exception.Message). Download it manually and place it in driver/." }
        Write-Host "[skip]    $($a.Name): $($_.Exception.Message)"
        continue
    }
    $got = (Get-FileHash $out -Algorithm SHA256).Hash
    if ($got -ine $a.Sha) {
        Remove-Item $out -Force
        throw "Hash mismatch for $($a.Name)`n  expected $($a.Sha)`n  got      $got"
    }
    Write-Host "[ok]      $($a.Name)"
}

# WinRing0x64.sys: no stable public URL, so we extract it from the
# LibreHardwareMonitorLib.dll resource if the user has LHM installed, and
# otherwise tell them where to get it.
$sys = Join-Path $dest 'WinRing0x64.sys'
if (Test-Path $sys) {
    Write-Host "[ok]      WinRing0x64.sys (already present)"
} else {
    Write-Host ""
    Write-Host "WinRing0x64.sys is NOT fetched automatically (no stable public URL)."
    Write-Host "You only need it for the WinRing0 fallback backend; PawnIO works without it."
    Write-Host "To get it, install LibreHardwareMonitor and copy the embedded driver:"
    Write-Host "  https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases"
    Write-Host "and place WinRing0x64.sys in: $dest"
    Write-Host ""
    Write-Host "Or fetch it from the LibreHardwareMonitor resource blob:"
    Write-Host "  LibreHardwareMonitorLib/Resources/WinRing0x64.gz"
}
Write-Host ""
Write-Host "Done. Rebuild with 'cargo build --release'; build.rs copies these next to the exe."
