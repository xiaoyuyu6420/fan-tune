//! Fan-control algorithm engine.
//!
//! One [`ChannelState`] per channel persists across control-loop ticks; the
//! caller passes `now` in, so slew-rate timing is testable and never depends on
//! a hidden clock. Per channel the evaluation order is:
//! source combining -> EMA -> curve -> hysteresis -> slew-rate limit ->
//! spin-up boost -> clamp to 0..100.
//!
//! Only `FanMode::Curve` channels are evaluated. Mode dispatch stays with the
//! caller (Manual writes `manual_duty`, Auto stays untouched), and so does the
//! no-source safety counter: `None` in the returned vector means "no target
//! this tick" and callers must keep counting it as a failure.

use std::time::{Duration, Instant};

use crate::config::{eval_curve, AppConfig, FanChannelConfig, FanMode, FanProfile, SourceCombine};

/// How long the spin-up boost pins the output at 100%.
const SPIN_UP_MS: u64 = 800;
/// Duty below which a channel counts as stopped (spin-up trigger).
const SPIN_UP_BELOW: f32 = 20.0;

/// Duty pinned on every curve channel once the temperature sources are judged
/// dead — "blow, don't cook". One definition for both controllers (the GUI and
/// the daemon used to carry their own copies).
pub const SAFE_DUTY: f32 = 70.0;
/// Consecutive failed control passes before [`SAFE_DUTY`] engages.
pub const SOURCE_FAIL_LIMIT: u32 = 5;
/// Budget mode: how far past `budget_c` the temperature may go before the
/// closed loop is abandoned and the duty is pinned at 100% *bypassing* the
/// slew limiter. The loop is gentle by design — this is its hard backstop.
pub const EMERGENCY_MARGIN_C: f32 = 8.0;

/// Budget mode probing (the "learn the minimum that works" half of the loop):
/// with the temperature parked inside the band, the duty walks down 2% at a
/// time. A failed probe (temperature leaves the band upwards) doubles the
/// cooldown, so a fan sitting at its floor is probed rarely; a surviving probe
/// resets it, so an oversupplied fan keeps getting quieter.
const PROBE_STABLE_S: f32 = 180.0;
const PROBE_STEP: f32 = 2.0;
const PROBE_COOLDOWN_MIN_S: f32 = 300.0;
const PROBE_COOLDOWN_MAX_S: f32 = 1800.0;

/// Budget-mode decision layer: where the curve maps temperature to an absolute
/// duty, this holds the temperature inside `[budget_c - band, budget_c]`.
///
/// The output is a *direction* — never an absolute level:
/// - over the budget  → 100.0 (the slew limiter decides how fast it gets there,
///   so `ramp_up_pct_s` becomes the response-speed knob),
/// - under the band   → 0.0 (same story with `ramp_down_pct_s`),
/// - inside the band  → hold (= `last_duty`).
///
/// `band` is `hysteresis_c`: inside it the temperature is allowed to live for
/// free. The loop is silent instead of perfect — only leaving the band buys a
/// duty change, which is what keeps it from oscillating around the line.
///
/// An unconfigured budget (`0` / non-finite) falls back to the curve: a channel
/// switched to Budget without a value must not stop the fan.
fn budget_decide(c: &FanChannelConfig, temp_eff: f32, last_duty: f32) -> f32 {
    if !(c.budget_c.is_finite() && c.budget_c > 0.0) {
        return eval_curve(&c.points, temp_eff);
    }
    let band = if c.hysteresis_c.is_finite() {
        c.hysteresis_c.max(0.0)
    } else {
        0.0
    };
    if temp_eff > c.budget_c {
        100.0
    } else if temp_eff < c.budget_c - band {
        0.0
    } else {
        last_duty
    }
}

