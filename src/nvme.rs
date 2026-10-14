//! NVMe / drive temperature helpers.
//!
//! NOTE: this storage stack rejects direct NVMe protocol queries with
//! ERROR_INVALID_FUNCTION (tested: property 50/51 × len 48/52/56 all fail),
//! but smartmontools works fine when elevated — so we shell out to smartctl.

use std::ffi::c_void;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Run an external tool with a deadline.
///
/// `Command::output()` has no timeout: a wedged smartctl or nvidia-smi would
/// freeze the calling thread for ever — in the GUI that is the thread feeding
/// the control loop, and a frozen reading is worse than a missing one. stdout
/// goes to a temp file rather than a pipe so a chatty child cannot deadlock
/// against our polling.
pub fn run_tool(cmd: &mut std::process::Command, timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "fantune-probe-{}-{}.out",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = std::fs::File::create(&tmp).ok()?;
    // Without CREATE_NO_WINDOW every smartctl probe flashed a console window
    // on the user's desktop — twice per 10 s cycle (read_smart_full sweeps all
    // three drives, and the cycle re-runs whenever a probe fails). The tool is
    // a headless probe; its console is never wanted.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&tmp);
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
                return None;
            }
        }
    }
    let out = std::fs::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    out
}

/// smartctl can be slow on a busy disk; nvidia-smi is usually instant.
const SMARTCTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
pub const NVIDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Paths to try for smartctl, most specific first. On a machine that is not the
/// one this was developed on, `FANTUNE_SMARTCTL` points at the install; then the
/// usual Windows locations; `smartctl.exe` last so a PATH install still wins.
fn smartctl_candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("FANTUNE_SMARTCTL") {
        if !p.is_empty() {
            v.push(p);
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        // A scoop *shim* is a launcher that spawns the real binary: two
        // processes per query. Prefer the real one when it is installed.
        v.push(format!(
            "{home}\\scoop\\apps\\smartmontools\\current\\bin\\smartctl.exe"
        ));
        v.push(format!("{home}\\scoop\\shims\\smartctl.exe"));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        v.push(format!("{pf}\\smartmontools\\bin\\smartctl.exe"));
    }
    v.push("smartctl.exe".to_string());
    v
}

/// Same idea for nvidia-smi (the only GPU source we have — NVIDIA only).
pub fn nvidia_smi_candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("FANTUNE_NVIDIA_SMI") {
        if !p.is_empty() {
            v.push(p);
        }
    }
    v.push("C:\\Windows\\System32\\nvidia-smi.exe".to_string());
    if let Ok(pf) = std::env::var("ProgramFiles") {
        v.push(format!("{pf}\\NVIDIA Corporation\\NVSMI\\nvidia-smi.exe"));
    }
    v.push("nvidia-smi.exe".to_string());
    v
}

/// Physical drives to probe, in the order they get their index. smartctl on
/// Windows maps `/dev/sdX` by enumeration order, so the config keys
/// (`nvme:0`, `nvme:1`, …) are positional — swapping or adding a disk can move
/// them. `FANTUNE_DRIVES=sd0,sd1` overrides the list when that happens.
fn drives() -> Vec<String> {
    if let Ok(list) = std::env::var("FANTUNE_DRIVES") {
        let v: Vec<String> = list
            .split(',')
            .map(|s| s.trim().trim_start_matches("/dev/").to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    ["sda", "sdb", "sdc"].iter().map(|s| s.to_string()).collect()
}

/// GPU temp + utilization via nvidia-smi (no extra deps).
pub fn read_gpu() -> Option<(f32, f32)> {
    let mut out = None;
    for exe in nvidia_smi_candidates() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["--query-gpu=temperature.gpu,utilization.gpu", "--format=csv,noheader,nounits"]);
        if let Some(bytes) = run_tool(&mut cmd, NVIDIA_TIMEOUT) {
            out = Some(bytes);
            break;
        }
    }
    let out = out?;
    let s = String::from_utf8_lossy(&out);
    let mut it = s.split(',');
    let t: f32 = it.next()?.trim().parse().ok()?;
    let u: f32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0.0);
    Some((t, u))
}

