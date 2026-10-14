//! Fan curve + persistent configuration model (serde JSON).

use serde::{Deserialize, Serialize};

/// Location of the live config: next to the executable, independent of the
/// working directory the process happened to start with (a CWD-relative path
/// let two diverging fantune.json copies exist — the UI edited one, the repo
/// template was the other).
///
/// Sidecars written next to it: `fantune.json.bak` (previous revision),
/// `fantune.json.corrupt` (a file that failed to parse), `fantune-originals.json`
/// (the BIOS fan registers captured before the first override, see `nct.rs`).
pub fn config_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fantune.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("fantune.json"))
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Debug)]
pub enum FanMode {
    /// BIOS automatic control (we stop writing and restore saved mode).
    Auto,
    /// Fixed duty cycle percentage.
    Manual,
    /// Temperature-following curve.
    Curve,
    /// Closed-loop thermostat: hold the temperature inside
    /// `[budget_c - hysteresis_c, budget_c]` with the least duty that keeps it
    /// there. The duty moves only when the temperature leaves that band, so a
    /// temperature inside the budget is free (and silent). `budget_c` = 0
    /// means "not configured" and falls back to curve behaviour.
    Budget,
}

/// How several temperature sources of one channel are combined.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceCombine {
    /// Hottest reading wins (safest for cooling).
    #[default]
    Max,
    /// Mean of the sources that resolved this tick.
    Avg,
}

/// Global control profile — how computed duties reach the fans.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanProfile {
    /// Every channel does its own thing (curve / manual / auto).
    #[default]
    Standard,
    /// Gaming: curves are evaluated as if it were `gaming_temp_offset` °C
    /// warmer — the ramp engages earlier when heat soaks in, while the low end
    /// of the curve (idle) stays exactly as quiet as Standard.
    Gaming,
    /// Server: curves still follow temperatures, but the response-rate
    /// parameters are pinned to very stable values (ramp ≤ 3 %/s, hysteresis
    /// ≥ 3 °C, EMA ≤ 0.25, user values win when more conservative) — the duty
    /// can barely move per tick and never oscillates. A per-channel guard
    /// forces 100% while the channel's own hottest source reads at or above
    /// `server_guard_c` (released 5 °C below).
    Server,
}

/// serde default for `gaming_temp_offset`.
fn default_gaming_offset() -> f32 {
    5.0
}
/// serde default for `server_guard_c`.
fn default_server_guard() -> f32 {
    85.0
}

/// serde default for `ui_scale`: 1.2 — the token type scale runs small on
/// purpose (it is an instrument aesthetic), so the out-of-box zoom leans up.
fn default_ui_scale() -> f32 {
    1.2
}

/// serde default for `ema_alpha`: 1.0 = no filtering.
fn default_ema_alpha() -> f32 {
    1.0
}

