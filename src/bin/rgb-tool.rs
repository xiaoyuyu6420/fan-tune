//! `rgb-tool` — light controller diagnostics and calibration, deliberately a
//! *separate* binary without the GUI's requireAdministrator manifest so it can
//! be started from a normal shell (the GUI binary always raises UAC).
//!
//! Modes:
//!   read                       device registers + what each output holds now
//!   profile                    the loaded light profile, verbatim
//!   set  --ch1 RRGGBB ...      write colours, verified by reading them back
//!   walk --ch 1 --count 48     light LEDs one by one (count/order of a chain)
//!   sweep --secs 12 --fps 20   smooth colour sweep (judder test for the eyes)
//!   bench                      measure the write path (per-output cost)
//!   apply --cpu 50 --gpu 60    render the profile's strategies once, for these
//!                              sensor values, and put them on the lights
//!   release                    hand the lights back to the board
//!
//! Every write is followed by a read-back comparison: the controller lets us
//! read its LED buffers, so "it should be lit" is never an assumption here.

use fan_tune::rgb::{controller, effects, RgbEngine, RgbProfile};
use std::time::{Duration, Instant};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn arg_num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    arg(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn parse_color(s: &str) -> Option<[u8; 3]> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    Some([
        u8::from_str_radix(&s[0..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..6], 16).ok()?,
    ])
}

fn solid(n: usize, c: [u8; 3]) -> Vec<[u8; 3]> {
    vec![c; n]
}

fn runs(leds: &[[u8; 3]]) -> Vec<(usize, usize, [u8; 3])> {
    let mut out: Vec<(usize, usize, [u8; 3])> = Vec::new();
    for (i, c) in leds.iter().enumerate() {
        match out.last_mut() {
            Some(last) if last.2 == *c => last.1 = i,
            _ => out.push((i, i, *c)),
        }
    }
    out
}

/// Is the GUI holding its hardware lock? It opens the file with no sharing, so
/// any open attempt failing while the file exists means "the app is running".
fn gui_running() -> bool {
    let lock = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fan-tune-hardware.lock")));
    match lock {
        Some(p) if p.exists() => std::fs::OpenOptions::new().write(true).open(&p).is_err(),
        _ => false,
    }
}

