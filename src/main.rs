mod algo;
mod config;
mod cpu;
mod driver;
mod nct;
mod nvme;
mod ui;

use algo::{Engine, SAFE_DUTY, SOURCE_FAIL_LIMIT};
use config::AppConfig;
use nct::Nct;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// requireAdministrator manifest, built by build.rs into libapp_resource.a.
// Excluded from `cargo test`: an admin-manifested test harness cannot start
// from a normal shell (os error 740).
#[cfg(not(test))]
#[link(name = "app_resource", kind = "static", modifiers = "+whole-archive")]
extern "C" {}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("gui");

    match cmd {
        "read" => cmd_read(),
        "set" => {
            let ch: usize = args[2].parse().expect("channel");
            let pct: f32 = args[3].parse().expect("pct");
            cmd_set(ch, pct);
        }
        "gui" | "tray" => {
            let minimized = cmd == "tray";
            // leftover from a rename-based in-place update (the running build
            // renamed itself aside so a rebuild could replace the exe)
            if let Ok(p) = std::env::current_exe() {
                if let Some(d) = p.parent() {
                    let _ = std::fs::remove_file(d.join("fan-tune-running.exe"));
                }
            }
            hide_own_console();
            // GUI (esp. tray mode) has no visible console — send panics and
            // fatal errors to a log file next to the exe
            use std::io::Write;
            let log_file = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("fan-tune-gui.log")));
            if let Some(path) = log_file.clone() {
                std::panic::set_hook(Box::new(move |info| {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                        let _ = writeln!(f, "[{}] PANIC: {info}", chrono_now());
                    }
                }));
            }
            let res = ui::run(minimized);
            if let Err(e) = &res {
                if let Some(path) = &log_file {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                        let _ = writeln!(f, "[{}] FATAL: {e}", chrono_now());
                    }
                }
                eprintln!("GUI error: {e}");
            }
            if res.is_err() {
                std::process::exit(1);
            }
        }
        "daemon" => cmd_daemon(),
        "install" => cmd_install_task(),
        "uninstall" => cmd_uninstall_task(),
        "help" | _ => {
            println!(
                "fan-tune — NCT6796DR fan controller\n\
                 \x20 read                 print all sensors\n\
                 \x20 set <ch> <pct>       set fan channel duty %\n\
                 \x20 gui                  graphical interface — owns the fan hardware\n\
                 \x20                      while open; stops the daemon on start and\n\
                 \x20                      hands control back to it on exit\n\
                 \x20 tray                 same as gui, but starts hidden (autostart)\n\
                 \x20 daemon               background curve loop (GUI-exit fallback)\n\
                 \x20 install              register tray GUI as logon autostart task (admin)\n\
                 \x20 uninstall            remove the scheduled task"
            );
        }
    }
}

fn open_nct() -> Nct {
    // CLI paths honour the same backend preference as the GUI/daemon
    let pref = config::AppConfig::load().0.backend;
    let driver = driver::Driver::install_and_open(pref).expect("driver open failed (admin required)");
    eprintln!("[+] WinRing0 driver loaded");
    let nct = Nct::detect(driver).expect("NCT6796DR detect failed");
    eprintln!("[+] {} @ 0x{:04X}", nct.chip_name(), nct.base_port());
    nct
}

/// Hide the console window — but only when this process is its sole owner
/// (double-click / scheduled-task launch). If launched from an existing
/// terminal, leave that terminal alone. GUI/tray modes only.
fn hide_own_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(pids: *mut u32, count: u32) -> u32;
        fn GetConsoleWindow() -> *mut core::ffi::c_void;
    }
    #[link(name = "user32")]
    extern "system" {
        fn ShowWindow(hwnd: *mut core::ffi::c_void, cmd: i32) -> i32;
    }
    unsafe {
        let mut pids = [0u32; 4];
        let n = GetConsoleProcessList(pids.as_mut_ptr(), 4);
        if n == 1 {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_null() {
                ShowWindow(hwnd, 0); // SW_HIDE
            }
        }
    }
}

