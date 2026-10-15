//! fan-tune GUI — the single owner of the fan hardware while it runs: it stops
//! the background daemon at startup and hands control back (daemon via the
//! scheduled task) when it exits. One writer at a time.
//!
//! Design language: **RhinE** — Swiss grid, a monochrome palette carrying one
//! accent colour, monospace type with wide-tracked uppercase micro labels,
//! 1 px hairlines, zero corner radius, no shadows. Tokens and the hand-drawn
//! primitives live in [`theme`].

use eframe::egui;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{eval_curve, AppConfig, FanChannelConfig, FanMode, FanProfile, SourceCombine};
use crate::control::{ControlPass, PassInput, QUIET_DUTY};
use crate::nct::{Nct, NctReading, FAN_COUNT};
use crate::nvme;
use fan_tune::rgb::{self, Diffusion, RgbEngine, RgbProfile, Shape, Strategy};

// ---------- palette ----------
//
// Colours are RhinE design tokens, read through these accessors rather than
// baked in as constants: the active palette is switchable at runtime (the top
// bar carries a scheme picker), so every colour has to be looked up per frame.
// See `theme` for the token tables.
mod theme;

#[inline]
/// Over-temperature warnings for the banner. Extracted because the same two
/// thresholds also decide the light controller's `alert` input — two copies
/// would drift, and "the fans are blowing but the banner says nothing" is
/// exactly the kind of disagreement that costs an afternoon.
fn build_alerts(temps: &[(String, f32)], nvme: &[(usize, f32)]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(t) = temps.iter().find(|(n, _)| n.contains("PECI")).map(|(_, t)| *t) {
        if t >= ALERT_CPU_C {
            out.push(format!("CPU {t:.0}°C 过热"));
        }
    }
    for (i, t) in nvme {
        if *t >= ALERT_DISK_C {
            out.push(format!("硬盘 {i} {t:.0}°C 偏高"));
        }
    }
    out
}

/// True when any alert condition holds — the light controller reads the same
/// rule as the banner, by construction rather than by copy.
fn any_alert(temps: &[(String, f32)], nvme: &[(usize, f32)]) -> bool {
    !build_alerts(temps, nvme).is_empty()
}
fn panel() -> egui::Color32 {
    theme::t().panel
}
#[inline]
fn bg() -> egui::Color32 {
    theme::t().bg
}
#[inline]
fn card() -> egui::Color32 {
    theme::t().case
}
#[inline]
fn card_hover() -> egui::Color32 {
    theme::t().case_hi
}
#[inline]
fn text() -> egui::Color32 {
    theme::t().fg
}
#[inline]
fn text_dim() -> egui::Color32 {
    theme::t().muted
}
#[inline]
fn accent() -> egui::Color32 {
    theme::t().accent
}
#[inline]
fn warn() -> egui::Color32 {
    theme::t().warn
}
#[inline]
fn hot() -> egui::Color32 {
    theme::t().hot
}

/// Reading colour for a temperature: quiet grey → soft grey → accent → red.
fn temp_color(t: f32) -> egui::Color32 {
    theme::t().temp_color(t)
}

fn lerp_color(a: egui::Color32, b: egui::Color32, f: f32) -> egui::Color32 {
    let f = f.clamp(0.0, 1.0);
    egui::Color32::from_rgb(
        (a.r() as f32 + (b.r() as f32 - a.r() as f32) * f) as u8,
        (a.g() as f32 + (b.g() as f32 - a.g() as f32) * f) as u8,
        (a.b() as f32 + (b.b() as f32 - a.b() as f32) * f) as u8,
    )
}

/// How many 2-second samples the left-panel history keeps (300 * 2s = 10 min).
pub const HIST_CAP: usize = 300;

/// Total wall time of the boot self-test sequence (overlay at the end of
/// `update`): 0.15 s lead-in + 8 lines × 0.22 s typing + 0.9 s hold + 0.45 s
/// fade.
const BOOT_DUR: f32 = 3.4;
/// CPU (PECI) temperature at or above which the banner warns.
const ALERT_CPU_C: f32 = 85.0;
/// Drive temperature at or above which the banner warns.
const ALERT_DISK_C: f32 = 65.0;

/// One 2-second sample of the history chart.
#[derive(Clone, Copy, Default)]
pub struct HistPoint {
    pub cpu: Option<f32>,
    pub nvme: Option<f32>,
    pub duty: f32,
}

pub struct Shared {
    pub reading: NctReading,
    pub nvme: Vec<(usize, f32)>,
    /// drive model names (same indexes as `nvme`)
    pub nvme_model: Vec<(usize, String)>,
    /// per-drive extended SMART fields, same drive indexes as `nvme`
    pub nvme_ext: Vec<(usize, Vec<(String, String)>)>,
    /// per-logical-processor temps (MSR 0x19C), refreshed on the 2s beat
    pub core_temps: Vec<(String, f32)>,
    pub gpu: Option<(f32, f32)>, // (temp C, util %)
    pub gpu_ext: Vec<(String, String)>, // additional gpu key-values
    pub sys: SysInfo,
    pub chip: String,
    /// how many fan channels the detected chip actually has
    pub fans: usize,
    pub last_update: Instant,
    /// false until the poller has produced its first reading: lets the control
    /// loop tell "still warming up" (nothing to judge) apart from "the reading
    /// went empty or stale" (a dead source, count it).
    pub has_reading: bool,
    /// duty the control loop computed this tick per channel (None = not driven)
    pub targets: [Option<f32>; 7],
    /// ring buffer for the left-panel history chart
    pub hist: VecDeque<HistPoint>,
}

/// Channel name shown to the user: `label` when set, `name` otherwise.
fn channel_display_name(c: &FanChannelConfig) -> String {
    if c.label.is_empty() {
        c.name.clone()
    } else {
        c.label.clone()
    }
}

/// Drive label for the UI: the model name (recognizable) when known, else the
/// bare index. Full name kept on hover by the caller.
fn drive_display_name(i: usize, models: &[(usize, String)]) -> String {
    match models.iter().find(|(idx, _)| *idx == i) {
        Some((_, m)) if !m.is_empty() => {
            let mut short: String = m.chars().take(24).collect();
            if m.chars().count() > 24 {
                short.push('…');
            }
            format!("硬盘 {i} · {short}")
        }
        _ => format!("硬盘 {i}"),
    }
}

#[derive(Default, Clone, Copy)]
pub struct SysInfo {
    pub cpu_load: f32,  // %
    pub ram_load: f32,  // %
    pub ram_used: f32,  // GB
    pub ram_total: f32, // GB
}

#[derive(Default, Clone)]
pub struct GpuInfo {
    pub temp: Option<f32>,
    pub mem_temp: Option<f32>,
    pub util: Option<f32>,
    pub mem_used: Option<f32>,  // MB
    pub mem_total: Option<f32>, // MB
    pub power: Option<f32>,     // W
    pub fan: Option<f32>,       // %
}

/// Query a batch of GPU stats via nvidia-smi; "N/A" fields become None.
/// One spawn per call — the caller decides how often (see the sensor thread's
/// 5s cache: a spawn per second was the largest waste in the whole program).
fn read_gpu_ext() -> GpuInfo {
    let mut g = GpuInfo::default();
    let mut out = None;
    for exe in nvme::nvidia_smi_candidates() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args([
            "--query-gpu=temperature.gpu,temperature.memory,utilization.gpu,memory.used,memory.total,power.draw,fan.speed",
            "--format=csv,noheader,nounits",
        ]);
        if let Some(bytes) = nvme::run_tool(&mut cmd, nvme::NVIDIA_TIMEOUT) {
            out = Some(bytes);
            break;
        }
    }
    let Some(bytes) = out else { return g };
    let s = String::from_utf8_lossy(&bytes);
    let fields: Vec<&str> = s.trim().split(',').map(|x| x.trim()).collect();
    let parse = |i: usize| -> Option<f32> {
        fields.get(i).and_then(|f| f.parse::<f32>().ok())
    };
    g.temp = parse(0);
    g.mem_temp = parse(1);
    g.util = parse(2);
    g.mem_used = parse(3);
    g.mem_total = parse(4);
    g.power = parse(5);
    g.fan = parse(6);
    g
}

fn cpu_usage() -> Option<f32> {
    #[repr(C)]
    struct FT { lo: u32, hi: u32 }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemTimes(idle: *mut FT, kernel: *mut FT, user: *mut FT) -> i32;
    }
    let to_u64 = |ft: &FT| ((ft.hi as u64) << 32) | ft.lo as u64;
    unsafe {
        let (mut i1, mut k1, mut u1) = (FT { lo: 0, hi: 0 }, FT { lo: 0, hi: 0 }, FT { lo: 0, hi: 0 });
        if GetSystemTimes(&mut i1, &mut k1, &mut u1) == 0 {
            return None;
        }
        std::thread::sleep(Duration::from_millis(400));
        let (mut i2, mut k2, mut u2) = (FT { lo: 0, hi: 0 }, FT { lo: 0, hi: 0 }, FT { lo: 0, hi: 0 });
        if GetSystemTimes(&mut i2, &mut k2, &mut u2) == 0 {
            return None;
        }
        let idle = to_u64(&i2) - to_u64(&i1);
        let total = (to_u64(&k2) - to_u64(&k1)) + (to_u64(&u2) - to_u64(&u1));
        if total == 0 {
            return None;
        }
        Some(((total - idle) as f32 / total as f32 * 100.0).clamp(0.0, 100.0))
    }
}

fn ram_info() -> Option<(f32, f32, f32)> {
    // MEMORYSTATUSEX — nine fields, 64 bytes. A short struct makes the API
    // reject the call (dwLength must equal sizeof(MEMORYSTATUSEX)), which is
    // why this used to silently report 0/0 GB.
    #[repr(C)]
    struct MSX { dw_length: u32, dw_memory_load: u32, total: u64, avail: u64, total_page: u64, avail_page: u64, total_virt: u64, avail_virt: u64, avail_ext: u64 }
    // The call is rejected unless dwLength == sizeof(MEMORYSTATUSEX) == 64,
    // and the failure mode is a silent 0/0 GB report rather than an error —
    // this already happened once. Pin the size so it cannot happen again.
    const _: () = assert!(std::mem::size_of::<MSX>() == 64);
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(p: *mut MSX) -> i32;
    }
    let mut m = MSX {
        dw_length: std::mem::size_of::<MSX>() as u32,
        dw_memory_load: 0, total: 0, avail: 0, total_page: 0, avail_page: 0, total_virt: 0, avail_virt: 0, avail_ext: 0,
    };
    unsafe {
        if GlobalMemoryStatusEx(&mut m) == 0 {
            return None;
        }
    }
    let total = m.total as f32 / 1073741824.0;
    let used = (m.total - m.avail) as f32 / 1073741824.0;
    Some((m.dw_memory_load as f32, used, total))
}

/// System stats poller thread (CPU load + RAM + per-core temps), ~2s cadence.
/// Also the single producer of the history ring buffer. GPU stats come from the
/// sensor thread's cache — this loop must not spawn nvidia-smi of its own.
/// Monotonic origin for the sensor poller's slow-refresh schedule. Wall-clock
/// time can step backwards (NTP, resume from suspend); this cannot.
static POLL_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
fn sys_poller(shared: Arc<Mutex<Shared>>, driver: crate::driver::Driver) {
    loop {
        let cpu = cpu_usage();
        let ram = ram_info();
        // MSR reads hop cores via affinity — do it outside the shared lock
        let cores = crate::cpu::read_core_temps(&driver);
        if let Ok(mut s) = shared.lock() {
            if let Some((load, used, total)) = ram {
                s.sys.ram_load = load;
                s.sys.ram_used = used;
                s.sys.ram_total = total;
            }
            if let Some(cl) = cpu {
                s.sys.cpu_load = cl;
            }
            s.core_temps = cores;
            // history: one point per 2s tick (CPU temp / hottest drive / max duty)
            let cpu_t = s
                .reading
                .temps
                .iter()
                .find(|(n, _)| n.contains("PECI"))
                .map(|(_, t)| *t);
            let hot_nvme = s
                .nvme
                .iter()
                .map(|(_, t)| *t)
                .fold(None, |a: Option<f32>, b| Some(a.map_or(b, |x| x.max(b))));
            let max_duty = s
                .reading
                .controls_pct
                .iter()
                .filter_map(|x| *x)
                .fold(0.0f32, f32::max);
            s.hist.push_back(HistPoint { cpu: cpu_t, nvme: hot_nvme, duty: max_duty });
            while s.hist.len() > HIST_CAP {
                s.hist.pop_front();
            }
        }
        std::thread::sleep(Duration::from_millis(2000));
    }
}

/// The glyph slot at the left of a sensor row. The colour is carried by the
/// variant so a row can go quiet (line_dim / mark) without the caller having to
/// reason about which icon function wants which colour argument.
enum RowIcon {
    None,
    Temp(egui::Color32),
    Drive(egui::Color32),
    Chip(egui::Color32),
    Fan { color: egui::Color32, spin: f32 },
}

/// One row of the sensor rail: a wide-tracked micro label on the left, its value
/// pushed to the right edge in mono, and a hairline closing the row. The rule is
/// deliberately faint — the table should read as a ruled sheet, not as a stack
/// of list items.
fn sensor_row(
    ui: &mut egui::Ui,
    name: &str,
    value: &str,
    color: egui::Color32,
    icon: RowIcon,
) -> egui::Response {
    let tk = theme::t();
    let inner = ui
        .horizontal(|ui| {
            ui.set_min_height(theme::space::ROW);
            ui.set_min_width(ui.available_width());
            let (slot, _) = ui.allocate_exact_size(
                egui::vec2(16.0, theme::space::ROW),
                egui::Sense::hover(),
            );
            let p = ui.painter();
            let mid = |h: f32| slot.min + egui::vec2(1.0, (theme::space::ROW - h) * 0.5);
            match icon {
                RowIcon::None => {}
                RowIcon::Temp(c) => draw_temp_icon(p, mid(11.0), 11.0, c),
                RowIcon::Drive(c) => draw_drive_icon(p, mid(12.0) - egui::vec2(1.0, 0.0), 12.0, c),
                RowIcon::Chip(c) => draw_chip_icon(p, mid(12.0), 12.0, c),
                RowIcon::Fan { color, spin } => draw_fan_icon(p, slot.center(), 6.0, color, spin),
            }
            // The name gets whatever width the value column does not need, and
            // truncates there: a full drive model string used to run straight
            // under the right-aligned value.
            let name_w = (ui.available_width() - 16.0 - 104.0).max(48.0);
            ui.allocate_ui(egui::vec2(name_w, theme::space::ROW), |ui| {
                ui.add(
                    egui::Label::new(theme::micro(name, tk.muted))
                        .wrap_mode(egui::TextWrapMode::Truncate),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // the value column truncates at the row edge: an unconstrained
                // long value (OS build strings, drive stats) used to widen the
                // whole row past the panel and push the centre stage 100+ pt
                // to the right — the "black band". Full text on hover.
                ui.add(
                    egui::Label::new(theme::num(
                        value,
                        theme::size::LABEL,
                        theme::Weight::Regular,
                        color,
                    ))
                    .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .on_hover_text(value);
            });
        });
    let row = inner.response.rect;
    theme::hline(
        ui.painter(),
        egui::Rangef::new(row.left(), row.right()),
        row.bottom(),
        tk.line_faint,
    );
    inner.response
}

/// Left-panel history sparkline (~100px): CPU temp, hottest drive temp and
/// max duty over the last ~10 minutes. Hand-painted engineering plot: an inset
/// well, hairline guides, mono tick labels and square end markers.
fn draw_history(ui: &mut egui::Ui, hist: &VecDeque<HistPoint>) {
    let tk = theme::t();
    ui.horizontal(|ui| {
        ui.label(theme::micro("历史", tk.muted));
        let legend = |ui: &mut egui::Ui, color: egui::Color32, text: &str| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(r, egui::Rounding::ZERO, color);
            ui.label(theme::micro(text, tk.mark));
        };
        legend(ui, accent(), "CPU");
        legend(ui, tk.fg_soft, "最热硬盘");
        legend(ui, tk.mark, "最大占空");
    });
    // spread statistics — min..max per series in the legend row, so the
    // chart can be read without tracing it
    let spread = |pick: &dyn Fn(&HistPoint) -> Option<f32>| -> Option<(f32, f32)> {
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for hp in hist {
            if let Some(v) = pick(hp) {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        (lo <= hi).then_some((lo, hi))
    };
    if let Some((lo, hi)) = spread(&|hp| hp.cpu) {
        ui.label(theme::micro(&format!("CPU {lo:.0}–{hi:.0}°"), tk.line_dim));
    }
    if let Some((lo, hi)) = spread(&|hp| hp.nvme) {
        ui.label(theme::micro(&format!("盘 {lo:.0}–{hi:.0}°"), tk.line_dim));
    }
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.add_space(theme::space::HALF);
        ui.label(theme::micro("近 10 分钟", tk.line_dim));
    });
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 100.0),
        egui::Sense::hover(),
    );
    let p = ui.painter();
    p.rect_filled(rect, egui::Rounding::ZERO, tk.well);
    theme::box_line(p, rect, tk.hair);
    let n = hist.len();
    if n < 2 {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "采集中...",
            theme::Weight::Regular.font(theme::size::LABEL),
            text_dim(),
        );
        return;
    }
    let inner = rect.shrink(4.0);
    let x_at = |k: usize| inner.left() + (k as f32 / (n - 1) as f32) * inner.width();
    let y_temp = |t: f32| inner.bottom() - ((t - 20.0) / 80.0).clamp(0.0, 1.0) * inner.height();
    let y_duty = |d: f32| inner.bottom() - (d / 100.0).clamp(0.0, 1.0) * inner.height();
    // guide lines, one per scale: 50°C on the temp mapping, 50% on the duty
    // mapping (they sit at slightly different heights — by design)
    theme::hline(p, egui::Rangef::new(inner.left(), inner.right()), y_temp(50.0), tk.line_faint);
    theme::hline(p, egui::Rangef::new(inner.left(), inner.right()), y_duty(50.0), tk.line_faint);
    let tick = theme::Weight::Regular.font(theme::size::MICRO);
    let tick_font = tick.clone();
    p.text(
        egui::pos2(inner.left() + 3.0, y_temp(50.0) - 2.0),
        egui::Align2::LEFT_BOTTOM,
        "50°",
        tick.clone(),
        tk.mark,
    );
    p.text(
        egui::pos2(inner.left() + 3.0, y_duty(50.0) - 2.0),
        egui::Align2::LEFT_BOTTOM,
        "50%",
        tick,
        tk.mark,
    );
    let series = |pick: &dyn Fn(&HistPoint) -> Option<f32>,
                  y: &dyn Fn(f32) -> f32,
                  color: egui::Color32| {
        let mut seg: Vec<egui::Pos2> = Vec::new();
        let flush = |seg: &mut Vec<egui::Pos2>| {
            if seg.len() >= 2 {
                p.add(egui::Shape::line(seg.clone(), egui::Stroke::new(1.5_f32, color)));
            }
            seg.clear();
        };
        for (k, hp) in hist.iter().enumerate() {
            match pick(hp) {
                Some(v) => seg.push(egui::pos2(x_at(k), y(v))),
                None => flush(&mut seg),
            }
        }
        flush(&mut seg);
        // live square on the newest sample — the "now" end of the line
        if let Some(hp) = hist.back() {
            if let Some(v) = pick(hp) {
                let c = egui::pos2(x_at(n - 1), y(v));
                p.rect_filled(
                    egui::Rect::from_center_size(c, egui::Vec2::splat(5.0)),
                    egui::Rounding::ZERO,
                    color,
                );
            }
        }
    };
    series(&|hp| Some(hp.duty), &y_duty, tk.mark);
    series(&|hp| hp.nvme, &y_temp, tk.fg_soft);
    series(&|hp| hp.cpu, &y_temp, accent());
    // now-values at the line ends: the chart's right edge is its readout
    if let Some(hp) = hist.back() {
        if let Some(v) = hp.cpu {
            p.text(
                egui::pos2(inner.right() + 2.0, y_temp(v)),
                egui::Align2::RIGHT_CENTER,
                format!("{v:.0}°"),
                tick_font.clone(),
                accent(),
            );
        }
        if let Some(v) = hp.nvme {
            p.text(
                egui::pos2(inner.right() + 2.0, y_temp(v) - 11.0),
                egui::Align2::RIGHT_CENTER,
                format!("{v:.0}°"),
                tick_font.clone(),
                tk.fg_soft,
            );
        }
        p.text(
            egui::pos2(inner.right() + 2.0, y_duty(hp.duty)),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}%", hp.duty),
            tick_font.clone(),
            tk.mark,
        );
    }
}

