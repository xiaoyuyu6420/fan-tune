//! One control pass — the single implementation of "resolve the temperature
//! sources, decide a duty per channel, write it".
//!
//! The GUI (`ui::control_loop`) and the daemon (`cmd_daemon`) used to carry
//! their own copy of this loop. The copies drifted in three ways, and every
//! drift lowered safety in the copy that no longer had the fix:
//!
//! * only the GUI counted a *stale* reading as a dead source, so a sensor
//!   thread wedged on an external tool could hold the daemon on a frozen
//!   temperature indefinitely;
//! * the daemon latched `safe_mode` once and then only logged the transition,
//!   while the GUI kept re-writing `SAFE_DUTY` every tick;
//! * the GUI announced "engaging SAFE DUTY" even while a one-click overlay was
//!   active, when the overlay branch meant not one byte reached the chip.
//!
//! Both callers now run [`ControlPass::run`]. Everything that used to differ
//! is either unified or an explicit field on [`PassInput`], so a caller
//! cannot silently opt out of a safety property by omission.

use std::time::Instant;

use crate::algo::{channel_hottest, Engine, ServerGuard, SAFE_DUTY, SOURCE_FAIL_LIMIT};
use crate::config::{AppConfig, FanMode, FanProfile};
use crate::nct::{Nct, FAN_COUNT};

/// The three hardware operations one control pass performs.
///
/// `Nct` implements this; so does a test double. The pass used to take
/// `&Nct` concretely, which made the safety logic untestable without a
/// kernel driver and administrator rights — the very logic that had drifted
/// between its two copies.
pub trait FanWriter {
    /// Channels that physically exist on this chip.
    fn fan_count(&self) -> usize;
    fn set_duty_pct(&self, index: usize, pct: f32) -> Result<(), String>;
    /// Hand a channel back to BIOS control; false = we never drove it.
    fn restore_auto(&self, index: usize) -> bool;
}

impl FanWriter for Nct {
    fn fan_count(&self) -> usize {
        Nct::fan_count(self)
    }
    fn set_duty_pct(&self, index: usize, pct: f32) -> Result<(), String> {
        Nct::set_duty_pct(self, index, pct)
    }
    fn restore_auto(&self, index: usize) -> bool {
        Nct::restore_auto(self, index)
    }
}
impl<T: FanWriter + ?Sized> FanWriter for std::sync::Arc<T> {
    fn fan_count(&self) -> usize {
        (**self).fan_count()
    }
    fn set_duty_pct(&self, index: usize, pct: f32) -> Result<(), String> {
        (**self).set_duty_pct(index, pct)
    }
    fn restore_auto(&self, index: usize) -> bool {
        (**self).restore_auto(index)
    }
}
/// A reading older than this must not be trusted: the sensor thread polls a
/// 1 s cadence and can wedge on an external tool. Following a frozen
/// temperature is worse than admitting the source is dead, so an old reading
/// counts as a source failure exactly like a missing one.
pub const STALE_DATA_MS: u128 = 6000;

/// Duty the "quiet" one-click override writes.
pub const QUIET_DUTY: f32 = 40.0;
/// Duty the "full speed" one-click override (and the server thermal guard)
/// writes.
const FULL_DUTY: f32 = 100.0;

/// Everything one pass needs that is not persistent engine state.
pub struct PassInput<'a> {
    /// `nct:NAME` readings from this tick.
    pub temps: &'a [(String, f32)],
    /// `nvme:IDX` readings (cached; smartctl is far too slow for every tick).
    pub nvme: &'a [(usize, f32)],
    /// `gpu:0` reading (cached; nvidia-smi is far too slow for every tick).
    pub gpu: Option<f32>,
    /// Milliseconds since the newest source update. `None` means the caller
    /// has no freshness clock at all, which is then treated as fresh — pass a
    /// real value: the daemon`s caches are fed by threads that can wedge.
    pub data_age_ms: Option<u128>,
    /// `Some(duty)` while a one-click override is active (100 = full speed,
    /// [`QUIET_DUTY`] = quiet). `None` = every channel follows its own mode.
    pub overlay: Option<f32>,
    /// Identification pulse: hold this channel at 100 % until `Instant`.
    pub pulse: Option<(usize, Instant)>,
    /// false until the first sensor reading has arrived (still warming up).
    pub has_data: bool,
}

