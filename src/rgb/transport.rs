//! USB transport for the board's ARGB controller.
//!
//! The controller is the ENE 6k7750 that Maxsun boards ship (`USB\VID_0CF2&PID_7750`).
//! Its `MI_00` function is bound to the Microsoft WinUSB driver through the
//! firmware's MS OS descriptors, so no vendor driver and no ring-0 access are
//! needed — only a handle with `FILE_FLAG_OVERLAPPED`, without which
//! `WinUsb_Initialize` fails with ERROR_INVALID_HANDLE (6).
//!
//! Protocol (transcribed from the community SignalRGB plugin, see
//! `refs/signalrgb-maxsun/`): raw USB control transfers on the default pipe,
//! addressed by register in `wIndex`, `wValue` always 0 —
//! read `bmRequestType 0xC0 / bRequest 0x81`, write `0x40 / 0x80`.
//!
//! Declared by hand rather than through a crate: the project already talks to
//! kernel32 this way, and it keeps the dependency list empty.

use std::ffi::c_void;

type Handle = *mut c_void;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
/// Same numeric value as GENERIC_WRITE, but a different parameter: this one
/// is `dwFlagsAndAttributes`, not the access mask.
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const INVALID_HANDLE_VALUE: isize = -1;

const DIGCF_PRESENT: u32 = 0x02;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;

