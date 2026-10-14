//! Light (RGB) subsystem: transport → controller → segments → effects.
//!
//! Design rules this module follows (see the README's "RGB 灯控" section):
//!
//! * **never a second owner**: the profile file must exist before a single
//!   register is written, and `fantune-rgb.json` is only created by setup;
//! * **write only what changed** — the controller is told nothing unless a
//!   segment's colours actually moved, which makes static and slow effects
//!   free and keeps animations inside a measured transfer budget;
//! * **no effect logic here**: rendering lives in [`effects`], hardware in
//!   [`controller`]/[`transport`], so the pure parts stay unit-testable.

pub mod controller;
pub mod effects;
pub mod profile;
pub mod transport;

use std::time::Instant;

pub use controller::{Output, OUTPUTS};
pub use effects::{Diffusion, Inputs, Missing, Segment, Shape, Strategy, Style};
pub use profile::RgbProfile;
pub use transport::Device;

/// Which segment (index) is currently being flashed by the 识别 button.
pub type Pulse = Option<(usize, Instant)>;

pub struct RgbEngine {
    /// `None` while the controller has been handed to the calibration tool:
    /// WinUSB grants one handle per interface, so sharing means letting go.
    ctrl: Option<controller::Controller>,
    /// Remembered so the device can be re-opened after calibration.
    path: String,
    profile: RgbProfile,
    /// Last colours written per output — the change detector.
    last: [Option<Vec<[u8; 3]>>; OUTPUTS.len()],
    /// A segment held at full white to identify it (index, until).
    pub pulse: Pulse,
    /// While true the engine renders but never writes; used by the calibration
    /// wizard so it can drive the lights itself.
    pub paused: bool,
    /// Sensor keys that failed to resolve in the last pass (panel display).
    pub missing: Vec<String>,
    /// Writes performed since start, for the panel / diagnostics.
    pub writes: u64,
    /// Last error from the controller (kept so the panel can show it once).
    pub last_error: Option<String>,
}

impl RgbEngine {
    pub fn open(profile: RgbProfile) -> Result<RgbEngine, String> {
        let ctrl = controller::Controller::open()?;
        let path = ctrl.path().to_string();
        Ok(RgbEngine {
            ctrl: Some(ctrl),
            path,
            profile,
            last: Default::default(),
            pulse: None,
            paused: false,
            missing: Vec::new(),
            writes: 0,
            last_error: None,
        })
    }

    pub fn profile(&self) -> &RgbProfile {
        &self.profile
    }

    /// Replace the profile (from the panel or the wizard). Forces the next
    /// pass to rewrite every output.
    pub fn set_profile(&mut self, profile: RgbProfile) {
        self.profile = profile;
        self.invalidate();
    }

    /// Forget what is on the wire, so the next pass writes everything again.
    pub fn invalidate(&mut self) {
        self.last = Default::default();
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Is the controller handle held by this process right now?
    pub fn holds_device(&self) -> bool {
        self.ctrl.is_some()
    }

    /// Hand the controller to another process (the calibration tool). The
    /// colours stay on the wire; only the handle is dropped.
    pub fn yield_device(&mut self) {
        self.ctrl = None;
        self.invalidate();
    }

    /// Take the controller back after calibration.
    pub fn take_device(&mut self) -> Result<(), String> {
        if self.ctrl.is_none() {
            self.ctrl = Some(controller::Controller::open()?);
        }
        // whatever the tool wrote is unknown: rewrite everything
        self.invalidate();
        Ok(())
    }

    /// One render pass. Returns how many outputs were written (0 = nothing
    /// changed, which is the steady state of every non-animated strategy).
    pub fn apply(&mut self, inputs: &Inputs, now: Instant) -> Result<usize, String> {
        // 1. render every output that has at least one segment we own
        let mut frames: [Vec<[u8; 3]>; OUTPUTS.len()] =
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        self.missing.clear();
        for (idx, seg) in self.profile.segments.iter().enumerate() {
            if seg.output >= OUTPUTS.len() || matches!(seg.strategy, Strategy::Off) {
                continue;
            }
            let out = OUTPUTS[seg.output];
            let (mut colors, missing) = effects::render(seg, inputs, &self.profile.style);
            if missing != Missing::None {
                self.missing.push(seg.name.clone());
            }
            // the 识别 flash overrides whatever the strategy produced
            if let Some((who, until)) = self.pulse {
                if who == idx && now < until {
                    colors = vec![[255, 255, 255]; colors.len()];
                }
            }
            let start = seg.first_led as usize;
            let frame = &mut frames[seg.output];
            let need = (start + colors.len()).min(out.leds);
            if frame.len() < need {
                frame.resize(need, [0, 0, 0]);
            }
            for (i, c) in colors.into_iter().enumerate() {
                if start + i < out.leds {
                    frame[start + i] = c;
                }
            }
        }
        if let Some((_, until)) = self.pulse {
            if now >= until {
                self.pulse = None;
            }
        }

        // 2. write only the outputs whose bytes changed
        if self.paused {
            return Ok(0);
        }
        let mut wrote = 0;
        let ctrl = match self.ctrl.as_mut() {
            Some(c) => c,
            None => return Ok(0), // controller handed to the calibration tool
        };
        for (o, frame) in frames.iter().enumerate() {
            if frame.is_empty() {
                continue; // nothing of ours on this output
            }
            if self.last[o].as_ref() == Some(frame) {
                continue; // unchanged since the last write: no traffic at all
            }
            match ctrl.write_leds(&OUTPUTS[o], frame) {
                Ok(()) => {
                    self.last[o] = Some(frame.clone());
                    self.writes += 1;
                    wrote += 1;
                }
                Err(e) => {
                    // A failed write drops the cache so the next pass retries
                    // from scratch instead of believing the wire is current.
                    self.last[o] = None;
                    let msg = format!("{} 写入失败（Win32 {e}）", OUTPUTS[o].name);
                    self.last_error = Some(msg.clone());
                    return Err(msg);
                }
            }
        }
        Ok(wrote)
    }

    /// Read an output's buffer back (diagnostics / calibration).
    pub fn read_output(&self, output: usize) -> Result<Vec<[u8; 3]>, String> {
        let out = OUTPUTS
            .get(output)
            .ok_or_else(|| format!("输出 {output} 不存在"))?;
        self.ctrl
            .as_ref()
            .ok_or_else(|| "控制器已让给校准工具".to_string())?
            .read_leds(out, out.leds)
            .map_err(|e| format!("读取 {} 失败（Win32 {e}）", out.name))
    }

    /// Status register dump (diagnostics).
    pub fn status(&mut self) -> String {
        match self.ctrl.as_mut() {
            Some(c) => c.status(),
            None => "控制器已让给校准工具".to_string(),
        }
    }

    /// Hand the lights back to the controller's own effect.
    pub fn release(&mut self) -> Result<(), String> {
        self.ctrl
            .as_mut()
            .ok_or_else(|| "控制器已让给校准工具".to_string())?
            .release()
            .map_err(|e| format!("还灯给系统失败（Win32 {e}）"))?;
        self.invalidate();
        Ok(())
    }

    /// Exit behaviour: `keep` leaves the last colours on the wire (the
    /// controller holds them), otherwise the lights go back to the board.
    pub fn shutdown(&mut self) {
        if !self.profile.keep_on_exit {
            let _ = self.release();
        }
    }
}