// ---------- single writer: takeover / handover ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FanOverride {
    #[default]
    None,
    /// all channels 100%
    Full,
    /// all channels 40%
    Quiet,
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn msg_box(text: &str, title: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut core::ffi::c_void, text: *const u16, cap: *const u16, ty: u32) -> i32;
    }
    let t: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let c: Vec<u16> = title.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), 0x40);
    }
}

/// Named-mutex single instance guard. Returns the mutex handle, which must be
/// kept alive for the process lifetime AND released (via [`release_singleton`])
/// right before handing over to a successor — otherwise the successor's own
/// guard refuses to start ("already running").
fn ensure_single_instance() -> Option<*mut core::ffi::c_void> {
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(sa: *mut core::ffi::c_void, initial: i32, name: *const u16) -> *mut core::ffi::c_void;
        fn GetLastError() -> u32;
    }
    let name: Vec<u16> = "FanTuneGuiSingleton\0".encode_utf16().collect();
    let h = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    const ERROR_ALREADY_EXISTS: u32 = 183;
    if h.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        None
    } else {
        Some(h)
    }
}

/// Release the singleton mutex so a successor instance may start.
fn release_singleton(h: *mut core::ffi::c_void) {
    #[link(name = "kernel32")]
    extern "system" {
        fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
    }
    unsafe {
        CloseHandle(h);
    }
}

/// Bring the already-running instance's window to the front — called when a
/// second launch finds the singleton taken (double-clicking the exe while the
/// tray instance runs should open the panel, not dead-end on a message box).
fn focus_existing_window() -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut core::ffi::c_void;
        fn ShowWindow(h: *mut core::ffi::c_void, cmd: i32) -> i32;
        fn SetForegroundWindow(h: *mut core::ffi::c_void) -> i32;
    }
    let title: Vec<u16> = "fan-tune · 风扇调校\0".encode_utf16().collect();
    unsafe {
        let h = FindWindowW(std::ptr::null(), title.as_ptr());
        if h.is_null() {
            return false;
        }
        ShowWindow(h, 5); // SW_SHOW — a hidden window has no taskbar entry
        ShowWindow(h, 9); // SW_RESTORE
        SetForegroundWindow(h);
        true
    }
}

/// Show + focus our own window through win32 directly.
///
/// While the window is hidden or minimized winit produces no redraw events,
/// so eframe runs no frames — and `ViewportCommand`s queued from another
/// thread (tray "显示面板", the maintenance watcher) would sit unapplied
/// forever. Those paths must wake the window with a real ShowWindow call;
/// being same-process, the call is also unaffected by UIPI (unlike anything
/// an external helper script could do from a lower integrity level).
fn wake_own_window() {
    #[link(name = "user32")]
    extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut core::ffi::c_void;
        fn ShowWindow(h: *mut core::ffi::c_void, cmd: i32) -> i32;
        fn SetForegroundWindow(h: *mut core::ffi::c_void) -> i32;
    }
    let title: Vec<u16> = "fan-tune · 风扇调校\0".encode_utf16().collect();
    unsafe {
        let h = FindWindowW(std::ptr::null(), title.as_ptr());
        if h.is_null() {
            return;
        }
        ShowWindow(h, 5); // SW_SHOW — covers the hidden state
        ShowWindow(h, 9); // SW_RESTORE — covers the minimized state
        SetForegroundWindow(h);
    }
}

/// True if a `fan-tune.exe daemon` process is currently running.
fn daemon_alive() -> bool {
    use std::os::windows::process::CommandExt;
    let script = "(Get-CimInstance Win32_Process -Filter \"Name='fan-tune.exe'\" | Where-Object { $_.CommandLine -match 'daemon' } | Measure-Object).Count";
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().unwrap_or(0) > 0,
        Err(_) => false,
    }
}

/// Block until no daemon process remains — polls inside a single PowerShell
/// instance (one spawn, not one per tick). true = daemon is gone.
fn wait_daemon_exit(timeout_ms: u64) -> bool {
    use std::os::windows::process::CommandExt;
    let script = format!(
        "$d=(Get-Date).AddMilliseconds({timeout_ms}); \
         while((Get-Date) -lt $d) {{ \
           $n=(Get-CimInstance Win32_Process -Filter \"Name='fan-tune.exe'\" | \
           Where-Object {{ $_.CommandLine -match 'daemon' }} | Measure-Object).Count; \
           if($n -eq 0) {{ exit 0 }}; Start-Sleep -Milliseconds 400 }}; exit 1"
    );
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o.status.success(),
        Err(_) => !daemon_alive(),
    }
}

/// GUI takes ownership of the fan hardware. Preferred path: ask the daemon to
/// exit via the sentinel file (clean handle teardown — keeps the driver
/// healthy). Only if that times out do we force-kill (which can wedge the
/// kernel driver until reboot). Never uses `schtasks /End` — the GUI itself
/// may BE the task instance (tray autostart), and /End would kill us.
fn take_over_from_daemon() {
    use std::os::windows::process::CommandExt;
    if !daemon_alive() {
        return;
    }
    // 1) graceful: sentinel → daemon exits its loop and closes handles
    let sentinel = crate::driver::stop_sentinel_path();
    let _ = std::fs::write(&sentinel, "stop");
    let graceful = wait_daemon_exit(8000);
    let _ = std::fs::remove_file(&sentinel);
    if graceful {
        glog("daemon exited gracefully");
        return;
    }
    // 2) last resort: force kill (matches strictly on the `daemon` argument,
    // so we never kill ourselves)
    glog("graceful stop timed out, forcing");
    let script = "Get-CimInstance Win32_Process -Filter \"Name='fan-tune.exe'\" | Where-Object { $_.CommandLine -match 'daemon' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    for _ in 0..3 {
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if wait_daemon_exit(5000) {
            return;
        }
    }
    glog("warning: daemon still alive after force attempts");
}

/// Hand control to a successor on a NORMAL exit: a direct detached spawn, not
/// the scheduled task — the task refuses to start a second instance while ours
/// still runs (IgnoreNew), and the successor must open the driver device
/// BEFORE we release it, or the kernel driver unloads and hangs for minutes.
/// The task takes over again at the next logon. Returns whether a successor
/// was launched; the caller then waits briefly for the handle overlap.
fn start_daemon_fallback() -> bool {
    use std::os::windows::process::CommandExt;
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("tray").creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        match cmd.spawn() {
            Ok(_) => return true,
            Err(e) => glog(&format!("successor spawn failed: {e}")),
        }
    }
    // last resort: the task scheduler (works once we have actually exited)
    for _ in 0..4 {
        if let Ok(o) = std::process::Command::new("schtasks")
            .args(["/Run", "/TN", "FanTune"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if o.status.success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(600));
    }
    false
}

// ---------- in-GUI control loop (the daemon's logic, ported) ----------

/// Resolve a temperature-source key ("nct:NAME" / "nvme:IDX" / "gpu:0")
/// against the poller thread's cached readings. Used by the panels; the
/// control loop resolves through [`crate::control`].
fn resolve_key(
    key: &str,
    temps: &[(String, f32)],
    nvme: &[(usize, f32)],
    gpu: Option<f32>,
) -> Option<f32> {
    if key.starts_with("gpu") {
        return gpu;
    }
    if let Some(idx) = key.strip_prefix("nvme:") {
        let i: usize = idx.parse().ok()?;
        return nvme.iter().find(|(n, _)| *n == i).map(|(_, t)| *t);
    }
    if let Some(name) = key.strip_prefix("nct:") {
        return temps.iter().find(|(n, _)| n == name).map(|(_, t)| *t);
    }
    None
}

/// Write every non-Auto channel according to the config right now — the
/// immediate write after an overlay clears. Profile effects (gaming offset,
/// server smoothing/guard) are engine-side and land on the next tick.
fn apply_all_channels(
    nct: &Nct,
    cfg: &AppConfig,
    temps: &[(String, f32)],
    nvme: &[(usize, f32)],
    gpu: Option<f32>,
) {
    for (i, c) in cfg.channels.iter().enumerate().take(nct.fan_count()) {
        if !c.managed() {
            continue; // enabled: false = leave it to the BIOS
        }
        match c.mode {
            FanMode::Manual => {
                let _ = nct.set_duty_pct(i, c.manual_duty.clamp(0.0, 100.0));
            }
            FanMode::Curve => {
                let keys: &[String] = if c.sources.is_empty() {
                    std::slice::from_ref(&c.source)
                } else {
                    &c.sources
                };
                let vals: Vec<f32> = keys
                    .iter()
                    .filter_map(|k| resolve_key(k, temps, nvme, gpu))
                    .collect();
                if vals.is_empty() {
                    continue; // same as the engine: nothing to follow yet
                }
                let t = match c.combine {
                    SourceCombine::Max => vals.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                    SourceCombine::Avg => vals.iter().sum::<f32>() / vals.len() as f32,
                };
                let _ = nct.set_duty_pct(i, eval_curve(&c.points, t));
            }
            // Budget channels are driven by the control loop's engine (it owns
            // the loop's persistent state); a one-shot write here has no
            // previous duty to nudge from — leave them to the loop.
            FanMode::Budget => {}
            FanMode::Auto => {}
        }
    }
}

/// Publish this pass's per-channel target duties for the UI (D1).
fn publish_targets(shared: &Arc<Mutex<Shared>>, targets: [Option<f32>; 7]) {
    if let Ok(mut s) = shared.lock() {
        s.targets = targets;
    }
}

/// Everything the light strategies may read, in the same key space the fan
/// curves use (`nct:...`, `gpu:0`, `nvme:i`) so the panel can offer one list.
fn rgb_inputs(
    shared: &Arc<Mutex<Shared>>,
    cfg: &Arc<Mutex<AppConfig>>,
    ovr: &Arc<Mutex<FanOverride>>,
) -> rgb::Inputs {
    let mut inp = rgb::Inputs::default();
    if let Ok(s) = shared.lock() {
        inp.temps.extend(s.reading.temps.iter().cloned());
        for (i, t) in &s.nvme {
            inp.temps.push((format!("nvme:{i}"), *t));
        }
        if let Some((t, _)) = s.gpu {
            inp.temps.push(("gpu:0".to_string(), t));
        }
        for (i, d) in s.reading.controls_pct.iter().enumerate().take(7) {
            inp.duty[i] = *d;
        }
        // the same rule as the banner, by construction: one definition, so
        // the lights cannot disagree with what the panel is telling you
        inp.alert = any_alert(&s.reading.temps, &s.nvme);
    }
    if let Ok(c) = cfg.lock() {
        inp.profile = match c.profile {
            FanProfile::Standard => 0,
            FanProfile::Gaming => 1,
            FanProfile::Server => 2,
        };
    }
    if let Ok(o) = ovr.lock() {
        inp.quiet = matches!(*o, FanOverride::Quiet);
        inp.full = matches!(*o, FanOverride::Full);
    }
    inp
}

/// Strategy kinds the panel offers, in menu order.
const STRATEGY_KINDS: [&str; 6] = ["关闭", "固定色", "温度色阶", "温度水位", "占空色", "状态色"];

fn strategy_kind(s: &Strategy) -> usize {
    match s {
        Strategy::Off => 0,
        Strategy::Static { .. } => 1,
        Strategy::TempColor { .. } => 2,
        Strategy::TempLevel { .. } => 3,
        Strategy::DutyColor { .. } => 4,
        Strategy::Status { .. } => 5,
    }
}

fn strategy_kind_label(s: &Strategy) -> &'static str {
    STRATEGY_KINDS[strategy_kind(s)]
}

/// Switch a segment to another strategy kind, carrying over the colours the
/// user already tuned where the kinds share them.
fn strategy_with_kind(prev: &Strategy, kind: usize, source: &str) -> Strategy {
    let (cold, warm) = match prev {
        Strategy::TempColor { cold, warm, .. } | Strategy::TempLevel { cold, warm, .. } => {
            (*cold, *warm)
        }
        Strategy::Static { color } => (*color, *color),
        _ => ([0, 0, 255], [255, 0, 0]),
    };
    match kind {
        1 => Strategy::Static { color: warm },
        2 => Strategy::TempColor {
            source: source.to_string(),
            cold,
            warm,
            t_cold: 30.0,
            t_warm: 75.0,
        },
        3 => Strategy::TempLevel {
            source: source.to_string(),
            cold,
            warm,
            t_min: 30.0,
            t_max: 75.0,
        },
        4 => Strategy::DutyColor { channel: 0, low: cold, high: warm },
        5 => Strategy::Status {
            standard: [0, 60, 130],
            gaming: [130, 70, 0],
            server: [0, 90, 90],
            quiet: [12, 12, 24],
            full: [255, 40, 0],
            alert: [255, 0, 0],
        },
        _ => Strategy::Off,
    }
}

/// Sensor picker for a light strategy: one list, the same keys the fan curves
/// offer (`nct:...`, `gpu:0`, `nvme:i`).
fn source_combo(ui: &mut egui::Ui, id: usize, source: &mut String, keys: &[String]) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(("rgbsrc", id))
        .selected_text(source.clone())
        .width(180.0)
        .show_ui(ui, |ui| {
            for k in keys {
                if ui.selectable_value(source, k.clone(), k).changed() {
                    changed = true;
                }
            }
        });
    changed
}

/// Save the config for a toast: the failure is appended to the message instead
/// of being dropped — the panel used to announce "配置已保存" even when the
/// write had failed.
fn save_toast(cfg: &AppConfig, msg: String) -> String {
    match cfg.save() {
        Ok(()) => msg,
        Err(e) => format!("{msg}（保存失败：{e}）"),
    }
}

/// The GUI owns the hardware, so this thread keeps curves enforced while the
/// window is hidden (tray) or the user is just watching. Same safety rules as
/// the old daemon: panic-isolated passes, safe duty on dead temp sources.
/// Curve channels are evaluated by the shared algorithm engine; inputs come
/// from the poller's cache, this thread only ever writes.
fn control_loop(
    nct: Arc<Nct>,
    shared: Arc<Mutex<Shared>>,
    cfg: Arc<Mutex<AppConfig>>,
    ovr: Arc<Mutex<FanOverride>>,
    pulse: Arc<Mutex<Option<(usize, Instant)>>>,
) {
    let mut pass = ControlPass::new(FAN_COUNT);
    let glogf = |msg: &str| glog(msg);
    loop {
        let interval_ms = cfg.lock().map(|c| c.interval_ms.max(500)).unwrap_or(2000);
        // one control pass; a panic here must not kill the only controller
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (temps, nvme, gpu_t, has_data, age_ms) = match shared.lock() {
                Ok(s) => (
                    s.reading.temps.clone(),
                    s.nvme.clone(),
                    s.gpu.map(|(t, _)| t),
                    s.has_reading,
                    s.last_update.elapsed().as_millis(),
                ),
                // poisoned lock: treat it as a dead source, not as "fine"
                Err(_) => (vec![], vec![], None, true, u128::from(u32::MAX)),
            };
            let cfg_snap = cfg.lock().map(|c| c.clone()).unwrap_or_default();
            let ov = ovr.lock().map(|o| *o).unwrap_or(FanOverride::None);
            let now = Instant::now();

            let out = pass.run(
                &nct,
                &cfg_snap,
                PassInput {
                    temps: &temps,
                    nvme: &nvme,
                    gpu: gpu_t,
                    data_age_ms: Some(age_ms),
                    overlay: match ov {
                        FanOverride::Full => Some(100.0),
                        FanOverride::Quiet => Some(QUIET_DUTY),
                        FanOverride::None => None,
                    },
                    pulse: *pulse.lock().unwrap_or_else(|e| e.into_inner()),
                    has_data,
                },
                now,
                &glogf,
            );
            if out.pulse_expired {
                *pulse.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            publish_targets(&shared, out.written);
        }));
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}
/// Append a milestone line to fan-tune-gui.log (the GUI has no console).
fn glog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("fan-tune-gui.log")
    {
        let _ = writeln!(f, "[gui] {msg}");
    }
}

/// Absolute path of the GUI maintenance-stop sentinel (next to the exe).
/// Writing this file makes the GUI exit through its normal path — a
/// force-kill can leave the kernel driver transiently unloadable for
/// minutes, so maintenance should always use the sentinel.
fn gui_stop_sentinel_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fan-tune-gui-stop.req")))
        .unwrap_or_else(|| std::path::PathBuf::from("fan-tune-gui-stop.req"))
}

/// While this file exists the light controller is handed to whoever is
/// calibrating (`rgb-tool`), the same way the fan hardware is handed over for
/// maintenance: create it before poking the lights, delete it to take them back.
fn rgb_yield_sentinel_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fan-tune-rgb-yield.req")))
        .unwrap_or_else(|| std::path::PathBuf::from("fan-tune-rgb-yield.req"))
}