/// Budget-mode probing: with the temperature parked inside the band, walk the
/// duty down one [`PROBE_STEP`] at a time and remember (by behaviour, not by
/// bookkeeping) where it stops being enough.
///
/// - a probe that overshoots stops itself and doubles the cooldown — the fan
///   settles near its floor and is bothered rarely;
/// - a probe that survives resets the cooldown — an oversupplied fan keeps
///   getting quieter until it finds the floor.
///
/// Adaptive by construction: dust build-up or a warmer room makes probes fail
/// sooner (the floor rises), a cooler season makes them succeed (it falls).
/// Probing only shapes the *quiet* side — an over-budget reading always keeps
/// `base` (the add-air decision) untouched.
fn budget_probe(
    c: &FanChannelConfig,
    st: &mut ChannelState,
    temp_eff: f32,
    base: f32,
    now: Instant,
) -> f32 {
    if !(c.budget_c.is_finite() && c.budget_c > 0.0) {
        return base;
    }
    let band = if c.hysteresis_c.is_finite() {
        c.hysteresis_c.max(0.0)
    } else {
        0.0
    };
    let over = temp_eff > c.budget_c;
    let inside = temp_eff >= c.budget_c - band && !over;

    // A probe is running: steer it, judge it, or finish it.
    if let Some(from) = st.probe_from {
        let target = from - PROBE_STEP;
        if over {
            // Failed: the duty we came from was the last sufficient one. Stop
            // probing and back off exponentially; adding air stays `base`'s job.
            st.probe_from = None;
            let interval = st.probe_interval_s.max(PROBE_COOLDOWN_MIN_S);
            st.probe_interval_s = (interval * 2.0).min(PROBE_COOLDOWN_MAX_S);
            st.next_probe_at = Some(now + Duration::from_secs_f32(st.probe_interval_s));
            return base;
        }
        if st.last_duty <= target + 0.01 {
            // Survived all the way down: the new level is the resting point.
            st.probe_from = None;
            st.probe_interval_s = PROBE_COOLDOWN_MIN_S;
            st.next_probe_at = Some(now + Duration::from_secs_f32(st.probe_interval_s));
            return base;
        }
        if inside {
            return target; // keep descending toward the probe target
        }
        // Temperature dropped below the band mid-probe: back to the base
        // decision (which is now the back-off side) and stop the probe.
        st.probe_from = None;
        st.next_probe_at = Some(now + Duration::from_secs_f32(st.probe_interval_s.max(PROBE_COOLDOWN_MIN_S)));
        return base;
    }

    // No probe running: start one once the temperature sits inside the band,
    // the duty has been steady for a while, and the cooldown has passed.
    if inside && st.last_duty > PROBE_STEP {
        let steady = st
            .last_change_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32() >= PROBE_STABLE_S)
            .unwrap_or(false);
        let cooled = st.next_probe_at.map(|t| now >= t).unwrap_or(true);
        if steady && cooled {
            if st.probe_interval_s <= 0.0 {
                st.probe_interval_s = PROBE_COOLDOWN_MIN_S;
            }
            st.probe_from = Some(st.last_duty);
            return st.last_duty - PROBE_STEP;
        }
    }
    base
}

/// Hottest reading among a channel's configured sources — the number the
/// server-mode thermal guard watches (max even when combine = Avg: the guard
/// is a safety net, not an average).
pub fn channel_hottest(c: &FanChannelConfig, resolve: &dyn Fn(&str) -> Option<f32>) -> Option<f32> {
    let keys: &[String] = if c.sources.is_empty() {
        std::slice::from_ref(&c.source)
    } else {
        &c.sources
    };
    keys.iter()
        .filter_map(|k| resolve(k))
        .filter(|v| v.is_finite())
        .fold(None, |a: Option<f32>, b| Some(a.map_or(b, |x| x.max(b))))
}

/// Server-mode thermal guard: returns true while a channel must be forced to
/// 100% — its own hottest source at or above `guard_c`, latched until the
/// reading falls 5 °C below, so a temperature hovering at the line cannot make
/// the fan flap.
#[derive(Clone, Debug, Default)]
pub struct ServerGuard {
    latched: Vec<bool>,
}