fn cmd_read() {
    let nct = open_nct();
    let r = nct.read_all(true);

    println!("\n=== fans ===");
    for (i, rpm) in r.fans_rpm.iter().enumerate() {
        let duty = r.controls_pct[i].unwrap_or(0.0);
        println!(
            "  Fan #{i:<2} {:>7} rpm   duty {duty:>5.1}%",
            rpm.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into())
        );
    }
    println!("\n=== temps ===");
    for (name, t) in &r.temps {
        println!("  {name:<16} {t:>6.1} C");
    }
    println!("\n=== voltages ===");
    for (name, v) in &r.voltages {
        println!("  {name:<10} {v:>6.3} V");
    }
    println!("\n=== nvme ===");
    for (i, t) in nvme::read_smart_drives() {
        println!("  硬盘 {i}: {t:.1} C");
    }
}

fn cmd_set(channel: usize, pct: f32) {
    let nct = open_nct();
    println!("[+] setting Fan #{channel} -> {pct:.0}%");
    nct.set_duty_pct(channel, pct).expect("set failed");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let r = nct.read_all(false);
    println!(
        "[verify] Fan #{channel}: duty={:?}% rpm={:?}",
        r.controls_pct[channel], r.fans_rpm[channel]
    );
}

// ---------- resilience helpers ----------

struct Log {
    path: std::path::PathBuf,
}

impl Log {
    fn new(path: &str) -> Log {
        Log { path: std::path::PathBuf::from(path) }
    }
    fn write(&self, level: &str, msg: &str) {
        // rotate at 512KB
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.len() > 512 * 1024 {
                let _ = std::fs::rename(&self.path, self.path.with_extension("log.old"));
            }
        }
        let line = format!(
            "[{}] {} {}\n",
            chrono_now(),
            level,
            msg
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(line.as_bytes());
        }
        eprint!("{line}");
    }
    fn info(&self, msg: &str) {
        self.write("INFO", msg);
    }
    fn err(&self, msg: &str) {
        self.write("ERR ", msg);
    }
}

fn chrono_now() -> String {
    let (days, tod) = local_time_parts();
    let (y, month, day) = civil_from_days(days);
    format!(
        "{y:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Local time as `(days since 1970-01-01, seconds since local midnight)`.
/// The header clock and the log timestamps both come from here.
pub(crate) fn local_time_parts() -> (i64, i64) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // local time (the offset was hard-coded to CST/+8, which made the logs
    // wrong on any machine in another time zone)
    let secs = d.as_secs() as i64 + local_utc_offset_secs();
    (secs.div_euclid(86_400), secs.rem_euclid(86_400))
}

/// `(year, month, day)` for a day count since the epoch (Howard Hinnant's
/// civil-from-days).
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// Seconds to add to UTC to get local time, from the system time-zone settings
/// (DST included). Cached: this is called for every log line.
fn local_utc_offset_secs() -> i64 {
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: [u16; 8],
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: [u16; 8],
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
    }
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| unsafe {
        let mut tzi: TimeZoneInformation = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut tzi);
        if id == 0xFFFF_FFFF {
            return 0; // TIME_ZONE_ID_INVALID: fall back to UTC
        }
        // Bias reads "UTC = local + bias", and id 2 means DST is in effect.
        let bias = if id == 2 {
            tzi.bias + tzi.daylight_bias
        } else {
            tzi.bias
        };
        -(bias as i64) * 60
    })
}