pub fn run(minimized: bool) -> Result<(), String> {
    // The poller's slow-refresh schedule is measured from here (see
    // `POLL_START`): a monotonic origin cannot be stepped backwards by NTP or
    // by a suspend/resume cycle.
    let _ = POLL_START.set(Instant::now());
    let singleton = match ensure_single_instance() {
        Some(h) => h,
        None => {
            // second launch: open the running instance's panel instead of a
            // dead-end message box
            if !focus_existing_window() {
                msg_box(
                    "fan-tune 已在后台运行（正在启动中，稍后托盘图标会出现）。",
                    "风扇调校",
                );
            }
            return Err("another GUI instance is already running".into());
        }
    };

    // Config first: it decides which ring-0 backend to prefer (and a corrupt
    // file is worth telling the user about before we touch hardware).
    let (cfg_loaded, cfg_warn) = AppConfig::load();
    if let Some(w) = &cfg_warn {
        // The curves cannot be re-derived, so the user has to know their file
        // was moved aside. Never a modal for an autostart (tray) instance: a
        // dialog nobody can see is how a process ends up stuck holding the
        // hardware lock.
        glog(&format!("config problem: {w}"));
        if !minimized {
            msg_box(w, "风扇调校 · 配置问题");
        }
    }
    let backend_pref = cfg_loaded.backend;
    // Pick the palette before anything is drawn. An unknown id in the file
    // (hand-edited, or written by a newer build) falls back to the default
    // rather than failing: cosmetics must never block the hardware bring-up.
    if !theme::set_theme(&cfg_loaded.ui_theme) {
        theme::set_theme(theme::DEFAULT_THEME);
    }
    let cfg: Arc<Mutex<AppConfig>> = Arc::new(Mutex::new(cfg_loaded));
    let ovr: Arc<Mutex<FanOverride>> = Arc::new(Mutex::new(FanOverride::None));

    // Driver bring-up with patience. Right after logon the SCM, vendor tools
    // and kernel-driver installs can be mid-churn for minutes (observed: a
    // driver-install storm at first logon racing our startup). Each round here
    // is a full ~30s install_and_open sweep; keep going for ~8 minutes
    // instead of failing the whole startup after 40 seconds. The warning box
    // is informational and runs on its own thread — it never blocks a retry.
    let driver = {
        let mut opened = None;
        let mut warned = false;
        let mut last_err = String::from("unknown");
        for round in 1..=8 {
            if gui_stop_sentinel_path().exists() {
                let _ = std::fs::remove_file(gui_stop_sentinel_path());
                glog("stop request during bring-up — exiting for maintenance");
                return Ok(());
            }
            match crate::driver::Driver::install_and_open(backend_pref) {
                Ok(d) => {
                    glog(&format!("driver open (round {round})"));
                    opened = Some(d);
                    break;
                }
                Err(e) => {
                    glog(&format!("driver open failed (round {round}): {e}"));
                    last_err = e;
                    // tray/autostart instances stay silent — a popup over a game
                    // or a fullscreen app is worse than a late tray icon; only
                    // an interactive launch (double-click) gets the notice
                    if round == 2 && !warned && !minimized {
                        warned = true;
                        let hint = crate::driver::environment_hint()
                            .map(|h| format!("\n\n还可能是因为：\n{h}"))
                            .unwrap_or_default();
                        let msg = format!(
                            "风扇驱动暂时打不开，已在后台自动重试（每 30 秒一次，最多约 8 分钟）。\n\n\
                             最后一次错误：{last_err}\n\n\
                             界面会在驱动就绪后自动出现，无需等这个提示。\n\
                             如果一直失败：重启电脑即可恢复。{hint}"
                        );
                        std::thread::spawn(move || msg_box(&msg, "风扇调校 · 自动重试中"));
                    }
                    std::thread::sleep(Duration::from_secs(30));
                }
            }
        }
        match opened {
            Some(d) => d,
            None => {
                // Giving up means nobody is controlling the fans: say so out
                // loud (the tray autostart instance has no other way to report
                // this, and it used to fail completely silently).
                let hint = crate::driver::environment_hint()
                    .map(|h| format!("\n\n可能的原因：\n{h}"))
                    .unwrap_or_default();
                let msg = format!(
                    "风扇驱动 8 轮重试都没能打开，程序退出。\n\n\
                     最后一次错误：{last_err}{hint}\n\n\
                     风扇会停在最后一次设定（当前没有程序在控速）。重启电脑通常即可恢复。"
                );
                glog(&format!("giving up on driver: {last_err}"));
                if !minimized {
                    // An autostart (tray) instance must never block on a modal
                    // nobody can see: it would sit there holding the hardware
                    // lock forever (observed for real).
                    msg_box(&msg, "风扇调校 · 启动失败");
                }
                return Err(format!("driver open failed after all retry rounds: {last_err}"));
            }
        }
    };
    glog("driver open");

    // Takeover AFTER the driver handle is ours: the predecessor keeps its own
    // handle open until it exits, and that overlap is what stops the kernel
    // driver from unloading (and hanging for minutes) between owners.
    take_over_from_daemon();
    glog("takeover done");
    eprintln!("[takeover] GUI is now the single fan controller");

    let nct: Arc<Nct<crate::driver::Driver>> = match Nct::detect(driver) {
        Ok(n) => Arc::new(n),
        Err(e) => {
            glog(&format!("chip detection failed: {e}"));
            if !minimized {
                msg_box(
                    &format!("驱动已就绪，但检测不到支持的 NCT 芯片。\n\n错误：{e}"),
                    "风扇调校 · 硬件错误",
                );
            }
            return Err(e);
        }
    };
    glog("nct detected");
    let shared = Arc::new(Mutex::new(Shared {
        reading: NctReading::default(),
        nvme: vec![],
        nvme_model: vec![],
        nvme_ext: vec![],
        core_temps: vec![],
        gpu: None,
        gpu_ext: vec![],
        sys: SysInfo::default(),
        chip: format!(
            "{} · {} 通道 · 端口 0x{:04X}",
            nct.chip_name(),
            nct.fan_count(),
            nct.base_port()
        ),
        fans: nct.fan_count(),
        last_update: Instant::now(),
        has_reading: false,
        targets: [None; 7],
        hist: VecDeque::new(),
    }));
    // (App is built a few lines below; see its field initialisers)
    // Self-heal: every channel the config leaves on "Auto" must really be on the
    // BIOS. A previous process — or an overlay nobody cancelled — can have left
    // it latched in software mode at an arbitrary duty, and the persisted
    // originals file lets us hand it back even after a restart.
    {
        let cfg_now = cfg.lock().map(|c| c.clone()).unwrap_or_default();
        let mut healed = 0;
        for (i, c) in cfg_now.channels.iter().enumerate().take(nct.fan_count()) {
            if matches!(c.mode, FanMode::Auto) && nct.restore_auto(i) {
                healed += 1;
            }
        }
        if healed > 0 {
            glog(&format!("handed {healed} Auto channel(s) back to the BIOS"));
        }
    }

    {
        let nct = nct.clone();
        let shared = shared.clone();
        std::thread::spawn(move || loop {
            // A panic here used to kill the only temperature producer: the
            // control loop would then follow a frozen reading for ever (the
            // staleness check now catches it, but not dying is better).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Refresh slot, computed before the chip read so the slow blocks
            // (voltages, smartctl, nvidia-smi) share one schedule: a full chip
            // read every second, the slow parts once per 10s.
            //
            // Measured from a monotonic clock captured once at startup. This
            // used to be `SystemTime`, which NTP and suspend/resume can move
            // backwards; a backward step changes the slot number and fires a
            // full smartctl sweep (3 drives x ~230 ms) on a tick that was
            // supposed to be cheap — and the `unwrap_or(0)` fallback made a
            // clock failure do the same.
            let cycle = (POLL_START.get_or_init(Instant::now).elapsed().as_secs() / 10) as u32;
            let slow = LAST.with(|c| c.borrow().cycle != cycle);
            let r = nct.read_all(slow);
            // one full smartctl pass per drive per 10s slot, reused in between
            // (temps, model names and extended fields together, no extra thread)
            #[derive(Default)]
            struct NvmeSnap {
                cycle: u32,
                ran: bool,
                temps: Vec<(usize, f32)>,
                models: Vec<(usize, String)>,
                ext: Vec<(usize, Vec<(String, String)>)>,
                gpu: Option<(f32, f32)>,
                gpu_ext: Vec<(String, String)>,
            }
            thread_local! {
                static LAST: std::cell::RefCell<NvmeSnap> =
                    std::cell::RefCell::new(NvmeSnap::default());
            }
            // (cycle was computed above; smartctl costs ~230 ms of process work
            // per drive, so 10 s is plenty for a temperature that moves with the
            // case's thermal mass)
            let (nvme, nvme_model, nvme_ext, gpu, gpu_ext) = LAST.with(|c| {
                let mut cell = c.borrow_mut();
                // `ran` (not "result non-empty") gates the refresh: an empty
                // result must not re-trigger the smartctl sweep every tick
                if cell.cycle != cycle || !cell.ran {
                    let full = nvme::read_smart_full();
                    cell.temps = full.iter().map(|(i, t, _, _)| (*i, *t)).collect();
                    cell.models = full.iter().map(|(i, _, m, _)| (*i, m.clone())).collect();
                    cell.ext = full.into_iter().map(|(i, _, _, f)| (i, f)).collect();
                    // One nvidia-smi spawn per 5s slot, and this is the only
                    // place that spawns one at all: it used to run once a second
                    // here plus once per 2s in the sys poller, which was the
                    // single biggest waste in the program.
                    let g = read_gpu_ext();
                    cell.gpu = g.temp.map(|t| (t, g.util.unwrap_or(0.0)));
                    let mut v = Vec::new();
                    if let Some(t) = g.mem_temp {
                        v.push(("显存温度".into(), format!("{t:.0} °C")));
                    }
                    if let Some(u) = g.util {
                        v.push(("GPU 占用".into(), format!("{u:.0} %")));
                    }
                    if let (Some(u), Some(t)) = (g.mem_used, g.mem_total) {
                        v.push(("显存".into(), format!("{u:.0} / {t:.0} MB")));
                    }
                    if let Some(p) = g.power {
                        v.push(("GPU 功耗".into(), format!("{p:.1} W")));
                    }
                    if let Some(f) = g.fan {
                        v.push(("GPU 风扇".into(), format!("{f:.0} %")));
                    }
                    cell.gpu_ext = v;
                    cell.cycle = cycle;
                    cell.ran = true;
                }
                (
                    cell.temps.clone(),
                    cell.models.clone(),
                    cell.ext.clone(),
                    cell.gpu,
                    cell.gpu_ext.clone(),
                )
            });
            if let Ok(mut s) = shared.lock() {
                s.reading = r;
                s.nvme = nvme;
                s.nvme_model = nvme_model;
                s.nvme_ext = nvme_ext;
                s.gpu = gpu;
                s.gpu_ext = gpu_ext;
                s.last_update = Instant::now();
                s.has_reading = true;
            }
            }));
            std::thread::sleep(Duration::from_millis(1000));
        });
    }

    // GPU + CPU load + RAM + per-core temps poller (2s cadence)
    {
        let shared = shared.clone();
        let driver = nct.driver();
        std::thread::spawn(move || sys_poller(shared, driver));
    }

    // the control loop: GUI process is now the daemon
    let pulse: Arc<Mutex<Option<(usize, Instant)>>> = Arc::new(Mutex::new(None));
    {
        let (nct, shared, cfg, ovr, pulse) = (
            nct.clone(),
            shared.clone(),
            cfg.clone(),
            ovr.clone(),
            pulse.clone(),
        );
        std::thread::spawn(move || control_loop(nct, shared, cfg, ovr, pulse));
    }

    // ---- lights: the board's ENE 6k7750 ARGB controller ----
    // A machine without `fantune-rgb.json` is never touched — the profile file
    // *is* the consent to drive the lights — but the panel always says which of
    // the two states this is.
    let rgb_engine: Arc<Mutex<Option<RgbEngine>>> = Arc::new(Mutex::new(None));
    let mut rgb_note = "未接管：还没有 fantune-rgb.json（用 rgb-tool init 建立）".to_string();
    {
        let (prof, warn) = RgbProfile::load();
        if let Some(w) = &warn {
            glog(&format!("rgb: {w}"));
            rgb_note = w.clone();
        }
        if let Some(p) = prof {
            match RgbEngine::open(p) {
                Ok(e) => {
                    glog(&format!(
                        "rgb: {} · {} 段 · {} fps 上限 · 退出后{}",
                        e.path(),
                        e.profile().segments.len(),
                        e.profile().max_fps(),
                        if e.profile().keep_on_exit { "保留灯色" } else { "交还主板" }
                    ));
                    rgb_note = format!("已接管 · {} 段", e.profile().segments.len());
                    *rgb_engine.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
                Err(e) => {
                    glog(&format!("rgb: controller unavailable: {e}"));
                    rgb_note = format!("控制器不可用：{e}");
                }
            }
        }
    }
    {
        // Render loop. Temperature strategies change slowly and the engine
        // writes only when a colour actually moved, so the steady state is zero
        // USB traffic; the fps cap only matters for animated strategies (every
        // changed write costs ~3.4 ms of control transfers).
        let rgb_engine = rgb_engine.clone();
        let shared = shared.clone();
        let cfg = cfg.clone();
        let ovr = ovr.clone();
        std::thread::spawn(move || loop {
            let interval = {
                let g = rgb_engine.lock().unwrap_or_else(|e| e.into_inner());
                g.as_ref()
                    .map(|e| e.profile().tick_interval())
                    .unwrap_or(Duration::from_millis(1000))
            };
            std::thread::sleep(interval);
            let yielded = rgb_yield_sentinel_path().exists();
            let inputs = rgb_inputs(&shared, &cfg, &ovr);
            let mut g = rgb_engine.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(eng) = g.as_mut() {
                if yielded {
                    // Calibration in progress: hold nothing, render nothing.
                    if eng.holds_device() {
                        eng.yield_device();
                        glog("rgb: controller handed to the calibration tool");
                    }
                } else {
                    if !eng.holds_device() {
                        match eng.take_device() {
                            Ok(()) => glog("rgb: controller taken back after calibration"),
                            Err(e) => glog(&format!("rgb: take_device failed: {e}")),
                        }
                    }
                    if eng.profile().enabled {
                        if let Err(e) = eng.apply(&inputs, Instant::now()) {
                            glog(&format!("rgb write failed: {e}"));
                        }
                    }
                }
            }
        });
    }

    // the Arc is moved into the eframe closure below; keep one for the exit path
    let rgb_exit = rgb_engine.clone();
    let cfg_exit = cfg.clone();
    // hidden state drives the repaint cadence; shared with the tray thread
    let hidden = Arc::new(AtomicBool::new(minimized));
    // set by the maintenance sentinel; checked after run_native to skip the
    // exit-time config save
    let maintenance_exit = Arc::new(AtomicBool::new(false));
    let maintenance_exit_exit = maintenance_exit.clone();
    // set by the tray quit path so update()'s X-intercept lets that close
    // through (and the exit-time behaviour below stays a normal exit)
    let user_quit = Arc::new(AtomicBool::new(false));
    // profile switch requests routed from the tray thread into the UI frame
    let profile_req = Arc::new(AtomicU8::new(255));
    glog("threads spawned, entering eframe");
    let result = eframe::run_native(
        "fan-tune",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1180.0, 780.0])
                .with_min_inner_size([960.0, 640.0])
                .with_title("fan-tune · 风扇调校")
                // autostart ("tray" mode): no window, tray icon only
                .with_visible(!minimized),
            ..Default::default()
        },
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            setup_style(&cc.egui_ctx);
            apply_ui_scale(
                &cc.egui_ctx,
                cfg.lock().unwrap_or_else(|e| e.into_inner()).ui_scale,
            );
            let _tray_ok = match create_tray(
                cc.egui_ctx.clone(),
                nct.clone(),
                cfg.clone(),
                ovr.clone(),
                hidden.clone(),
                user_quit.clone(),
                profile_req.clone(),
            ) {
                Ok(()) => {
                    glog("tray icon created");
                    true
                }
                Err(e) => {
                    glog(&format!("tray unavailable: {e}"));
                    eprintln!("[tray] unavailable: {e}");
                    false
                }
            };
            // Maintenance-sentinel watcher. The check inside update() only
            // runs while frames render — and a hidden or minimized window
            // gets no redraw events from winit, so the request file used to
            // sit there ignored (a stuck exe blocks every rebuild). This
            // thread owns the exit: it wakes the window first (frames flow
            // again, which also lets pending viewport commands apply), then
            // closes through the normal path.
            {
                let ctx_watch = cc.egui_ctx.clone();
                let maint_flag = maintenance_exit.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(600));
                    let p = gui_stop_sentinel_path();
                    if !p.exists() {
                        continue;
                    }
                    let _ = std::fs::remove_file(&p);
                    maint_flag.store(true, Ordering::Relaxed);
                    glog("stop request (watcher) — waking the window for a graceful exit");
                    loop {
                        wake_own_window();
                        ctx_watch.send_viewport_cmd(egui::ViewportCommand::Close);
                        std::thread::sleep(Duration::from_millis(1200));
                    }
                });
            }
            Ok(Box::new(App {
                cfg,
                ovr,
                shared,
                nct,
                selected_fan: 0,
                curve_edit: None,
                view: View::Channels,
                scan_at: None,
                grain: None,
                toast: None,
                chan_stats: vec![ChanSample::default(); 7],
                rename: None,
                hidden,
                maintenance_exit: maintenance_exit.clone(),
                user_quit: user_quit.clone(),
                profile_req,
                pulse,
                spin_phase: 0.0,
                boot_start: Instant::now(),
                duty_hist: vec![std::collections::VecDeque::new(); 7],
                rgb: rgb_engine.clone(),
                rgb_note,
            }))
        }),
    )
    .map_err(|e| e.to_string());

    // ---- handover: persist state, let the successor carry on ----
    // Lights first: the profile decides whether they keep their colours (the
    // controller holds them) or go back to the board's own effect.
    if let Ok(mut g) = rgb_exit.lock() {
        if let Some(eng) = g.as_mut() {
            eng.shutdown();
        }
    }
    // A maintenance stop is different on purpose: the maintainer is taking
    // over (usually to replace the exe on disk), so NO successor is spawned —
    // otherwise the app folder would stay locked forever. A normal exit hands
    // control to a spawned successor with an overlapping device handle.
    let maint = maintenance_exit_exit.load(Ordering::Relaxed);
    if !maint {
        if let Ok(c) = cfg_exit.lock() {
            if let Err(e) = c.save() {
                glog(&format!("config save failed on exit: {e}"));
            }
        }
    } else {
        glog("maintenance exit — config untouched, no successor spawned");
    }
    release_singleton(singleton);
    let successor = if maint {
        false
    } else {
        start_daemon_fallback()
    };
    if successor {
        glog("successor launched — holding the device 8s for a clean overlap");
        std::thread::sleep(Duration::from_millis(8000));
    }
    result
}

fn setup_fonts(ctx: &egui::Context) {
    // Geist Mono (embedded, OFL) leads every family; a system CJK face is
    // appended as fallback because Geist Mono has no Han coverage.
    theme::install_fonts(ctx);
}

fn setup_style(ctx: &egui::Context) {
    // Everything visual (palette, zero radius, hairlines, mono type scale)
    // lives in the token layer — see `theme::install_style`.
    theme::install_style(ctx);
}

/// Re-apply the style after a palette switch. Colours are read per frame, so
/// only egui's cached `Visuals` needs refreshing.
fn refresh_style(ctx: &egui::Context) {
    theme::install_style(ctx);
    ctx.request_repaint();
}

/// Global UI zoom: the user multiplier on top of the OS DPI factor, so a
/// 150 % Windows display stays 150 % under any zoom setting. Clamped here so
/// a hand-edited config cannot shrink the UI into unreadability.
fn apply_ui_scale(ctx: &egui::Context, scale: f32) {
    let native = ctx.native_pixels_per_point().unwrap_or(1.0);
    ctx.set_pixels_per_point(native * scale.clamp(0.8, 2.0));
}

// ---------- system tray ----------

fn tray_icon_rgba(size: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; size * size * 4];
    let cx = (size as f32 - 1.0) / 2.0;
    let cy = cx;
    let accent = [88u8, 166, 255];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let i = (y * size + x) * 4;
            let hit = if d < size as f32 * 0.16 {
                true // hub
            } else {
                // three blades at 120° apart
                let mut ang = dy.atan2(dx).to_degrees() + 90.0;
                if ang < 0.0 {
                    ang += 360.0;
                }
                let sector = ang % 120.0;
                d < size as f32 * 0.44 && sector > 50.0 && sector < 70.0
            };
            if hit {
                rgba[i] = accent[0];
                rgba[i + 1] = accent[1];
                rgba[i + 2] = accent[2];
                rgba[i + 3] = 255;
            }
        }
    }
    rgba
}