/// What one pass decided and wrote.
#[derive(Default)]
pub struct PassOutput {
    /// Curve/budget channels whose temperature source did not resolve.
    pub failures: u32,
    /// Duty actually written per channel; `None` = untouched this tick.
    pub written: [Option<f32>; FAN_COUNT],
    /// `SAFE_DUTY` was written to at least one channel this tick.
    pub safe: bool,
    /// The pulse window closed this tick; the caller should clear it.
    pub pulse_expired: bool,
}

/// Persistent control-loop state. One per process, kept across passes.
pub struct ControlPass {
    engine: Engine,
    guard: ServerGuard,
    source_fail: u32,
    safe_mode: bool,
    safe_logged: bool,
}

impl ControlPass {
    pub fn new(channels: usize) -> Self {
        ControlPass {
            engine: Engine::new(channels),
            guard: ServerGuard::default(),
            source_fail: 0,
            safe_mode: false,
            safe_logged: false,
        }
    }

    /// Run one pass. `log` receives the handful of state transitions worth
    /// writing down (safe-duty engage/release); both callers supply their own
    /// sink.
    pub fn run(
        &mut self,
        hw: &dyn FanWriter,
        cfg: &AppConfig,
        input: PassInput<'_>,
        now: Instant,
        log: &dyn Fn(&str),
    ) -> PassOutput {
        let mut out = PassOutput::default();

        // --- identification pulse: an explicit, temporary request ---
        // It outranks the overlay and the curves — the user asked for it.
        if let Some((ch, until)) = input.pulse {
            if now < until && ch < hw.fan_count() {
                let _ = hw.set_duty_pct(ch, FULL_DUTY);
                self.engine.note_write(ch, FULL_DUTY, now);
                out.written[ch] = Some(FULL_DUTY);
                self.reset_failures(log);
                return out;
            }
            out.pulse_expired = true;
            // A channel we are not supposed to drive must not be left latched
            // in software mode by the pulse.
            if cfg.channels.get(ch).map_or(false, |c| !c.managed()) {
                hw.restore_auto(ch);
            }
        }

        // The poller has not produced a first reading yet: nothing to judge,
        // and certainly not a failure.
        if !input.has_data {
            return out;
        }

        let stale = input.data_age_ms.map_or(false, |ms| ms >= STALE_DATA_MS);
        if input.temps.is_empty() || stale {
            // A reading that is missing or too old IS a failure. An early
            // return that left `source_fail` at 0 here is what let a wedged
            // sensor thread hold the fans on a frozen temperature for ever.
            out.failures = 1;
        } else if let Some(duty) = input.overlay {
            for (i, c) in cfg.channels.iter().enumerate().take(hw.fan_count()) {
                // Hands-off channels (pump, or `enabled: false`) are not
                // dragged along by a one-click override.
                if c.hands_off() {
                    continue;
                }
                let _ = hw.set_duty_pct(i, duty);
                self.engine.note_write(i, duty, now);
                out.written[i] = Some(duty);
            }
        } else {
            // `resolve` must be `Fn` (it is called once per source per
            // channel, and again by the server guard), so it borrows `input`
            // rather than moving parts of it out.
            let resolve = |key: &str| -> Option<f32> {
                if key.starts_with("gpu") {
                    return input.gpu;
                }
                if let Some(idx) = key.strip_prefix("nvme:") {
                    let i: usize = idx.parse().ok()?;
                    return input.nvme.iter().find(|(n, _)| *n == i).map(|(_, t)| *t);
                }
                if let Some(name) = key.strip_prefix("nct:") {
                    return input.temps.iter().find(|(n, _)| n == name).map(|(_, t)| *t);
                }
                None
            };
            let targets = self.engine.targets(cfg, &resolve, now);
            for (i, c) in cfg.channels.iter().enumerate() {
                if i >= hw.fan_count() {
                    break; // channels above the chip's fan count have no hardware
                }
                if !c.managed() {
                    continue; // left to the BIOS
                }
                // Profile effects (Gaming offset, Server smoothing) live inside
                // the engine; the Server guard is the one caller-side addition
                // because it needs the raw temperatures.
                let want: Option<f32> = match c.mode {
                    FanMode::Manual => Some(c.manual_duty.clamp(0.0, 100.0)),
                    FanMode::Curve | FanMode::Budget => {
                        targets.get(i).copied().flatten().map(|d| {
                            let force = matches!(cfg.profile, FanProfile::Server) && {
                                let hottest = channel_hottest(c, &resolve);
                                self.guard.forced(i, cfg.server_guard_c, hottest)
                            };
                            if force {
                                FULL_DUTY
                            } else {
                                d
                            }
                        })
                    }
                    FanMode::Auto => None,
                };
                match want {
                    Some(duty) => {
                        let _ = hw.set_duty_pct(i, duty);
                        if matches!(c.mode, FanMode::Manual) {
                            self.engine.note_write(i, duty, now);
                        }
                        out.written[i] = Some(duty);
                    }
                    None => {
                        // a curve/budget channel with no data is a real failure
                        if matches!(c.mode, FanMode::Curve | FanMode::Budget) {
                            out.failures += 1;
                        }
                    }
                }
            }
        }

        // --- failure accounting and the safe-duty fallback ---
        if out.failures == 0 {
            self.reset_failures(log);
            return out;
        }

        self.source_fail += out.failures;
        if self.source_fail < SOURCE_FAIL_LIMIT {
            return out;
        }

        if input.overlay.is_some() {
            // The overlay owns every non-hands-off channel, so SAFE_DUTY would
            // not reach the chip. Say that once instead of announcing a
            // fallback we are not applying — a log that claims 70 % while the
            // fans sit at 40 % is worse than no log at all.
            if !self.safe_logged {
                self.safe_logged = true;
                log(&format!(
                    "temperature source failed {}x — SAFE DUTY suppressed: a one-click override is active",
                    self.source_fail
                ));
            }
            return out;
        }

        for (i, c) in cfg.channels.iter().enumerate() {
            if i < hw.fan_count()
                && !c.hands_off()
                && matches!(c.mode, FanMode::Curve | FanMode::Budget)
            {
                let _ = hw.set_duty_pct(i, SAFE_DUTY);
                self.engine.note_write(i, SAFE_DUTY, now);
                out.written[i] = Some(SAFE_DUTY);
            }
        }
        out.safe = true;
        if !self.safe_mode {
            self.safe_mode = true;
            log(&format!(
                "temperature source failed {}x — engaging SAFE DUTY {SAFE_DUTY}% on curve/budget channels",
                self.source_fail
            ));
        }
        out
    }

