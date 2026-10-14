//! Ring-0 access for the fan/sensor chip: I/O port access (the SuperIO
//! protocol) and MSR reads. Two backends behind one API:
//!
//! * **PawnIO** (used when installed): a signed kernel driver plus signed
//!   modules loaded per device handle — no service of ours, nothing on the
//!   vulnerable-driver blocklist. Protocol and module list taken from
//!   LibreHardwareMonitor's `PawnIo/` sources (see `refs/`).
//! * **WinRing0x64.sys** (fallback): our own service on the OpenLibSys 1.2.0
//!   binary extracted from the local LibreHardwareMonitorLib.dll resource.
//!   Refused by Windows when HVCI / Smart App Control enforce the
//!   vulnerable-driver list.
//!
//! `Cargo.toml`/the UI only ever see `read_io_port` / `write_io_port` /
//! `read_msr`, so the choice of backend is invisible above this file.

use std::ffi::c_void;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, CreateServiceW, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    StartServiceW, ControlService, DeleteService,
    SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
    SERVICE_ERROR_NORMAL, SERVICE_KERNEL_DRIVER, SERVICE_STATUS, SERVICE_STOPPED,
};

const DEVICE_PATH: &str = "\\\\.\\WinRing0_1_2_0";
/// Our service names (same signed WinRing0 binary — it is Authenticode-signed,
/// so the binary must NOT be patched; a second service name is the only safe
/// diversification).
pub const SERVICE_NAME: &str = "FtuneR0";
/// Second service name for the same driver binary — used when the primary
/// service is wedged by a force-killed process.
const SERVICE_NAME_ALT: &str = "FtuneR0B";
/// Service names from experimental builds — stopped/cleaned during recovery.
const SERVICE_NAME_LEGACY: [&str; 2] = ["FtuneFan", "FtuneFanB"];
const DRIVER_FILE: &str = "WinRing0x64.sys";
/// Sentinel file: when present, the daemon exits cleanly (graceful handover).
pub const STOP_SENTINEL: &str = "fan-tune-daemon-stop.req";
/// Exclusive-ownership lock file (see [`HardwareLock`]).
const HARDWARE_LOCK_FILE: &str = "fan-tune-hardware.lock";

/// Absolute path of the handover sentinel (next to the exe, independent of cwd).
pub fn stop_sentinel_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(STOP_SENTINEL)))
        .unwrap_or_else(|| std::path::PathBuf::from(STOP_SENTINEL))
}

// CTL_CODE(DeviceType=40000, Func, Access, METHOD_BUFFERED)
const fn ctl_code(device: u32, func: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (func << 2)
}
const OLS_TYPE: u32 = 40000;
const FILE_ANY_ACCESS: u32 = 0;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;

pub const IOCTL_READ_MSR: u32 = ctl_code(OLS_TYPE, 0x821, FILE_ANY_ACCESS);
pub const IOCTL_WRITE_MSR: u32 = ctl_code(OLS_TYPE, 0x822, FILE_ANY_ACCESS);
pub const IOCTL_READ_IO_PORT_BYTE: u32 = ctl_code(OLS_TYPE, 0x833, FILE_READ_ACCESS);
pub const IOCTL_WRITE_IO_PORT_BYTE: u32 = ctl_code(OLS_TYPE, 0x836, FILE_WRITE_ACCESS);