fn create_tray(
    ctx: egui::Context,
    nct: Arc<Nct>,
    cfg: Arc<Mutex<AppConfig>>,
    ovr: Arc<Mutex<FanOverride>>,
    hidden: Arc<AtomicBool>,
    user_quit: Arc<AtomicBool>,
    profile_req: Arc<AtomicU8>,
) -> Result<(), String> {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    // UIPI: explorer runs at medium integrity, this process is elevated. The
    // shell's tray notifications (the private WM_USER callback below and the
    // TaskbarCreated broadcast) are user-range messages and are silently
    // dropped for a high-integrity window unless the process explicitly
    // allows them — the icon draws, but clicks and menus do nothing, which
    // is exactly the "tray does not respond" symptom. Allow both.
    {
        #[link(name = "user32")]
        extern "system" {
            fn ChangeWindowMessageFilter(message: u32, dw_flag: u32) -> i32;
            fn RegisterWindowMessageW(s: *const u16) -> u32;
        }
        const MSGFLT_ADD: u32 = 1;
        // private callback message used by tray-icon 0.14 (pinned in Cargo.lock)
        const WM_USER_TRAYICON: u32 = 6002;
        unsafe {
            let a = ChangeWindowMessageFilter(WM_USER_TRAYICON, MSGFLT_ADD);
            let mut s: Vec<u16> = "TaskbarCreated".encode_utf16().collect();
            s.push(0);
            let m = RegisterWindowMessageW(s.as_ptr());
            let b = if m != 0 { ChangeWindowMessageFilter(m, MSGFLT_ADD) } else { 0 };
            glog(&format!("tray UIPI filter: callback={a} taskbar_created={b}"));
        }
    }

    let icon = Icon::from_rgba(tray_icon_rgba(32), 32, 32).map_err(|e| e.to_string())?;
    let menu = Menu::new();
    menu.append(&MenuItem::with_id("show", "显示面板", true, None))
        .map_err(|e| e.to_string())?;
    menu.append(&MenuItem::with_id("boost", "全速散热 (100%)", true, None))
        .map_err(|e| e.to_string())?;
    menu.append(&MenuItem::with_id("quiet", "静音模式 (40%)", true, None))
        .map_err(|e| e.to_string())?;
    menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
        .map_err(|e| e.to_string())?;
    menu.append(&MenuItem::with_id("profile_std", "模式：标准", true, None))
        .map_err(|e| e.to_string())?;
    menu.append(&MenuItem::with_id("profile_game", "模式：游戏（增强散热）", true, None))
        .map_err(|e| e.to_string())?;
    menu.append(&MenuItem::with_id("profile_srv", "模式：服务器（固定转速）", true, None))
        .map_err(|e| e.to_string())?;
    menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
        .map_err(|e| e.to_string())?;
    menu.append(&MenuItem::with_id("quit", "退出（后台守护自动接管）", true, None))
        .map_err(|e| e.to_string())?;

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("fan-tune · 风扇调校 · 点击显示面板")
        .with_icon(icon)
        .build()
        .map_err(|e| e.to_string())?;
    std::mem::forget(_tray); // keep tray alive for process lifetime

    // Event pumps. tray-icon 0.14 on Windows delivers events through its own
    // (hidden) message window; polling with try_recv() misses them — the tray
    // then looks dead (click → nothing happens, and with the window hidden
    // there is no way back to the panel). Block on each receiver on its own
    // thread instead. `egui::Context` is a cheap clone that wakes the UI
    // event loop from any thread.
    let ctx_icon = ctx.clone();
    let hidden_icon = hidden.clone();
    std::thread::spawn(move || loop {
        match tray_icon::TrayIconEvent::receiver().recv() {
            Ok(ev) => {
                if matches!(
                    ev,
                    tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    }
                ) {
                    glog("tray click — showing panel");
                    hidden_icon.store(false, Ordering::Relaxed);
                    // win32 wake first: while the window is hidden/minimized
                    // the queued egui commands below would not be applied
                    // (no frames run), so the OS-level show is what actually
                    // brings the panel back
                    wake_own_window();
                    ctx_icon.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx_icon.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx_icon.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
            Err(_) => break,
        }
    });
    let ctx_menu = ctx.clone();
    std::thread::spawn(move || {
        let show_window = || {
            hidden.store(false, Ordering::Relaxed);
            wake_own_window();
            ctx_menu.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx_menu.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx_menu.send_viewport_cmd(egui::ViewportCommand::Focus);
        };
        let set_all = |duty: f32| {
            let skip: Vec<bool> = cfg
                .lock()
                .map(|c| c.channels.iter().map(|ch| ch.hands_off()).collect())
                .unwrap_or_default();
            for i in 0..nct.fan_count() {
                if skip.get(i).copied().unwrap_or(false) {
                    continue; // pump / enabled:false is not a one-click target
                }
                let _ = nct.set_duty_pct(i, duty);
            }
        };
        loop {
            match tray_icon::menu::MenuEvent::receiver().recv() {
                Ok(ev) => match ev.id().0.as_str() {
                    "show" => {
                        glog("tray menu: show");
                        show_window();
                    }
                    "boost" => {
                        if let Ok(mut o) = ovr.lock() {
                            *o = FanOverride::Full;
                        }
                        set_all(100.0);
                    }
                    "quiet" => {
                        if let Ok(mut o) = ovr.lock() {
                            *o = FanOverride::Quiet;
                        }
                        set_all(40.0);
                    }
                    "profile_std" => {
                        profile_req.store(0, Ordering::Relaxed);
                    }
                    "profile_game" => {
                        profile_req.store(1, Ordering::Relaxed);
                    }
                    "profile_srv" => {
                        profile_req.store(2, Ordering::Relaxed);
                    }
                    // normal exit path: run_native returns → config saved →
                    // daemon started by start_daemon_fallback(). The X button
                    // is intercepted in update() and only hides the window, so
                    // this menu item is the way to actually stop the app.
                    "quit" => {
                        glog("tray quit — exiting, daemon takes over");
                        user_quit.store(true, Ordering::Relaxed);
                        ctx_menu.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => {}
                },
                Err(_) => break,
            }
        }
    });
    Ok(())
}

// ---------- vector icons (hand-drawn, no font needed) ----------

/// Spinning fan glyph: rotation speed follows a 0..1 "load" value.
fn draw_fan_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: egui::Color32, spin: f32) {
    let rot = spin;
    let stroke = egui::Stroke::new(radius * 0.22, color);
    for i in 0..3 {
        let a = rot + i as f32 * std::f32::consts::TAU / 3.0;
        // blade: curved-ish stroke from hub outward
        let tip = center + egui::vec2(a.cos(), a.sin()) * radius;
        let mid = center + egui::vec2((a + 0.5).cos(), (a + 0.5).sin()) * radius * 0.55;
        painter.line_segment([center + egui::vec2(a.cos(), a.sin()) * radius * 0.15, tip], stroke);
        painter.circle_filled(mid, radius * 0.14, color);
    }
    painter.circle_filled(center, radius * 0.22, color);
}

fn draw_temp_icon(painter: &egui::Painter, top_left: egui::Pos2, size: f32, color: egui::Color32) {
    let c = egui::pos2(top_left.x + size * 0.5, top_left.y + size * 0.5);
    let stroke = egui::Stroke::new(size * 0.10, color);
    // tube
    painter.line_segment(
        [egui::pos2(c.x, c.y - size * 0.42), egui::pos2(c.x, c.y + size * 0.15)],
        stroke,
    );
    // bulb
    painter.circle_filled(egui::pos2(c.x, c.y + size * 0.3), size * 0.24, color);
}

fn draw_chip_icon(painter: &egui::Painter, top_left: egui::Pos2, size: f32, color: egui::Color32) {
    let r = egui::Rect::from_min_size(
        egui::pos2(top_left.x + size * 0.22, top_left.y + size * 0.22),
        egui::vec2(size * 0.56, size * 0.56),
    );
    painter.rect_filled(r, 2.0, color);
    let pin = egui::Stroke::new(size * 0.06, color);
    for i in 0..3 {
        let f = (i as f32 + 1.0) / 4.0;
        painter.line_segment(
            [egui::pos2(r.left() - size * 0.16, r.top() + r.height() * f), egui::pos2(r.left(), r.top() + r.height() * f)],
            pin,
        );
        painter.line_segment(
            [egui::pos2(r.right(), r.top() + r.height() * f), egui::pos2(r.right() + size * 0.16, r.top() + r.height() * f)],
            pin,
        );
        painter.line_segment(
            [egui::pos2(r.left() + r.width() * f, r.top() - size * 0.16), egui::pos2(r.left() + r.width() * f, r.top())],
            pin,
        );
        painter.line_segment(
            [egui::pos2(r.left() + r.width() * f, r.bottom()), egui::pos2(r.left() + r.width() * f, r.bottom() + size * 0.16)],
            pin,
        );
    }
}

fn draw_drive_icon(painter: &egui::Painter, top_left: egui::Pos2, size: f32, color: egui::Color32) {
    let r = egui::Rect::from_min_size(top_left + egui::vec2(0.0, size * 0.25), egui::vec2(size, size * 0.5));
    painter.rect_filled(r, 3.0, color);
    painter.circle_filled(egui::pos2(r.right() - size * 0.15, r.center().y), size * 0.07, bg());
}

/// Per-channel sampling used for the conservative "空接口" mark (A2):
/// count every fresh sensor reading; a channel is only marked empty when
/// it has 60+ samples, never showed a single rotation, and was actually
/// driven at >= 40% duty at least once during the window.
#[derive(Default, Clone, Copy)]
struct ChanSample {
    last_update: Option<Instant>,
    samples: u32,
    max_rpm: f32,
    peak_duty: f32,
}

impl ChanSample {
    fn empty_channel(&self) -> bool {
        self.samples >= 60 && self.max_rpm <= 0.0 && self.peak_duty >= 40.0
    }
}

/// The centre stage. Everything else (HUD, readouts, sensor rail, status bar)
/// stays put, so switching views never costs you the live context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Channels,
    Curve,
    Lights,
    Appearance,
}

impl View {
    /// `(view, index, label)` — the index is the zero-padded tab number the
    /// reference theme puts in front of every view name.
    const ALL: [(View, &'static str, &'static str); 4] = [
        (View::Channels, "01", "通道"),
        (View::Curve, "02", "曲线"),
        (View::Lights, "03", "灯效"),
        (View::Appearance, "04", "外观"),
    ];

    /// Latin name for the status bar (the tab itself is Chinese).
    fn label(self) -> &'static str {
        match self {
            View::Channels => "CHANNELS",
            View::Curve => "CURVE",
            View::Lights => "LIGHTS",
            View::Appearance => "STYLE",
        }
    }
}

/// Local wall clock, formatted the way the reference header does: `17:12:19`
/// over `TUE · 08 OCT 2025`.
fn clock_strings() -> (String, String) {
    const DOW: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    const MON: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let (days, tod) = crate::local_time_parts();
    let (y, m, d) = crate::civil_from_days(days);
    let dow = DOW[(days + 4).rem_euclid(7) as usize]; // 1970-01-01 was a Thursday
    let mon = MON[(m.clamp(1, 12) - 1) as usize];
    (
        format!("{:02}:{:02}:{:02}", tod / 3600, (tod % 3600) / 60, tod % 60),
        format!("{dow} · {d:02} {mon} {y}"),
    )
}

pub struct App {
    cfg: Arc<Mutex<AppConfig>>,
    ovr: Arc<Mutex<FanOverride>>,
    shared: Arc<Mutex<Shared>>,
    nct: Arc<Nct>,
    selected_fan: usize,
    curve_edit: Option<(usize, usize)>,
    /// which stage the centre panel is showing
    view: View,
    /// when the current view was entered — drives the scan transition
    /// (only set while the window is visible and focused)
    scan_at: Option<Instant>,
    /// the tiled noise overlay, built once on the first frame
    grain: Option<egui::TextureHandle>,
    /// (message, shown_at) — transient action feedback
    toast: Option<(String, Instant)>,
    /// per-channel sampling for the "空接口" mark
    chan_stats: Vec<ChanSample>,
    /// inline rename in progress: (channel, draft label)
    rename: Option<(usize, String)>,
    /// true while the window is hidden to the tray — drops the repaint
    /// cadence from 30 fps to 1 fps (the spin/animation only matters when
    /// someone is looking)
    hidden: Arc<AtomicBool>,
    /// set when the exit was requested via the maintenance sentinel: the exit
    /// must NOT write the config back (maintenance may be fixing the file on
    /// disk while this instance holds stale state in memory)
    maintenance_exit: Arc<AtomicBool>,
    /// set by the tray menu's 退出: the one path that really closes the
    /// window (the X button is intercepted and minimizes instead)
    user_quit: Arc<AtomicBool>,
    /// profile switch request from the tray menu (255 = none, 0/1/2 = profile)
    profile_req: Arc<AtomicU8>,

    /// one-shot identification pulse: (channel, until). Set by the 识别 button,
    /// honoured by the control loop, which also hands the channel back.
    pulse: Arc<Mutex<Option<(usize, Instant)>>>,
    /// Animation clock for the spinning icons. It only advances while the panel
    /// is visible AND focused — a frozen clock means a tray panel or a window
    /// sitting behind another app costs 1 fps instead of 30.
    spin_phase: f32,
    /// Boot self-test sequence: wall-clock from process start, so it ALWAYS
    /// terminates 3.4 s after launch regardless of window focus. (An earlier
    /// version advanced on the focus-gated animation clock instead: a window
    /// that never received focus kept the veil up forever — the "stuck at
    /// boot" bug.)
    boot_start: Instant,
    /// Recent duty trace per channel (one sample per sensor tick, ~48 samples
    /// ≈ 96 s) — the raw material for the per-card sparkline.
    duty_hist: Vec<std::collections::VecDeque<f32>>,
    /// The board's light controller, when this machine has a light profile.
    /// `None` = never opened (no `fantune-rgb.json`) or unavailable; `rgb_note`
    /// says which, for the panel.
    rgb: Arc<Mutex<Option<RgbEngine>>>,
    rgb_note: String,
}

impl App {
    fn resolve_source(&self, key: &str) -> Option<f32> {
        let s = self.shared.lock().ok()?;
        resolve_key(key, &s.reading.temps, &s.nvme, s.gpu.map(|(t, _)| t))
    }

    /// Effective temperature of a channel: multi-source list with Max/Avg, or
    /// the legacy single `source` when the list is empty (same rule as Engine).
    fn resolve_channel(&self, c: &FanChannelConfig) -> Option<f32> {
        let keys: &[String] = if c.sources.is_empty() {
            std::slice::from_ref(&c.source)
        } else {
            &c.sources
        };
        let vals: Vec<f32> = keys.iter().filter_map(|k| self.resolve_source(k)).collect();
        if vals.is_empty() {
            return None;
        }
        Some(match c.combine {
            SourceCombine::Max => vals.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            SourceCombine::Avg => vals.iter().sum::<f32>() / vals.len() as f32,
        })
    }

    fn apply_channel(&mut self, i: usize, cfg: &AppConfig) {
        let c = &cfg.channels[i];
        let name = channel_display_name(c);
        let res = match c.mode {
            FanMode::Manual => self.nct.set_duty_pct(i, c.manual_duty),
            FanMode::Curve => match self.resolve_channel(c) {
                Some(t) => self.nct.set_duty_pct(i, eval_curve(&c.points, t)),
                None => {
                    self.toast = Some((format!("{name}：温度源无读数"), Instant::now()));
                    return;
                }
            },
            // Budget is a closed loop: it needs the engine's persistent state
            // (the previous duty to nudge from), so the control loop takes it
            // over on the next tick instead of a one-shot write here.
            FanMode::Budget => {
                let budget = cfg.channels[i].budget_c;
                self.toast = Some((
                    format!("{name}：预算 {budget:.0}°C，由控制循环调节（下个周期生效）"),
                    Instant::now(),
                ));
                return;
            }
            FanMode::Auto => return,
        };
        let duty_now = match c.mode {
            FanMode::Manual => c.manual_duty,
            _ => eval_curve(&c.points, self.resolve_channel(c).unwrap_or(0.0)),
        };
        self.toast = Some((
            match res {
                Ok(()) => format!("已写入 {name} → {duty_now:.0}%"),
                Err(e) => format!("{name} 写入失败：{e}"),
            },
            Instant::now(),
        ));
    }

    fn restore_all_auto(&mut self, cfg: &mut AppConfig, ovr: &mut FanOverride) {
        let mut restored = 0;
        // bounded by both the chip's fan count and the config: a short config
        // used to index past its own end here
        for i in 0..self.nct.fan_count().min(cfg.channels.len()) {
            if self.nct.restore_auto(i) {
                restored += 1;
            }
            cfg.channels[i].mode = FanMode::Auto;
        }
        *ovr = FanOverride::None;
        let msg = if restored > 0 {
            format!("已恢复 {restored} 个通道的 BIOS 自动控制")
        } else {
            "本会话未接管过任何通道".to_string()
        };
        self.toast = Some((save_toast(cfg, msg), Instant::now()));
    }