/// `USB\VID_0CF2` — the vendor id the controller enumerates with.
pub const VENDOR_ID: u16 = 0x0cf2;
/// Product ids seen on Maxsun boards (0x7750 on this one).
pub const PRODUCT_IDS: [u16; 2] = [0x7750, 0x7758];

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Device interface GUID the firmware registers for `MI_00` (found in
/// `HKLM\SYSTEM\CurrentControlSet\Control\DeviceClasses`). A second GUID is
/// registered for the same function; both are tried.
const GUID_IFACE_A: Guid = Guid {
    data1: 0x6987_b675,
    data2: 0x8cbc,
    data3: 0x4bd3,
    data4: [0xa5, 0x57, 0xc6, 0x27, 0xce, 0xa2, 0x23, 0xdc],
};
const GUID_IFACE_B: Guid = Guid {
    data1: 0xdee8_24ef,
    data2: 0x729b,
    data3: 0x4a0e,
    data4: [0x9c, 0x14, 0xb7, 0x11, 0x7d, 0x33, 0xa8, 0x17],
};

#[repr(C)]
struct SpDeviceInterfaceData {
    cb_size: u32,
    interface_class_guid: Guid,
    flags: u32,
    reserved: usize,
}
// SP_DEVICE_INTERFACE_DATA: `Reserved` is UINT_PTR, so the size is
// pointer-width. A wrong size makes SetupDi calls fail in ways that look like
// "no such device" rather than "wrong struct".
const _: () = assert!(std::mem::size_of::<SpDeviceInterfaceData>() == std::mem::size_of::<usize>() + 24);

#[repr(C)]
#[derive(Clone, Copy)]
struct WinusbSetupPacket {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        sa: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

#[link(name = "winusb")]
extern "system" {
    fn WinUsb_Initialize(device: Handle, interface: *mut Handle) -> i32;
    fn WinUsb_Free(interface: Handle) -> i32;
    fn WinUsb_ControlTransfer(
        interface: Handle,
        setup: WinusbSetupPacket,
        buffer: *mut u8,
        length: u32,
        transferred: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsW(
        class_guid: *const Guid,
        enumerator: *const u16,
        hwnd: Handle,
        flags: u32,
    ) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        devices: Handle,
        dev_info: *mut c_void,
        class_guid: *const Guid,
        index: u32,
        data: *mut SpDeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailW(
        devices: Handle,
        data: *mut SpDeviceInterfaceData,
        detail: *mut u8,
        detail_size: u32,
        required: *mut u32,
        dev_info: *mut c_void,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(devices: Handle) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Symbolic paths of every present function registered under `guid`.
fn interface_paths(guid: &Guid) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let devices = SetupDiGetClassDevsW(
            guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if devices as isize == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut index = 0u32;
        loop {
            let mut data = SpDeviceInterfaceData {
                cb_size: std::mem::size_of::<SpDeviceInterfaceData>() as u32,
                interface_class_guid: Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
                flags: 0,
                reserved: 0,
            };
            if SetupDiEnumDeviceInterfaces(devices, std::ptr::null_mut(), guid, index, &mut data) == 0
            {
                break;
            }
            index += 1;
            let mut buf = vec![0u8; 1024];
            // SP_DEVICE_INTERFACE_DETAIL_DATA_W.cbSize is 4 + 2 bytes padded to
            // the pointer alignment: 8 on x64, 6 on x86.
            let cb = if std::mem::size_of::<usize>() == 8 { 8u32 } else { 6u32 };
            buf[0..4].copy_from_slice(&cb.to_ne_bytes());
            let mut required = 0u32;
            if SetupDiGetDeviceInterfaceDetailW(
                devices,
                &mut data,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut required,
                std::ptr::null_mut(),
            ) != 0
            {
                let path_ptr = buf[4..].as_ptr() as *const u16;
                let mut len = 0usize;
                while *path_ptr.add(len) != 0 {
                    len += 1;
                }
                out.push(String::from_utf16_lossy(std::slice::from_raw_parts(path_ptr, len)));
            }
        }
        SetupDiDestroyDeviceInfoList(devices);
    }
    out
}

/// One ARGB controller, opened through WinUSB.
///
/// Two handles, two owners: `WinUsb_Initialize` keeps the *file* handle as its
/// own — it does not adopt it — so both must be released, or the interface stays
/// claimed (WinUSB grants one handle per interface) and nobody else can open it
/// until this process exits.
pub struct Device {
    handle: Handle,
    iface: Handle,
    path: String,
}

impl Device {
    /// Find the controller and open it. `Err` carries a human-readable reason
    /// (no controller present / access denied / driver mismatch).
    pub fn open() -> Result<Device, String> {
        let mut paths = interface_paths(&GUID_IFACE_A);
        for p in interface_paths(&GUID_IFACE_B) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        let path = paths.into_iter().next().ok_or_else(|| {
            "没找到 ARGB 控制器（VID_0CF2 / PID_7750）——设备不在或驱动没起来".to_string()
        })?;
        Device::open_path(&path)
    }

    pub fn open_path(path: &str) -> Result<Device, String> {
        // The path identifies the function; the vendor/product check keeps a
        // wrong device from being poked if the GUID ever collides.
        let lower = path.to_ascii_lowercase();
        let wanted_vid = format!("vid_{VENDOR_ID:04x}");
        if !lower.contains(&wanted_vid) {
            return Err(format!("{path} 不是 ARGB 控制器（缺少 {wanted_vid}）"));
        }
        unsafe {
            let handle = CreateFileW(
                wide(path).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            );
            if handle as isize == INVALID_HANDLE_VALUE {
                let e = GetLastError();
                return Err(match e {
                    5 => format!("打不开 ARGB 控制器：拒绝访问（Win32 {e}）"),
                    _ => format!("CreateFile 失败（Win32 {e}）"),
                });
            }
            let mut iface: Handle = std::ptr::null_mut();
            if WinUsb_Initialize(handle, &mut iface) == 0 {
                let e = GetLastError();
                CloseHandle(handle);
                return Err(format!(
                    "WinUsb_Initialize 失败（Win32 {e}）——句柄必须带 FILE_FLAG_OVERLAPPED，且该接口要由 WinUSB 驱动接管"
                ));
            }
            Ok(Device { handle, iface, path: path.to_string() })
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// One vendor control transfer: `write` selects direction, `reg` is the
    /// register carried in `wIndex`.
    fn transfer(&self, write: bool, reg: u16, data: &mut [u8]) -> Result<usize, u32> {
        let setup = WinusbSetupPacket {
            request_type: if write { 0x40 } else { 0xC0 },
            request: if write { 0x80 } else { 0x81 },
            value: 0,
            index: reg,
            length: data.len() as u16,
        };
        let mut transferred = 0u32;
        let ok = unsafe {
            WinUsb_ControlTransfer(
                self.iface,
                setup,
                data.as_mut_ptr(),
                data.len() as u32,
                &mut transferred,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(transferred as usize)
        }
    }

    /// Read `len` bytes from register `reg`.
    pub fn read(&self, reg: u16, len: usize) -> Result<Vec<u8>, u32> {
        let mut buf = vec![0u8; len];
        let n = self.transfer(false, reg, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Write `data` to register `reg`. Returns bytes transferred.
    pub fn write(&self, reg: u16, data: &[u8]) -> Result<usize, u32> {
        let mut buf = data.to_vec();
        self.transfer(true, reg, &mut buf)
    }

    /// Read and write in one call — the shape every controller write uses.
    pub fn write_then_read(&self, reg: u16, data: &[u8], back: usize) -> Result<Vec<u8>, u32> {
        self.write(reg, data)?;
        self.read(reg, back)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            if !self.iface.is_null() {
                WinUsb_Free(self.iface);
            }
            if !self.handle.is_null() && self.handle as isize != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle);
            }
        }
    }
}

// The WinUSB interface handle is a process-wide handle and WinUSB itself
// serialises transfers per pipe; the owner always keeps it behind a mutex, so
// the only thing this assertion adds over the raw pointer is the ability to
// hold it in a `Mutex<Option<RgbEngine>>` shared with the render thread.
unsafe impl Send for Device {}