// ---------- PawnIO (github.com/namazso/PawnIO) ----------
// A signed kernel driver that runs signed modules; a module is loaded into a
// device handle and then called by name. Protocol and module choice mirror
// LibreHardwareMonitor 0.9.5 (PawnIo/PawnIo.cs, PawnIo/LpcIO.cs, PawnIo/IntelMsr.cs).
const PAWNIO_DEVICE: &str = "\\\\.\\PawnIO";
/// PawnIO uses buffered IOCTLs with no access bits (PawnIo.cs:17-20).
const PAWNIO_DEVICE_TYPE: u32 = 41394;
const PAWNIO_IOCTL_LOAD_BINARY: u32 = (PAWNIO_DEVICE_TYPE << 16) | (0x821 << 2);
const PAWNIO_IOCTL_EXECUTE: u32 = (PAWNIO_DEVICE_TYPE << 16) | (0x841 << 2);
/// Function names travel as 32 ASCII bytes, then the arguments as i64s.
const PAWNIO_FN_NAME_LEN: usize = 32;
/// Module files (LGPL-2.1, from the PawnIO.Modules release): raw port I/O plus
/// MSR reads. They live next to the exe, like the .sys.
const LPCIO_MODULE: &str = "LpcIO.bin";
const INTELMSR_MODULE: &str = "IntelMSR.bin";

/// Which ring-0 path is in use. Everything above this file is backend-agnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// PawnIO: nothing of ours to install, not on any blocklist.
    PawnIo,
    /// Our own service on WinRing0x64.sys.
    WinRing0,
}

/// Exclusive ownership of the fan hardware, held for the whole process life.
///
/// With WinRing0 this came for free — the device is opened `FILE_SHARE_NONE`,
/// so a second process cannot reach the chip at all. PawnIO opens *with*
/// sharing (PawnIo.cs:68-71), which would silently allow two controllers. So
/// ownership is now explicit: an exclusively-opened lock file next to the exe,
/// taken by both backends. Whoever holds it is the single controller; the
/// successor's retry loop waits for the predecessor to exit, exactly as it
/// used to wait for the device handle.
struct HardwareLock {
    handle: HANDLE,
}

impl HardwareLock {
    fn acquire() -> Option<HardwareLock> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.join(HARDWARE_LOCK_FILE);
        let wide = to_wide(&path.to_string_lossy());
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0xC0000000u32, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_NONE, // the whole point: a second opener fails
                std::ptr::null(),
                OPEN_ALWAYS, // create if missing, keep the file
                0x80,        // FILE_ATTRIBUTE_NORMAL
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            None
        } else {
            Some(HardwareLock { handle })
        }
    }
}

// NOTE: no Drop — like the device handles, this is released by the OS when the
// process ends. An explicit close would release ownership while clones of
// `Driver` are still writing to the chip.

/// Read a module file: next to the exe first, then the repo's `driver/` dir so
/// a source-tree build works without deploying.
fn module_bytes(file: &str) -> Option<Vec<u8>> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                return Some(bytes);
            }
        }
    }
    std::fs::read(std::path::Path::new("driver").join(file)).ok()
}

/// Open `\\.\PawnIO` and load one module into that handle. Each handle carries
/// exactly one module, so ports and MSRs need one handle each (as LHM does).
fn pawnio_load(file: &str) -> Result<HANDLE, String> {
    let bytes = module_bytes(file)
        .ok_or_else(|| format!("{file} not found (next to the exe or in driver/)"))?;
    let wide = to_wide(PAWNIO_DEVICE);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0xC0000000u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(format!("PawnIO device not available (err={})", last_error()));
    }
    let mut ret = 0u32;
    let ok = unsafe {
        device_io_control(
            handle,
            PAWNIO_IOCTL_LOAD_BINARY,
            bytes.as_ptr() as *mut c_void,
            bytes.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
        )
    };
    if ok {
        Ok(handle)
    } else {
        let err = last_error();
        unsafe { CloseHandle(handle) };
        Err(format!("loading {file} failed (err={err})"))
    }
}