fn cmd_daemon() {
    let log = Log::new("fan-tune.log");
    log.info(&format!("daemon starting, pid={}", std::process::id()));
    // clear any stale handover sentinel left by a crashed takeover
    let _ = std::fs::remove_file(driver::stop_sentinel_path());

    // ---- config (hot-reloaded on file change) ----
    let config_path = config::config_path();
    let mut cfg_mtime = std::fs::metadata(&config_path).and_then(|m| m.modified()).ok();
    let (mut cfg, cfg_warn) = AppConfig::load();
    if let Some(w) = cfg_warn {
        log.err(&w);
    }

    // ---- hardware access with retry (driver may not be ready yet at boot) ----
    let nct = {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match driver::Driver::install_and_open(cfg.backend)
                .and_then(|d| Nct::detect(d))
            {
                Ok(n) => {
                    log.info(&format!("NCT6796DR detected (attempt {attempt})"));
                    break n;
                }
                Err(e) => {
                    log.err(&format!("detect failed (attempt {attempt}): {e}"));
                    if attempt >= 30 {
                        log.err("giving up after 30 attempts");
                        std::process::exit(1);
                    }
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }
    };

    // ---- nvme cache thread (smartctl is slow; keep it off the control loop) ----
    let nvme_cache: Arc<Mutex<Vec<(usize, f32)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let cache = nvme_cache.clone();
        let log = Log::new("fan-tune.log");
        std::thread::spawn(move || loop {
            let v = nvme::read_smart_drives();
            if let Ok(mut c) = cache.lock() {
                *c = v;
            }
            std::thread::sleep(Duration::from_secs(10));
        });
        let _ = log;
    }

    // ---- gpu temp cache thread (nvidia-smi) — needed by "gpu:0" sources ----
    let gpu_cache: Arc<Mutex<Option<f32>>> = Arc::new(Mutex::new(None));
    {
        let cache = gpu_cache.clone();
        std::thread::spawn(move || loop {
            let t = nvme::read_gpu().map(|(t, _)| t);
            if let Ok(mut c) = cache.lock() {
                *c = t;
            }
            std::thread::sleep(Duration::from_secs(5));
        });
    }

    log.info(&format!(
        "control loop started, {} channels, interval {}ms, safe_duty={SAFE_DUTY}%",
        cfg.channels.len(),
        cfg.interval_ms
    ));

    let mut source_fail: u32 = 0;
    let mut safe_mode = false;
    // algorithm engine: persistent per-channel curve state (EMA/hysteresis/
    // ramp/spin-up). States survive config hot reloads (indexed like channels).
    let mut engine = Engine::new(7);
    // server-profile thermal guard (per-channel latch)
    let mut guard = algo::ServerGuard::default();

    loop {
        // graceful handover: GUI takeover writes a sentinel; exit cleanly so
        // the driver stays in a healthy state (no forced-kill zombie risk)
        let sentinel = driver::stop_sentinel_path();
        if sentinel.exists() {
            let _ = std::fs::remove_file(&sentinel);
            log.info("stop request received — exiting cleanly for GUI takeover");
            return;
        }
        // one control pass; a panic here must not kill the daemon
        let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // temperatures only: the control loop needs nothing else, and this
            // skips most of the chip's register traffic every tick
            let temps = nct.read_temps();
            let nvme_temps = nvme_cache.lock().map(|c| c.clone()).unwrap_or_default();
            let gpu_t = gpu_cache.lock().ok().and_then(|c| *c);

            let resolve = |key: &str| -> Option<f32> {
                if key.starts_with("gpu") {
                    return gpu_t;
                }
                if let Some(idx) = key.strip_prefix("nvme:") {
                    let i: usize = idx.parse().ok()?;
                    return nvme_temps.iter().find(|(n, _)| *n == i).map(|(_, t)| *t);
                }
                if let Some(name) = key.strip_prefix("nct:") {
                    return temps.iter().find(|(n, _)| n == name).map(|(_, t)| *t);
                }
                None
            };
            let now = Instant::now();
            let targets = engine.targets(&cfg, &resolve, now);

            let mut pass_fail = 0u32;
            for (i, c) in cfg.channels.iter().enumerate() {
                if i >= nct.fan_count() {
                    break; // channels above the chip's fan count have no hardware
                }
                // `enabled: false` = leave the channel to the BIOS entirely
                if !c.managed() {
                    continue;
                }
                // profile effects live inside the engine (Gaming temp-offset,
                // Server smoothing); the Server guard is the caller-side bit
                let want: Option<f32> = match c.mode {
                    config::FanMode::Manual => Some(c.manual_duty.clamp(0.0, 100.0)),
                    config::FanMode::Curve | config::FanMode::Budget => {
                        targets.get(i).copied().flatten().map(|d| {
                            let force = matches!(cfg.profile, config::FanProfile::Server) && {
                                let hottest = algo::channel_hottest(c, &resolve);
                                guard.forced(i, cfg.server_guard_c, hottest)
                            };
                            if force {
                                100.0
                            } else {
                                d
                            }
                        })
                    }
                    config::FanMode::Auto => None,
                };
                match want {
                    Some(duty) => {
                        let _ = nct.set_duty_pct(i, duty);
                        if matches!(c.mode, config::FanMode::Manual) {
                            engine.note_write(i, duty, now);
                        }
                    }
                    None => {
                        // a curve/budget channel with no data is a real failure
                        if matches!(
                            c.mode,
                            config::FanMode::Curve | config::FanMode::Budget
                        ) {
                            pass_fail += 1;
                        }
                    }
                }
            }
            pass_fail
        }));

        match pass {
            Ok(0) => {
                if safe_mode {
                    log.info("temperature sources recovered, exiting safe mode");
                }
                source_fail = 0;
                safe_mode = false;
            }
            Ok(n) => {
                source_fail += n;
                if source_fail >= SOURCE_FAIL_LIMIT && !safe_mode {
                    safe_mode = true;
                    log.err(&format!(
                        "temperature source failed {source_fail}x, engaging SAFE DUTY {SAFE_DUTY}% on all curve channels"
                    ));
                }
                if safe_mode {
                    let now = Instant::now();
                    for (i, c) in cfg.channels.iter().enumerate() {
                        if i < nct.fan_count()
                            && !c.hands_off()
                            && matches!(c.mode, config::FanMode::Curve | config::FanMode::Budget)
                        {
                            let _ = nct.set_duty_pct(i, SAFE_DUTY);
                            engine.note_write(i, SAFE_DUTY, now);
                        }
                    }
                }
            }
            Err(_) => {
                log.err("panic in control pass — recovering loop");
            }
        }

        // config hot reload
        if let Ok(m) = std::fs::metadata(&config_path).and_then(|m| m.modified()) {
            if cfg_mtime.as_ref() != Some(&m) {
                cfg_mtime = Some(m);
                let (c, warn) = AppConfig::load();
                cfg = c;
                log.info("config reloaded (file changed)");
                if let Some(w) = warn {
                    log.err(&w);
                }
            }
        }

        std::thread::sleep(Duration::from_millis(cfg.interval_ms.max(500)));
    }
}