/// Which ring-0 backend to drive the chip with.
///
/// WinRing0 is roughly three times cheaper per port operation (a native IOCTL
/// against PawnIO's interpreted module), but it is the driver Windows
/// blocklists and Defender flags — and it needs a kernel service of ours.
/// PawnIO is the durable path. `Auto` picks by what the machine allows.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum DriverBackend {
    /// WinRing0 when Windows would let it load, else PawnIO.
    #[default]
    Auto,
    /// Force WinRing0 first (falls back to PawnIO if it will not load).
    Winring0,
    /// Force PawnIO first (falls back to WinRing0 if it is not installed).
    Pawnio,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct FanChannelConfig {
    pub enabled: bool,
    pub name: String,
    /// Human-facing name shown by the UI; empty = fall back to `name`.
    /// `#[serde(default)]` keeps pre-label fantune.json files loadable.
    #[serde(default)]
    pub label: String,
    pub mode: FanMode,
    /// manual duty % when mode == Manual
    pub manual_duty: f32,
    /// temp source key: "nct:PECI_0 (CPU)" | "nvme:0" ...
    pub source: String,
    /// (temp C, duty %) pairs, sorted by temp
    pub points: Vec<[f32; 2]>,
    /// extra temperature sources; empty = the legacy `source` is used alone
    #[serde(default)]
    pub sources: Vec<String>,
    /// how `sources` combine when more than one resolves
    #[serde(default)]
    pub combine: SourceCombine,
    /// `FanMode::Budget`: the temperature ceiling the loop holds. The band
    /// `[budget_c - hysteresis_c, budget_c]` is "good enough" — inside it the
    /// duty holds still. 0 (the serde default) means not configured: the
    /// engine falls back to the curve instead of stopping the fan.
    #[serde(default)]
    pub budget_c: f32,
    /// hysteresis deadband in °C (0 = off). In budget mode this is the width
    /// of the hold band below `budget_c`.
    #[serde(default)]
    pub hysteresis_c: f32,
    /// max duty rise per second (0 = unlimited)
    #[serde(default)]
    pub ramp_up_pct_s: f32,
    /// max duty fall per second (0 = unlimited)
    #[serde(default)]
    pub ramp_down_pct_s: f32,
    /// EMA smoothing factor 0..=1 (1 = no filtering)
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f32,
    /// kick the fan to 100% briefly when starting from a standstill
    #[serde(default)]
    pub spin_up: bool,
    /// Hands off for the one-click overrides (full speed / quiet) and for the
    /// safe-duty fallback: only this channel's own curve/manual setting writes
    /// it. For parts that must not be slowed down or stopped on a whim — pumps,
    /// above all. (`enabled: false` is the stronger form: nothing writes it.)
    #[serde(default)]
    pub protected: bool,
}

impl FanChannelConfig {
    /// May the controller write this channel at all? `enabled: false` means the
    /// channel is left entirely to the BIOS — no curve, no manual duty, no
    /// override.
    pub fn managed(&self) -> bool {
        self.enabled
    }

    /// Must the one-click overrides and the safe-duty fallback leave it alone?
    pub fn hands_off(&self) -> bool {
        !self.enabled || self.protected
    }
}

