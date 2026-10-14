//! Per-logical-processor core temperature via IA32_THERM_STATUS (MSR 0x19C).
//!
//! The MSR lives in ring 0 and is reachable only through the WinRing0 driver,
//! so each read goes through `Driver::read_msr`. IA32_THERM_STATUS is a package
//! MSR whose digital readout is per-core, so the thread must run on the core it
//! wants to sample: we pin with SetThreadAffinityMask and restore the previous
//! mask afterwards.

use std::ffi::c_void;

use crate::driver::Driver;

const MSR_IA32_THERM_STATUS: u32 = 0x19C;
const MSR_IA32_TEMPERATURE_TARGET: u32 = 0x1A2;
/// Fallback when IA32_TEMPERATURE_TARGET is unreadable or reports 0.
const TJMAX_FALLBACK: f32 = 100.0;

/// The affinity mask is one `usize`, i.e. a single processor group (<= 64 CPUs).
const MAX_LOGICAL: usize = 64;
/// ALL_PROCESSOR_GROUPS
const ALL_PROCESSOR_GROUPS: u16 = 0xFFFF;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThread() -> *mut c_void;
    /// Returns the thread's previous affinity mask, or 0 on failure.
    fn SetThreadAffinityMask(thread: *mut c_void, mask: usize) -> usize;
    fn GetActiveProcessorCount(group: u16) -> u32;
}

/// Read one temperature per logical processor: `[("LP0", 47.0), ...]`.
///
/// Tjmax is a package-level value, read once from MSR 0x1A2 (bits 16-23) and
/// shared by every core. Cores whose affinity pin or MSR read fails are skipped
/// silently — no partial read is reported as an error.
pub fn read_core_temps(driver: &Driver) -> Vec<(String, f32)> {
    let logical = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
    let logical = logical.clamp(1, MAX_LOGICAL);

    let tjmax = driver
        .read_msr(MSR_IA32_TEMPERATURE_TARGET)
        .map(|m| ((m >> 16) & 0xFF) as f32)
        .filter(|v| *v > 0.0)
        .unwrap_or(TJMAX_FALLBACK);

    let mut out = Vec::new();
    for i in 0..logical {
        let thread = unsafe { GetCurrentThread() };
        let prev = unsafe { SetThreadAffinityMask(thread, 1usize << i) };
        if prev == 0 {
            continue; // core outside this process's affinity — skip it
        }
        let temp = read_therm(driver, tjmax);
        unsafe { SetThreadAffinityMask(thread, prev) };
        if let Some(t) = temp {
            out.push((format!("LP{i}"), t));
        }
    }
    out
}

/// One core's temperature, after validating the readout bits.
fn read_therm(driver: &Driver, tjmax: f32) -> Option<f32> {
    let msr = driver.read_msr(MSR_IA32_THERM_STATUS)?;
    // bit31 = readout valid; a failed transaction already surfaces as None
    // from read_msr, so a zero readout here is a real reading: the field
    // saturates at 0, meaning "at or above Tjmax". Never hide that core.
    if msr & (1 << 31) == 0 {
        return None;
    }
    let readout = ((msr >> 16) & 0x7F) as f32;
    let t = tjmax - readout;
    (0.0..=125.0).contains(&t).then_some(t)
}