/// Call a module function: `[32-byte ASCII name][i64 args...]` in, at most one
/// i64 out. Every function we use returns either nothing or a single value, so
/// this needs no heap buffers (it runs hundreds of times a second).
fn pawnio_execute(handle: HANDLE, name: &str, args: &[i64], want_value: bool) -> Option<i64> {
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // all of the functions we call take 0..2 arguments
    if args.len() > 2 {
        return None;
    }
    let mut in_buf = [0u8; PAWNIO_FN_NAME_LEN + 16];
    let n = name.len().min(PAWNIO_FN_NAME_LEN - 1);
    in_buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    for (i, a) in args.iter().enumerate() {
        let at = PAWNIO_FN_NAME_LEN + i * 8;
        in_buf[at..at + 8].copy_from_slice(&a.to_le_bytes());
    }
    let in_len = (PAWNIO_FN_NAME_LEN + args.len() * 8) as u32;
    let mut out_buf = [0u8; 8];
    let out_len = if want_value { 8u32 } else { 0 };
    let mut ret = 0u32;
    let ok = unsafe {
        device_io_control(
            handle,
            PAWNIO_IOCTL_EXECUTE,
            in_buf.as_mut_ptr() as *mut c_void,
            in_len,
            out_buf.as_mut_ptr() as *mut c_void,
            out_len,
            &mut ret,
        )
    };
    if !ok {
        return None;
    }
    if !want_value {
        return Some(0);
    }
    Some(i64::from_le_bytes(out_buf))
}

/// Is PawnIO installed (driver reachable and modules present)? Used for the
/// diagnostics shown when nothing works.
pub fn pawnio_available() -> bool {
    module_bytes(LPCIO_MODULE).is_some() && module_bytes(INTELMSR_MODULE).is_some() && {
        let wide = to_wide(PAWNIO_DEVICE);
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0xC0000000u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        let ok = h != INVALID_HANDLE_VALUE && !h.is_null();
        if ok {
            unsafe { CloseHandle(h) };
        }
        ok
    }
}

/// Open both PawnIO modules (ports + MSR).
fn open_pawnio() -> Result<(HANDLE, HANDLE), String> {
    let lpc = pawnio_load(LPCIO_MODULE)?;
    let msr = match pawnio_load(INTELMSR_MODULE) {
        Ok(h) => h,
        Err(e) => {
            unsafe { CloseHandle(lpc) };
            return Err(e);
        }
    };
    // LHM selects the slot matching its config port (0x2E -> 0, 0x4E -> 1,
    // LpcPort.cs:18). Our raw port I/O does not depend on it, but doing it
    // keeps the module state identical to the reference implementation.
    let _ = pawnio_execute(lpc, "ioctl_select_slot", &[0], false);
    dlog("pawnio: LpcIO + IntelMSR loaded");
    Ok((lpc, msr))
}

pub struct Driver {
    backend: Backend,
    /// WinRing0 device handle (WinRing0 backend only)
    handle: HANDLE,
    /// PawnIO: one handle per loaded module — a handle carries its module, so
    /// ports and MSRs need one each.
    pawn_lpc: HANDLE,
    pawn_msr: HANDLE,
    /// Exclusive ownership of the chip; held until the process exits.
    _lock: Arc<HardwareLock>,
}

impl Clone for Driver {
    fn clone(&self) -> Self {
        Driver {
            backend: self.backend,
            handle: self.handle,
            pawn_lpc: self.pawn_lpc,
            pawn_msr: self.pawn_msr,
            _lock: self._lock.clone(),
        }
    }
}

// SAFETY: the raw HANDLE is only used for synchronous DeviceIoControl calls
// which are thread-safe at the OS level; we never free it (Drop is a no-op).
unsafe impl Send for Driver {}
unsafe impl Sync for Driver {}