impl Default for FanChannelConfig {
    fn default() -> Self {
        FanChannelConfig {
            enabled: false,
            name: "Fan".into(),
            label: String::new(),
            mode: FanMode::Auto,
            manual_duty: 50.0,
            source: "nct:PECI_0 (CPU)".into(),
            points: vec![[40.0, 40.0], [70.0, 100.0]],
            sources: Vec::new(),
            combine: SourceCombine::Max,
            budget_c: 0.0,
            hysteresis_c: 0.0,
            ramp_up_pct_s: 0.0,
            ramp_down_pct_s: 0.0,
            ema_alpha: 1.0,
            spin_up: false,
            protected: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct AppConfig {
    pub channels: Vec<FanChannelConfig>,
    /// daemon loop interval seconds
    pub interval_ms: u64,
    /// ring-0 backend preference; `auto` asks the machine (see [`DriverBackend`])
    #[serde(default)]
    pub backend: DriverBackend,
    /// active control profile (standard / gaming / server)
    #[serde(default)]
    pub profile: FanProfile,
    /// Gaming: evaluate every curve as if it were this many °C warmer
    /// (earlier ramp-up; the idle end of the curve stays untouched)
    #[serde(default = "default_gaming_offset")]
    pub gaming_temp_offset: f32,
    /// Server: per-channel thermal guard threshold °C (0 = off)
    #[serde(default = "default_server_guard")]
    pub server_guard_c: f32,
    /// UI palette id, e.g. `"archive-dark"` / `"front-light"`. Purely
    /// cosmetic: empty or unrecognised means "use the built-in default", so a
    /// stale or hand-edited value can never keep the window from opening.
    #[serde(default)]
    pub ui_theme: String,
    /// Global UI zoom applied on top of the OS DPI factor (1.0 = no extra
    /// zoom). Purely cosmetic and clamped at the application site.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
}

impl AppConfig {
    /// Load the live config.
    ///
    /// Returns `(config, warning)`. A file that exists but does not parse is
    /// moved aside as `fantune.json.corrupt` — never silently overwritten — and
    /// the last good `fantune.json.bak` is used when it parses. The curves are
    /// the one thing in this project that cannot be re-derived, so a corrupt
    /// file must not end in a silent revert to defaults.
    pub fn load() -> (AppConfig, Option<String>) {
        let path = config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return (AppConfig::default_with_7(), None), // no file yet
        };
        match serde_json::from_str::<AppConfig>(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                let corrupt = path.with_extension("json.corrupt");
                let _ = std::fs::rename(&path, &corrupt);
                let bak = path.with_extension("json.bak");
                if let Ok(s) = std::fs::read_to_string(&bak) {
                    if let Ok(cfg) = serde_json::from_str::<AppConfig>(&s) {
                        return (
                            cfg,
                            Some(format!(
                                "配置文件解析失败（{e}）\n\n已改用上一版备份：{}\n损坏的文件保留为：{}",
                                bak.display(),
                                corrupt.display()
                            )),
                        );
                    }
                }
                (
                    AppConfig::default_with_7(),
                    Some(format!(
                        "配置文件解析失败（{e}）\n\n损坏的文件已保留为：{}\n本次使用默认配置",
                        corrupt.display()
                    )),
                )
            }
        }
    }

    pub fn default_with_7() -> AppConfig {
        let mut channels = Vec::new();
        for i in 0..7 {
            let mut c = FanChannelConfig::default();
            c.name = format!("Fan #{i}");
            // A fresh install must be able to switch a channel to Curve/Manual
            // without first hunting for an `enabled` flag — Auto mode writes
            // nothing anyway, so this changes no behaviour, only the trap.
            c.enabled = true;
            channels.push(c);
        }
        AppConfig {
            channels,
            interval_ms: 2000,
            backend: DriverBackend::Auto,
            profile: FanProfile::Standard,
            gaming_temp_offset: default_gaming_offset(),
            server_guard_c: default_server_guard(),
            ui_theme: String::new(),
            ui_scale: default_ui_scale(),
        }
    }

    /// Write the config atomically (temp file + replace) and keep the previous
    /// revision as `fantune.json.bak`, so a crash or a power cut mid-write can
    /// never leave a half-written file behind.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, text).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
        if path.exists() {
            let _ = std::fs::copy(&path, path.with_extension("json.bak"));
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("替换 {} 失败：{e}", path.display()))
    }
}