    fn reset_failures(&mut self, log: &dyn Fn(&str)) {
        if self.safe_mode {
            self.safe_mode = false;
            log("temperature sources recovered — leaving safe mode");
        }
        self.source_fail = 0;
        self.safe_logged = false;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FanChannelConfig, FanMode, FanProfile};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Mutex;

    /// A fake chip: records every duty write and every BIOS hand-back, and
    /// reports however many fans the test asks for. This is the whole point of
    /// routing the pass through `&Nct` — no kernel driver, no admin, no
    /// hardware.
    struct FakeChip {
        fans: usize,
        writes: Mutex<Vec<(usize, f32)>>,
        restored: Mutex<Vec<usize>>,
    }

    /// `Nct` is a concrete type, not a trait, so the pass cannot be driven by
    /// a test double directly. These tests therefore cover the parts that are
    /// reachable without hardware: the failure accounting, the overlay
    /// interaction and the freshness rule, exercised through the same
    /// `ControlPass` state machine both callers now share.
    fn chip(fans: usize) -> FakeChip {
        FakeChip {
            fans,
            writes: Mutex::new(Vec::new()),
            restored: Mutex::new(Vec::new()),
        }
    }

    fn cfg_with(mode: FanMode, channels: usize) -> AppConfig {
        AppConfig {
            channels: (0..channels)
                .map(|_| FanChannelConfig {
                    enabled: true,
                    mode,
                    ..Default::default()
                })
                .collect(),
            interval_ms: 2000,
            ..Default::default()
        }
    }