impl Driver {
    /// Install (if needed), start and open a ring-0 backend. Requires admin.
    ///
    /// `pref` picks the *order*: both backends are always attempted, so a
    /// machine where one cannot load still works with the other. Retries for up
    /// to ~30s — right after a force-killed process the WinRing0 device may
    /// still be unloading, and a successor waits for the hardware lock.
    pub fn install_and_open(pref: crate::config::DriverBackend) -> Result<Driver, String> {
        let wide: Vec<u16> = DEVICE_PATH.encode_utf16().collect();
        let mut last_err = String::from("unknown");
        // Probing PawnIO costs file reads; the machine either has it or not.
        let pawnio = pawnio_available();
        // WinRing0 costs roughly a third of PawnIO per port operation (native
        // IOCTL vs interpreted module), so it is worth preferring — but only
        // where Windows lets it load. `auto` asks the machine: the vulnerable
        // driver blocklist is enforced exactly when HVCI (memory integrity) or
        // Smart App Control are on.
        let blocked = blocklist_enforced();
        let pawnio_first = match pref {
            crate::config::DriverBackend::Pawnio => true,
            crate::config::DriverBackend::Winring0 => false,
            crate::config::DriverBackend::Auto => blocked || !winring0_file_present(),
        };
        dlog(&format!(
            "backend preference {pref:?}: {} first (blocklist enforced: {blocked}, pawnio available: {pawnio})",
            if pawnio_first { "PawnIO" } else { "WinRing0" }
        ));
        for attempt in 0..20 {
            // Ownership first, backend second: whoever holds the lock is the
            // single controller, so a successor waits here instead of racing
            // the predecessor for the chip.
            let Some(lock) = HardwareLock::acquire() else {
                last_err = "风扇硬件正被另一个实例占用（等它退出）".to_string();
                if attempt == 0 {
                    dlog("hardware lock held by another controller — waiting");
                }
                std::thread::sleep(std::time::Duration::from_millis(1500));
                continue;
            };

            for want_pawnio in [pawnio_first, !pawnio_first] {
                if want_pawnio {
                    if !pawnio {
                        continue;
                    }
                    match open_pawnio() {
                        Ok((lpc, msr)) => {
                            if attempt > 0 {
                                eprintln!("[driver] PawnIO available on attempt {attempt}");
                            }
                            dlog("backend: PawnIO (LpcIO + IntelMSR modules)");
                            // PawnIO makes our own WinRing0 service dead weight —
                            // and that .sys is the driver the blocklist and
                            // Defender care about. Retire it; `ensure_service`
                            // recreates it if we ever fall back.
                            retire_own_winring0();
                            return Ok(Driver {
                                backend: Backend::PawnIo,
                                handle: INVALID_HANDLE_VALUE,
                                pawn_lpc: lpc,
                                pawn_msr: msr,
                                _lock: Arc::new(lock),
                            });
                        }
                        Err(e) => last_err = format!("PawnIO: {e}"),
                    }
                } else {
                    // Device first: when the service is already running and
                    // healthy this opens instantly, and only a failure pays for
                    // the stop/start round trip below.
                    if let Some(handle) = try_open_device(&wide) {
                        if attempt > 0 {
                            eprintln!("[driver] device available on attempt {attempt}");
                            dlog(&format!("device opened on attempt {attempt}"));
                        }
                        dlog("backend: WinRing0 (our own service)");
                        return Ok(Driver {
                            backend: Backend::WinRing0,
                            handle,
                            pawn_lpc: INVALID_HANDLE_VALUE,
                            pawn_msr: INVALID_HANDLE_VALUE,
                            _lock: Arc::new(lock),
                        });
                    }
                    // Device absent (service stopped or never registered —
                    // e.g. retired by an earlier PawnIO run): start it and wait
                    // for the device object, which appears a moment after the
                    // driver loads. Giving up here would hand the chip to the
                    // fallback, which then retires this service again — the two
                    // backends would trade places at every startup.
                    let name = if attempt < 8 { SERVICE_NAME } else { SERVICE_NAME_ALT };
                    match ensure_service(name) {
                        Ok(()) => {
                            let mut opened = None;
                            for _ in 0..6 {
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                if let Some(handle) = try_open_device(&wide) {
                                    opened = Some(handle);
                                    break;
                                }
                            }
                            if let Some(handle) = opened {
                                dlog(&format!("backend: WinRing0 (service '{name}' (re)started)"));
                                return Ok(Driver {
                                    backend: Backend::WinRing0,
                                    handle,
                                    pawn_lpc: INVALID_HANDLE_VALUE,
                                    pawn_msr: INVALID_HANDLE_VALUE,
                                    _lock: Arc::new(lock),
                                });
                            }
                            last_err = format!("service '{name}' started but the device never appeared");
                        }
                        Err(e) => {
                            dlog(&format!("attempt {attempt}: {e}"));
                            last_err = e;
                        }
                    }
                }
            }
            if attempt == 3 {
                // device absent + ours won't load ⇒ some foreign WinRing0
                // instance (e.g. FanControl's leftover R0FanControl) is a
                // zombie holding the kernel object names — evict it once
                stop_foreign_winring0();
            }
            // The service dance already happened inside the WinRing0 branch;
            // here we only wait out a driver that is still unloading (the
            // ERROR_ALREADY_EXISTS / service-starting window).
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        dlog(&format!("giving up this sweep: {last_err}"));
        Err(last_err)
    }
   
    fn ioctl(&self, code: u32, in_buf: &mut [u8], out_buf: &mut [u8]) -> bool {
        let mut ret = 0u32;
        let ok = unsafe {
            device_io_control(
                self.handle,
                code,
                in_buf.as_mut_ptr() as *mut c_void,
                in_buf.len() as u32,
                out_buf.as_mut_ptr() as *mut c_void,
                out_buf.len() as u32,
                &mut ret,
            )
        };
        ok
    }

    pub fn read_io_port(&self, port: u16) -> u8 {
        match self.backend {
            Backend::PawnIo => {
                pawnio_execute(self.pawn_lpc, "ioctl_pio_inb", &[port as i64], true)
                    .map(|v| v as u8)
                    .unwrap_or(0)
            }
            Backend::WinRing0 => {
                let mut in_buf = [0u8; 4];
                in_buf[0..4].copy_from_slice(&(port as u32).to_le_bytes());
                let mut out_buf = [0u8; 4];
                if self.ioctl(IOCTL_READ_IO_PORT_BYTE, &mut in_buf, &mut out_buf) {
                    out_buf[0]
                } else {
                    0
                }
            }
        }
    }

    pub fn write_io_port(&self, port: u16, value: u8) {
        match self.backend {
            Backend::PawnIo => {
                pawnio_execute(self.pawn_lpc, "ioctl_pio_outb", &[port as i64, value as i64], false);
            }
            Backend::WinRing0 => {
                // struct { u32 port; u8 value; } pack=1 -> 5 bytes
                let mut in_buf = [0u8; 5];
                in_buf[0..4].copy_from_slice(&(port as u32).to_le_bytes());
                in_buf[4] = value;
                let mut out_buf = [];
                self.ioctl(IOCTL_WRITE_IO_PORT_BYTE, &mut in_buf, &mut out_buf);
            }
        }
    }

    pub fn read_msr(&self, index: u32) -> Option<u64> {
        match self.backend {
            Backend::PawnIo => pawnio_execute(self.pawn_msr, "ioctl_read_msr", &[index as i64], true)
                .map(|v| v as u64),
            Backend::WinRing0 => {
                let mut in_buf = [0u8; 4];
                in_buf[0..4].copy_from_slice(&index.to_le_bytes());
                let mut out_buf = [0u8; 8];
                if self.ioctl(IOCTL_READ_MSR, &mut in_buf, &mut out_buf) {
                    Some(u64::from_le_bytes(out_buf))
                } else {
                    None
                }
            }
        }
    }

    /// Which ring-0 path this handle is using (logged at bring-up).
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Tell the PawnIO LpcIO module which SuperIO config port pair we are
    /// talking to: slot 0 = 0x2E/0x2F, slot 1 = 0x4E/0x4F. Must be called
    /// before that port's config-mode entry, and it *resets* the module
    /// (including the BAR allow-list), so never call it after `pawnio_find_bars`
    /// in the same detection pass. No-op for the WinRing0 backend.
    pub fn pawnio_select_slot(&self, slot: i64) {
        if self.backend == Backend::PawnIo {
            let r = pawnio_execute(self.pawn_lpc, "ioctl_select_slot", &[slot], false);
            dlog(&format!("pawnio select_slot({slot}) -> {}", r.is_some()));
        }
    }

    /// Let the LpcIO module discover the chip's I/O BARs.
    ///
    /// The module refuses every port access outside its allow-list, and that
    /// list only contains the config port pair plus the BARs it has *found*
    /// (`is_port_allowed`, LpcIO.p). Without this call every EC register read
    /// returns ACCESS_DENIED — which the raw read path reports as 0, so the
    /// chip looks dead. The module's own documentation requires configuration
    /// mode to be entered and the chip ID to be valid first.
    ///
    /// No-op for the WinRing0 backend, which has no such list.
    pub fn pawnio_find_bars(&self) {
        if self.backend == Backend::PawnIo {
            let r = pawnio_execute(self.pawn_lpc, "ioctl_find_bars", &[], false);
            dlog(&format!("pawnio find_bars -> {}", r.is_some()));
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // NOTE: Clone produces shallow handle copies; we skip CloseHandle
        // to avoid double-free. The OS reclaims the handle at process exit.
    }
}

unsafe fn device_io_control(
    handle: HANDLE,
    code: u32,
    in_ptr: *mut c_void,
    in_len: u32,
    out_ptr: *mut c_void,
    out_len: u32,
    ret: &mut u32,
) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            h: HANDLE,
            code: u32,
            in_buf: *mut c_void,
            in_len: u32,
            out_buf: *mut c_void,
            out_len: u32,
            ret: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }
    DeviceIoControl(handle, code, in_ptr, in_len, out_ptr, out_len, ret, std::ptr::null_mut()) != 0
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Append a diagnostic line to fan-tune-driver.log (cwd-relative, next to the
/// other logs). The GUI has no console — this is the durable trace of driver
/// bring-up, which matters because boot-time failures are transient.
/// Diagnose the environment when ring-0 access fails.
///
/// Windows security features that enforce the vulnerable-driver blocklist, and
/// antivirus quarantine of our `.sys`, all surface as the same bare Win32 error
/// code. Name the real cause instead, with the one action that fixes it.
pub fn environment_hint() -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    if !pawnio_available() {
        notes.push(
            "没有可用的 PawnIO（驱动没装，或 exe 旁边缺 LpcIO.bin / IntelMSR.bin）。\
             装上它（pawnio.eu，约 5 MB）可以让本程序彻底绕开 WinRing0：\
             微软签名驱动 + 签名模块，不在易受攻击驱动名单里，也不会被 Defender 报 HackTool。"
                .to_string(),
        );
    }
    if reg_dword(
        r"HKLM\SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity",
        "Enabled",
    )
    .unwrap_or(0)
        != 0
    {
        notes.push(
            "「内核隔离 / 内存完整性（HVCI）」开着：Windows 会拒绝加载 WinRing0x64.sys。\n\
             关掉它（Windows 安全中心 → 设备安全性 → 内核隔离 → 内存完整性）再重启。"
                .to_string(),
        );
    }
    if reg_dword(
        r"HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy",
        "VerifiedAndReputablePolicyState",
    )
    .unwrap_or(0)
        == 1
    {
        notes.push("「智能应用控制」处于开启状态，同样会拦截这个驱动。".to_string());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sys = dir.join(DRIVER_FILE);
            if !sys.exists() {
                notes.push(format!(
                    "驱动文件不见了：{}\n多半被杀毒软件当成 HackTool 隔离了——在 Defender 的「保护历史记录」里恢复，并给它加排除项。",
                    sys.display()
                ));
            }
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n\n"))
    }
}

/// Read a REG_DWORD via `reg query`. Only ever called on a failure path, so the
/// extra process does not matter and no registry dependency is needed.
fn reg_dword(key: &str, value: &str) -> Option<u32> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(["query", key, "/v", value])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("REG_DWORD"))?;
    let hex = line.split_whitespace().last()?;
    u32::from_str_radix(hex.trim_start_matches("0x"), 16).ok()
}

