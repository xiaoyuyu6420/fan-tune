//! The machine's light profile: which segments exist, what they show, and the
//! global look — stored next to the exe as `fantune-rgb.json`.
//!
//! Separate from `fantune.json` on purpose: the fan configuration must keep
//! loading (and saving) even if the light profile is missing or hand-edited
//! badly, and a machine that never ran the light setup must never have its
//! lights touched. The profile file *is* the consent.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::effects::{Diffusion, Segment, Strategy, Style};

/// Update-rate cap. Every *changed* output write costs ~3 ms of USB control
/// transfers (see README), so animations are limited while slow strategies
/// (temperature) are effectively free — they only write when the colour
/// actually moves.
pub const DEFAULT_MAX_FPS: f32 = 5.0;
pub const MAX_FPS_LIMIT: f32 = 20.0;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RgbProfile {
    pub enabled: bool,
    /// Board name the segments were measured on (documentation + sanity).
    pub board: String,
    /// Cap on render passes per second.
    #[serde(default = "default_fps")]
    pub max_fps: f32,
    /// Keep the last colours when the program exits (true) or hand the lights
    /// back to the controller's own effect (false).
    #[serde(default = "yes")]
    pub keep_on_exit: bool,
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub style: Style,
    /// Set once positions and lengths were measured with the wizard.
    #[serde(default)]
    pub calibrated: bool,
}

fn default_fps() -> f32 {
    DEFAULT_MAX_FPS
}
fn yes() -> bool {
    true
}

impl RgbProfile {
    /// Where the profile lives: next to the executable, like the fan config.
    pub fn path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("fantune-rgb.json")))
            .unwrap_or_else(|| PathBuf::from("fantune-rgb.json"))
    }

    /// The starting point for a machine whose lights were never measured: one
    /// segment per ARGB chain, showing a fixed colour, marked uncalibrated so
    /// the panel can say so. `first_led..128` is written because the physical
    /// chain length is not known yet — extra data simply falls off the end.
    pub fn uncalibrated_identify() -> RgbProfile {
        let mut ch1 = Segment::new("ARGB 通道 1（待识别）", 0, 128);
        ch1.strategy = Strategy::Static { color: [0x00, 0xc0, 0x00] };
        ch1.diffusion = Diffusion::Diffused;

        let mut ch2 = Segment::new("ARGB 通道 2（待识别）", 1, 128);
        ch2.strategy = Strategy::Static { color: [0x20, 0x60, 0xff] };
        ch2.diffusion = Diffusion::Diffused;

        RgbProfile {
            enabled: true,
            board: "铭瑄 B760M 终结者 D4（ENE 6k7750，未校准）".to_string(),
            max_fps: DEFAULT_MAX_FPS,
            keep_on_exit: true,
            segments: vec![ch1, ch2],
            style: Style::default(),
            calibrated: false,
        }
    }

    /// Load the profile. `Ok(None)` means "this machine has no light profile"
    /// — the caller must then leave the lights alone.
    pub fn load() -> (Option<RgbProfile>, Option<String>) {
        let path = RgbProfile::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return (None, None),
        };
        match serde_json::from_str::<RgbProfile>(&text) {
            Ok(p) => (Some(p), None),
            Err(e) => {
                let corrupt = path.with_extension("json.corrupt");
                let _ = std::fs::rename(&path, &corrupt);
                let bak = path.with_extension("json.bak");
                if let Ok(s) = std::fs::read_to_string(&bak) {
                    if let Ok(p) = serde_json::from_str::<RgbProfile>(&s) {
                        return (
                            Some(p),
                            Some(format!("灯光配置解析失败（{e}），已改用上一版备份 {}", bak.display())),
                        );
                    }
                }
                (
                    None,
                    Some(format!(
                        "灯光配置解析失败（{e}），损坏文件保留为 {}，本次不动灯",
                        corrupt.display()
                    )),
                )
            }
        }
    }

    /// Atomic save (temp file + rename, previous revision as `.bak`), same
    /// discipline as the fan config.
    pub fn save(&self) -> Result<(), String> {
        let path = RgbProfile::path();
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, text).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
        if path.exists() {
            let _ = std::fs::copy(&path, path.with_extension("json.bak"));
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("替换 {} 失败：{e}", path.display()))
    }

    pub fn max_fps(&self) -> f32 {
        if self.max_fps.is_finite() {
            self.max_fps.clamp(0.5, MAX_FPS_LIMIT)
        } else {
            DEFAULT_MAX_FPS
        }
    }

    pub fn tick_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(1.0 / self.max_fps())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncalibrated_profile_is_the_identify_constant() {
        let p = RgbProfile::uncalibrated_identify();
        assert!(p.enabled);
        assert!(!p.calibrated);
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].strategy, Strategy::Static { color: [0, 192, 0] });
        assert_eq!(p.segments[1].output, 1);
    }

    #[test]
    fn profile_round_trips_through_json() {
        let p = RgbProfile::uncalibrated_identify();
        let text = serde_json::to_string(&p).unwrap();
        let back: RgbProfile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.segments, p.segments);
        assert_eq!(back.style, p.style);
    }

    #[test]
    fn missing_optional_fields_still_load() {
        // a hand-written minimal profile: only what is really required
        let text = r#"{
            "enabled": true,
            "board": "hand-written",
            "segments": [ { "name": "x", "output": 0, "leds": 12,
                            "strategy": { "kind": "static", "color": [1,2,3] } } ]
        }"#;
        let p: RgbProfile = serde_json::from_str(text).expect("parses");
        assert_eq!(p.max_fps(), DEFAULT_MAX_FPS);
        assert!(p.keep_on_exit);
        assert_eq!(p.style, Style::default());
        assert!(p.segments[0].gain == 1.0);
    }

    #[test]
    fn fps_is_clamped() {
        let mut p = RgbProfile::uncalibrated_identify();
        p.max_fps = 1000.0;
        assert_eq!(p.max_fps(), MAX_FPS_LIMIT);
        p.max_fps = 0.0;
        assert_eq!(p.max_fps(), 0.5);
    }
}