    /// Sensor keys the light strategies may follow — the same names the fan
    /// curves use, so the panel offers one consistent list.
    fn rgb_sensor_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if let Ok(s) = self.shared.lock() {
            keys.extend(s.reading.temps.iter().map(|(n, _)| n.clone()));
            if s.gpu.is_some() {
                keys.push("gpu:0".to_string());
            }
            for (i, _) in &s.nvme {
                keys.push(format!("nvme:{i}"));
            }
        }
        keys
    }

    /// The light section: master switches, the global look, then one collapsible
    /// row per segment (strategy, geometry, calibration).
    /// Switch the centre stage. The scan transition only starts when someone
    /// is actually watching: an animation nobody can see would just burn
    /// repaints, which is exactly what the `animating` gate exists to stop.
    fn switch_view(&mut self, v: View, animating: bool) {
        if self.view == v {
            return;
        }
        self.view = v;
        self.scan_at = if animating { Some(Instant::now()) } else { None };
    }

    fn rgb_panel(&mut self, ui: &mut egui::Ui) {
        let keys = self.rgb_sensor_keys();
        let default_source = keys
            .iter()
            .find(|k| k.contains("PECI"))
            .cloned()
            .or_else(|| keys.first().cloned())
            .unwrap_or_else(|| "nct:PECI_0 (CPU)".to_string());
        let note = self.rgb_note.clone();
        let fans = self.nct.fan_count();
        let arc = self.rgb.clone();
        let mut guard = match arc.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
let dim = |t: &str| theme::micro(t, text_dim());
        let mut changed = false;
        let mut toast: Option<String> = None;

        theme::panel_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                let eng = match guard.as_mut() {
                    Some(e) => e,
                    None => {
                        theme::section(ui, "灯效", Some("RGB"));
                        ui.horizontal(|ui| {
                            ui.label(theme::micro("未接管", theme::t().muted));
                            ui.label(theme::label(&note, text_dim()));
                        });
                        return;
                    }
                };
                let mut prof = eng.profile().clone();

                theme::section(ui, "灯效", Some(&format!("{:02}", prof.segments.len())));
                ui.horizontal(|ui| {
                    changed |= ui
                        .checkbox(&mut prof.enabled, theme::micro("接管灯光", theme::t().fg_soft))
                        .on_hover_text("取消勾选 = 程序不再写灯(灯保持当前颜色)。校准、或换别的灯控软件时先取消")
                        .changed();
                    // WinUSB allows one handle per interface, so the only way
                    // to let the calibration tool in is to let go of the device.
                    let mut yielded = !eng.holds_device();
                    if ui
                        .checkbox(&mut yielded, theme::micro("让给校准", theme::t().fg_soft))
                        .on_hover_text("放开控制器句柄,让 rgb-tool 校准(灯保持当前颜色);取消勾选重新接管")
                        .changed()
                    {
                        if yielded {
                            eng.yield_device();
                            toast = Some("已放开灯光控制器,rgb-tool 现在可以用".to_string());
                        } else {
                            toast = Some(match eng.take_device() {
                                Ok(()) => "已重新接管灯光控制器".to_string(),
                                Err(e) => e,
                            });
                        }
                    }
                    if theme::chip(ui, "还灯给系统", false)
                        .on_hover_text("把灯交回主板自己的灯效(重启一次也能达到同样效果)")
                        .clicked()
                    {
                        toast = Some(match eng.release() {
                            Ok(()) => "已把灯交回主板".to_string(),
                            Err(e) => e,
                        });
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !eng.holds_device() {
                            ui.label(theme::micro("控制器已让出(校准中)", warn()));
                        } else if let Some(e) = &eng.last_error {
                            ui.label(theme::micro(e, hot()));
                        } else {
                            let mut st = format!("写入 {} 次", eng.writes);
                            if !eng.missing.is_empty() {
                                st.push_str(&format!(" · 读不到:{}", eng.missing.join("、")));
                            }
                            ui.label(theme::micro(&st, theme::t().line_dim));
                        }
                    });
                });

                ui.horizontal(|ui| {
                    ui.label(dim("全局"));
                    changed |= ui
                        .add(egui::Slider::new(&mut prof.style.brightness, 0.05..=1.0).text(dim("亮度")))
                        .changed();
                    changed |= ui
                        .add(egui::Slider::new(&mut prof.style.saturation, 0.0..=1.5).text(dim("饱和")))
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut prof.max_fps, 0.5..=rgb::profile::MAX_FPS_LIMIT)
                                .text(dim("刷新"))
                                .suffix(" fps"),
                        )
                        .on_hover_text("只影响会动的效果；温度类效果只在颜色变化时才写，稳态零流量")
                        .changed();
                    changed |= ui
                        .checkbox(&mut prof.keep_on_exit, egui::RichText::new("退出后保留灯色").size(11.0))
                        .on_hover_text("取消 = 退出时把灯交回主板自己的灯效")
                        .changed();
                });

                for i in 0..prof.segments.len() {
                    let mut identify = false;
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("{} · {}", prof.segments[i].name, strategy_kind_label(&prof.segments[i].strategy)))
                            .size(theme::size::MICRO)
                            .color(theme::t().muted)
                            .extra_letter_spacing(theme::track::MICRO),
                    )
                        .id_salt(("rgbseg", i))
                        .default_open(false)
                        .show(ui, |ui| {
                            let seg = &mut prof.segments[i];
                            ui.horizontal(|ui| {
                                changed |= ui
                                    .add(egui::TextEdit::singleline(&mut seg.name).desired_width(130.0))
                                    .changed();
                                let mut kind = strategy_kind(&seg.strategy);
                                let before = kind;
                                egui::ComboBox::from_id_salt(("rgbkind", i))
                                    .selected_text(STRATEGY_KINDS[kind])
                                    .show_ui(ui, |ui| {
                                        for (k, label) in STRATEGY_KINDS.iter().enumerate() {
                                            ui.selectable_value(&mut kind, k, *label);
                                        }
                                    });
                                if kind != before {
                                    seg.strategy = strategy_with_kind(&seg.strategy, kind, &default_source);
                                    changed = true;
                                }
                                identify = theme::chip(ui, "识别", false)
                                    .on_hover_text("这一路闪白 3 秒——比让风扇满速更安静地确认它对应哪个部件")
                                    .clicked();
                            });

                            match &mut seg.strategy {
                                Strategy::Off => {}
                                Strategy::Static { color } => {
                                    ui.horizontal(|ui| {
                                        ui.label(dim("颜色"));
                                        changed |= ui.color_edit_button_srgb(color).changed();
                                    });
                                }
                                Strategy::TempColor { source, cold, warm, t_cold, t_warm } => {
                                    ui.horizontal(|ui| {
                                        ui.label(dim("温度源"));
                                        changed |= source_combo(ui, i, source, &keys);
                                        ui.label(dim("凉色"));
                                        changed |= ui.color_edit_button_srgb(cold).changed();
                                        ui.label(dim("热色"));
                                        changed |= ui.color_edit_button_srgb(warm).changed();
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(dim("区间"));
                                        changed |= ui
                                            .add(egui::DragValue::new(t_cold).range(-20.0..=125.0).suffix("°C"))
                                            .changed();
                                        ui.label(dim("→"));
                                        changed |= ui
                                            .add(egui::DragValue::new(t_warm).range(-20.0..=125.0).suffix("°C"))
                                            .changed();
                                    });
                                }
                                Strategy::TempLevel { source, cold, warm, t_min, t_max } => {
                                    ui.horizontal(|ui| {
                                        ui.label(dim("温度源"));
                                        changed |= source_combo(ui, i, source, &keys);
                                        ui.label(dim("底色"));
                                        changed |= ui.color_edit_button_srgb(cold).changed();
                                        ui.label(dim("水位色"));
                                        changed |= ui.color_edit_button_srgb(warm).changed();
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(dim("水位区间"));
                                        changed |= ui
                                            .add(egui::DragValue::new(t_min).range(-20.0..=125.0).suffix("°C"))
                                            .changed();
                                        ui.label(dim("→"));
                                        changed |= ui
                                            .add(egui::DragValue::new(t_max).range(-20.0..=125.0).suffix("°C"))
                                            .changed();
                                        ui.label(dim("（环形从相位点往上填，条状从起点往后填）"));
                                    });
                                }
                                Strategy::DutyColor { channel, low, high } => {
                                    ui.horizontal(|ui| {
                                        ui.label(dim("通道"));
                                        let mut ch = *channel as usize;
                                        egui::ComboBox::from_id_salt(("rgbch", i))
                                            .selected_text(format!("风扇 {ch}"))
                                            .show_ui(ui, |ui| {
                                                for c in 0..fans {
                                                    if ui.selectable_value(&mut ch, c, format!("风扇 {c}")).changed() {
                                                        changed = true;
                                                    }
                                                }
                                            });
                                        *channel = ch as u8;
                                        ui.label(dim("低占空"));
                                        changed |= ui.color_edit_button_srgb(low).changed();
                                        ui.label(dim("高占空"));
                                        changed |= ui.color_edit_button_srgb(high).changed();
                                    });
                                }
                                Strategy::Status { standard, gaming, server, quiet, full, alert } => {
                                    ui.horizontal_wrapped(|ui| {
                                        for (label, c) in [
                                            ("标准", &mut *standard),
                                            ("游戏", &mut *gaming),
                                            ("服务器", &mut *server),
                                            ("静音", &mut *quiet),
                                            ("全速", &mut *full),
                                            ("告警", &mut *alert),
                                        ] {
                                            ui.label(dim(label));
                                            changed |= ui.color_edit_button_srgb(c).changed();
                                        }
                                    });
                                }
                            }

ui.horizontal(|ui| {
                                ui.label(dim("形状"));
                                let mut shape = |ui: &mut egui::Ui, want: Shape, label: &str| {
                                    if theme::chip(ui, label, seg.shape == want).clicked() {
                                        seg.shape = want;
                                        changed = true;
                                    }
                                };
                                shape(ui, Shape::Line, "条状");
                                shape(ui, Shape::Ring, "环形");
                                if seg.shape == Shape::Ring {
                                    changed |= ui
                                        .add(egui::DragValue::new(&mut seg.phase).range(0..=1024).prefix("相位 "))
                                        .on_hover_text("硬件灯珠编号里哪一个该当作位置 0(校准:把亮点转到 12 点)")
                                        .changed();
                                }
                                changed |= ui.checkbox(&mut seg.reversed, theme::micro("反向", theme::t().fg_soft)).changed();
                                ui.label(dim("材质"));
                                for (d, label) in [
                                    (Diffusion::Diffused, "柔光罩"),
                                    (Diffusion::Guide, "导光条"),
                                    (Diffusion::Bare, "裸灯珠"),
                                    (Diffusion::Solid, "整块同色"),
                                ] {
                                    if theme::chip(ui, label, seg.diffusion == d).clicked() {
                                        seg.diffusion = d;
                                        changed = true;
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(dim("校准"));
                                changed |= ui
                                    .add(egui::Slider::new(&mut seg.gain, 0.05..=1.0).text(dim("亮度")))
                                    .changed();
                                changed |= ui
                                    .add(egui::Slider::new(&mut seg.sat, 0.0..=1.5).text(dim("饱和")))
                                    .changed();
                                ui.label(dim("白点"));
                                for (label, v) in [("R", 0usize), ("G", 1), ("B", 2)] {
                                    ui.label(dim(label));
                                    changed |= ui
                                        .add(
                                            egui::DragValue::new(&mut seg.white[v])
                                                .range(0.5..=1.5)
                                                .speed(0.01)
                                                .fixed_decimals(2),
                                        )
                                        .changed();
                                }
                            });
                            ui.label(dim("灯珠：起始 / 数量"));
                            ui.horizontal(|ui| {
                                changed |= ui
                                    .add(egui::DragValue::new(&mut seg.first_led).range(0..=1024))
                                    .changed();
                                changed |= ui
                                    .add(egui::DragValue::new(&mut seg.leds).range(1..=128))
                                    .changed();
                            });
                        });
                    if identify {
                        eng.pulse = Some((i, Instant::now() + Duration::from_secs(3)));
                    }
                }

if !prof.calibrated {
                    ui.label(
                        egui::RichText::new(
                            "未校准:灯珠数量 / 形状 / 相位还没实测(用 rgb-tool walk 或识别向导确认)。\
                             现在温度类效果会按 128 灯珠铺满整条链。",
                        )
                        .size(theme::size::LABEL)
                        .color(warn())
                        .extra_letter_spacing(1.0),
                    );
                }

                if changed {
                    prof.calibrated = prof.calibrated || prof.segments.iter().all(|s| s.leds < 128);
                    match prof.save() {
                        Ok(()) => {
                            // the profile is ambient state other processes read;
                            // log what was written when the panel saves it
                            glog(&format!(
                                "rgb profile saved: fps {:.1} · 亮度 {:.2} · 饱和 {:.2} · {} 段",
                                prof.max_fps,
                                prof.style.brightness,
                                prof.style.saturation,
                                prof.segments.len()
                            ));
                            eng.set_profile(prof)
                        }
                        Err(e) => {
                            toast = Some(format!("灯效配置保存失败：{e}"));
                        }
                    }
                }
            });

        if let Some(msg) = toast {
            self.toast = Some((msg, Instant::now()));
        }
    }

    /// The style view: palette picker, grain/motion switches and a type
    /// specimen. All of it is cosmetic state that lives *outside* the config
    /// save path (the theme id is the one exception, written back by the
    /// caller), so the panel can also show what the type system looks like.
    fn appearance_panel(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        let tk = theme::t();
        theme::panel_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section(ui, "配色方案", Some("05"));
            ui.add_space(theme::space::HALF);
            let current = theme::theme_id();
            if let Some(picked) = theme::theme_switcher(ui, &current) {
                if theme::set_theme(&picked) {
                    // Cosmetic change, but it has to survive a restart: publish
                    // it through the frame-local config like every other edit.
                    cfg.ui_theme = picked.clone();
                    // Re-apply Visuals so egui's own widgets (sliders, text
                    // edits, collapsing headers) follow the new palette too.
                    refresh_style(ui.ctx());
                    self.toast = Some((save_toast(cfg, format!("配色已切换到 {picked}")), Instant::now()));
                }
            }
            ui.add_space(theme::space::COL);
            // Global zoom — the single knob for "字太小" / "字太大". It scales
            // everything (text, rows, spacing) on top of the OS DPI factor and
            // takes effect the moment it is clicked.
            theme::section(ui, "界面缩放", None);
            ui.add_space(theme::space::HALF);
            ui.horizontal(|ui| {
                for z in [1.0, 1.2, 1.4, 1.6, 1.8, 2.0] {
                    let on = (cfg.ui_scale - z).abs() < 0.01;
                    if theme::chip(ui, &format!("{:.0}%", z * 100.0), on).clicked() {
                        cfg.ui_scale = z;
                        apply_ui_scale(ui.ctx(), z);
                    }
                }
            });
            ui.label(theme::micro(
                &format!("当前 {:.0}% · 放大所有文字与间距,改完立即生效", cfg.ui_scale * 100.0),
                tk.line_dim,
            ));
            ui.add_space(theme::space::COL);
            theme::section(ui, "质感与动效", None);
            ui.add_space(theme::space::HALF);
            ui.horizontal(|ui| {
                let grain = theme::grain_on();
                if theme::chip(ui, "纸感", grain)
                    .on_hover_text("舞台的两层质感:极淡静态噪点 + 蓝图坐标纸网格。全部静态绘制,不影响性能")
                    .clicked()
                {
                    theme::GRAIN.store(!grain, Ordering::Relaxed);
                }
                let reduce = theme::reduce_motion();
                if theme::chip(ui, "减少动效", reduce)
                    .on_hover_text("冻结风扇图标旋转、取消切换视图时的扫描转场(数字照常刷新)")
                    .clicked()
                {
                    theme::REDUCE_MOTION.store(!reduce, Ordering::Relaxed);
                }
            });
            ui.label(theme::micro(
                "动效只在窗口可见且前台时才跑;托盘/后台时整机降到 1fps",
                tk.line_dim,
            ));
            ui.add_space(theme::space::COL);
            theme::section(ui, "排版", None);
            ui.add_space(theme::space::HALF);
            ui.horizontal(|ui| {
                ui.label(theme::micro("拉丁/数字", tk.muted));
                ui.add_space(theme::space::HALF);
                ui.label(theme::label("GEIST MONO · Light/Regular/Medium/SemiBold", tk.fg_soft));
            });
            ui.horizontal(|ui| {
                ui.label(theme::micro("中文回退", tk.muted));
                ui.add_space(theme::space::HALF);
                ui.label(theme::label("微软雅黑 msyh.ttc(系统字体)", tk.fg_soft));
            });
            ui.add_space(theme::space::HALF);
            // a small living specimen of the seven-step scale
            ui.horizontal(|ui| {
                theme::readout(ui, "MICRO", "11", "pt", tk.fg_soft, 96.0);
                theme::readout(ui, "LABEL", "11.5", "pt", tk.fg_soft, 104.0);
                theme::readout(ui, "BODY", "13.5", "pt", tk.fg_soft, 104.0);
                theme::readout(ui, "NUM", "16.5", "pt", tk.fg_soft, 96.0);
            });
            ui.horizontal(|ui| {
                theme::readout(ui, "TITLE", "20", "pt", tk.fg_soft, 96.0);
                theme::readout(ui, "DISPLAY", "30", "pt", tk.fg_soft, 104.0);
                theme::readout(ui, "HERO", "48", "pt", tk.fg_soft, 104.0);
            });
        });
    }

    /// The curve plotting board. Split out of `update()` so the centre stage can
    /// show it alone (View::Curve, taller) or alongside the channel rack
    /// (View::Channels). `height` is the plot's min height in points.
    fn curve_editor(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut AppConfig,
        all_sources: &[String],
        height: f32,
    ) {
        // Drawn as a plotting board: an inset well, hairlines for the
        // grid, square control points, one accent curve.
        let tk = theme::t();
        let ch = self.selected_fan.min(cfg.channels.len().saturating_sub(1));
        theme::section(ui, "曲线编辑", Some(&format!("CH {ch:02}")));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
            // channel picker — clicking a card's name also selects,
            // but that alone was not discoverable
            for i in 0..self.nct.fan_count().min(cfg.channels.len()) {
                let name = channel_display_name(&cfg.channels[i]);
                if theme::chip_raw(ui, &name, self.selected_fan == i, tk.fg_soft)
                    .on_hover_text("点这里切换要编辑的通道")
                    .clicked()
                {
                    self.selected_fan = i;
                }
            }
        });
        let c = &mut cfg.channels[ch];
        ui.horizontal(|ui| {
            ui.label(theme::micro("温度源", tk.muted));
            egui::ComboBox::from_id_salt("src")
                .selected_text(&c.source)
                .width(180.0)
                .show_ui(ui, |ui| {
                    for s in all_sources {
                        if s != &c.source {
                            ui.selectable_value(&mut c.source, s.clone(), s);
                        }
                    }
                });
            ui.label(theme::micro("双击添加 · 拖动调整 · 右键删除", tk.line_dim));
        });

        let (resp, painter) = ui.allocate_painter(
            egui::vec2(ui.available_width(), height),
            egui::Sense::click_and_drag(),
        );
        let rect = resp.rect;
        let (tmin, tmax) = (20.0f32, 95.0f32);
        let (dmin, dmax) = (0.0f32, 100.0f32);
        let tx = |t: f32| rect.left() + (t - tmin) / (tmax - tmin) * rect.width();
        let ty = |d: f32| rect.bottom() - (d - dmin) / (dmax - dmin) * rect.height();

        painter.rect_filled(rect, egui::Rounding::ZERO, tk.well);
        theme::box_line(&painter, rect, tk.hair);
        // corner crop marks, faint: the board is a registration frame
        theme::corner_marks(&painter, rect, 5.0, tk.line_faint);
        let tick_font = theme::Weight::Regular.font(theme::size::MICRO);
        for t in (20..=90).step_by(10) {
            let x = tx(t as f32);
            theme::vline(&painter, x, egui::Rangef::new(rect.top(), rect.bottom()), tk.line_faint);
            painter.text(
                egui::pos2(x, rect.bottom() - 4.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{t}°"),
                tick_font.clone(),
                tk.mark,
            );
        }
        for d in (0..=100).step_by(25) {
            let y = ty(d as f32);
            theme::hline(&painter, egui::Rangef::new(rect.left(), rect.right()), y, tk.line_faint);
            painter.text(
                egui::pos2(rect.left() + 4.0, y - 2.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{d}%"),
                tick_font.clone(),
                tk.mark,
            );
        }

        // current-temp marker. Snaps rather than eases: the temperature is a
        // live value, so an eased cursor would never settle and egui would
        // chase it with a repaint every vsync (see the duty-bar note in the
        // channel card).
        let cur = self.resolve_source(&cfg.channels[ch].source);
        if let Some(t) = cur {
            let x_target = tx(t.clamp(tmin, tmax));
            let x = x_target;
            theme::vline(&painter, x, egui::Rangef::new(rect.top(), rect.bottom()), accent());
            painter.text(
                egui::pos2(x + 6.0, rect.top() + 8.0),
                egui::Align2::LEFT_TOP,
                format!("{t:.1}°C"),
                theme::Weight::Medium.font(theme::size::LABEL),
                accent(),
            );
        }

        // filled area under curve
        let pts = cfg.channels[ch].points.clone();
        if pts.len() >= 2 {
            let mut poly: Vec<egui::Pos2> = pts
                .iter()
                .map(|p| egui::pos2(tx(p[0].clamp(tmin, tmax)), ty(p[1].clamp(dmin, dmax))))
                .collect();
            poly.insert(0, egui::pos2(poly[0].x, rect.bottom()));
            poly.push(egui::pos2(poly[poly.len() - 1].x, rect.bottom()));
            painter.add(egui::Shape::convex_polygon(
                poly,
                accent().gamma_multiply(0.10),
                egui::Stroke::NONE,
            ));
            let lines: Vec<egui::Pos2> = pts
                .iter()
                .map(|p| egui::pos2(tx(p[0].clamp(tmin, tmax)), ty(p[1].clamp(dmin, dmax))))
                .collect();
            painter.add(egui::Shape::line(lines, egui::Stroke::new(1.8_f32, accent())));
        }
        // control points — hover or drag shows the exact (duty @ temp)
        // values; the active point goes accent. Squares, not dots:
        // every corner in this language is square.
        let ptr = resp.interact_pointer_pos();
        let mut delete_idx: Option<usize> = None;
        let dragging_pt = if resp.dragged() { self.curve_edit } else { None };
        for (pi, p) in pts.iter().enumerate() {
            let cpos = egui::pos2(tx(p[0]), ty(p[1]));
            let near = ptr.map_or(false, |ip| ip.distance(cpos) < 10.0);
            let is_drag = matches!(dragging_pt, Some((ech, epi)) if ech == ch && epi == pi);
            let hot = near || is_drag;
            let side = if hot { 8.0 } else { 6.0 };
            let pr = egui::Rect::from_center_size(cpos, egui::Vec2::splat(side));
            let fill = if hot { accent() } else { tk.fg };
            painter.rect_filled(pr, egui::Rounding::ZERO, fill);
            theme::box_line(&painter, pr, tk.bg);
            if hot {
                // flip the readout below the point near the top edge
                let (lpos, align) = if cpos.y - 16.0 < rect.top() {
                    (cpos + egui::vec2(0.0, 13.0), egui::Align2::CENTER_TOP)
                } else {
                    (cpos + egui::vec2(0.0, -13.0), egui::Align2::CENTER_BOTTOM)
                };
                painter.text(
                    lpos,
                    align,
                    format!("{:.0}% @ {:.0}°C", p[1], p[0]),
                    theme::Weight::Medium.font(theme::size::LABEL),
                    tk.fg,
                );
            }
            if ptr.is_some() {
                if resp.dragged() && near {
                    self.curve_edit = Some((ch, pi));
                }
                if resp.secondary_clicked() && near && pts.len() > 2 {
                    delete_idx = Some(pi);
                }
            }
        }
        if let Some(pi) = delete_idx {
            cfg.channels[ch].points.remove(pi);
        }
        if resp.dragged() {
            if let Some((ech, epi)) = self.curve_edit {
                if ech == ch {
                    if let Some(ip) = ptr {
                        let t = (tmin + (ip.x - rect.left()) / rect.width() * (tmax - tmin))
                            .clamp(tmin, tmax);
                        let d = (dmin + (rect.bottom() - ip.y) / rect.height() * (dmax - dmin))
                            .clamp(dmin, dmax);
                        let p = &mut cfg.channels[ch].points;
                        p[epi] = [t, d];
                        p.sort_by(|a, b| a[0].total_cmp(&b[0]));
                        if let Some(newi) = p.iter().position(|q| q[0] == t) {
                            self.curve_edit = Some((ch, newi));
                        }
                    }
                }
            }
            self.apply_channel(ch, cfg);
        }
        if resp.double_clicked() {
            if let Some(ip) = ptr {
                let t = tmin + (ip.x - rect.left()) / rect.width() * (tmax - tmin);
                let d = dmin + (rect.bottom() - ip.y) / rect.height() * (dmax - dmin);
                cfg.channels[ch].points.push([t, d]);
                cfg.channels[ch]
                    .points
                    .sort_by(|a, b| a[0].total_cmp(&b[0]));
            }
        }
    }

    fn restore_channel_auto(&mut self, i: usize, cfg: &mut AppConfig) {
        // Bounded here as well as at the call site: this writes the config, and
        // a channel index from elsewhere (a future hotkey, a saved selection)
        // must not index past the config's end.
        if i >= cfg.channels.len() || i >= self.nct.fan_count() {
            return;
        }
        let ok = self.nct.restore_auto(i);
        cfg.channels[i].mode = FanMode::Auto;
        let name = channel_display_name(&cfg.channels[i]);
        self.toast = Some((
            if ok {
                format!("{name} 已恢复 BIOS 自动")
            } else {
                format!("{name} 本会话未接管过")
            },
            Instant::now(),
        ));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // graceful maintenance stop: exit through the normal path (daemon
        // takes over) so the driver is released cleanly; the config file is
        // deliberately left as-is on disk during a maintenance exit
        let stop_req = gui_stop_sentinel_path();
        if stop_req.exists() {
            let _ = std::fs::remove_file(&stop_req);
            self.maintenance_exit.store(true, Ordering::Relaxed);
            glog("stop request received — exiting for maintenance");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // frame-local working copies; published back at the end of the frame
        let mut cfg: AppConfig = self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // profile switch requested from the tray menu — applied here so the
        // frame-local copy and the end-of-frame publish-back agree
        let req = self.profile_req.swap(255, Ordering::Relaxed);
        if req != 255 {
            cfg.profile = match req {
                0 => FanProfile::Standard,
                1 => FanProfile::Gaming,
                2 => FanProfile::Server,
                _ => cfg.profile,
            };
            let msg = match cfg.profile {
                FanProfile::Standard => "已切回标准模式".to_string(),
                FanProfile::Gaming => "游戏模式：曲线按 +5°C 提前响应".to_string(),
                FanProfile::Server => "服务器模式：缓跟随（限速+迟滞+平滑）".to_string(),
            };
            self.toast = Some((save_toast(&cfg, msg), Instant::now()));
        }
        let mut ovr: FanOverride = *self.ovr.lock().unwrap_or_else(|e| e.into_inner());

        let (reading, nvme, nvme_model, nvme_ext, core_temps, gpu, gpu_ext, sys, chip, fans, age, last_update, targets, hist) = {
            let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.reading.clone(),
                s.nvme.clone(),
                s.nvme_model.clone(),
                s.nvme_ext.clone(),
                s.core_temps.clone(),
                s.gpu,
                s.gpu_ext.clone(),
                s.sys,
                s.chip.clone(),
                s.fans,
                s.last_update.elapsed().as_secs_f32(),
                s.last_update,
                s.targets,
                s.hist.clone(),
            )
        };

        // The animation clock runs only while someone can actually see it:
        // visible AND focused. Everything else repaints at 1 fps with the icons
        // frozen (numbers still update) — an unfocused or tray-resident panel
        // used to keep redrawing at 30 fps.
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        // self-heal: a focused window is definitionally not hidden, no matter
        // which path brought it back (tray click, focus_existing_window from a
        // second launch, OS restore) — otherwise it would render at 1 fps with
        // the icons frozen while plainly visible on screen
        if focused {
            self.hidden.store(false, Ordering::Relaxed);
        }
        let animating = !self.hidden.load(Ordering::Relaxed) && focused;
        // "Reduce motion" freezes the fan icons too: stop advancing the clock
        // rather than passing 0 to every call site, so the phase also does not
        // jump when the switch is toggled back off.
        if animating && !theme::reduce_motion() {
            self.spin_phase += ctx.input(|i| i.stable_dt).min(0.1);
        }
        let spin_t = if theme::reduce_motion() { 0.0 } else { self.spin_phase };
        // boot ceremony runs on the wall clock: focus must never hold the UI
        // hostage
        let boot_t = self.boot_start.elapsed().as_secs_f32();

        // Intercept the window X button: closing the window must NEVER kill
        // the control loop (the fan duty is being written from this process).
        // Cancel the close and hide to the tray instead — the tray menu's
        // 退出 remains the real exit. The maintenance sentinel and the tray
        // quit both send ViewportCommand::Close themselves; eframe delivers
        // that as close_requested too, so they must bypass this intercept via
        // the explicit-exit flag.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.maintenance_exit.load(Ordering::Relaxed)
                || self.user_quit.load(Ordering::Relaxed)
            {
                // let the close through — the maintenance sentinel or the
                // tray menu asked for it
            } else {
                // X means "put the window away, keep the fans running": cancel
                // the close and hide to the tray — no taskbar residue. The
                // tray click was verified end-to-end (UIPI filter + win32
                // wake), and the exe itself can bring the window back via
                // focus_existing_window, so hiding cannot strand anyone. The
                // tray menu's 退出 remains the real quit.
                self.hidden.store(true, Ordering::Relaxed);
                glog("close intercepted — hiding to tray");
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }


        // A2: accumulate one sample per fresh sensor reading, per channel.
        // Conservative by construction: `max_rpm` never resets, so a channel
        // that ever spun can never be marked "空接口".
        for (i, st) in self.chan_stats.iter_mut().enumerate() {
            if st.last_update == Some(last_update) {
                continue;
            }
            st.last_update = Some(last_update);
            st.samples = st.samples.saturating_add(1);
            let rpm = reading.fans_rpm.get(i).copied().flatten().unwrap_or(0.0);
            st.max_rpm = st.max_rpm.max(rpm);
            let duty = reading.controls_pct.get(i).copied().flatten().unwrap_or(0.0);
            st.peak_duty = st.peak_duty.max(duty);
            // same tick feeds the per-card duty sparkline (~96 s of trace)
            if let Some(h) = self.duty_hist.get_mut(i) {
                h.push_back(duty);
                while h.len() > 48 {
                    h.pop_front();
                }
            }
        }

        // temperature source keys offered by the UI (curve editor + algo settings)
        let mut all_sources: Vec<String> = vec!["gpu:0".to_string()];
        all_sources.extend(reading.temps.iter().map(|(n, _)| format!("nct:{n}")));
        for (i, _) in &nvme {
            all_sources.push(format!("nvme:{i}"));
        }
        for c in &cfg.channels {
            if !c.source.is_empty() && !all_sources.contains(&c.source) {
                all_sources.push(c.source.clone());
            }
        }

        // high-temp alert banner
        let alerts = build_alerts(&reading.temps, &nvme);

        // Boot self-test log, built from the *real* machine state (drawn as an
        // overlay at the very end of update, after cfg is published back).
        let boot_lines: Option<Vec<String>> = if !theme::reduce_motion() && boot_t < BOOT_DUR {
            let armed = cfg
                .channels
                .iter()
                .filter(|c| c.managed() && matches!(c.mode, FanMode::Budget))
                .count();
            Some(vec![
                "FAN-TUNE · 风扇调校台 · SELF-TEST".to_string(),
                format!("RING0 后端 ·················· {chip}"),
                format!("风扇通道 ···················· {fans:02}"),
                format!(
                    "温度源 ······················ NCT {} · NVME {} · GPU {}",
                    reading.temps.len(),
                    nvme.len(),
                    if gpu.is_some() { "在线" } else { "无" }
                ),
                format!(
                    "控制档位 ···················· {}",
                    match cfg.profile {
                        FanProfile::Standard => "标准",
                        FanProfile::Gaming => "游戏",
                        FanProfile::Server => "服务器",
                    }
                ),
                format!(
                    "闭环预算 ···················· {}",
                    if armed > 0 {
                        format!("{armed} 通道 ARMED")
                    } else {
                        "未启用".to_string()
                    }
                ),
                format!("调色板 ······················ {}", theme::theme_id().to_uppercase()),
                "自检完成 ····················· LIVE".to_string(),
            ])
        } else {
            None
        };

        // ------- header: a two-row HUD -------
        //
        // Row 1 carries identity (wordmark, view tabs, clock), row 2 the live
        // context (chip, average duty, control profile, link state). Hairlines
        // separate them - no fills, no rounded cards, no shadows.
        let tk = theme::t();
        let (clock, dateline) = clock_strings();
        let avg_duty = reading.controls_pct.iter().filter_map(|x| *x).sum::<f32>()
            / reading.controls_pct.len().max(1) as f32;

        egui::TopBottomPanel::top("hud")
            .frame(egui::Frame::none().fill(bg()))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(theme::space::UNIT, 0.0);

                // ---- row 1: wordmark - view tabs - clock ----
                let r1 = ui
                    .horizontal(|ui| {
                        ui.add_space(theme::space::GUTTER);
                        let (mark, _) =
                            ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                        theme::box_line(ui.painter(), mark, tk.hair);
                        let spin = spin_t * (0.8 + avg_duty / 100.0 * 2.4);
                        draw_fan_icon(ui.painter(), mark.center(), 6.0, accent(), spin);
                        ui.add_space(theme::space::UNIT);
                        ui.label(theme::tracked(
                            "FAN-TUNE",
                            theme::size::TITLE,
                            theme::Weight::SemiBold,
                            text(),
                            theme::track::WORDMARK,
                        ));
                        ui.add_space(theme::space::COL);
                        for (v, num, name) in View::ALL {
                            if theme::chip(ui, &format!("{num} {name}"), self.view == v).clicked() {
                                self.switch_view(v, animating);
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(theme::space::GUTTER);
                            ui.label(theme::micro(&dateline, text_dim()));
                            ui.add_space(theme::space::COL);
                            ui.label(theme::num(
                                &clock,
                                theme::size::TITLE,
                                theme::Weight::Regular,
                                text(),
                            ));
                        });
                    })
                    .response
                    .rect;
                theme::hline(
                    ui.painter(),
                    egui::Rangef::new(r1.left(), r1.right()),
                    r1.bottom(),
                    tk.hair,
                );

                // ---- row 2: chip + average duty - profile - link ----
                let r2 = ui
                    .horizontal(|ui| {
                        ui.add_space(theme::space::GUTTER);
                        let (dot, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().rect_filled(dot, egui::Rounding::ZERO, accent());
                        ui.add_space(theme::space::HALF);
                        ui.label(theme::micro(
                            &format!("{chip} · AVG DUTY {avg_duty:.0}%"),
                            text_dim(),
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(theme::space::GUTTER);
                            let live = age < 3.0;
                            let (lamp, _) =
                                ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                            // The lamp breathes while the link is up — but only
                            // while someone is watching: `spin_t` freezes with the
                            // `animating` gate, so a hidden window pays nothing.
                            let beat = if live && animating {
                                0.55 + 0.45 * (spin_t * std::f32::consts::TAU / 1.6).sin()
                            } else {
                                1.0
                            };
                            ui.painter().rect_filled(
                                lamp,
                                egui::Rounding::ZERO,
                                if live {
                                    accent().gamma_multiply(beat)
                                } else {
                                    hot()
                                },
                            );
                            ui.add_space(theme::space::HALF);
                            ui.label(theme::micro(
                                if live { "LIVE" } else { "NO DATA" },
                                if live { text_dim() } else { hot() },
                            ));
                            ui.add_space(theme::space::COL);
                            // nested left-to-right row: inside the header's
                            // right-to-left layout the chips would otherwise
                            // land in reverse reading order
                            ui.horizontal(|ui| {
                            for (p, label, tip) in [
                                (
                                    FanProfile::Standard,
                                    "标准",
                                    "每条通道按自己的曲线 / 手动 / 自动运行",
                                ),
                                (
                                    FanProfile::Gaming,
                                    "游戏",
                                    "更早响应：曲线按“温度 +5°C”提前拉\n（偏移量 gaming_temp_offset 可在 fantune.json 调整）",
                                ),
                                (
                                    FanProfile::Server,
                                    "服务器",
                                    "长期稳定优先：限速 3%/s + 迟滞 3°C + 平滑\n通道温度 ≥85°C 时强制 100%（server_guard_c 可调）",
                                ),
                            ] {
                                let on = cfg.profile == p;
                                if theme::chip(ui, label, on).on_hover_text(tip).clicked() && !on {
                                    cfg.profile = p;
                                    let msg = match p {
                                        FanProfile::Standard => "已切回标准模式".to_string(),
                                        FanProfile::Gaming => {
                                            "游戏模式：曲线按 +5°C 提前响应".to_string()
                                        }
                                        FanProfile::Server => {
                                            "服务器模式：缓跟随（限速+迟滞+平滑）".to_string()
                                        }
                                    };
                                    self.toast = Some((save_toast(&cfg, msg), Instant::now()));
                                }
                            }
                            });
                        });
                    })
                    .response
                    .rect;
                // A heavy rule closes the header: the reference separates its
                // chrome from the stage with a 2 px line, not another hairline.
                theme::hline(
                    ui.painter(),
                    egui::Rangef::new(r2.left(), r2.right()),
                    r2.bottom(),
                    tk.line_dim,
                );
            });

        // ------- alert banner: hazard tape -------
        if !alerts.is_empty() {
            egui::TopBottomPanel::top("alerts")
                .frame(egui::Frame::none().fill(bg()))
                .show(ctx, |ui| {
                    let (bar, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 24.0),
                        egui::Sense::hover(),
                    );
                    let tape = egui::Rect::from_min_size(bar.min, egui::vec2(84.0, bar.height()));
                    theme::hazard_stripes(ui.painter(), tape, 7.0, hot(), bg());
                    let p = ui.painter();
                    p.text(
                        egui::Pos2::new(bar.left() + 12.0, bar.center().y),
                        egui::Align2::LEFT_CENTER,
                        "ALERT",
                        theme::Weight::SemiBold.font(theme::size::MICRO),
                        bg(),
                    );
                    p.text(
                        egui::Pos2::new(bar.left() + 100.0, bar.center().y),
                        egui::Align2::LEFT_CENTER,
                        alerts.join("   ·   "),
                        theme::Weight::Medium.font(theme::size::LABEL),
                        hot(),
                    );
                });
        }

        // ------- readout strip: five instruments -------
        egui::TopBottomPanel::top("readouts")
            .frame(egui::Frame::none().fill(bg()))
            .show(ctx, |ui| {
                let cpu_t = reading
                    .temps
                    .iter()
                    .find(|(n, _)| n.contains("PECI"))
                    .map(|(_, t)| *t);
                let hottest_nvme = nvme.iter().max_by(|a, b| a.1.total_cmp(&b.1));
                let pump_rpm = reading.fans_rpm[5];
                let max_duty = reading
                    .controls_pct
                    .iter()
                    .filter_map(|x| *x)
                    .fold(0.0f32, f32::max);

                let gpu_label = match gpu {
                    Some((_, u)) => format!("GPU TEMP · LOAD {u:.0}%"),
                    None => "GPU TEMP".to_string(),
                };
                let instruments: Vec<(String, String, &'static str, egui::Color32)> = vec![
                    (
                        "CPU TEMP · PECI".to_string(),
                        cpu_t.map(|t| format!("{t:.1}")).unwrap_or_else(|| "—".into()),
                        "°C",
                        cpu_t.map(temp_color).unwrap_or(text_dim()),
                    ),
                    (
                        gpu_label,
                        gpu.map(|(t, _)| format!("{t:.1}")).unwrap_or_else(|| "—".into()),
                        "°C",
                        gpu.map(|(t, _)| temp_color(t)).unwrap_or(text_dim()),
                    ),
                    (
                        "HOTTEST NVME".to_string(),
                        hottest_nvme
                            .map(|(_, t)| format!("{t:.1}"))
                            .unwrap_or_else(|| "—".into()),
                        "°C",
                        hottest_nvme
                            .map(|(_, t)| temp_color(*t))
                            .unwrap_or(text_dim()),
                    ),
                    (
                        "PUMP · 5.5K=MAX".to_string(),
                        pump_rpm.map(|v| format!("{v:.0}")).unwrap_or_else(|| "—".into()),
                        "RPM",
                        pump_rpm
                            .map(|v| if v > 4500.0 { text() } else { warn() })
                            .unwrap_or(text_dim()),
                    ),
                    (
                        "MAX DUTY".to_string(),
                        format!("{max_duty:.0}"),
                        "%",
                        if max_duty >= 90.0 { hot() } else { accent() },
                    ),
                ];

                let mut rects: Vec<egui::Rect> = Vec::new();
                ui.horizontal(|ui| {
                    ui.add_space(theme::space::GUTTER);
                    let w = ((ui.available_width() - theme::space::GUTTER * 2.0) / 5.0
                        - theme::space::COL)
                        .max(88.0);
                    for (label, value, unit, color) in instruments {
                        rects.push(theme::readout(ui, &label, &value, unit, color, w));
                        ui.add_space(theme::space::COL);
                    }
                });
                for pair in rects.windows(2) {
                    let x = (pair[0].right() + pair[1].left()) / 2.0;
                    theme::vline(
                        ui.painter(),
                        x,
                        egui::Rangef::new(pair[0].top(), pair[0].bottom() - 8.0),
                        tk.hair,
                    );
                }
            });

        // ------- status + action bar -------
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::none().fill(bg()))
            .show(ctx, |ui| {
                let managed = cfg.channels.iter().filter(|c| c.managed()).count();
                let budget = cfg.channels.iter().map(|c| c.budget_c).fold(0.0f32, f32::max);
                let status = if budget > 0.0 {
                    format!(
                        "{} CH · {} ON · BUDGET {budget:.0}°C · UPD {age:.0}s",
                        reading.controls_pct.len(),
                        managed
                    )
                } else {
                    format!(
                        "{} CH · {} ON · UPD {age:.0}s",
                        reading.controls_pct.len(),
                        managed
                    )
                };
                let r = ui
                    .horizontal(|ui| {
                        ui.add_space(theme::space::GUTTER);
                        // registration crosses pin the corners of the chrome,
                        // the way engineering drawings are framed
                        let (cross_l, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        theme::reg_cross(ui.painter(), cross_l.center(), 5.0, tk.mark);
                        ui.add_space(theme::space::UNIT);
                        let (dot, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().rect_filled(dot, egui::Rounding::ZERO, tk.hair);
                        ui.add_space(theme::space::HALF);
                        ui.label(theme::micro(&status, text_dim()));
                        ui.add_space(theme::space::COL);
                        ui.label(theme::micro(self.view.label(), tk.mark));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(theme::space::GUTTER);
                            // the matching cross pins the other corner
                            let (cross_r, _) =
                                ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            theme::reg_cross(ui.painter(), cross_r.center(), 5.0, tk.mark);
                            ui.add_space(theme::space::UNIT);
                            if theme::chip(ui, "收起", false)
                                .on_hover_text("等同于点击右上角 X：收起到托盘（任务栏不留图标；点右下角托盘图标回来），控制循环继续运行。要真正退出请用托盘菜单的“退出”")
                                .clicked()
                            {
                                self.hidden.store(true, Ordering::Relaxed);
                                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                            }
                            if theme::chip(ui, "保存配置", false).clicked() {
                                self.toast =
                                    Some((save_toast(&cfg, "配置已保存".into()), Instant::now()));
                            }
                            if theme::chip(ui, "恢复 BIOS", false)
                                .on_hover_text("所有通道交回主板，不再写入")
                                .clicked()
                            {
                                self.restore_all_auto(&mut cfg, &mut ovr);
                            }
                            if theme::chip(ui, "静音", ovr == FanOverride::Quiet)
                                .on_hover_text("覆盖：所有通道 40%·再点一次取消")
                                .clicked()
                            {
                                ovr = if ovr == FanOverride::Quiet {
                                    FanOverride::None
                                } else {
                                    FanOverride::Quiet
                                };
                                self.toast = Some((
                                    if ovr == FanOverride::Quiet {
                                        "静音模式：所有通道 40%".into()
                                    } else {
                                        "已取消静音，恢复常规控制".into()
                                    },
                                    Instant::now(),
                                ));
                            }
                            if theme::chip(ui, "全速", ovr == FanOverride::Full)
                                .on_hover_text("覆盖：所有通道 100%·再点一次取消")
                                .clicked()
                            {
                                ovr = if ovr == FanOverride::Full {
                                    FanOverride::None
                                } else {
                                    FanOverride::Full
                                };
                                self.toast = Some((
                                    if ovr == FanOverride::Full {
                                        "全速散热：所有通道 100%".into()
                                    } else {
                                        "已取消全速，恢复常规控制".into()
                                    },
                                    Instant::now(),
                                ));
                            }
                        });
                    })
                    .response
                    .rect;
                theme::hline(
                    ui.painter(),
                    egui::Rangef::new(r.left(), r.right()),
                    r.top(),
                    tk.hair,
                );
            });

        // ------- left sensor list -------
        egui::SidePanel::left("sensors")
            .resizable(false)
            .exact_width(292.0)
            // One step up from the page background: the rail reads as its own
            // instrument stack instead of blending into the centre stage.
            .frame(egui::Frame::none().fill(panel()).inner_margin(egui::Margin::symmetric(12.0, 8.0)))
            .show(ctx, |ui| {
                // sensor list scrolls; the history chart stays pinned below it
                // (legend ~18 + spacing 8 + chart 100 + separator ~10 + pad)
                let list_h = (ui.available_height() - 150.0).max(120.0);
                egui::ScrollArea::vertical().max_height(list_h).show(ui, |ui| {
                    // ---- headline instrument: heat reactor + core spectrum ----
                    // The reactor's gauge sweeps with CPU temp (30..95 °C), its
                    // blades spin with the average duty — the phase comes from
                    // spin_t, so it freezes when nobody is watching. The dense
                    // spectrum next to it is the sixteen core temps.
                    if let Some(cpu_t) = reading
                        .temps
                        .iter()
                        .find(|(n, _)| n.contains("PECI"))
                        .map(|(_, t)| *t)
                    {
                        let heat = (cpu_t - 30.0) / (95.0 - 30.0);
                        ui.horizontal(|ui| {
                            let (reactor_rect, _) = ui.allocate_exact_size(
                                egui::vec2(44.0, 42.0),
                                egui::Sense::hover(),
                            );
                            theme::reactor(
                                ui.painter(),
                                reactor_rect.center(),
                                17.0,
                                heat,
                                spin_t * (0.35 + avg_duty / 100.0 * 1.8),
                            );
                            ui.add_space(theme::space::UNIT);
                            ui.vertical(|ui| {
                                ui.add_space(1.0);
                                ui.label(theme::micro(
                                    &(if core_temps.is_empty() {
                                        format!("CORE HEAT · {cpu_t:.0}°C")
                                    } else {
                                        format!(
                                            "CORE HEAT · {cpu_t:.0}°C · MAX {:.0}°",
                                            core_temps
                                                .iter()
                                                .map(|(_, t)| *t)
                                                .fold(f32::MIN, f32::max)
                                        )
                                    }),
                                    tk.fg_soft,
                                ));
                                if !core_temps.is_empty() {
                                    let n = core_temps.len();
                                    let (spec, _) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), 26.0),
                                        egui::Sense::hover(),
                                    );
                                    let gap = 2.0;
                                    let cw = ((spec.width() - gap * (n.max(1) - 1) as f32)
                                        / n.max(1) as f32)
                                        .clamp(1.5, 12.0);
                                    for (k, (_, t)) in core_temps.iter().enumerate() {
                                        let f = ((t - 40.0) / (90.0 - 40.0)).clamp(0.06, 1.0);
                                        let x = spec.left() + k as f32 * (cw + gap);
                                        ui.painter().rect_filled(
                                            egui::Rect::from_min_size(
                                                egui::pos2(
                                                    x,
                                                    spec.bottom() - spec.height() * f,
                                                ),
                                                egui::vec2(cw, spec.height() * f),
                                            ),
                                            egui::Rounding::ZERO,
                                            temp_color(*t),
                                        );
                                    }
                                    theme::hline(
                                        ui.painter(),
                                        egui::Rangef::new(spec.left(), spec.right()),
                                        spec.bottom() + 1.0,
                                        tk.line_faint,
                                    );
                                }
                            });
                        });
                        ui.add_space(theme::space::UNIT);
                    }

                    // The rail reads as one ruled instrument sheet: section rules
                    // in the header style, then rows whose values all land on the
                    // same right edge.
                    // detailed_temps additionally carries the filtered-out
                    // readings (floating = true) — they are "no signal" rows now
                    let temp_rows: Vec<(String, f32, bool)> = if reading.detailed_temps.is_empty() {
                        reading.temps.iter().map(|(n, t)| (n.clone(), *t, false)).collect()
                    } else {
                        reading
                            .detailed_temps
                            .iter()
                            .map(|r| (r.name.clone(), r.celsius, r.floating))
                            .collect()
                    };
                    theme::section(ui, "温度", Some(&format!("{:02}", temp_rows.len())));
                    for (name, t, floating) in &temp_rows {
                        // A floating pin is not a reading, it is wire noise: drop
                        // it to the dimmest token and say so, rather than
                        // colouring a number that means nothing.
                        let color = if *floating { tk.mark } else { temp_color(*t) };
                        let label = if *floating {
                            format!("{name} 悬空")
                        } else {
                            name.clone()
                        };
                        let value = if *floating {
                            "—".to_string()
                        } else {
                            format!("{t:.1} °C")
                        };
                        sensor_row(ui, &label, &value, color, RowIcon::Temp(color));
                    }
                    // (per-core chart used to live here — it moved up next to
                    // the reactor as the rail's headline spectrum)
                    ui.add_space(theme::space::HALF);
                    // drives: temp row + remaining-life bar + SMART footnote
                    for (i, t) in &nvme {
                        let color = temp_color(*t);
                        let full_model = nvme_model
                            .iter()
                            .find(|(idx, _)| idx == i)
                            .map(|(_, m)| m.clone())
                            .unwrap_or_default();
                        let name = drive_display_name(*i, &nvme_model);
                        let value = format!("{t:.1} °C");
                        let resp = sensor_row(ui, &name, &value, color, RowIcon::Drive(tk.mark));
                        if !full_model.is_empty() {
                            resp.on_hover_text(&full_model);
                        }
                        if let Some((_, fields)) = nvme_ext.iter().find(|(idx, _)| idx == i) {
                            if !fields.is_empty() {
                                let life = fields.iter().find(|(k, _)| k == "寿命").and_then(|(_, v)| {
                                    v.trim_end_matches('%').trim().parse::<f32>().ok()
                                });
                                let smart = fields
                                    .iter()
                                    .filter(|(k, _)| k != "寿命")
                                    .map(|(k, v)| format!("{k} {v}"))
                                    .collect::<Vec<_>>()
                                    .join(" · ");
                                ui.horizontal(|ui| {
                                    ui.add_space(16.0);
                                    let bar_w = if life.is_some() { 92.0 } else { 0.0 };
                                    if let Some(life) = life {
                                        // remaining life: quiet ink while healthy,
                                        // the state colours only as it runs out
                                        let fill = if life < 15.0 {
                                            hot()
                                        } else if life < 30.0 {
                                            warn()
                                        } else {
                                            tk.fg_soft
                                        };
                                        let (bar, _) = ui.allocate_exact_size(
                                            egui::vec2(bar_w, 3.0),
                                            egui::Sense::hover(),
                                        );
                                        theme::bar_at(ui.painter(), bar, life, fill);
                                        ui.add_space(theme::space::HALF);
                                        ui.label(theme::micro(&format!("余量 {life:.0}%"), tk.line_dim));
                                    }
                                    if !smart.is_empty() {
                                        // never let a long SMART string widen the
                                        // row (and with it the whole panel + the
                                        // centre stage): truncate, full text on
                                        // hover
                                        ui.add(
                                            egui::Label::new(theme::micro(&smart, tk.line_dim))
                                                .wrap_mode(egui::TextWrapMode::Truncate),
                                        )
                                        .on_hover_text(&smart);
                                    }
                                });
                            }
                        }
                    }
                    ui.add_space(theme::space::HALF);
                    // GPU: temp row + load/VRAM bars + power footnote
                    if let Some((t, u)) = gpu {
                        let color = temp_color(t);
                        sensor_row(
                            ui,
                            "显卡",
                            &format!("{t:.1} °C · {u:.0}%"),
                            color,
                            RowIcon::Chip(tk.mark),
                        );
                        // structured stats live in gpu_ext strings (built by the
                        // sensor thread); pull numbers back out for the bars —
                        // a miss degrades to the plain row, never to a panic
                        let parse_pct = |v: &str| -> Option<f32> {
                            v.trim().trim_end_matches('%').trim().parse::<f32>().ok()
                        };
                        let parse_vram = |v: &str| -> Option<f32> {
                            let mut it = v.split('/');
                            let used: f32 = it.next()?.trim().parse().ok()?;
                            let total: f32 = it
                                .next()?
                                .trim()
                                .trim_end_matches("MB")
                                .trim()
                                .parse()
                                .ok()?;
                            (total > 0.0).then(|| used / total * 100.0)
                        };
                        let find = |key: &str| -> Option<String> {
                            gpu_ext
                                .iter()
                                .find(|(k, _)| k == key)
                                .map(|(_, v)| v.clone())
                        };
                        for (key, pct, cap) in [
                            ("GPU 占用", Some(u), "负载"),
                            ("显存", None, "显存"),
                        ] {
                            let pct = pct.or_else(|| find(key).as_deref().and_then(|v| {
                                if key == "显存" { parse_vram(v) } else { parse_pct(v) }
                            }));
                            if let Some(pct) = pct {
                                ui.horizontal(|ui| {
                                    ui.add_space(16.0);
                                    let (bar, _) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width() - 16.0, 3.0),
                                        egui::Sense::hover(),
                                    );
                                    theme::bar_at(ui.painter(), bar, pct, tk.load_color(pct));
                                });
                                ui.label(theme::micro(
                                    &format!("{cap} {pct:.0}%"),
                                    tk.line_dim,
                                ));
                            }
                        }
                    }
                    ui.add_space(theme::space::HALF);
                    theme::section(ui, "系统", None);
                    // CPU load + RAM, each with an inline utilisation bar
                    let cpu_color = tk.load_color(sys.cpu_load);
                    sensor_row(
                        ui,
                        "CPU 负载",
                        &format!("{:.1} %", sys.cpu_load),
                        cpu_color,
                        RowIcon::Chip(tk.mark),
                    );
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        let (bar, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width() - 16.0, 3.0),
                            egui::Sense::hover(),
                        );
                        theme::bar_at(ui.painter(), bar, sys.cpu_load, cpu_color);
                    });
                    let ram_color = tk.load_color(sys.ram_load);
                    sensor_row(
                        ui,
                        "内存占用",
                        &format!("{:.1}% {:.0} / {:.0} GB", sys.ram_load, sys.ram_used, sys.ram_total),
                        ram_color,
                        RowIcon::Drive(tk.mark),
                    );
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        let (bar, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width() - 16.0, 3.0),
                            egui::Sense::hover(),
                        );
                        theme::bar_at(ui.painter(), bar, sys.ram_load, ram_color);
                    });
                    // extended GPU stats that have no bar of their own
                    for (k, v) in &gpu_ext {
                        if k == "GPU 占用" || k == "显存" {
                            continue; // already shown as bars above
                        }
                        sensor_row(ui, k, v, tk.line_dim, RowIcon::None);
                    }
                    ui.add_space(theme::space::UNIT);
                    theme::section(ui, "风扇", Some(&format!("{:02}", reading.fans_rpm.len())));
                    for (i, rpm) in reading.fans_rpm.iter().enumerate() {
                        let duty = reading.controls_pct[i].unwrap_or(0.0);
                        let rpm_s = rpm.map(|v| format!("{v:.0}")).unwrap_or_else(|| "—".into());
                        // same name as the channel card — "通道 3" told the user nothing
                        let chan_name = cfg
                            .channels
                            .get(i)
                            .map(channel_display_name)
                            .unwrap_or_else(|| format!("通道 {i}"));
                        // a stopped fan is a dim row, not a coloured one
                        let live = rpm.unwrap_or(0.0) > 100.0;
                        let color = if live { tk.fg_soft } else { tk.line_dim };
                        sensor_row(
                            ui,
                            &chan_name,
                            &format!("{rpm_s} rpm · {duty:.0}%"),
                            color,
                            RowIcon::Fan {
                                color: if live { tk.mark } else { tk.line_faint },
                                spin: spin_t * (0.5 + duty / 100.0 * 2.0),
                            },
                        );
                        // duty bar with this tick's target as a tick mark —
                        // the same working-ink rule as the channel cards
                        let fill = if !live {
                            tk.line_faint
                        } else if duty >= 90.0 {
                            hot()
                        } else {
                            accent()
                        };
                        let target = targets.get(i).copied().flatten();
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            let (bar, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width() - 16.0, 3.0),
                                egui::Sense::hover(),
                            );
                            theme::bar_at(ui.painter(), bar, duty, fill);
                            if let Some(tg) = target {
                                if (0.0..=100.0).contains(&tg) {
                                    let x = bar.left() + 1.0
                                        + (bar.width() - 2.0) * tg.clamp(0.0, 100.0) / 100.0;
                                    theme::vline(
                                        ui.painter(),
                                        x,
                                        egui::Rangef::new(bar.top() - 2.0, bar.bottom() + 2.0),
                                        tk.fg,
                                    );
                                }
                            }
                        });
                    }
                    ui.add_space(theme::space::UNIT);
                    // voltages: raw SuperIO readings only — guessing rail names
                    // ("VCore?") misfired on several pins and read as wrong data,
                    // so the whole thing is a collapsed reference list now
                    egui::CollapsingHeader::new(theme::micro(
                        &format!("电压 · {} 路", reading.voltages.len()),
                        tk.muted,
                    ))
                    .id_salt("voltages")
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::Grid::new("voltage_grid")
                            .num_columns(4)
                            .spacing([theme::space::COL, theme::space::HALF])
                            .show(ui, |ui| {
                                for (k, (name, v)) in reading.voltages.iter().enumerate() {
                                    ui.label(theme::micro(name, tk.mark));
                                    ui.label(theme::num(
                                        &format!("{v:.2} V"),
                                        theme::size::LABEL,
                                        theme::Weight::Regular,
                                        tk.fg_soft,
                                    ));
                                    if k % 2 == 1 {
                                        ui.end_row();
                                    }
                                }
                            });
                    });
                    ui.add_space(theme::space::HALF);
                });
                theme::hline(
                    ui.painter(),
                    egui::Rangef::new(ui.max_rect().left(), ui.max_rect().right()),
                    ui.cursor().top(),
                    tk.hair,
                );
                ui.add_space(theme::space::UNIT);
                draw_history(ui, &hist);
            });

        // ------- main panel: the centre stage -------
        // The header, readout strip, sensor rail and status bar stay put, so
        // switching views never costs you the live context; only this block
        // responds to `self.view`.
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg()).inner_margin(egui::Margin::symmetric(16.0, 8.0)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Scene layer, under everything: blueprint grid over the
                    // scroll extent (it scrolls with the content, like paper),
                    // gated by the 纸感 switch together with the grain. The
                    // vignette was cut — a black veil over a black page reads
                    // as dirt, not depth.
                    if theme::grain_on() {
                        let stage = ui.available_rect_before_wrap();
                        theme::blueprint_grid(ui.painter(), stage);
                    }
                    // Ghost word: the view's Latin name, huge and barely there,
                    // stamped behind the stage like the reference does. Pure
                    // paint — drawn first so every widget lands on top of it.
                    let stage = ui.available_rect_before_wrap();
                    theme::ghost_text(
                        ui.painter(),
                        egui::pos2(stage.right() - 6.0, stage.top() + 2.0),
                        egui::Align2::RIGHT_TOP,
                        self.view.label(),
                        64.0,
                        theme::t().line_faint,
                    );
                    match self.view {
                    View::Channels => {
                    // The rack is the whole stage here: RGB and the curve plot
                    // Cards follow the chip, not the config: `default_with_7`
                    // always builds 7 channels, and a 5-fan chip would other-
                    // wise show two dead cards with no hardware behind them.
                    let shown = self.nct.fan_count().min(cfg.channels.len());
                    // have their own tabs, so the cards get the full height and
                    // seven channels read in one glance.
                    theme::section(
                        ui,
                        "风扇通道",
                        Some(&format!("{:02}", shown)),
                    );
                    for i in 0..shown {
                        let rpm = reading.fans_rpm[i];
                        let duty = reading.controls_pct[i].unwrap_or(0.0);
                        let target = targets[i];
                        let is_sel = self.selected_fan == i;
                        let empty_channel = self.chan_stats.get(i).map_or(false, |st| st.empty_channel());
                        let display_name = channel_display_name(&cfg.channels[i]);
                        let editing = matches!(&self.rename, Some((ri, _)) if *ri == i);

                        // The case colour eases towards the lifted tone on selection.
                        // Animate only while someone is watching: an egui animation
                        // re-requests a repaint every frame it is in flight, and
                        // these targets change continuously — that alone pinned the
                        // app to the monitor's refresh rate. The selection *state*
                        // is carried by the outline and corner marks, so a frozen
                        // ease here still shows the right thing instantly.
                        let card_id = egui::Id::new(("card_hover", i));
                        let hover_f = if animating {
                            ctx.animate_bool_with_time(card_id, is_sel, 0.15)
                        } else if is_sel {
                            1.0
                        } else {
                            0.0
                        };
                        let bg = lerp_color(card(), card_hover(), hover_f);

                        let mut rename_commit: Option<String> = None;
                        let mut rename_cancel = false;

// The card is an instrument case: zero radius, hairline,
                        // and — when selected — an accent outline plus corner
                        // crop marks, which is how this language says "focused".
                        // Two tight rows (identity+mode, duty+gauge+speed): the
                        // rack has to fit seven channels without scrolling.
                        let card = theme::panel_frame()
                            .inner_margin(egui::Margin::symmetric(10.0, 5.0))
                            .fill(bg)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                let mode_prev = cfg.channels[i].mode;
                                let mut mode_now = mode_prev;

                                // ---- row A: fan · index · name · mode chips ----
                                ui.horizontal(|ui| {
                                    let c = &mut cfg.channels[i];
                                    // the fan glyph spins with the real speed —
                                    // the one piece of motion that is data, not
                                    // decoration (frozen with `animating`)
                                    let (ig, _) =
                                        ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                                    draw_fan_icon(
                                        ui.painter(),
                                        ig.center(),
                                        6.0,
                                        if is_sel { accent() } else { theme::t().mark },
                                        spin_t * (0.5 + duty / 100.0 * 2.5),
                                    );
                                    ui.add_space(theme::space::HALF);
                                    // The zero-padded index is the rail's anchor:
                                    // it survives every rename.
                                    ui.label(theme::num(
                                        &format!("{i:02}"),
                                        16.0,
                                        theme::Weight::Light,
                                        if is_sel { accent() } else { theme::t().mark },
                                    ));
                                    ui.add_space(theme::space::HALF);
                                    if editing {
                                        // inline rename: label wins over name once committed
                                        let buf = &mut self.rename.as_mut().expect("editing implies rename is set").1;
                                        let resp = ui.add(
                                            egui::TextEdit::singleline(buf)
                                                .desired_width(140.0)
                                                .font(theme::Weight::Medium.font(theme::size::BODY))
                                                .hint_text("通道名称"),
                                        );
                                        if resp.lost_focus() {
                                            rename_commit = Some(buf.trim().to_string());
                                        }
                                        if ui.input(|inp| inp.key_pressed(egui::Key::Escape)) {
                                            rename_cancel = true;
                                        }
                                    } else {
                                        // The name is the loudest text on the card:
                                        // body-size medium at full ink, clickable,
                                        // double-click to rename.
                                        let name_resp = ui
                                            .label(theme::tracked(
                                                &display_name,
                                                theme::size::BODY + 1.5,
                                                theme::Weight::Medium,
                                                if is_sel { accent() } else { theme::t().fg },
                                                0.4,
                                            ))
                                            .interact(egui::Sense::click())
                                            .on_hover_text("单击选中 • 双击改名");
                                        if name_resp.clicked() {
                                            self.selected_fan = i;
                                        }
                                        if name_resp.double_clicked() {
                                            self.rename = Some((i, c.label.clone()));
                                        }
                                        ui.add_space(theme::space::HALF);
                                        if theme::chip(ui, "改名", false)
                                            .on_hover_text("给这个通道起个名字(双击名称也可以)")
                                            .clicked()
                                        {
                                            self.rename = Some((i, c.label.clone()));
                                        }
                                        if i < fans
                                            && theme::chip(ui, "识别", false)
                                                .on_hover_text(
                                                    "让这个通道满速转 3 秒,靠听/看确认它到底是哪个风扇;\
                                                     结束后自动交还给原设定(不写入的通道直接还回 BIOS)",
                                                )
                                                .clicked()
                                        {
                                            *self.pulse.lock().unwrap_or_else(|e| e.into_inner()) =
                                                Some((i, Instant::now() + Duration::from_secs(3)));
                                            self.toast = Some((
                                                format!("{display_name}:满速 3 秒 —— 注意听/看是哪一个"),
                                                Instant::now(),
                                            ));
                                        }
                                        if i >= fans {
                                            theme::chip_raw(ui, "本机无此通道", false, theme::t().muted)
                                                .on_hover_text(
                                                    "检测到的芯片没有这么多风扇通道:这些通道的寄存器属于别的功能,程序不会写它们。",
                                                );
                                        }
                                        if empty_channel {
                                            theme::chip_raw(ui, "空接口", false, theme::t().muted)
                                                .on_hover_text(
                                                    "累计采样 ≥60 次、转速始终为 0,且读到过 ≥40% 占空——大概率未接风扇",
                                                );
                                        }
                                        if !c.enabled {
                                            theme::chip_raw(ui, "未启用·不写入", false, theme::t().warn)
                                                .on_hover_text(
                                                    "enabled = false:程序完全不碰这个通道(曲线、手动、一键全速/静音、安全兜底都不写),风扇只由 BIOS 控制。要交给我们管,把它改成 true(改完点保存)。",
                                                );
                                        }
                                    }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        // mode chips are hidden while the channel is
                                        // not managed: the program never writes it,
                                        // so offering modes is noise — and four chips
                                        // are the widest row-A item (on the empty
                                        // channels, which already carry two state
                                        // chips, they used to push the rack past the
                                        // window and drag a black band across the
                                        // stage)
                                        if c.enabled {
                                            // chips, not radios: the active one inverts
                                            // to a solid block (the language's "current")
                                            let mut pick = |ui: &mut egui::Ui, want: FanMode, label: &str| {
                                                let on_this = c.mode == want;
                                                if theme::chip(ui, label, on_this).clicked() {
                                                    c.mode = want;
                                                }
                                            };
                                            pick(ui, FanMode::Curve, "曲线");
                                            pick(ui, FanMode::Budget, "预算");
                                            pick(ui, FanMode::Manual, "手动");
                                            pick(ui, FanMode::Auto, "自动");
                                        }
                                        mode_now = c.mode;
                                    });
                                });

                                // ---- row B: duty · gauge · rpm · target · apply ----
                                // The accent is the working ink of the card: the
                                // hero number and the gauge fill share it, so the
                                // eye lands on the speed in one move. Grey means
                                // the channel is not being driven; red is reserved
                                // for a fan pinned near 100%.
                                let driven = !matches!(cfg.channels[i].mode, FanMode::Auto);
                                let hero = if !driven {
                                    text_dim()
                                } else if duty >= 90.0 {
                                    hot()
                                } else {
                                    accent()
                                };
                                let mut apply_now = false;
                                ui.horizontal(|ui| {
                                    ui.label(theme::num(
                                        &format!("{duty:.0}"),
                                        theme::size::NUM + 5.0,
                                        theme::Weight::Light,
                                        hero,
                                    ));
                                    ui.label(theme::micro("%", theme::t().muted));
                                    ui.add_space(theme::space::HALF);
                                    // No egui value animation here, on purpose:
                                    // the duty reading changes every control
                                    // tick, so an eased bar never settles and
                                    // egui re-requests a repaint every vsync —
                                    // the exact pattern that once pinned this
                                    // app to the monitor's refresh rate. The
                                    // bar snaps; the number itself is live.
                                    let duty_anim = duty;
                                    let reserved = 170.0 + if driven { 54.0 } else { 0.0 };
                                    let bar_w = (ui.available_width() - reserved).max(90.0);
                                    let (bar_rect, bar_resp) = ui.allocate_exact_size(
                                        egui::vec2(bar_w, 10.0),
                                        egui::Sense::hover(),
                                    );
                                    let painter = ui.painter();
                                    painter.rect_filled(bar_rect, egui::Rounding::ZERO, theme::t().well);
                                    theme::box_line(painter, bar_rect, theme::t().hair);
                                    let f = (duty_anim / 100.0).clamp(0.0, 1.0);
                                    if f > 0.005 {
                                        painter.rect_filled(
                                            egui::Rect::from_min_size(
                                                egui::pos2(bar_rect.left() + 1.0, bar_rect.top() + 1.0),
                                                egui::vec2(
                                                    (bar_rect.width() - 2.0) * f,
                                                    bar_rect.height() - 2.0,
                                                ),
                                            ),
                                            egui::Rounding::ZERO,
                                            hero,
                                        );
                                    }
                                    if let Some(tg) = target {
                                        let x = bar_rect.left()
                                            + 1.0
                                            + (bar_rect.width() - 2.0) * (tg.clamp(0.0, 100.0) / 100.0);
                                        theme::vline(
                                            painter,
                                            x,
                                            egui::Rangef::new(bar_rect.top() - 2.0, bar_rect.bottom() + 2.0),
                                            theme::t().fg,
                                        );
                                    }
                                    bar_resp.on_hover_text(match target {
                                        Some(t) => format!("实际 {duty:.0}% · 目标 {t:.0}% · 细线 = 近 96 秒轨迹"),
                                        None => format!("实际 {duty:.0}% · 当前无目标 · 细线 = 近 96 秒轨迹"),
                                    });
                                    // the recent duty trace rides the track:
                                    // ~96 s of history as a single hairline
                                    // (samples land once per sensor tick)
                                    if let Some(h) = self.duty_hist.get_mut(i) {
                                        theme::spark(
                                            painter,
                                            bar_rect,
                                            h.make_contiguous(),
                                            0.0,
                                            100.0,
                                            theme::t().fg.gamma_multiply(0.55),
                                        );
                                    }
                                    // a 0..100 ruler under the track, ticks every 5
                                    theme::ruler(
                                        painter,
                                        egui::Rangef::new(bar_rect.left(), bar_rect.right()),
                                        bar_rect.bottom() + 2.0,
                                        4.0,
                                        21,
                                        5,
                                        theme::t().line_faint,
                                    );
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if driven
                                            && theme::chip(ui, "应用", false)
                                                .on_hover_text("立即把当前设置写入风扇(控制循环每 2 秒也会自动写)")
                                                .clicked()
                                        {
                                            apply_now = true;
                                        }
                                        ui.label(theme::num(
                                            &format!("{:.0} rpm", rpm.unwrap_or(0.0)),
                                            theme::size::LABEL,
                                            theme::Weight::Regular,
                                            if rpm.unwrap_or(0.0) > 100.0 {
                                                theme::t().fg_soft
                                            } else {
                                                theme::t().line_dim
                                            },
                                        ));
                                        ui.add_space(theme::space::COL);
                                        ui.label(theme::micro(
                                            &match target {
                                                Some(t) => format!("目标 {t:.0}%"),
                                                None => "目标 —".to_string(),
                                            },
                                            theme::t().muted,
                                        ));
                                    });
                                });
                                if mode_now != mode_prev {
                                    match mode_now {
                                        FanMode::Auto => self.restore_channel_auto(i, &mut cfg),
                                        _ => self.apply_channel(i, &cfg),
                                    }
                                }
                                if apply_now {
                                    self.apply_channel(i, &cfg);
                                }
                                if apply_now {
                                    self.apply_channel(i, &cfg);
                                }
                                if let FanMode::Manual = cfg.channels[i].mode {
                                    ui.horizontal(|ui| {
                                        let c = &mut cfg.channels[i];
                                        ui.label(theme::micro("占空比", theme::t().muted));
                                        let dragged = ui
                                            .add(
                                                egui::Slider::new(&mut c.manual_duty, 0.0..=100.0)
                                                    .show_value(false),
                                            )
                                            .dragged();
                                        let v = c.manual_duty;
                                        ui.label(theme::num(
                                            &format!("{v:.0}%"),
                                            theme::size::LABEL,
                                            theme::Weight::Regular,
                                            theme::t().fg_soft,
                                        ));
                                        if dragged {
                                            self.apply_channel(i, &cfg);
                                        }
                                    });
                                }
                                if let FanMode::Budget = cfg.channels[i].mode {
                                    ui.horizontal(|ui| {
                                        let c = &mut cfg.channels[i];
                                        ui.label(theme::micro("预算", theme::t().muted));
                                        let dragged = ui
                                            .add(
                                                egui::Slider::new(&mut c.budget_c, 40.0..=95.0)
                                                    .show_value(false),
                                            )
                                            .dragged();
                                        let v = c.budget_c;
                                        ui.label(theme::num(
                                            &format!("{v:.0}°C"),
                                            theme::size::LABEL,
                                            theme::Weight::Regular,
                                            theme::t().fg_soft,
                                        ));
                                        if dragged {
                                            self.apply_channel(i, &cfg);
                                        }
                                    });
                                    ui.label(theme::micro(
                                        "温度停在预算内,转速用刚好够的;低于预算的热是免费的(不吵)",
                                        theme::t().line_dim,
                                    ));
                                }
                                // C4: per-channel algorithm settings, written
                                // straight into the frame-local config copy.
                                // The header summarises active non-default knobs
                                // so the state is readable while collapsed.
                                let algo_title = {
                                    let c = &cfg.channels[i];
                                    let mut knobs: Vec<String> = Vec::new();
                                    if c.budget_c > 0.0 {
                                        knobs.push(format!("预算 {:.0}°C", c.budget_c));
                                    }
                                    if !c.sources.is_empty() {
                                        knobs.push(format!("多源×{}", c.sources.len()));
                                    }
                                    if c.hysteresis_c > 0.0 {
                                        knobs.push(format!("迟滞 {:.0}°", c.hysteresis_c));
                                    }
                                    if c.ramp_up_pct_s > 0.0 {
                                        knobs.push(format!("升 {:.0}%/s", c.ramp_up_pct_s));
                                    }
                                    if c.ramp_down_pct_s > 0.0 {
                                        knobs.push(format!("降 {:.0}%/s", c.ramp_down_pct_s));
                                    }
                                    if c.ema_alpha < 0.999 {
                                        knobs.push(format!("平滑 {:.0}%", (1.0 - c.ema_alpha) * 100.0));
                                    }
                                    if c.spin_up {
                                        knobs.push("助推".to_string());
                                    }
                                    // registration tail: index + lifetime peak
                                    // RPM, straight from the sampler. Kept very
                                    // short on purpose — the earlier long form
                                    // outweighed the algo knobs and pushed the
                                    // whole row past the window, which made
                                    // egui shift the content (and clip it).
                                    let peak = self
                                        .chan_stats
                                        .get(i)
                                        .map(|st| st.max_rpm)
                                        .unwrap_or(0.0);
                                    let reg_tag = if peak > 0.0 {
                                        format!(" · REG{i:02}·{peak:.0}")
                                    } else {
                                        format!(" · REG{i:02}")
                                    };
                                    if knobs.is_empty() {
                                        format!("算法设置 · 默认{reg_tag}")
                                    } else {
                                        format!("算法设置 · {}{reg_tag}", knobs.join(" · "))
                                    }
                                };