fn dlog(msg: &str) {    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("fan-tune-driver.log")
    {
        let _ = writeln!(f, "[driver] {msg}");
    }
}

/// Try opening the driver device; None if it is not (yet) available.
fn try_open_device(wide: &[u16]) -> Option<HANDLE> {
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0xC0000000u32, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        Some(handle)
    } else {
        None
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLastError() -> u32;
}

#[inline]
fn last_error() -> u32 {
    unsafe { GetLastError() }
}

fn ensure_service(service_name: &str) -> Result<(), String> {
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return Err(format!("OpenSCManager failed, err={}", last_error()));
        }
        let name_w = to_wide(service_name);
        let mut service = OpenServiceW(manager, name_w.as_ptr(), SERVICE_ALL_ACCESS);
        if service.is_null() {
            // create it, pointing at the driver file next to our exe
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let bin = exe
                .parent()
                .ok_or("no parent dir")?
                .join(DRIVER_FILE);
            if !bin.exists() {
                // also look next to cwd (dev runs)
                let alt = std::path::Path::new("driver").join(DRIVER_FILE);
                if alt.exists() {
                    let full = std::fs::canonicalize(alt).map_err(|e| e.to_string())?;
                    return create_and_start(manager, &full, service_name);
                }
                return Err(format!("driver file not found at {}", bin.display()));
            }
            create_and_start(manager, &bin, service_name)?;
            CloseServiceHandle(manager);
            return Ok(());
        }
        // service exists: force a clean STOPPED state, then start it.
        // (After a forced daemon kill the SCM/driver can be stuck — the last
        // start recorded ERROR_ALREADY_EXISTS while the previous instance was
        // still unloading. A STOP round-trip clears that bookkeeping.)
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        QueryServiceStatus(service, &mut status);
        if status.dwCurrentState != SERVICE_STOPPED {
            let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
            for _ in 0..6 {
                QueryServiceStatus(service, &mut status);
                if status.dwCurrentState == SERVICE_STOPPED {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        if StartServiceW(service, 0, std::ptr::null()) == 0 {
            let err = last_error();
            // 1055 ERROR_SERVICE_REQUEST_FAILED (starting), 1056 already running,
            // 183 ERROR_ALREADY_EXISTS (previous instance still unloading) —
            // none are fatal here; the CreateFile retry loop decides.
            if !matches!(err, 1055 | 1056 | 183) {
                CloseServiceHandle(service);
                CloseServiceHandle(manager);
                return Err(format!("StartService failed, err={err}"));
            }
        }
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
        Ok(())
    }
}

unsafe fn create_and_start(
    manager: *mut c_void,
    bin: &std::path::Path,
    service_name: &str,
) -> Result<(), String> {
    let name_w = to_wide(service_name);
    let bin_str = bin.to_string_lossy();
    let bin_w = to_wide(&bin_str);
    let mut service = CreateServiceW(
        manager,
        name_w.as_ptr(),
        name_w.as_ptr(),
        SERVICE_ALL_ACCESS,
        SERVICE_KERNEL_DRIVER,
        SERVICE_DEMAND_START,
        SERVICE_ERROR_NORMAL,
        bin_w.as_ptr(),
        std::ptr::null(),
        std::ptr::null_mut(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
    );
    if service.is_null() {
        let err = last_error();
        if err == 183 {
            // ERROR_SERVICE_EXISTS — another process (or previous run) created it
            service = OpenServiceW(manager, name_w.as_ptr(), SERVICE_ALL_ACCESS);
            if service.is_null() {
                return Err(format!("service exists but cannot be opened, err={}", last_error()));
            }
        } else {
            return Err(format!("CreateService failed, err={err}"));
        }
    }
    let start_err = if StartServiceW(service, 0, std::ptr::null()) == 0 {
        Some(last_error())
    } else {
        None
    };
    CloseServiceHandle(service);
    match start_err {
        None => Ok(()),
        // 1055/1056 service (being) started, 183 previous instance unloading —
        // tolerated; the device-open retry loop is the real gate
        Some(1055) | Some(1056) | Some(183) => Ok(()),
        Some(e) => Err(format!("StartService failed, err={e}")),
    }
}

/// Best-effort: stop leftover WinRing0 kernel-driver services from other
/// tools and from our own earlier builds. Only called when our own driver
/// repeatedly fails to load. We are the machine's single fan controller at
/// that point, so evicting a dead driver is safe.
fn stop_foreign_winring0() {
    const FOREIGN: [&str; 6] = [
        "R0FanControl",
        "WinRing0",
        "WinRing0x64",
        "R0LibreHwMonitor",
        SERVICE_NAME_LEGACY[0],
        SERVICE_NAME_LEGACY[1],
    ];
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return;
        }
        for name in FOREIGN {
            if name == SERVICE_NAME || name == SERVICE_NAME_ALT {
                continue;
            }
            let name_w = to_wide(name);
            let service = OpenServiceW(manager, name_w.as_ptr(), SERVICE_ALL_ACCESS);
            if service.is_null() {
                continue;
            }
            let mut status: SERVICE_STATUS = std::mem::zeroed();
            if ControlService(service, SERVICE_CONTROL_STOP, &mut status) != 0 {
                eprintln!("[driver] stopped zombie foreign service '{name}'");
                std::thread::sleep(std::time::Duration::from_millis(2000));
            }
            if name.starts_with("Ftune") {
                // our own experimental leftovers — retire them for good
                DeleteService(service);
            }
            CloseServiceHandle(service);
        }
        CloseServiceHandle(manager);
    }
}