/// Linear interpolation on sorted points; clamps at ends.
pub fn eval_curve(points: &[[f32; 2]], t: f32) -> f32 {
    if points.is_empty() {
        return 50.0;
    }
    if t <= points[0][0] {
        return points[0][1];
    }
    for w in points.windows(2) {
        let (t0, d0) = (w[0][0], w[0][1]);
        let (t1, d1) = (w[1][0], w[1][1]);
        if t <= t1 {
            if t1 - t0 < 0.01 {
                return d1;
            }
            let f = (t - t0) / (t1 - t0);
            return d0 + f * (d1 - d0);
        }
    }
    points[points.len() - 1][1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `enabled` / `protected` decide what the one-click overrides and the
    /// safe-duty fallback are allowed to touch — the pump has to survive both
    /// 静音 and "the temperature sources are dead, blow".
    #[test]
    fn hands_off_covers_pumps_and_disabled_channels() {
        let mut c = FanChannelConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(c.managed());
        assert!(!c.hands_off(), "a normal channel follows the overrides");

        // protected: still driven by its own curve/manual setting, but no
        // one-click override and no safe-duty fallback
        c.protected = true;
        assert!(c.managed());
        assert!(c.hands_off());

        // enabled = false: nothing writes it at all, not even its own mode
        c.enabled = false;
        assert!(!c.managed());
        assert!(c.hands_off());
    }

    /// A fantune.json written before `label` / the algorithm fields existed must
    /// keep loading with the user's channels intact — `AppConfig::load` silently
    /// falls back to defaults on any deserialization error, and the GUI saves on
    /// exit, so a missing serde default would wipe the real config.
    #[test]
    fn legacy_config_loads_with_defaults() {
        let legacy = r#"{
            "interval_ms": 2000,
            "channels": [
              {
                "enabled": true,
                "name": "Fan #0 (机箱/冷排A)",
                "mode": "Curve",
                "manual_duty": 50.0,
                "source": "nvme:0",
                "points": [[42.0, 45.0], [62.0, 90.0]]
              }
            ]
        }"#;
        let cfg: AppConfig = serde_json::from_str(legacy).expect("legacy json must load");
        assert_eq!(cfg.channels.len(), 1);
        let c = &cfg.channels[0];
        assert!(matches!(c.mode, FanMode::Curve));
        assert_eq!(c.points, vec![[42.0, 45.0], [62.0, 90.0]]);
        assert_eq!(c.source, "nvme:0");
        // new fields fall back to their documented defaults
        assert!(c.label.is_empty());
        assert!(c.sources.is_empty());
        assert_eq!(c.combine, SourceCombine::Max);
        assert_eq!(c.budget_c, 0.0);
        assert_eq!(c.hysteresis_c, 0.0);
        assert_eq!(c.ramp_up_pct_s, 0.0);
        assert_eq!(c.ramp_down_pct_s, 0.0);
        assert_eq!(c.ema_alpha, 1.0);
        assert!(!c.spin_up);
        assert!(!c.protected);
        // global profile fields fall back to their documented defaults
        assert_eq!(cfg.profile, FanProfile::Standard);
        assert_eq!(cfg.gaming_temp_offset, 5.0);
        assert_eq!(cfg.server_guard_c, 85.0);
    }

    /// The shipped fantune.json is the machine-neutral default: every channel
    /// Auto and disabled, so a first run writes nothing at all. Someone else's
    /// curves and `nvme:N` indices must never ship as the default — a drive
    /// index that means one thing here means another on a different machine.
    ///
    /// It must also stay loadable by pre-label code (serde ignores unknown
    /// fields), and the labelled example must keep its labels.
    #[test]
    fn repo_config_keeps_labels_and_loads_as_legacy() {
        #[derive(Deserialize)]
        struct LegacyChannel {
            enabled: bool,
            name: String,
            mode: FanMode,
            manual_duty: f32,
            source: String,
            points: Vec<[f32; 2]>,
        }
        #[derive(Deserialize)]
        struct LegacyConfig {
            channels: Vec<LegacyChannel>,
        }

        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fantune.json"))
            .expect("repo fantune.json readable");
        let cfg: AppConfig = serde_json::from_str(&raw).expect("repo fantune.json parses");
        assert_eq!(cfg.channels.len(), 7);
        // safe by default: nothing is driven until the user asks for it
        assert!(
            cfg.channels.iter().all(|c| !c.enabled && matches!(c.mode, FanMode::Auto)),
            "the shipped config must not drive fans on someone else's machine"
        );
        // old code (no label/algorithm fields) still reads it
        let legacy: LegacyConfig =
            serde_json::from_str(&raw).expect("unknown fields are tolerated");
        assert_eq!(legacy.channels.len(), 7);

        // the labelled example keeps its labels and still parses
        let ex = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/fantune.example.json"
        ))
        .expect("example config readable");
        let ex_cfg: AppConfig = serde_json::from_str(&ex).expect("example parses");
        assert_eq!(ex_cfg.channels.len(), 7);
        assert_eq!(ex_cfg.channels[0].label, "机箱扇");
        assert_eq!(ex_cfg.channels[1].label, "280冷排");
        assert_eq!(ex_cfg.channels[5].label, "水泵");
        assert!(ex_cfg.channels[2].label.is_empty());
    }
}