fn open_engine(force: bool) -> Result<RgbEngine, String> {
    let (prof, warn) = RgbProfile::load();
    if let Some(w) = &warn {
        eprintln!("[warn] {w}");
    }
    let prof = match prof {
        Some(p) => p,
        None => {
            eprintln!(
                "[err] 没有灯光配置（{}）。先跑 `rgb-tool init` 建立。",
                RgbProfile::path().display()
            );
            std::process::exit(2);
        }
    };
    if prof.enabled && gui_running() && !force {
        eprintln!(
            "[warn] 主程序正在运行且灯效已启用：这次写入可能被它的下一次渲染覆盖。\
             \n       要校准请先在面板里点「让给校准」（放开句柄），或用 --force 忽略这条提示。"
        );
    }
    match RgbEngine::open(prof) {
        Ok(e) => Ok(e),
        Err(e) if e.contains("拒绝访问") => Err(format!(
            "{e}
       提示：主程序正握着灯光控制器——在面板里勾「让给校准」后重试"
        )),
        Err(e) => Err(e),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("read");
    let force = args.iter().any(|a| a == "--force");

    match mode {
        "init" => {
            // Writing the profile *is* the consent to drive the lights (the app
            // never touches a machine that has no profile), so creating or
            // replacing one takes an explicit word.
            let path = RgbProfile::path();
            if path.exists() && !force {
                eprintln!(
                    "[err] {} 已存在——要重建（会丢掉现有校准）请加 --force",
                    path.display()
                );
                std::process::exit(2);
            }
            match RgbProfile::uncalibrated_identify().save() {
                Ok(()) => {
                    println!("[init] 写入 {}", path.display());
                    println!(
                        "[init] 两条 ARGB 链先按「未校准」处理：CH1 绿 / CH2 蓝；\
                         \n       确认哪组灯在哪条链上、每链多少灯珠之后，再改成分段+形状+相位，才能上温度类效果"
                    );
                }
                Err(e) => eprintln!("[err] {e}"),
            }
        }
        "read" => {
            let mut eng = match open_engine(force) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[err] {e}");
                    std::process::exit(3);
                }
            };
            println!("[dev] {}", eng.path());
            println!("[status] {}", eng.status());
            for (i, out) in controller::OUTPUTS.iter().enumerate() {
                match eng.read_output(i) {
                    Ok(leds) => {
                        let r = runs(&leds);
                        let shown = r
                            .iter()
                            .take(6)
                            .map(|(a, b, c)| {
                                if a == b {
                                    format!("#{a}=#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
                                } else {
                                    format!("#{a}-{b}=#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        println!("[leds] {} ({} 灯珠) {} 段: {}", out.name, leds.len(), r.len(), shown);
                    }
                    Err(e) => println!("[leds] {} = ERROR {e}", out.name),
                }
            }
        }
        "profile" => {
            let (prof, warn) = RgbProfile::load();
            if let Some(w) = &warn {
                eprintln!("[warn] {w}");
            }
            match prof {
                Some(p) => {
                    println!("[file] {}", RgbProfile::path().display());
                    println!("{}", serde_json::to_string_pretty(&p).unwrap_or_default());
                }
                None => println!("[none] {} 不存在（这台机器还没设置过灯光）", RgbProfile::path().display()),
            }
        }
        "set" => {
            if gui_running() && !force {
                eprintln!(
                    "[warn] 主程序正在运行：如果它的灯效已启用，这次写入可能被下一次渲染覆盖。\
                     \n       要校准请先在面板里点「让给校准」（放开句柄），或用 --force 忽略这条提示。"
                );
            }
            let mut ctrl = match controller::Controller::open() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[err] {e}");
                    // WinUSB grants one handle per interface, so "access denied"
                    // here almost always means the GUI is holding the device.
                    if e.contains("拒绝访问") {
                        eprintln!("       提示：主程序正握着灯光控制器——在面板里勾「让给校准」后重试");
                    }
                    std::process::exit(3);
                }
            };
            let leds = arg_num(&args, "--leds", 128usize);
            let mut touched = false;
            for (flag, idx) in [("--ch1", 0usize), ("--ch2", 1), ("--twelvev", 2)] {
                if let Some(c) = arg(&args, flag).as_deref().and_then(parse_color) {
                    let out = controller::OUTPUTS[idx];
                    let n = if out.single_color { 1 } else { leds.min(out.leds) };
                    ctrl.write_leds(&out, &solid(n, c)).expect("write");
                    let back = ctrl.read_leds(&out, n.min(4)).unwrap_or_default();
                    let ok = back.len() == n.min(4) && back.iter().all(|l| *l == c);
                    println!(
                        "[set] {} <- #{:02X}{:02X}{:02X}（{} 灯珠）读回 {}",
                        out.name,
                        c[0],
                        c[1],
                        c[2],
                        n,
                        if ok { "一致 ✓".to_string() } else { format!("不一致 {back:?}") }
                    );
                    touched = true;
                }
            }
            if !touched {
                eprintln!("[err] 至少给一个 --ch1/--ch2/--twelvev RRGGBB");
                std::process::exit(2);
            }
        }
        "blocks" => {
            // Calibration primitive: paint successive blocks of `--size` LEDs in
            // different colours, so one glance at the hardware shows how many
            // LEDs belong to one fan/ring segment and in which order.
            let ch: usize = arg_num(&args, "--ch", 1usize);
            if ch == 0 || ch > 2 {
                eprintln!("[err] --ch 只能是 1 或 2");
                std::process::exit(2);
            }
            let size: usize = arg_num(&args, "--size", 12usize);
            let colors: Vec<[u8; 3]> = arg(&args, "--colors")
                .map(|v| v.split(',').filter_map(parse_color).collect())
                .unwrap_or_else(|| vec![[255, 0, 0], [0, 255, 0], [0, 0, 255]]);
            let dim: u8 = arg_num(&args, "--dim", 0u8);
            let mut ctrl = match controller::Controller::open() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[err] {e}");
                    if e.contains("拒绝访问") {
                        eprintln!("       提示：主程序正握着灯光控制器——建立 fan-tune-rgb-yield.req 或勾「让给校准」");
                    }
                    std::process::exit(3);
                }
            };
            let out = controller::OUTPUTS[ch - 1];
            let n = out.leds;
            let mut buf = vec![[0u8, 0, 0]; n];
            for (i, led) in buf.iter_mut().enumerate() {
                let block = i / size.max(1);
                let mut c = colors.get(block % colors.len()).copied().unwrap_or([0, 0, 0]);
                if dim > 0 {
                    c = [c[0] / dim.max(1), c[1] / dim.max(1), c[2] / dim.max(1)];
                }
                *led = c;
            }
            ctrl.write_leds(&out, &buf).expect("write");
            let back = ctrl.read_leds(&out, 4).unwrap_or_default();
            println!(
                "[blocks] {} 每 {size} 颗一段，颜色 {}，已写入（读回前 4 颗 {:?}）",
                out.name,
                colors.iter().map(|c| format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])).collect::<Vec<_>>().join("/"),
                back
            );
            println!("[hint] 另一条链保持原样；要清空另一条链用 set --chX 000000");
        }
        "walk" => {
            let ch: usize = arg_num(&args, "--ch", 1usize);
            let from: usize = arg_num(&args, "--from", 0usize);
            let count: usize = arg_num(&args, "--count", 48usize);
            let step: u64 = arg_num(&args, "--step-ms", 200u64);
            let color = arg(&args, "--rgb").as_deref().and_then(parse_color).unwrap_or([0, 255, 0]);
            if ch == 0 || ch > 2 {
                eprintln!("[err] --ch 只能是 1 或 2");
                std::process::exit(2);
            }
            let idx = ch - 1;
            let mut ctrl = controller::Controller::open().expect("controller");
            let out = controller::OUTPUTS[idx];
            println!(
                "[walk] {}：灯珠 {from}..{}，每颗 {step}ms，颜色 #{:02X}{:02X}{:02X}",
                out.name,
                from + count - 1,
                color[0],
                color[1],
                color[2]
            );
            for lit in from..from + count {
                let mut buf = vec![[0u8, 0, 0]; out.leds];
                buf[lit] = color;
                ctrl.write_leds(&out, &buf).expect("write");
                std::thread::sleep(Duration::from_millis(step));
            }
            println!("[done] 走完了：如果光点走到某颗就不再前进，那颗就是链条的末尾");
        }
        "sweep" => {
            let secs: f32 = arg_num(&args, "--secs", 12.0f32);
            let fps: f32 = arg_num(&args, "--fps", 20.0f32);
            let mut ctrl = controller::Controller::open().expect("controller");
            let period = Duration::from_micros((1_000_000.0 / fps.max(1.0)) as u64);
            println!("[sweep] {secs} 秒 @ {fps} fps —— 盯着灯看是否顺滑");
            let start = Instant::now();
            let mut frames = 0u32;
            let mut worst = 0f32;
            while start.elapsed().as_secs_f32() < secs {
                let t = start.elapsed().as_secs_f32();
                let f = (t * std::f32::consts::PI / 3.0).sin() * 0.5 + 0.5;
                let c = [(255.0 * f) as u8, (255.0 * (1.0 - f)) as u8, 0];
                let t0 = Instant::now();
                ctrl.write_leds(&controller::ARGB1, &solid(128, c)).ok();
                ctrl.write_leds(&controller::ARGB2, &solid(128, c)).ok();
                worst = worst.max(t0.elapsed().as_secs_f32());
                frames += 1;
                let spent = t0.elapsed();
                if spent < period {
                    std::thread::sleep(period - spent);
                }
            }
            println!("[sweep] {frames} 帧，最慢一帧 {:.1} ms", worst * 1000.0);
        }
        "bench" => {
            let frames: u32 = arg_num(&args, "--frames", 50u32);
            let leds: usize = arg_num(&args, "--leds", 128usize);
            let mut ctrl = controller::Controller::open().expect("controller");
            let payload = solid(leds, [0x10, 0x20, 0x30]);
            ctrl.write_leds(&controller::ARGB1, &payload).ok(); // warm up

            let mut per_write = Vec::new();
            for _ in 0..frames {
                let t = Instant::now();
                ctrl.write_leds(&controller::ARGB1, &payload).expect("write");
                per_write.push(t.elapsed().as_secs_f32());
            }
            let avg = per_write.iter().sum::<f32>() / per_write.len() as f32;
            let worst = per_write.iter().cloned().fold(0.0f32, f32::max);
            println!("[bench] 每次「输出有变化」的写入（{} 灯珠），{} 次平均：", leds, frames);
            println!(
                "        {:.2} ms / 次（最慢 {:.2} ms）→ 单通道 {:.0} Hz 上限",
                avg * 1000.0,
                worst * 1000.0,
                1.0 / avg
            );
            println!(
                "        静态/慢变效果只在颜色变化时才写，所以稳态是 0 次/秒；\
                 \n        两条链 5 Hz 全变色约 {:.1}% 单核，20 Hz 约 {:.0}%",
                avg * 2.0 * 5.0 * 100.0,
                avg * 2.0 * 20.0 * 100.0
            );
        }
        "apply" => {
            let mut eng = match open_engine(force) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[err] {e}");
                    std::process::exit(3);
                }
            };
            let mut inputs = effects::Inputs::default();
            for (flag, key) in [("--cpu", "nct:PECI_0 (CPU)"), ("--gpu", "gpu:0"), ("--disk", "nvme:0")] {
                if let Some(v) = arg(&args, flag).and_then(|v| v.parse::<f32>().ok()) {
                    inputs.temps.push((key.to_string(), v));
                }
            }
            if let Some(spec) = arg(&args, "--duty") {
                // form: "0:45" (channel:duty)
                if let Some((ch, pct)) = spec.split_once(':') {
                    if let (Ok(ch), Ok(pct)) = (ch.parse::<usize>(), pct.parse::<f32>()) {
                        if ch < 7 {
                            inputs.duty[ch] = Some(pct);
                        }
                    }
                }
            }
            if args.iter().any(|a| a == "--gaming") {
                inputs.profile = 1;
            } else if args.iter().any(|a| a == "--server") {
                inputs.profile = 2;
            }
            inputs.alert = args.iter().any(|a| a == "--alert");
            inputs.quiet = args.iter().any(|a| a == "--quiet");
            inputs.full = args.iter().any(|a| a == "--full");
            let n = eng.apply(&inputs, Instant::now()).map_err(|e| e.to_string());
            match n {
                Ok(wrote) => {
                    println!("[apply] 写入了 {wrote} 个输出");
                    for (i, out) in controller::OUTPUTS.iter().enumerate() {
                        if let Ok(leds) = eng.read_output(i) {
                            let r = runs(&leds);
                            let shown = r
                                .iter()
                                .take(4)
                                .map(|(a, b, c)| format!("#{a}-{b}=#{:02X}{:02X}{:02X}", c[0], c[1], c[2]))
                                .collect::<Vec<_>>()
                                .join(" ");
                            println!("[leds] {} {} 段: {}", out.name, r.len(), shown);
                        }
                    }
                }
                Err(e) => eprintln!("[err] {e}"),
            }
        }
        "release" | "exit-direct" => {
            let mut ctrl = controller::Controller::open().expect("controller");
            match ctrl.release() {
                Ok(()) => println!(
                    "[release] 已把灯还回主板（模式寄存器写 0）；若灯没变化，重启一次即可"
                ),
                Err(e) => eprintln!("[err] 还灯失败（Win32 {e}）"),
            }
        }
        "help" | "-h" | "--help" => {
            println!("用法: rgb-tool <read|profile|set|walk|sweep|bench|apply|release> [选项]");
            println!("  set   --ch1 RRGGBB --ch2 RRGGBB [--twelvev RRGGBB] [--leds N]");
            println!("  walk  --ch 1|2 [--from 0] [--count 48] [--step-ms 200] [--rgb RRGGBB]");
            println!("  blocks --ch 1|2 [--size 12] [--colors FF0000,00FF00,0000FF] [--dim N]");
            println!("  sweep [--secs 12] [--fps 20]");
            println!("  bench [--frames 50] [--leds 128]");
            println!("  apply [--cpu 50] [--gpu 60] [--disk 40] [--duty 0:45] [--gaming|--server|--alert|--quiet|--full]");
            println!("  --force 忽略「主程序正在运行」的提示");
        }
        other => {
            eprintln!("[err] 未知模式 {other}（试试 help）");
            std::process::exit(2);
        }
    }
}