egui::CollapsingHeader::new(theme::tracked_fitted(
                                    ui,
                                    &format!("ALGO SETTINGS · {algo_title}"),
                                    theme::size::MICRO,
                                    theme::Weight::Medium,
                                    theme::t().muted,
                                    theme::track::MICRO,
                                    // the header adds its own collapse icon +
                                    // spacing on top of the text — leave room
                                    (ui.available_width() - 40.0).max(140.0),
                                ))
                                .id_salt(("algo", i))
                                .default_open(false)
                                .show(ui, |ui| {
                                    let c = &mut cfg.channels[i];
                                    ui.label(theme::micro(
                                        "通道绑定(这块主板这个接口是什么)",
                                        theme::t().line_dim,
                                    ));
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut c.enabled, theme::micro("交给本程序", theme::t().fg_soft))
                                            .on_hover_text(
                                                "勾上 = 按下面的模式/曲线控制它。取消勾选 = 程序完全不写这个通道(泵、或不想动的东西),交给 BIOS。",
                                            );
                                        ui.checkbox(&mut c.protected, theme::micro("保护", theme::t().fg_soft))
                                            .on_hover_text(
                                                "勾上 = 一键全速/静音、以及温度源失效兜底都不会碰它,只有它自己的模式/曲线会写它。给水泵这类不能随便降速的部件用。",
                                            );
                                    });
                                    ui.label(theme::micro(
                                        "温度源(多选;未选则沿用单一来源)",
                                        theme::t().line_dim,
                                    ));
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
                                        for key in &all_sources {
                                            let on = c.sources.iter().any(|k| k == key);
                                            if theme::chip_raw(ui, key, on, theme::t().fg_soft).clicked() {
                                                if on {
                                                    c.sources.retain(|k| k != key);
                                                } else {
                                                    c.sources.push(key.clone());
                                                }
                                            }
                                        }
                                    });
                                    if c.sources.is_empty() {
                                        ui.label(theme::micro(
                                            &format!("单一来源:{}", c.source),
                                            theme::t().line_dim,
                                        ));
                                    }
                                    ui.add_space(theme::space::HALF);
                                    ui.horizontal(|ui| {
                                        ui.label(theme::micro("多源合成", theme::t().muted));
                                        let mut pick = |ui: &mut egui::Ui, want: SourceCombine, label: &str| {
                                            if theme::chip(ui, label, c.combine == want).clicked() {
                                                c.combine = want;
                                            }
                                        };
                                        pick(ui, SourceCombine::Max, "取最大");
                                        pick(ui, SourceCombine::Avg, "取平均");
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(theme::micro("迟滞", theme::t().muted));
                                        ui.add(
                                            egui::Slider::new(&mut c.hysteresis_c, 0.0..=10.0)
                                                .suffix(" °C")
                                                .fixed_decimals(1),
                                        );
                                        ui.label(theme::micro("0 = 关闭", theme::t().line_dim));
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(theme::micro("升速限制", theme::t().muted));
                                        ui.add(
                                            egui::Slider::new(&mut c.ramp_up_pct_s, 0.0..=200.0)
                                                .suffix(" %/s")
                                                .fixed_decimals(0),
                                        );
                                        ui.label(theme::micro("0 = 不限", theme::t().line_dim));
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(theme::micro("降速限制", theme::t().muted));
                                        ui.add(
                                            egui::Slider::new(&mut c.ramp_down_pct_s, 0.0..=200.0)
                                                .suffix(" %/s")
                                                .fixed_decimals(0),
                                        );
                                        ui.label(theme::micro("0 = 不限", theme::t().line_dim));
                                    });
                                    // slider shows smoothing %; alpha = 1 - pct/100
                                    let mut smooth = ((1.0 - c.ema_alpha.clamp(0.1, 1.0)) * 100.0).round();
                                    ui.horizontal(|ui| {
                                        ui.label(theme::micro("EMA 平滑", theme::t().muted));
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut smooth, 0.0..=90.0)
                                                    .suffix(" %")
                                                    .fixed_decimals(0),
                                            )
                                            .changed()
                                        {
                                            c.ema_alpha = 1.0 - smooth / 100.0;
                                        }
                                        ui.label(theme::num(
                                            &format!("α={:.2}", c.ema_alpha),
                                            theme::size::LABEL,
                                            theme::Weight::Regular,
                                            theme::t().line_dim,
                                        ));
                                    });
                                    ui.checkbox(
                                        &mut c.spin_up,
                                        theme::micro("启动助推(从静止启动时短暂 100%)", theme::t().fg_soft),
                                    );
                                });
                            });
                        // Selection is signalled twice: an accent hairline right on
                        // the box and the four corner crop marks just outside it.
                        // No fill, no radius — the marks are the state.
                        if is_sel {
                            let r = card.response.rect;
                            theme::box_line(ui.painter(), r, accent());
                            theme::corner_marks(ui.painter(), r, 6.0, accent());
                        } else {
                            theme::reg_cross(ui.painter(), card.response.rect.left_top(), 4.0, theme::t().line_faint);
                        }
                        // mounting brackets at the two bottom corners — the
                        // specimen-case garnish. Pure paint, no state, and
                        // deliberately unlike the selection crop marks.
                        {
                            let r = card.response.rect;
                            let p = ui.painter();
                            let s = egui::Stroke::new(theme::space::HAIR, theme::t().line_faint);
                            for (cx, dir) in [(r.left() + 3.0, 1.0), (r.right() - 3.0, -1.0)] {
                                p.line_segment(
                                    [
                                        egui::pos2(cx, r.bottom() - 3.0),
                                        egui::pos2(cx + 4.0 * dir, r.bottom() - 3.0),
                                    ],
                                    s,
                                );
                                p.line_segment(
                                    [
                                        egui::pos2(cx, r.bottom() - 7.0),
                                        egui::pos2(cx, r.bottom() - 3.0),
                                    ],
                                    s,
                                );
                            }
                        }
                        if rename_cancel {
                            self.rename = None;
                        } else if let Some(v) = rename_commit {
                            let name = cfg.channels[i].name.clone();
                            cfg.channels[i].label = v.clone();
                            self.rename = None;
                            self.toast = Some((
                                if v.is_empty() {
                                    format!("已清除 {name} 的别名")
                                } else {
                                    format!("通道已命名为 {v}")
                                },
                                Instant::now(),
                            ));
                        }
                        // a visible gap between cases: the reference never lets
                        // its specimen boxes touch
                        ui.add_space(theme::space::UNIT);
                    }
                    }
                    // Curve alone gets the height the channel rack would have
                    // taken: one plot, read from across the room.
                    View::Curve => {
                        let h = (ui.available_height() * 0.8).max(280.0);
                        self.curve_editor(ui, &mut cfg, &all_sources, h);
                    }
                    View::Lights => {
                        self.rgb_panel(ui);
                    }
                    View::Appearance => {
                        self.appearance_panel(ui, &mut cfg);
                    }
                    }
                });
            });

        // ------- publish frame-local edits + immediate overlay effect -------
        let prev_ovr = *self.ovr.lock().unwrap_or_else(|e| e.into_inner());
        *self.ovr.lock().unwrap_or_else(|e| e.into_inner()) = ovr;
        if prev_ovr != ovr {
            match ovr {
                FanOverride::Full | FanOverride::Quiet => {
                    let duty = if matches!(ovr, FanOverride::Full) { 100.0 } else { 40.0 };
                    for (i, c) in cfg.channels.iter().enumerate().take(self.nct.fan_count()) {
                        if c.hands_off() {
                            continue; // pump / enabled:false are not dragged along
                        }
                        let _ = self.nct.set_duty_pct(i, duty);
                    }
                }
                FanOverride::None => {
                    // Hand back everything the overlay took over before the
                    // config is re-applied: an Auto channel has no curve to
                    // fall back to, so without this it would stay latched at the
                    // overlay duty in software mode and the BIOS would never get
                    // it back — the opposite of what the panel promises.
                    for (i, c) in cfg.channels.iter().enumerate().take(self.nct.fan_count()) {
                        if matches!(c.mode, FanMode::Auto) {
                            self.nct.restore_auto(i);
                        }
                    }
                    apply_all_channels(&self.nct, &cfg, &reading.temps, &nvme, gpu.map(|(t, _)| t))
                }
            }
        }
        *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;

        // ~30 fps while the window is visible (fan icons, duty bars and the
        // toast fade all animate); 1 fps while hidden to the tray — the
        // animation is invisible then and 30 fps would burn ~25% of a core
        // 30 fps while watched; "减少动效" drops the watched cadence to
        // 10 fps too — the sensor poll is 2 s, so numbers lose nothing and a
        // background-resident instance gets a real power lever
        let cadence_ms = if animating {
            if theme::reduce_motion() { 100 } else { 33 }
        } else {
            1000
        };
        ctx.request_repaint_after(Duration::from_millis(cadence_ms));

        // toast feedback (auto-dismiss 2.5s; 120ms fade-in, 250ms fade-out).
        // The inverted solid block: ink on top, page below, zero radius. The
        // fade maths is untouched — it rides the frame cadence above.
        let mut expired = false;
        if let Some((msg, t)) = &self.toast {
            let el = t.elapsed().as_secs_f32();
            if el < 2.5 {
                let fade = if el < 0.12 {
                    el / 0.12
                } else if el > 2.25 {
                    ((2.5 - el) / 0.25).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -28.0])
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(theme::t().fg.gamma_multiply(fade))
                            .rounding(egui::Rounding::ZERO)
                            .stroke(egui::Stroke::NONE)
                            .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(msg)
                                        .size(theme::size::BODY)
                                        .color(theme::t().bg.gamma_multiply(fade))
                                        .extra_letter_spacing(theme::track::LABEL),
                                );
                            });
                    });
            } else {
                expired = true;
            }
        }
        if expired {
            self.toast = None;
        }

        // ---- grain + scan transition: drawn last, over everything ----
        // Both are pure paint and never take input. The grain is a finish laid
        // over the instruments — at a couple of percent of white it reads as a
        // paper texture, not as a veil — and is a single static textured quad
        // (no per-frame UV drift, so it costs nothing and never asks for a
        // repaint of its own).
        if theme::grain_on() {
            if self.grain.is_none() {
                self.grain = Some(theme::make_grain(ctx));
            }
            if let Some(tex) = &self.grain {
                let full = ctx.screen_rect();
                let p = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("grain-layer"),
                ));
                theme::paint_grain(&p, full, tex);
            }
        }
        // The scan band is the view-change transition. It only exists while
        // `animating`, so a tray/background panel does not spin a tick trying
        // to finish an animation nobody can see; with reduce-motion on it never
        // starts at all (switch_view leaves `scan_at` None).
        if let Some(t0) = self.scan_at {
            let el = t0.elapsed().as_secs_f32();
            const DUR: f32 = 0.56;
            if !animating || theme::reduce_motion() || el >= DUR {
                self.scan_at = None;
            } else {
                let f = el / DUR; // easeInOutCubic
                let e = if f < 0.5 { 4.0 * f * f * f } else { 1.0 - (-2.0 * f + 2.0).powi(3) / 2.0 };
                let full = ctx.screen_rect();
                let band_w = 220.0;
                let x = full.left() + (full.width() + band_w) * e - band_w;
                let band = egui::Rect::from_min_size(
                    egui::Pos2::new(x, full.top()),
                    egui::vec2(band_w, full.height()),
                );
                let p = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("scan"),
                ));
                let mut clip = p.clone();
                clip.set_clip_rect(full);
                clip.rect_filled(band, egui::Rounding::ZERO, theme::t().bg.gamma_multiply(0.55));
                theme::vline(&clip, x + band_w - 1.0, egui::Rangef::new(full.top(), full.bottom()), theme::t().accent);
            }
        }

        // ---- boot self-test sequence: the one-shot opening ceremony ----
        // Types the log line by line, holds, then the veil fades and hands
        // the stage over. Runs on wall clock from process start, so it always
        // finishes in 3.4 s even if the window never gets focus; reduce-motion
        // never starts it (boot_lines stays None).
        if let Some(lines) = &boot_lines {
            let el = boot_t;
            const LEAD: f32 = 0.15;
            const PER: f32 = 0.22;
            let shown = (((el - LEAD) / PER).floor().max(0.0) as usize).min(lines.len());
            let fade_start = LEAD + lines.len() as f32 * PER + 0.9;
            let fade = (1.0 - (el - fade_start) / 0.45).clamp(0.0, 1.0);
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("boot"),
            ));
            let full = ctx.screen_rect();
            let mut clip = p.clone();
            clip.set_clip_rect(full);
            clip.rect_filled(full, egui::Rounding::ZERO, tk.bg.gamma_multiply(fade));
            let mut y = full.top() + 96.0;
            for (k, line) in lines.iter().enumerate().take(shown) {
                clip.text(
                    egui::pos2(full.left() + theme::space::GUTTER + 8.0, y),
                    egui::Align2::LEFT_TOP,
                    line,
                    theme::Weight::Medium.font(theme::size::LABEL),
                    if k == 0 {
                        tk.fg.gamma_multiply(fade)
                    } else {
                        tk.fg_soft.gamma_multiply(fade)
                    },
                );
                y += 20.0;
            }
            // blinking cursor on the line still being typed
            if shown < lines.len() && (boot_t * 3.0).fract() < 0.6 {
                clip.text(
                    egui::pos2(full.left() + theme::space::GUTTER + 8.0, y),
                    egui::Align2::LEFT_TOP,
                    "▊",
                    theme::Weight::Medium.font(theme::size::LABEL),
                    tk.accent.gamma_multiply(fade),
                );
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The banner and the light controller's `alert` input read the same
    /// function now, so this pins the one rule both of them follow.
    #[test]
    fn alerts_fire_at_the_documented_thresholds() {
        let cpu = |t: f32| vec![("PECI_0 (CPU)".to_string(), t)];
        assert!(build_alerts(&cpu(84.9), &[]).is_empty());
        assert_eq!(build_alerts(&cpu(85.0), &[]).len(), 1, "85 C is the CPU threshold");
        assert!(any_alert(&cpu(90.0), &[]));

        // drives: index and temperature both appear in the message
        let hot = vec![(0usize, 65.0f32), (1, 40.0)];
        let a = build_alerts(&[], &hot);
        assert_eq!(a.len(), 1);
        assert!(a[0].contains('0'), "names the drive: {a:?}");
        assert!(!a[0].contains('1'), "a cool drive is not reported");
    }

    /// A machine with no PECI reading at all must not warn — several boards
    /// route the CPU probe to a slot this code does not recognise, and a
    /// banner that fires on nothing is worse than one that never fires.
    #[test]
    fn no_peci_reading_means_no_cpu_alert() {
        let temps = vec![("SYSTIN".to_string(), 95.0), ("CPUTIN".to_string(), 90.0)];
        assert!(build_alerts(&temps, &[]).is_empty());
    }
}