/// NVMe temps via smartctl. Returns [(drive index, temp C)] for /dev/sda..sdc.
pub fn read_smart_drives() -> Vec<(usize, f32)> {
    let mut out = Vec::new();
    let candidates = smartctl_candidates();
    for (i, dev) in drives().iter().enumerate() {
        for exe in &candidates {
            let mut cmd = std::process::Command::new(exe);
            cmd.args(["-a", "-d", "nvme", &format!("/dev/{dev}")]);
            let Some(bytes) = run_tool(&mut cmd, SMARTCTL_TIMEOUT) else {
                continue;
            };
            let s = String::from_utf8_lossy(&bytes);
            let mut found = None;
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("Temperature:") {
                    if let Ok(t) = rest.trim().trim_end_matches("Celsius").trim().parse::<f32>() {
                        found = Some(t);
                    }
                    break;
                }
            }
            if let Some(t) = found {
                out.push((i, t));
            }
            break;
        }
    }
    out
}

/// One smartctl pass per drive: temperature, model, plus the extended health
/// fields shown under each drive row. Returns
/// [(drive index, temp C, model, [(label, value)])] for /dev/sda..sdc.
/// A drive whose temperature line is missing is skipped; individual health
/// fields are skipped when their line is missing.
pub fn read_smart_full() -> Vec<(usize, f32, String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    let candidates = smartctl_candidates();
    for (i, dev) in drives().iter().enumerate() {
        for exe in &candidates {
            let mut cmd = std::process::Command::new(exe);
            cmd.args(["-a", "-d", "nvme", &format!("/dev/{dev}")]);
            let Some(bytes) = run_tool(&mut cmd, SMARTCTL_TIMEOUT) else {
                continue;
            };
            if let Some(entry) = parse_smart(&String::from_utf8_lossy(&bytes), i) {
                out.push(entry);
            }
            break;
        }
    }
    out
}

/// Parse one smartctl report. Tolerant on purpose: values may carry thousands
/// separators ("13,773") and bracketed conversions ("175,000,686 [89.6 TB]").
/// Fields come out in a fixed order (寿命, 通电, 写入) regardless of the order
/// smartctl printed them in.
fn parse_smart(
    text: &str,
    index: usize,
) -> Option<(usize, f32, String, Vec<(String, String)>)> {
    let mut temp: Option<f32> = None;
    let mut model: Option<String> = None;
    let mut used: Option<f64> = None;
    let mut hours: Option<f64> = None;
    let mut written: Option<String> = None;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let rest = rest.trim();
        match key.trim() {
            "Temperature" => {
                if temp.is_none() {
                    temp = first_number(rest).map(|v| v as f32);
                }
            }
            "Model Number" => {
                if model.is_none() && !rest.is_empty() {
                    // collapse the alignment padding smartctl emits
                    model = Some(rest.split_whitespace().collect::<Vec<_>>().join(" "));
                }
            }
            "Percentage Used" => used = used.or_else(|| first_number(rest)),
            "Power On Hours" => hours = hours.or_else(|| first_number(rest)),
            "Data Units Written" => written = written.or_else(|| format_written(rest)),
            _ => {}
        }
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(v) = used {
        // smartctl reports wear used; expose what is left of the rated life
        fields.push(("寿命".into(), format!("{:.0}%", (100.0 - v).clamp(0.0, 100.0))));
    }
    if let Some(v) = hours {
        fields.push(("通电".into(), format!("{} h", v.max(0.0) as u64)));
    }
    if let Some(v) = written {
        fields.push(("写入".into(), v));
    }
    Some((index, temp?, model.unwrap_or_default(), fields))
}

/// First number in `s`, ignoring thousands separators and trailing units.
fn first_number(s: &str) -> Option<f64> {
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else if ch == ',' {
            // thousands separator — drop it
        } else if num.is_empty() {
            continue; // skip leading label text
        } else {
            break;
        }
    }
    num.parse().ok()
}

/// "175,000,686 [89.6 TB]" -> "89.6 TB"; without brackets, convert the raw
/// data-unit count (1 unit = 512,000 bytes) to TB.
fn format_written(rest: &str) -> Option<String> {
    if let (Some(l), Some(r)) = (rest.find('['), rest.find(']')) {
        if l < r {
            let inner = &rest[l + 1..r];
            if let Some(v) = first_number(inner) {
                let unit = if inner.contains("PB") {
                    "PB"
                } else if inner.contains("TB") {
                    "TB"
                } else if inner.contains("GB") {
                    "GB"
                } else {
                    "TB"
                };
                return Some(format!("{v:.1} {unit}"));
            }
        }
    }
    let units = first_number(rest)?;
    Some(format!("{:.1} TB", units * 512_000.0 / 1e12))
}