fn cmd_install_task() {
    let exe = std::env::current_exe().unwrap();
    let workdir = exe.parent().unwrap().to_path_buf();
    let exe_str = exe.display().to_string();

    // XML task: at logon, highest privileges, restart on failure x3, no time limit
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_str}</Command>
      <Arguments>tray</Arguments>
      <WorkingDirectory>{workdir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#,
        sid = {
            // current user SID via whoami /user
            let out = std::process::Command::new("whoami").arg("/user").output().ok();
            let s = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
            s.lines()
                .filter(|l| l.contains('\\'))
                .last()
                .and_then(|l| l.split_whitespace().last().map(|x| x.to_string()))
                .unwrap_or_default()
        },
        exe_str = xml_escape(&exe_str),
        workdir = xml_escape(&workdir.display().to_string()),
    );

    let xml_path = workdir.join("fan-tune-task.xml");
    let utf16 = wide_utf16(&xml);
    if std::fs::write(&xml_path, utf16).is_err() {
        eprintln!("cannot write task xml");
        std::process::exit(1);
    }

    // retire the legacy powershell daemon task if present
    let _ = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", "FanCurveDaemon", "/F"])
        .output();

    let out = std::process::Command::new("schtasks")
        .args(["/Create", "/TN", "FanTune", "/XML"])
        .arg(&xml_path)
        .args(["/F"])
        .output()
        .expect("schtasks");
    let o = String::from_utf8_lossy(&out.stdout).to_string();
    let e = String::from_utf8_lossy(&out.stderr).to_string();
    println!("{o}{e}");
    if !e.contains("拒绝") && !o.is_empty() {
        println!("[+] FanTune tray GUI registered (logon autostart, restart on failure)");
        println!("[+] on exit it hands control to the background daemon");
        // start it right away
        let run = std::process::Command::new("schtasks")
            .args(["/Run", "/TN", "FanTune"])
            .output()
            .expect("schtasks run");
        println!("{}", String::from_utf8_lossy(&run.stdout));
    }
}

fn cmd_uninstall_task() {
    for tn in ["FanTune", "FanCurveDaemon"] {
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", tn, "/F"])
            .output()
            .expect("schtasks");
        println!("[{tn}] {}", String::from_utf8_lossy(&out.stdout).trim());
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn wide_utf16(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    v.splice(0..0, [0xFF, 0xFE]); // BOM
    v
}