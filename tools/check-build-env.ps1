# Check that this machine can build fan-tune, before `cargo build` fails
# somewhere in the middle of a dependency graph.
#
# Run:  powershell -ExecutionPolicy Bypass -File tools/check-build-env.ps1
#
# fan-tune embeds a requireAdministrator manifest into the exe (build.rs ->
# windres + ar), which is why it needs the GNU toolchain: windres comes from
# mingw64 and ar from binutils. A default Windows rustup install is MSVC, which
# has no windres, and the failure surfaces as "linker `link.exe` not found"
# while compiling unrelated dependencies — nowhere near the real cause.

$ErrorActionPreference = 'Continue'
$problems = 0

Write-Host "== rust toolchain =="
$rustup = Get-Command rustup -ErrorAction SilentlyContinue
if (-not $rustup) {
    Write-Host "  [FAIL] rustup not found. Install from https://rustup.rs"
    $problems++
} else {
    $tc = (& rustup toolchain list) -join "`n"
    Write-Host ($tc -split "`n" | ForEach-Object { "  $_" })
    if ($tc -notmatch 'windows-gnu') {
        Write-Host "  [FAIL] no *-pc-windows-gnu toolchain."
        Write-Host "         fix:  rustup toolchain install stable-x86_64-pc-windows-gnu"
        Write-Host "         then: rustup default stable-x86_64-pc-windows-gnu"
        $problems++
    }
    $default = (& rustup default)
    if ($default -match 'msvc') {
        Write-Host "  [warn] default toolchain is MSVC ($default)."
        Write-Host "         fix:  rustup default stable-x86_64-pc-windows-gnu"
        $problems++
    }
}

Write-Host "== mingw64 (windres + ar) =="
foreach ($tool in 'windres', 'ar') {
    $c = Get-Command $tool -ErrorAction SilentlyContinue
    if ($c) {
        Write-Host "  [ok]   $tool -> $($c.Source)"
    } else {
        Write-Host "  [FAIL] $tool not on PATH (mingw64)."
        Write-Host "         e.g. choco install mingw  /  scoop install mingw  /  MSYS2 + "
        Write-Host "         add <mingw64>\bin to PATH"
        $problems++
    }
}

Write-Host "== runtime assets =="
foreach ($f in 'driver/LpcIO.bin', 'driver/IntelMSR.bin') {
    if (Test-Path $f) { Write-Host "  [ok]   $f" }
    else {
        Write-Host "  [warn] $f missing -> tools/fetch-driver-assets.ps1"
    }
}
if (Test-Path 'driver/WinRing0x64.sys') { Write-Host "  [ok]   driver/WinRing0x64.sys" }
else {
    Write-Host "  [info] driver/WinRing0x64.sys absent (optional)"
    Write-Host "         only the WinRing0 fallback backend needs it; PawnIO does not"
}

Write-Host ""
if ($problems -gt 0) {
    Write-Host "$problems problem(s) — cargo build will likely fail. See the fixes above."
    exit 1
}
Write-Host "Environment looks able to build fan-tune."