impl ServerGuard {
    /// `hottest` = the channel's combined source reading (None = nothing
    /// resolved this tick; the guard is skipped).
    pub fn forced(&mut self, index: usize, guard_c: f32, hottest: Option<f32>) -> bool {
        if self.latched.len() <= index {
            self.latched.resize(index + 1, false);
        }
        let latched = &mut self.latched[index];
        if guard_c.is_finite() && guard_c > 0.0 {
            if let Some(t) = hottest {
                if t >= guard_c {
                    *latched = true;
                } else if t <= guard_c - 5.0 {
                    *latched = false;
                }
            }
        } else {
            *latched = false;
        }
        *latched
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChannelState {
    /// Last duty the engine produced — or the last external write recorded via
    /// [`Engine::note_write`]; the basis for slew-rate limiting.
    pub last_duty: f32,
    /// Budget mode: the duty the running downward probe started from
    /// (`None` = not probing).
    pub probe_from: Option<f32>,
    /// Budget mode: cooldown between probes in seconds (exponential backoff).
    pub probe_interval_s: f32,
    /// Budget mode: earliest time the next probe may start.
    pub next_probe_at: Option<Instant>,
    /// Budget mode: when the duty last changed — the stability clock a probe
    /// waits on, so it never fights the loop while the duty is still moving.
    pub last_change_at: Option<Instant>,
    /// Smoothed combined temperature; `None` until the first valid reading.
    pub ema: Option<f32>,
    /// Temperature the current duty level was re-anchored at (hysteresis).
    pub anchor_temp: Option<f32>,
    /// While in the future, output stays pinned at 100%.
    pub spin_until: Option<Instant>,
    /// Timestamp of the last evaluation, for the slew-rate dt.
    pub last_tick: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
pub struct Engine {
    pub states: Vec<ChannelState>,
}

impl Engine {
    pub fn new(n: usize) -> Self {
        Engine {
            states: (0..n).map(|_| ChannelState::default()).collect(),
        }
    }

    /// Targets for every channel in `cfg.channels`, indexed like the config.
    ///
    /// `None` means the engine has no target for that slot this tick: the mode
    /// is not `Curve`, or no source resolved (all missing / non-finite).
    /// `resolve` maps a config source key ("nct:NAME" / "nvme:IDX") to a
    /// temperature; anything it does not know must return `None`.
    pub fn targets(
        &mut self,
        cfg: &AppConfig,
        resolve: &dyn Fn(&str) -> Option<f32>,
        now: Instant,
    ) -> Vec<Option<f32>> {
        // config can grow on hot reload; state grows with it and never shrinks
        if self.states.len() < cfg.channels.len() {
            self.states.resize_with(cfg.channels.len(), ChannelState::default);
        }
        cfg.channels
            .iter()
            .enumerate()
            .map(|(i, c)| self.channel_target(i, c, cfg, resolve, now))
            .collect()
    }

    /// Record a duty written outside the engine (overlay, safe mode) so
    /// slew-rate limiting resumes from what the fan actually got.
    pub fn note_write(&mut self, index: usize, duty: f32, now: Instant) {
        if index >= self.states.len() {
            self.states.resize_with(index + 1, ChannelState::default);
        }
        let state = &mut self.states[index];
        // The write was not temperature-driven (overlay / safe mode / manual),
        // so the old anchor no longer describes this duty level: drop it, like
        // the spin-up expiry path does. Otherwise hysteresis would hold the
        // externally written duty until the temperature falls `hysteresis_c`
        // below the stale anchor, ignoring the curve for that whole time.
        state.anchor_temp = None;
        state.last_duty = duty.clamp(0.0, 100.0);
        state.last_tick = Some(now);
    }

    fn channel_target(
        &mut self,
        index: usize,
        c: &FanChannelConfig,
        cfg: &AppConfig,
        resolve: &dyn Fn(&str) -> Option<f32>,
        now: Instant,
    ) -> Option<f32> {
        if !matches!(c.mode, FanMode::Curve | FanMode::Budget) {
            return None;
        }

        // sources: explicit multi-source list, or the legacy single source
        let keys: &[String] = if c.sources.is_empty() {
            std::slice::from_ref(&c.source)
        } else {
            &c.sources
        };
        let mut vals: Vec<f32> = Vec::new();
        for key in keys {
            if let Some(v) = resolve(key) {
                if v.is_finite() {
                    vals.push(v);
                }
            }
        }
        if vals.is_empty() {
            return None; // no usable source this tick
        }
        let raw = match c.combine {
            SourceCombine::Max => vals.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            SourceCombine::Avg => vals.iter().sum::<f32>() / vals.len() as f32,
        };

        let state = &mut self.states[index];

        // Server profile pins the response-rate parameters to very stable
        // values: the curve still follows the temperature (cooling stays
        // adequate) but the duty can barely move per tick and never
        // oscillates. User values win when they are the more conservative ones.
        let server = matches!(cfg.profile, FanProfile::Server);

        // --- EMA (alpha < 1 mixes in the previous value) ---
        let alpha = if c.ema_alpha.is_finite() {
            let a = c.ema_alpha.clamp(0.0, 1.0);
            if server {
                a.min(0.25)
            } else {
                a
            }
        } else {
            1.0
        };
        let temp = if alpha >= 1.0 {
            raw
        } else {
            let prev = state.ema.unwrap_or(raw);
            let v = alpha * raw + (1.0 - alpha) * prev;
            state.ema = Some(v);
            v
        };

        // Gaming profile: evaluate the curve as if it were
        // `gaming_temp_offset` °C warmer — the ramp engages earlier when heat
        // soaks into the case, while the idle end of the curve stays exactly
        // as quiet as Standard.
        let eff_temp = if matches!(cfg.profile, FanProfile::Gaming) {
            let off = if cfg.gaming_temp_offset.is_finite() {
                cfg.gaming_temp_offset.clamp(0.0, 20.0)
            } else {
                0.0
            };
            temp + off
        } else {
            temp
        };
        // Budget mode swaps the decision layer; everything downstream (the
        // hysteresis bypass below, the slew limiter, spin-up, clamp) is shared.
        // Past `budget_c + EMERGENCY_MARGIN_C` the gentle loop is off the table.
        let emergency = matches!(c.mode, FanMode::Budget)
            && c.budget_c.is_finite()
            && c.budget_c > 0.0
            && eff_temp > c.budget_c + EMERGENCY_MARGIN_C;
        let curve_duty = if matches!(c.mode, FanMode::Budget) {
            let base = budget_decide(c, eff_temp, state.last_duty);
            budget_probe(c, state, eff_temp, base, now)
        } else {
            eval_curve(&c.points, eff_temp)
        };

        // A boost that just ended was not temperature-driven: drop the old
        // anchor so the curve resumes instead of holding the boosted duty.
        if let Some(until) = state.spin_until {
            if now >= until {
                state.spin_until = None;
                state.anchor_temp = None;
            }
        }

        // --- hysteresis: rises follow at once, falls wait for anchor - h ---
        let mut hyst = if c.hysteresis_c.is_finite() {
            c.hysteresis_c.max(0.0)
        } else {
            0.0
        };
        if server {
            hyst = hyst.max(3.0);
        }
        // Budget mode carries its own hold band (the band lives inside
        // `budget_decide`) — stacking the curve hysteresis on top would make
        // two deadbands fight and stall the descent.
        if matches!(c.mode, FanMode::Budget) {
            hyst = 0.0;
        }
        // Anchor moves are deferred until after the slew limiter: a down move
        // the limiter truncates has not actually reached the curve value yet,
        // so re-anchoring here would strand the output above the curve until
        // the temperature falls another `hyst` below the *new* anchor.
        let mut anchor_commit: Option<f32> = None;
        let mut duty = if hyst <= 0.0 {
            state.anchor_temp = Some(temp);
            curve_duty
        } else {
            match state.anchor_temp {
                Some(anchor) if curve_duty < state.last_duty => {
                    if temp <= anchor - hyst {
                        anchor_commit = Some(temp);
                        curve_duty
                    } else {
                        state.last_duty // hold the current level
                    }
                }
                _ => {
                    anchor_commit = Some(temp);
                    curve_duty
                }
            }
        };

        // --- slew-rate limit (0 = unlimited); no history = no limit ---
        let dt = state
            .last_tick
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0);
        if dt > 0.0 {
            let mut up = if c.ramp_up_pct_s.is_finite() {
                c.ramp_up_pct_s.max(0.0)
            } else {
                0.0
            };
            let mut down = if c.ramp_down_pct_s.is_finite() {
                c.ramp_down_pct_s.max(0.0)
            } else {
                0.0
            };
            if server {
                // 0 = unlimited in Standard; Server turns that into 3 %/s
                up = if up <= 0.0 { 3.0 } else { up.min(3.0) };
                down = if down <= 0.0 { 3.0 } else { down.min(3.0) };
            }
            if duty > state.last_duty && up > 0.0 {
                duty = duty.min(state.last_duty + up * dt);
            } else if duty < state.last_duty && down > 0.0 {
                duty = duty.max(state.last_duty - down * dt);
            }
        }

        // Commit a deferred anchor only once the output has actually settled on
        // the curve value. A rate-limited descent (duty still above the curve)
        // keeps the previous anchor, so the next tick keeps releasing instead
        // of freezing at the truncated duty.
        if let Some(t) = anchor_commit {
            if duty <= curve_duty {
                state.anchor_temp = Some(t);
            }
        }

        // Budget emergency: past the hard margin, pin 100% without waiting for
        // the ramp — the closed loop is gentle by design, this is the backstop.
        if emergency {
            duty = 100.0;
        }

        // --- spin-up: kick from standstill, then hold for SPIN_UP_MS ---
        if state.spin_until.is_some() {
            duty = 100.0;
        } else if c.spin_up && state.last_duty < SPIN_UP_BELOW && duty >= SPIN_UP_BELOW {
            state.spin_until = Some(now + Duration::from_millis(SPIN_UP_MS));
            duty = 100.0;
        }

        let duty = duty.clamp(0.0, 100.0);
        if (duty - state.last_duty).abs() > 0.01 {
            state.last_change_at = Some(now); // stability clock for probing
        }
        state.last_duty = duty;
        state.last_tick = Some(now);
        Some(duty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, FanChannelConfig, FanMode, FanProfile, SourceCombine};

    const MS: Duration = Duration::from_millis(1);

    fn chan(points: Vec<[f32; 2]>) -> FanChannelConfig {
        FanChannelConfig {
            enabled: true,
            name: "test".into(),
            label: String::new(),
            mode: FanMode::Curve,
            manual_duty: 50.0,
            source: "nct:T".into(),
            points,
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

    fn app(chan: FanChannelConfig) -> AppConfig {
        AppConfig {
            channels: vec![chan],
            interval_ms: 2000,
            ..Default::default()
        }
    }

    /// straight line: duty == temperature
    fn linear() -> Vec<[f32; 2]> {
        vec![[0.0, 0.0], [100.0, 100.0]]
    }

    fn one(v: f32) -> impl Fn(&str) -> Option<f32> {
        move |_| Some(v)
    }

    fn duty_ok(got: Option<f32>, want: f32) -> bool {
        matches!(got, Some(v) if (v - want).abs() < 0.01)
    }

    #[test]
    fn curve_basic() {
        let cfg = app(chan(vec![[40.0, 40.0], [70.0, 100.0]]));
        let mut e = Engine::new(1);
        let out = e.targets(&cfg, &one(55.0), Instant::now());
        assert_eq!(out.len(), 1);
        assert!(duty_ok(out[0], 70.0), "{out:?}");
    }

    #[test]
    fn sources_max_and_avg() {
        let mut c = chan(linear());
        c.sources = vec!["a".into(), "b".into()];
        let resolve = |k: &str| match k {
            "a" => Some(40.0),
            "b" => Some(60.0),
            _ => None,
        };
        let mut cfg = app(c);
        let mut e = Engine::new(1);
        assert!(duty_ok(e.targets(&cfg, &resolve, Instant::now())[0], 60.0));
        cfg.channels[0].combine = SourceCombine::Avg;
        assert!(duty_ok(e.targets(&cfg, &resolve, Instant::now())[0], 50.0));
    }

    #[test]
    fn empty_sources_fall_back_to_legacy_source() {
        let mut c = chan(linear());
        c.source = "nct:CPU".into();
        c.sources = Vec::new();
        let cfg = app(c);
        let mut e = Engine::new(1);
        let r = |k: &str| if k == "nct:CPU" { Some(42.0) } else { None };
        assert!(duty_ok(e.targets(&cfg, &r, Instant::now())[0], 42.0));
    }

    #[test]
    fn missing_sources_are_ignored() {
        let mut c = chan(linear());
        c.sources = vec!["a".into(), "gone".into()];
        let mut cfg = app(c);
        let mut e = Engine::new(1);
        let r = |k: &str| if k == "a" { Some(40.0) } else { None };
        assert!(duty_ok(e.targets(&cfg, &r, Instant::now())[0], 40.0));
        // Avg over the resolvable subset only — never over the raw key count
        cfg.channels[0].combine = SourceCombine::Avg;
        assert!(duty_ok(e.targets(&cfg, &r, Instant::now())[0], 40.0));
    }

    #[test]
    fn all_sources_missing_returns_none() {
        let c = chan(linear());
        let mut cfg = app(c);
        let mut e = Engine::new(1);
        assert_eq!(e.targets(&cfg, &|_| None, Instant::now())[0], None);
        cfg.channels[0].sources = vec!["x".into(), "y".into()];
        assert_eq!(e.targets(&cfg, &|_| None, Instant::now())[0], None);
    }

    #[test]
    fn ema_converges() {
        let mut c = chan(linear());
        c.ema_alpha = 0.5;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(20.0), t0)[0], 20.0)); // first reading seeds it
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS)[0], 30.0));
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + 2 * MS)[0], 35.0));
        assert!(duty_ok(e.states[0].ema, 35.0));
    }

    #[test]
    fn hysteresis_holds_small_drops() {
        let mut c = chan(linear());
        c.hysteresis_c = 5.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0)[0], 50.0)); // anchor 50
        assert!(duty_ok(e.targets(&cfg, &one(55.0), t0 + MS)[0], 55.0)); // rise: follow, anchor 55
        assert!(duty_ok(e.targets(&cfg, &one(52.0), t0 + 2 * MS)[0], 55.0)); // 52 > 55-5: hold
        assert!(duty_ok(e.targets(&cfg, &one(49.0), t0 + 3 * MS)[0], 49.0)); // 49 <= 50: release
        assert!(duty_ok(e.targets(&cfg, &one(47.0), t0 + 4 * MS)[0], 49.0)); // new anchor 49, hold
    }

    #[test]
    fn hysteresis_zero_follows_immediately() {
        let cfg = app(chan(linear())); // hysteresis_c = 0
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(60.0), t0)[0], 60.0));
        assert!(duty_ok(e.targets(&cfg, &one(38.0), t0 + MS)[0], 38.0));
    }

    #[test]
    fn ramp_up_limits_duty_rise() {
        let mut c = chan(linear());
        c.ramp_up_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0)[0], 50.0)); // no history: free
        assert!(duty_ok(e.targets(&cfg, &one(80.0), t0 + MS * 1000)[0], 60.0));
        assert!(duty_ok(e.targets(&cfg, &one(80.0), t0 + MS * 2000)[0], 70.0));
        assert!(duty_ok(e.targets(&cfg, &one(80.0), t0 + MS * 3000)[0], 80.0));
    }

    #[test]
    fn ramp_down_limits_duty_fall() {
        let mut c = chan(linear());
        c.ramp_down_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(80.0), t0)[0], 80.0));
        assert!(duty_ok(e.targets(&cfg, &one(20.0), t0 + MS * 1000)[0], 70.0));
        assert!(duty_ok(e.targets(&cfg, &one(20.0), t0 + MS * 2000)[0], 60.0));
    }

    #[test]
    fn ramp_zero_is_unlimited() {
        let cfg = app(chan(linear())); // both ramps 0
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(20.0), t0)[0], 20.0));
        assert!(duty_ok(e.targets(&cfg, &one(90.0), t0 + MS)[0], 90.0));
    }

    #[test]
    fn note_write_resumes_ramp_from_actual_duty() {
        let mut c = chan(linear());
        c.ramp_down_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        e.note_write(0, 100.0, t0); // e.g. overlay wrote full speed
        assert!(duty_ok(e.targets(&cfg, &one(20.0), t0 + MS * 1000)[0], 90.0));
    }

    #[test]
    fn spin_up_boosts_then_expires() {
        let mut c = chan(linear());
        c.spin_up = true;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0)[0], 100.0)); // 0 -> 50 triggers
        assert!(e.states[0].spin_until.is_some());
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0 + MS * 400)[0], 100.0)); // held
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0 + MS * 800)[0], 50.0)); // expired
        assert!(e.states[0].spin_until.is_none());
    }

    #[test]
    fn spin_up_ignores_low_targets() {
        let mut c = chan(linear());
        c.spin_up = true;
        let cfg = app(c);
        let mut e = Engine::new(1);
        assert!(duty_ok(e.targets(&cfg, &one(10.0), Instant::now())[0], 10.0));
        assert!(e.states[0].spin_until.is_none());
    }

    #[test]
    fn spin_expiry_reanchors_hysteresis() {
        let mut c = chan(linear());
        c.spin_up = true;
        c.hysteresis_c = 5.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0)[0], 100.0)); // boost
        // the boost was not temperature-driven: right after it ends the curve
        // value must resume instead of being held by hysteresis
        assert!(duty_ok(e.targets(&cfg, &one(50.0), t0 + MS * 800)[0], 50.0));
    }

    /// Regression: a hysteresis down-release truncated by the slew limiter must
    /// keep descending on later ticks. Writing the new anchor at release time
    /// stranded the output above the curve until the temperature fell another
    /// `hysteresis_c` below the *new* threshold.
    #[test]
    fn hysteresis_release_continues_through_rate_limit() {
        let mut c = chan(linear());
        c.hysteresis_c = 5.0;
        c.ramp_down_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(60.0), t0)[0], 60.0)); // anchor 60
        // temp crashes: release is allowed (25 <= 55) but the limiter truncates
        assert!(duty_ok(e.targets(&cfg, &one(25.0), t0 + MS * 2000)[0], 40.0));
        // the anchor must not have moved to 25: the descent continues
        assert!(duty_ok(e.targets(&cfg, &one(25.0), t0 + MS * 4000)[0], 25.0));
        // and holds once the curve value is reached
        assert!(duty_ok(e.targets(&cfg, &one(25.0), t0 + MS * 6000)[0], 25.0));
    }

    /// Regression: an external write (overlay / safe mode) is not a
    /// temperature-driven level, so hysteresis must not keep holding it after
    /// the write path clears. `note_write` has to drop the stale anchor the
    /// same way the spin-up expiry path does.
    #[test]
    fn note_write_drops_stale_hysteresis_anchor() {
        let mut c = chan(linear());
        c.hysteresis_c = 5.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0)[0], 40.0)); // anchor 40
        e.note_write(0, 100.0, t0 + MS); // e.g. full-speed overlay
        // overlay released: curve resumes instead of holding the overlay level
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 2)[0], 40.0));
    }

    /// Regression: after an external write, a configured ramp-down must be
    /// allowed to walk back to the curve. Clearing the anchor alone is not
    /// enough — re-anchoring while the limiter still truncates re-arms the
    /// same trap on the very next tick.
    #[test]
    fn overlay_release_ramps_back_to_curve() {
        let mut c = chan(linear());
        c.hysteresis_c = 5.0;
        c.ramp_down_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        e.note_write(0, 100.0, t0); // full-speed overlay, then released
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 1000)[0], 90.0));
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 2000)[0], 80.0));
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 6000)[0], 40.0));
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 7000)[0], 40.0));
    }

    /// Regression (non-monotonic curve): the same truncated-release trap bites
    /// when the temperature turns back up into a falling curve segment — the
    /// stale anchor from the truncated release used to hold the duty.
    #[test]
    fn hysteresis_release_continues_on_non_monotonic_curve() {
        let mut c = chan(vec![[30.0, 30.0], [50.0, 25.0], [70.0, 80.0], [85.0, 100.0]]);
        c.hysteresis_c = 5.0;
        c.ramp_down_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(60.0), t0)[0], 52.5)); // anchor 60
        // crash to 30: release truncated by the limiter (max(30, 52.5-20))
        assert!(duty_ok(e.targets(&cfg, &one(30.0), t0 + MS * 2000)[0], 32.5));
        // temp turns back up into the falling segment: keep following the curve
        assert!(duty_ok(e.targets(&cfg, &one(45.0), t0 + MS * 4000)[0], 26.25));
    }

    #[test]
    fn non_curve_modes_return_none() {
        let mut c = chan(linear());
        c.mode = FanMode::Manual;
        let mut cfg = app(c);
        let mut e = Engine::new(1);
        assert_eq!(e.targets(&cfg, &one(50.0), Instant::now())[0], None);
        cfg.channels[0].mode = FanMode::Auto;
        assert_eq!(e.targets(&cfg, &one(50.0), Instant::now())[0], None);
    }

    #[test]
    fn engine_grows_with_config_len() {
        let cfg = AppConfig {
            channels: vec![chan(linear()), chan(linear()), chan(linear())],
            interval_ms: 2000,
            ..Default::default()
        };
        let mut e = Engine::new(1);
        let out = e.targets(&cfg, &one(50.0), Instant::now());
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| duty_ok(*v, 50.0)));
        assert_eq!(e.states.len(), 3);
    }

    /// Gaming evaluates the curve as if it were `offset` °C warmer: the ramp
    /// engages earlier, but a curve with an idle floor stays exactly as quiet
    /// at low temperatures (that is the whole point of a temperature offset
    /// instead of a duty bump).
    #[test]
    fn gaming_profile_shifts_the_warm_end_only() {
        // the case-fan shape in use: floor 45% up to 42 °C, 90% at 62 °C
        let c = chan(vec![[42.0, 45.0], [62.0, 90.0]]);
        let cfg_std = app(c);
        let mut game = cfg_std.clone();
        game.profile = FanProfile::Gaming;
        game.gaming_temp_offset = 5.0;

        let t0 = Instant::now();
        // 35 °C → standard floor 45; gaming evaluates at 40 → still the floor
        let mut e1 = Engine::new(1);
        assert!(duty_ok(e1.targets(&cfg_std, &one(35.0), t0)[0], 45.0));
        let mut e2 = Engine::new(1);
        assert!(duty_ok(e2.targets(&game, &one(35.0), t0)[0], 45.0));
        // 45 °C → standard 51.75; gaming evaluates at 50 → 63
        let mut e3 = Engine::new(1);
        assert!(duty_ok(e3.targets(&cfg_std, &one(45.0), t0)[0], 51.75));
        let mut e4 = Engine::new(1);
        assert!(duty_ok(e4.targets(&game, &one(45.0), t0)[0], 63.0));
    }

    /// Server keeps following the curve but caps the response rate: ramps the
    /// user left unlimited become 3 %/s, so a big step takes several ticks
    /// instead of one jump.
    #[test]
    fn server_profile_caps_response_rate() {
        let mut cfg = app(chan(linear()));
        cfg.profile = FanProfile::Server;
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(20.0), t0)[0], 20.0)); // seed
        // jump to 90: unlimited in Standard, capped at 3 %/s here
        assert!(duty_ok(e.targets(&cfg, &one(90.0), t0 + MS * 1000)[0], 23.0));
        assert!(duty_ok(e.targets(&cfg, &one(90.0), t0 + MS * 2000)[0], 26.0));
    }

    #[test]
    fn server_guard_forces_full_and_releases_below_the_band() {
        let mut g = ServerGuard::default();
        assert!(!g.forced(0, 85.0, Some(40.0)));
        assert!(g.forced(0, 85.0, Some(85.0))); // at the threshold
        assert!(g.forced(0, 85.0, Some(83.0))); // latched (83 > 80)
        assert!(!g.forced(0, 85.0, Some(80.0))); // released at guard-5
        assert!(!g.forced(0, 85.0, None)); // no source: no force
        assert!(!g.forced(0, 0.0, Some(99.0))); // guard off
    }

    #[test]
    fn channel_hottest_uses_max_across_sources() {
        let mut c = chan(linear());
        c.sources = vec!["a".into(), "b".into()];
        let r = |k: &str| match k {
            "a" => Some(40.0),
            "b" => Some(62.0),
            _ => None,
        };
        assert_eq!(channel_hottest(&c, &r), Some(62.0));
        // legacy single-source fallback
        c.sources = Vec::new();
        c.source = "nct:CPU".into();
        let r2 = |k: &str| if k == "nct:CPU" { Some(48.0) } else { None };
        assert_eq!(channel_hottest(&c, &r2), Some(48.0));
        assert_eq!(channel_hottest(&c, &|_| None), None);
    }

    // ---------- budget mode ----------

    fn budget_chan(budget: f32, band: f32) -> FanChannelConfig {
        let mut c = chan(linear());
        c.mode = FanMode::Budget;
        c.budget_c = budget;
        c.hysteresis_c = band;
        c
    }

    /// Inside the band the duty holds exactly where it is: the temperature may
    /// live anywhere in `[budget - band, budget]` for free.
    #[test]
    fn budget_holds_inside_band() {
        let cfg = app(budget_chan(60.0, 5.0)); // band = [55, 60]
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(65.0), t0)[0], 100.0)); // over -> 100
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + MS)[0], 100.0)); // in band: hold
        assert!(duty_ok(e.targets(&cfg, &one(56.0), t0 + MS * 2)[0], 100.0)); // still holding
        assert!(duty_ok(e.targets(&cfg, &one(54.0), t0 + MS * 3)[0], 0.0)); // below -> fall
    }

    /// Over the budget the duty climbs, but the slew limiter paces it —
    /// `ramp_up_pct_s` is the response-speed knob.
    #[test]
    fn budget_ramp_up_paces_the_climb() {
        let mut c = budget_chan(60.0, 5.0);
        c.ramp_up_pct_s = 10.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0)[0], 0.0)); // seed below the band
        // 65 is over the budget but under the emergency line (60 + 8): the ramp
        // paces the climb instead of the emergency pinning 100 at once.
        assert!(duty_ok(e.targets(&cfg, &one(65.0), t0 + MS * 1000)[0], 10.0));
        assert!(duty_ok(e.targets(&cfg, &one(65.0), t0 + MS * 2000)[0], 20.0));
    }

    /// Under the band the duty backs off, paced by `ramp_down_pct_s` — slow by
    /// design: backing off is what makes it quiet, so it must not flap.
    #[test]
    fn budget_ramp_down_paces_the_backoff() {
        let mut c = budget_chan(60.0, 5.0);
        c.ramp_down_pct_s = 2.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(70.0), t0)[0], 100.0)); // seed over -> 100
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 1000)[0], 98.0));
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0 + MS * 2000)[0], 96.0));
    }

    /// Temperature jitter around the budget line must not move the duty: the
    /// band absorbs it below the line, and over the line only a rise is asked
    /// for — never a drop. This is the anti-oscillation guarantee.
    #[test]
    fn budget_is_silent_around_the_line() {
        let cfg = app(budget_chan(60.0, 4.0)); // band = [56, 60]
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(70.0), t0)[0], 100.0)); // climb to 100
        for (k, t) in [59.0f32, 61.0, 58.0, 62.0].iter().enumerate() {
            assert!(
                duty_ok(e.targets(&cfg, &one(*t), t0 + MS * (k as u32 + 1))[0], 100.0),
                "jitter at {t} moved the duty"
            );
        }
    }

    /// Past `budget + EMERGENCY_MARGIN_C` the ramp is bypassed: 100% at once.
    #[test]
    fn budget_emergency_bypasses_the_ramp() {
        let mut c = budget_chan(60.0, 5.0);
        c.ramp_up_pct_s = 1.0; // would take 100 s to reach 100% the polite way
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(40.0), t0)[0], 0.0)); // seed
        // 69 > 60 + 8: emergency pins 100 immediately, ignoring the limiter
        assert!(duty_ok(e.targets(&cfg, &one(69.0), t0 + MS)[0], 100.0));
    }

    /// A Budget channel without a configured value must not stop the fan: it
    /// falls back to the curve.
    #[test]
    fn budget_unset_falls_back_to_curve() {
        let mut c = chan(vec![[40.0, 40.0], [70.0, 100.0]]);
        c.mode = FanMode::Budget;
        c.budget_c = 0.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        assert!(duty_ok(e.targets(&cfg, &one(55.0), Instant::now())[0], 70.0));
    }

    /// Multi-source: combine runs first, the budget compares the combined
    /// reading — with max, the hottest source owns the budget.
    #[test]
    fn budget_uses_combined_sources() {
        let mut c = budget_chan(60.0, 5.0);
        c.sources = vec!["a".into(), "b".into()];
        let r = |k: &str| match k {
            "a" => Some(50.0),
            "b" => Some(70.0),
            _ => None,
        };
        let cfg = app(c);
        let mut e = Engine::new(1);
        assert!(duty_ok(e.targets(&cfg, &r, Instant::now())[0], 100.0)); // max 70 > 60
    }

    // ---------- budget probing (stage 2) ----------

    const SEC: Duration = Duration::from_secs(1);

    /// A steady fan parked inside the band probes one step down after the
    /// stability window; an over-budget reading never starts a probe.
    #[test]
    fn budget_probe_descends_a_steady_fan() {
        let mut c = budget_chan(60.0, 5.0);
        c.ramp_down_pct_s = 100.0; // land the probe immediately
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        // Climb to 100 (over the budget but under the emergency line, 60 + 8).
        assert!(duty_ok(e.targets(&cfg, &one(65.0), t0)[0], 100.0));
        // Over-budget ticks never start a probe.
        assert!(e.states[0].probe_from.is_none());
        // Park inside the band: the duty holds, no probe yet (not steady).
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + SEC)[0], 100.0));
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + SEC * 100)[0], 100.0));
        // Past the 180 s stability window the probe fires: one step down.
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + SEC * 200)[0], 98.0));
        assert!(e.states[0].probe_from.is_some());
    }

    /// A failed probe stops, doubles the cooldown, and never blocks adding air.
    #[test]
    fn budget_probe_failure_backs_off() {
        let mut c = budget_chan(60.0, 5.0);
        c.ramp_down_pct_s = 100.0;
        let cfg = app(c);
        let mut e = Engine::new(1);
        let t0 = Instant::now();
        assert!(duty_ok(e.targets(&cfg, &one(65.0), t0)[0], 100.0)); // climb
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + SEC * 200)[0], 98.0)); // probe 1 → 98
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + SEC * 201)[0], 98.0)); // probe 1 done
        assert_eq!(e.states[0].probe_interval_s, PROBE_COOLDOWN_MIN_S);
        // Cooldown passed, still steady: probe 2 → 96.
        assert!(duty_ok(e.targets(&cfg, &one(57.0), t0 + SEC * 600)[0], 96.0));
        // The temperature leaves the band upwards mid-probe: judged failed,
        // the cooldown doubles, and the add-air decision goes through.
        assert!(duty_ok(e.targets(&cfg, &one(63.0), t0 + SEC * 601)[0], 100.0));
        assert_eq!(e.states[0].probe_interval_s, PROBE_COOLDOWN_MIN_S * 2.0);
        assert!(e.states[0].probe_from.is_none());
    }
}