// ---------- direct NVMe protocol query (kept for future use; fails on this
// machine with ERROR_INVALID_FUNCTION) ----------

// IOCTL_STORAGE_QUERY_PROPERTY = CTL_CODE(0x2D, 0x0400, METHOD_BUFFERED, FILE_ANY_ACCESS)
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x2D1000;

const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 51;
const STORAGE_STANDARD_QUERY: u32 = 0;
const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_LOG_PAGE_HEALTH: u32 = 2;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLastError() -> u32;
    fn DeviceIoControl(
        h: *mut c_void,
        code: u32,
        in_buf: *mut c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
        ret: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

/// Debug probe: try the parameter matrix to find what this storage stack accepts.
pub fn probe(drive_index: usize) {
    let path = format!("\\\\.\\PhysicalDrive{drive_index}");
    let wide = to_wide(&path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000u32 | 0x4000_0000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle as isize == -1 {
        eprintln!("[probe] PD{drive_index} CreateFile failed err={}", unsafe { GetLastError() });
        return;
    }
    for &prop in &[50u32, 51u32] {
        for &in_len in &[48u32, 52u32, 56u32] {
            let mut in_buf = [0u8; 56];
            in_buf[0..4].copy_from_slice(&prop.to_le_bytes());
            in_buf[4..8].copy_from_slice(&STORAGE_STANDARD_QUERY.to_le_bytes());
            in_buf[8..12].copy_from_slice(&PROTOCOL_TYPE_NVME.to_le_bytes());
            in_buf[12..16].copy_from_slice(&NVME_DATA_TYPE_LOG_PAGE.to_le_bytes());
            in_buf[16..20].copy_from_slice(&NVME_LOG_PAGE_HEALTH.to_le_bytes());
            in_buf[24..28].copy_from_slice(&40u32.to_le_bytes());
            in_buf[28..32].copy_from_slice(&512u32.to_le_bytes());
            let mut out_buf = [0u8; 1024];
            let mut ret = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_QUERY_PROPERTY,
                    in_buf.as_mut_ptr() as *mut c_void,
                    in_len,
                    out_buf.as_mut_ptr() as *mut c_void,
                    out_buf.len() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            let err = unsafe { GetLastError() };
            eprintln!("[probe] PD{drive_index} prop={prop} in_len={in_len} -> ok={ok} err={err}");
        }
    }
    unsafe { CloseHandle(handle) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines copied verbatim from a real `smartctl -a -d nvme` report
    /// (smart_out.txt).
    const SAMPLE: &str = "\
Model Number:                       ZHITAI TiPlus7100 1TB
SMART/Health Information (NVMe Log 0x02, NSID 0xffffffff)
Critical Warning:                   0x00
Temperature:                        55 Celsius
Available Spare:                    100%
Percentage Used:                    14%
Data Units Written:                 175,000,686 [89.6 TB]
Power On Hours:                     13,773
Media and Data Integrity Errors:    0
";

    #[test]
    fn parses_real_smartctl_sample() {
        let (idx, temp, model, fields) = parse_smart(SAMPLE, 2).expect("temperature present");
        assert_eq!(idx, 2);
        assert!((temp - 55.0).abs() < 0.01);
        assert_eq!(model, "ZHITAI TiPlus7100 1TB");
        assert_eq!(
            fields,
            vec![
                ("寿命".to_string(), "86%".to_string()), // 100 - 14 used
                ("通电".to_string(), "13773 h".to_string()),
                ("写入".to_string(), "89.6 TB".to_string()),
            ]
        );
    }

    #[test]
    fn missing_temperature_skips_the_drive() {
        assert!(parse_smart("Percentage Used: 14%\n", 0).is_none());
    }

    #[test]
    fn missing_fields_are_skipped() {
        let (_, _, model, fields) = parse_smart("Temperature: 41 Celsius\n", 0).unwrap();
        assert!(model.is_empty());
        assert!(fields.is_empty());
    }

    #[test]
    fn written_falls_back_to_data_units_without_brackets() {
        let (_, _, _, fields) = parse_smart("Temperature: 30 C\nData Units Written: 175,000,686\n", 0)
            .unwrap();
        assert_eq!(fields, vec![("写入".to_string(), "89.6 TB".to_string())]);
    }
}