/// Do Windows' security features enforce the vulnerable-driver blocklist right
/// now? That is exactly when WinRing0 cannot load: HVCI (memory integrity) or
/// Smart App Control are the enforcement switches.
fn blocklist_enforced() -> bool {
    reg_dword(
        r"HKLM\SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity",
        "Enabled",
    )
    .unwrap_or(0)
        != 0
        || reg_dword(
            r"HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy",
            "VerifiedAndReputablePolicyState",
        )
        .unwrap_or(0)
            == 1
}

/// Is our WinRing0 binary still sitting next to the exe (i.e. not quarantined
/// away by an antivirus)?
fn winring0_file_present() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(DRIVER_FILE).exists()))
        .unwrap_or(false)
}

/// Drop our own WinRing0 service once PawnIO is driving the chip: no reason to
/// keep a blocklisted kernel driver loaded when nothing uses it.
fn retire_own_winring0() {
    uninstall_service();
    dlog("retired our own WinRing0 service (PawnIO is driving)");
}

/// Best-effort uninstall (stop + delete the service).
pub fn uninstall_service() {
    let mut names: Vec<&str> = vec![SERVICE_NAME, SERVICE_NAME_ALT];
    names.extend(SERVICE_NAME_LEGACY);
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return;
        }
        for name in names {
            let name_w = to_wide(name);
            let service = OpenServiceW(manager, name_w.as_ptr(), SERVICE_ALL_ACCESS);
            if !service.is_null() {
                let mut status: SERVICE_STATUS = std::mem::zeroed();
                ControlService(service, SERVICE_CONTROL_STOP, &mut status);
                DeleteService(service);
                CloseServiceHandle(service);
            }
        }
        CloseServiceHandle(manager);
    }
}

#[allow(dead_code)]
fn _unused(_: HANDLE) {}
const _: () = {
    // touch SERVICE_STOPPED so the import isn't flagged unused on some versions
    let _ = SERVICE_STOPPED;
};