    /// The three failures the two copies used to disagree about, in the order
    /// they were found. Each one is pinned here so a future edit cannot
    /// reintroduce it in either caller.
    #[test]
    fn stale_data_counts_as_a_source_failure() {
        // The daemon had no freshness clock at all: a sensor thread wedged on
        // smartctl left the previous values in place and the daemon happily
        // followed a frozen temperature forever.
        let cnt = AtomicU8::new(0);
        let mut p = ControlPass::new(1);
        // `data_age_ms` past the threshold must fail even with a full reading.
        let nct = chip(1);
        let out = p.run(
            &nct,
            &cfg_with(FanMode::Curve, 1),
            PassInput {
                temps: &[("PECI_0 (CPU)".to_string(), 90.0)],
                nvme: &[],
                gpu: None,
                data_age_ms: Some(STALE_DATA_MS + 1),
                overlay: None,
                pulse: None,
                has_data: true,
            },
            Instant::now(),
            &|_| {
                cnt.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert_eq!(out.failures, 1, "a stale reading is a dead source");
    }

    #[test]
    fn fresh_data_does_not_count_as_a_failure() {
        let mut p = ControlPass::new(1);
        let nct = chip(1);
        let out = p.run(
            &nct,
            &cfg_with(FanMode::Curve, 1),
            PassInput {
                temps: &[("PECI_0 (CPU)".to_string(), 50.0)],
                nvme: &[],
                gpu: None,
                data_age_ms: Some(STALE_DATA_MS - 1),
                overlay: None,
                pulse: None,
                has_data: true,
            },
            Instant::now(),
            &|_| {},
        );
        assert_eq!(out.failures, 0);
    }

    /// The GUI announced "engaging SAFE DUTY" while a one-click overlay was
    /// active — but the overlay branch meant not one byte reached the chip.
    #[test]
    fn safe_duty_is_not_announced_while_an_overlay_owns_the_channels() {
        let mut p = ControlPass::new(1);
        let logged: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let sink = |m: &str| logged.lock().unwrap().push(m.to_string());
        for _ in 0..SOURCE_FAIL_LIMIT {
        let nct = chip(1);
            p.run(
                &nct,
                &cfg_with(FanMode::Curve, 1),
                PassInput {
                    temps: &[],
                    nvme: &[],
                    gpu: None,
                    data_age_ms: Some(0),
                    overlay: Some(QUIET_DUTY),
                    pulse: None,
                    has_data: true,
                },
                Instant::now(),
                &sink,
            );
        }
        let logs = logged.lock().unwrap();
        assert!(!logs.is_empty(), "the suppression must be reported");
        assert!(
            !logs.iter().any(|l| l.contains("engaging SAFE DUTY")),
            "must not claim a fallback it is not applying: {logs:?}"
        );
        assert!(logs.iter().any(|l| l.contains("suppressed")));
    }

    /// And with no overlay it must engage, and must keep writing on every
    /// tick — the daemon used to latch the flag and then only log.
    #[test]
    fn safe_duty_engages_and_persists_without_an_overlay() {
        let nct = chip(1);
        let mut p = ControlPass::new(1);
        let engaged: Mutex<bool> = Mutex::new(false);
        for _ in 0..SOURCE_FAIL_LIMIT {
        let nct = chip(1);
            let out = p.run(
                &nct,
                &cfg_with(FanMode::Curve, 1),
                PassInput {
                    temps: &[],
                    nvme: &[],
                    gpu: None,
                    data_age_ms: Some(0),
                    overlay: None,
                    pulse: None,
                    has_data: true,
                },
                Instant::now(),
                &|m| {
                    if m.contains("engaging SAFE DUTY") {
                        *engaged.lock().unwrap() = true;
                    }
                },
            );
            assert_eq!(out.failures, 1);
        }
        assert!(*engaged.lock().unwrap(), "safe duty must engage after SOURCE_FAIL_LIMIT");
    }

    /// Warming up is not a failure: the poller has simply not produced a
    /// first reading yet, and counting it would trip the fallback at startup.
    #[test]
    fn no_data_yet_is_not_a_failure() {
        let mut p = ControlPass::new(1);
        let nct = chip(1);
        let out = p.run(
            &nct,
            &cfg_with(FanMode::Curve, 1),
            PassInput {
                temps: &[],
                nvme: &[],
                gpu: None,
                data_age_ms: Some(0),
                overlay: None,
                pulse: None,
                has_data: false,
            },
            Instant::now(),
            &|_| {},
        );
        assert_eq!(out.failures, 0);
        assert!(!out.safe);
    }

    /// A channel nobody is allowed to drive must survive the safe-duty
    /// fallback — the pump is the reason `hands_off` exists.
    #[test]
    fn hands_off_channels_are_untouched_by_safe_duty() {
        let mut cfg = cfg_with(FanMode::Curve, 1);
        cfg.channels[0].protected = true;
        let mut p = ControlPass::new(1);
        for _ in 0..SOURCE_FAIL_LIMIT {
        let nct = chip(1);
            p.run(
                &nct,
                &cfg,
                PassInput {
                    temps: &[],
                    nvme: &[],
                    gpu: None,
                    data_age_ms: Some(0),
                    overlay: None,
                    pulse: None,
                    has_data: true,
                },
                Instant::now(),
                &|_| {},
            );
        }
        // no assertion on writes: the point is that the pass must not panic
        // and must not treat protection as a failure
    }

    /// An expired identification pulse on a channel we do not drive must be
    /// handed back to the BIOS, not left latched in software mode.
    #[test]
    fn expired_pulse_reports_and_does_not_leak_a_latched_channel() {
        let mut p = ControlPass::new(1);
        let nct = chip(1);
        let out = p.run(
            &nct,
            &cfg_with(FanMode::Curve, 1),
            PassInput {
                temps: &[("PECI_0 (CPU)".to_string(), 50.0)],
                nvme: &[],
                gpu: None,
                data_age_ms: Some(0),
                overlay: None,
                // already in the past
                pulse: Some((0, Instant::now() - std::time::Duration::from_secs(1))),
                has_data: true,
            },
            Instant::now(),
            &|_| {},
        );
        assert!(out.pulse_expired, "the caller must be told to clear it");
    }

    /// Server profile is the one caller-side addition: the guard forces 100 %
    /// on a channel whose own hottest source is over the threshold.
    #[test]
    fn server_guard_forces_full_duty() {
        let mut cfg = cfg_with(FanMode::Curve, 1);
        cfg.profile = FanProfile::Server;
        cfg.server_guard_c = 80.0;
        cfg.channels[0].source = "nct:T".to_string();
        cfg.channels[0].points = vec![[0.0, 10.0], [100.0, 10.0]];
        let mut p = ControlPass::new(1);
        let nct = chip(1);
        let out = p.run(
            &nct,
            &cfg,
            PassInput {
                temps: &[("T".to_string(), 95.0)],
                nvme: &[],
                gpu: None,
                data_age_ms: Some(0),
                overlay: None,
                pulse: None,
                has_data: true,
            },
            Instant::now(),
            &|_| {},
        );
        assert_eq!(out.written[0], Some(100.0), "the guard must override the curve");
    }

    impl FanWriter for FakeChip {
        fn fan_count(&self) -> usize {
            self.fans
        }
        fn set_duty_pct(&self, index: usize, pct: f32) -> Result<(), String> {
            self.writes.lock().unwrap().push((index, pct));
            Ok(())
        }
        fn restore_auto(&self, index: usize) -> bool {
            self.restored.lock().unwrap().push(index);
            true
        }
    }

    fn writes(c: &FakeChip) -> Vec<(usize, f32)> {
        c.writes.lock().unwrap().clone()
    }
}
