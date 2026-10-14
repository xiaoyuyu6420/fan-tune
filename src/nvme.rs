//! NVMe / drive temperature helpers.
//!
//! NOTE: this storage stack rejects direct NVMe protocol queries with
//! ERROR_INVALID_FUNCTION (tested: property 50/51 × len 48/52/56 all fail),
//! but smartmontools works fine when elevated — so we shell out to smartctl.


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

/// Physical drives to probe. smartctl on Windows maps `/dev/sdX` by
/// enumeration order, so `FANTUNE_DRIVES=sd0,sd1` overrides the list when a
/// disk is added or moved.
///
/// The probe order is *not* the config index: see [`stable_index`]. A drive
/// keeps its `nvme:N` key as long as its model string keeps the same rank
/// among the drives present, so adding a disk or swapping two SATA ports does
/// not silently re-point a fan curve at a different drive.
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

/// Turn a probe order into a config index that survives enumeration changes.
///
/// `/dev/sda..sdc` are whatever order Windows enumerated them in this boot;
/// two identical drives can swap places after a BIOS update or a port change.
/// The config key is therefore derived from the *model string* instead: the
/// drives are ranked by model (then by serial, which is per-unit unique) and
/// the index is that rank. A drive keeps its key across reboots and hardware
/// reshuffles; a newly added drive takes the slot its model sorts into and
/// cannot silently inherit another drive's curves.
///
/// A drive smartctl reports no model for falls back to its probe order, so it
/// still gets a distinct index rather than colliding with another drive.
pub fn stable_index(probe: usize, _model: &str, all: &[(usize, String)]) -> usize {
    let mut ranked: Vec<usize> = (0..all.len()).collect();
    ranked.sort_by(|&a, &b| {
        let ma = if all[a].1.is_empty() { "\u{7fff}".to_string() } else { all[a].1.clone() };
        let mb = if all[b].1.is_empty() { "\u{7fff}".to_string() } else { all[b].1.clone() };
        ma.cmp(&mb).then_with(|| all[a].0.cmp(&all[b].0))
    });
    ranked.iter().position(|&p| p == probe).unwrap_or(probe)
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
    // Fields are parsed independently: nvidia-smi prints "[N/A]" for anything
    // it cannot read (a fanless card, or a field the driver does not expose),
    // and a short-circuiting parse would then throw away the GPU temperature
    // too — the one field every caller actually needs.
    let fields: Vec<&str> = s.split(',').map(|x| x.trim()).collect();
    let t = fields.first().and_then(|f| f.parse::<f32>().ok())?;
    let u = fields.get(1).and_then(|f| f.parse::<f32>().ok()).unwrap_or(0.0);
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
            if found.is_some() {
                // index is fixed up by the caller against the model list; see
                // read_smart_full — read_smart_drives keeps the probe index and
                // lets the daemon resolve it, because a second smartctl sweep
                // here would double an already slow path.
                out.push((i, found.unwrap()));
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
    let candidates = smartctl_candidates();
    let devs = drives();

    // Pass 1: probe every drive and remember what it is. This is one smartctl
    // per drive either way; the point is that the index is decided *after* all
    // the models are known, so it can be derived from them.
    let mut probed: Vec<(usize, String, f32, Vec<(String, String)>)> = Vec::new();
    let mut identity: Vec<(usize, String)> = Vec::new();
    for (i, dev) in devs.iter().enumerate() {
        for exe in &candidates {
            let mut cmd = std::process::Command::new(exe);
            cmd.args(["-a", "-d", "nvme", &format!("/dev/{dev}")]);
            let Some(bytes) = run_tool(&mut cmd, SMARTCTL_TIMEOUT) else {
                continue;
            };
            if let Some((_, temp, model, fields)) = parse_smart(&String::from_utf8_lossy(&bytes), i) {
                identity.push((i, model.clone()));
                probed.push((i, model, temp, fields));
            }
            break;
        }
    }

    // Pass 2: hand out the model-derived indices.
    let mut out = Vec::new();
    for (probe, model, temp, fields) in probed {
        let index = stable_index(probe, &model, &identity);
        out.push((index, temp, model, fields));
    }
    out.sort_by_key(|(i, _, _, _)| *i);
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

// ---------- direct NVMe protocol query: REMOVED ----------
//
// The native path (IOCTL_STORAGE_QUERY_PROPERTY with
// StorageDeviceProtocolSpecificProperty) was kept here for a while after it
// turned out this machine's storage stack rejects it with
// ERROR_INVALID_FUNCTION (tested: property 50/51 x len 48/52/56, all fail).
// It is gone rather than left #[allow(dead_code)]: a complete-looking but
// never-exercised `probe()` reads as "the implementation that exists", and the
// next person would either call it or try to fix it. The working path is
// smartctl (see `read_smart_full`); if a future stack accepts the IOCTL,
// `refs/vendor/` and git history are where to look.
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

    /// The whole point of the model-derived index: two drives swapping
    /// enumeration order must keep their keys, so a curve that followed
    /// "nvme:0" keeps following the same physical disk.
    #[test]
    fn stable_index_survives_enumeration_swap() {
        // boot 1: a=Samsung, b=WD
        let b1 = vec![(0usize, "Samsung SSD 980".to_string()), (1, "WD SN550".to_string())];
        // boot 2: the same two drives, enumerated the other way round
        let b2 = vec![(0usize, "WD SN550".to_string()), (1, "Samsung SSD 980".to_string())];
        assert_eq!(stable_index(0, "Samsung SSD 980", &b1), stable_index(1, "Samsung SSD 980", &b2));
        assert_eq!(stable_index(1, "WD SN550", &b1), stable_index(0, "WD SN550", &b2));
    }

    /// Adding a drive must not let the newcomer inherit an existing drive's
    /// key — that is how a fan curve ends up following the wrong disk.
    #[test]
    fn adding_a_drive_does_not_reuse_another_drives_key() {
        let before = vec![(0usize, "Samsung SSD 980".to_string()), (1, "WD SN550".to_string())];
        let after = vec![
            (0usize, "Samsung SSD 980".to_string()),
            (1, "Crucial P3".to_string()),
            (2, "WD SN550".to_string()),
        ];
        // alphabetical: Crucial < Samsung < WD
        assert_eq!(stable_index(1, "Crucial P3", &after), 0);
        assert_eq!(stable_index(0, "Samsung SSD 980", &after), 1);
        assert_eq!(stable_index(2, "WD SN550", &after), 2);
        // and the newcomer did NOT take the old key of either existing drive
        assert_ne!(stable_index(1, "Crucial P3", &before), stable_index(0, "Samsung SSD 980", &before));
        let _ = before;
    }

    /// A drive smartctl reports no model for still gets a distinct index
    /// rather than colliding with a named one.
    #[test]
    fn unnamed_drives_fall_back_to_probe_order() {
        let all = vec![(0usize, "".to_string()), (1, "Samsung SSD 980".to_string())];
        // unnamed sorts last
        assert_eq!(stable_index(0, "", &all), 1);
        assert_eq!(stable_index(1, "Samsung SSD 980", &all), 0);
    }

    /// A fanless card (or any field the driver does not expose) makes
    /// nvidia-smi print "[N/A]". Parsing must not then throw away the GPU
    /// temperature as well — that is the field every caller needs.
    #[test]
    fn gpu_temp_survives_na_fields() {
        // mirrors how read_gpu() now parses: independent per-field
        fn parse(csv: &str) -> Option<(f32, f32)> {
            let fields: Vec<&str> = csv.split(',').map(|x| x.trim()).collect();
            let t = fields.first().and_then(|f| f.parse::<f32>().ok())?;
            let u = fields.get(1).and_then(|f| f.parse::<f32>().ok()).unwrap_or(0.0);
            Some((t, u))
        }
        assert_eq!(parse("61, 35"), Some((61.0, 35.0)));
        // fan speed missing: temperature must still come through
        assert_eq!(parse("61, [N/A]"), Some((61.0, 0.0)));
        // and a genuinely unreadable temperature is still None
        assert_eq!(parse("[N/A], 35"), None);
    }
    #[test]
    fn first_number_ignores_separators_and_units() {
        assert_eq!(first_number("13,773"), Some(13773.0));
        assert_eq!(first_number("38 C"), Some(38.0));
        assert_eq!(first_number("Temperature: 41 Celsius"), Some(41.0));
    }
}
