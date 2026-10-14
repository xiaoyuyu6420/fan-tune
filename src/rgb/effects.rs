//! Segments, strategies and the colour pipeline.
//!
//! A *segment* is a run of LEDs on one controller output, described by what it
//! physically is and how it is wired ([`Shape`], [`Segmentation::phase`],
//! [`Diffusion`]) instead of by raw LED indices. Effects are written against
//! that description, so the same "temperature level" fills a ring from the
//! bottom or a strip from the left without a second implementation.
//!
//! Pipeline per segment:
//! strategy (temperatures/duty → colour intent)
//!   → geometry (phase / direction → LED order)
//!   → diffusion (how the light leaves the hardware → smooth vs wide blocks)
//!   → calibration (per-segment gain / white point / saturation)
//!   → global style (brightness / saturation)
//!
//! Nothing here talks to hardware: it is pure `segment + inputs → colours`,
//! which is what the unit tests below exercise.

use serde::{Deserialize, Serialize};

/// Physical arrangement of a segment's LEDs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// A strip, fan bar or GPU slot: position 0 is one end.
    #[default]
    Line,
    /// A fan or pump ring: position 0 is wherever [`Segment::phase`] says.
    Ring,
}

/// How the light leaves the hardware — decides how an effect should be drawn.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Diffusion {
    /// Behind a frosted ring or diffuser: smooth gradients read as one glow.
    #[default]
    Diffused,
    /// Light-guide slot (e.g. a GPU bar): neighbouring LEDs bleed into each
    /// other, so gradients turn muddy — draw few, wide blocks instead.
    Guide,
    /// Bare LEDs: keep the steps crisp and dither the transitions.
    Bare,
    /// One colour for the whole segment (12 V RGB headers).
    Solid,
}

/// What a segment shows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Strategy {
    /// Not ours: the controller keeps whatever was last written there.
    #[default]
    Off,
    /// Fixed colour.
    Static {
        color: [u8; 3],
    },
    /// Colour follows a temperature: `cold` at `t_cold`, `warm` at `t_warm`.
    TempColor {
        source: String,
        cold: [u8; 3],
        warm: [u8; 3],
        t_cold: f32,
        t_warm: f32,
    },
    /// A level that fills the segment with `warm` up to the fraction of
    /// `t_min..t_max` (bottom→top on a ring, start→end on a line).
    TempLevel {
        source: String,
        cold: [u8; 3],
        warm: [u8; 3],
        t_min: f32,
        t_max: f32,
    },
    /// Colour follows one fan channel's duty.
    DutyColor {
        channel: u8,
        low: [u8; 3],
        high: [u8; 3],
    },
    /// One colour per machine state (profile / one-click overrides / alert).
    Status {
        standard: [u8; 3],
        gaming: [u8; 3],
        server: [u8; 3],
        quiet: [u8; 3],
        full: [u8; 3],
        alert: [u8; 3],
    },
}

/// A run of LEDs on one output.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Segment {
    pub name: String,
    /// Index into [`super::controller::OUTPUTS`].
    pub output: usize,
    /// First LED of this segment inside the output's buffer.
    #[serde(default)]
    pub first_led: u16,
    pub leds: u16,
    #[serde(default)]
    pub shape: Shape,
    /// Ring only: hardware LED index that should read as position 0 (measured
    /// by the calibration wizard — "turn the dot to 12 o'clock").
    #[serde(default)]
    pub phase: u16,
    /// Travel along the segment runs the other way.
    #[serde(default)]
    pub reversed: bool,
    #[serde(default)]
    pub diffusion: Diffusion,
    #[serde(default)]
    pub strategy: Strategy,
    /// Calibration: brightness trim (1.0 = as written).
    #[serde(default = "one")]
    pub gain: f32,
    /// Calibration: per-channel white point trim.
    #[serde(default = "white")]
    pub white: [f32; 3],
    /// Calibration: saturation trim (1.0 = as written).
    #[serde(default = "one")]
    pub sat: f32,
}

fn one() -> f32 {
    1.0
}
fn white() -> [f32; 3] {
    [1.0, 1.0, 1.0]
}

impl Segment {
    pub fn new(name: &str, output: usize, leds: u16) -> Segment {
        Segment {
            name: name.to_string(),
            output,
            first_led: 0,
            leds,
            shape: Shape::Line,
            phase: 0,
            reversed: false,
            diffusion: Diffusion::Diffused,
            strategy: Strategy::Off,
            gain: 1.0,
            white: [1.0, 1.0, 1.0],
            sat: 1.0,
        }
    }
}

/// Global look: applied after the per-segment calibration, to every segment the
/// same way. This is what makes mismatched hardware read as one system —
/// matching "flavour" beats matching values (see README, RGB section).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub brightness: f32,
    pub saturation: f32,
}

impl Default for Style {
    fn default() -> Self {
        Style { brightness: 1.0, saturation: 1.0 }
    }
}

/// What the strategies are allowed to read.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    /// Resolved sensors by key: the app passes its own names through
    /// (`nct:PECI_0 (CPU)`, `nvme:0`, `gpu:0`, ...).
    pub temps: Vec<(String, f32)>,
    pub duty: [Option<f32>; 7],
    /// 0 = standard, 1 = gaming, 2 = server.
    pub profile: u8,
    pub quiet: bool,
    pub full: bool,
    pub alert: bool,
}

impl Inputs {
    pub fn temp(&self, key: &str) -> Option<f32> {
        self.temps
            .iter()
            .find(|(n, _)| n == key)
            .map(|(_, t)| *t)
            .filter(|t| t.is_finite())
    }
}

/// Why a segment could not be rendered from live data (drives the alert).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    None,
    Source,
    Duty,
}

/// Render one segment into `seg.leds` RGB triplets.
pub fn render(seg: &Segment, inputs: &Inputs, style: &Style) -> (Vec<[u8; 3]>, Missing) {
    let n = seg.leds.max(1) as usize;
    let mut missing = Missing::None;

    // --- 1. strategy: colour intent -------------------------------------------------
    let mut colors: Vec<[u8; 3]> = match &seg.strategy {
        Strategy::Off | Strategy::Static { .. } => {
            let c = match &seg.strategy {
                Strategy::Static { color } => *color,
                _ => [0, 0, 0],
            };
            vec![c; n]
        }
        Strategy::TempColor { source, cold, warm, t_cold, t_warm } => {
            let t = inputs.temp(source);
            if t.is_none() {
                missing = Missing::Source;
            }
            let t = t.unwrap_or(*t_cold);
            let span = (t_warm - t_cold).abs().max(0.01);
            let f = ((t - t_cold) / span).clamp(0.0, 1.0);
            vec![lerp(*cold, *warm, f); n]
        }
        Strategy::TempLevel { source, cold, warm, t_min, t_max } => {
            let t = inputs.temp(source);
            if t.is_none() {
                missing = Missing::Source;
            }
            let t = t.unwrap_or(*t_min);
            let span = (t_max - t_min).abs().max(0.01);
            let f = ((t - t_min) / span).clamp(0.0, 1.0);
            // how many LEDs the level covers; position 0 is the start of the
            // segment (geometry below decides where that is in space)
            let lit = (f * n as f32).round() as usize;
            (0..n)
                .map(|i| if i < lit { *warm } else { *cold })
                .collect()
        }
        Strategy::DutyColor { channel, low, high } => {
            let d = inputs.duty.get(*channel as usize).copied().flatten();
            if d.is_none() {
                missing = Missing::Duty;
            }
            let f = (d.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0);
            vec![lerp(*low, *high, f); n]
        }
        Strategy::Status { standard, gaming, server, quiet, full, alert } => {
            let c = if inputs.alert {
                *alert
            } else if inputs.full {
                *full
            } else if inputs.quiet {
                *quiet
            } else {
                match inputs.profile {
                    1 => *gaming,
                    2 => *server,
                    _ => *standard,
                }
            };
            vec![c; n]
        }
    };

    // --- 2. geometry: reorder so index 0 is the physical start --------------------
    // `phase` is the hardware LED index that should read as position 0, so the
    // array is rotated right by it: new[i] = old[(i - phase) mod n].
    if seg.shape == Shape::Ring && seg.phase > 0 {
        colors.rotate_right((seg.phase as usize) % n);
    }
    if seg.reversed {
        colors.reverse();
    }

    // --- 3. diffusion: draw what the hardware can actually show --------------------
    match seg.diffusion {
        Diffusion::Diffused => {}
        Diffusion::Guide => colors = wide_blocks(&colors, 6),
        Diffusion::Bare => colors = dither(&colors),
        Diffusion::Solid => {
            let first = colors.first().copied().unwrap_or([0, 0, 0]);
            colors = vec![first; n];
        }
    }

    // --- 4.+5. calibration, then the global style ----------------------------------
    for c in colors.iter_mut() {
        *c = calibrate(*c, seg, style);
    }

    (colors, missing)
}

/// Light guides blend neighbouring LEDs, so a per-LED gradient turns muddy:
/// collapse the gradient into a handful of wide, clean blocks.
fn wide_blocks(colors: &[[u8; 3]], blocks: usize) -> Vec<[u8; 3]> {
    let n = colors.len();
    let blocks = blocks.max(1).min(n);
    (0..n)
        .map(|i| {
            // the sample LED is the middle of the block this LED falls into
            let b = i * blocks / n;
            let mid = ((b * n) + (n / blocks) / 2).min(n - 1);
            colors[mid]
        })
        .collect()
}

/// Bare LEDs show hard steps; mixing in a neighbour value makes the transition
/// read as smoother without looking washed out.
fn dither(colors: &[[u8; 3]]) -> Vec<[u8; 3]> {
    let n = colors.len();
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                colors[i]
            } else {
                let a = colors[i];
                let b = colors.get(i + 1).copied().unwrap_or(a);
                lerp(a, b, 0.35)
            }
        })
        .collect()
}

fn calibrate(c: [u8; 3], seg: &Segment, style: &Style) -> [u8; 3] {
    let mut f = [
        c[0] as f32 * seg.white[0],
        c[1] as f32 * seg.white[1],
        c[2] as f32 * seg.white[2],
    ];
    if (seg.sat - 1.0).abs() > 0.001 {
        let gray = (f[0] + f[1] + f[2]) / 3.0;
        for v in f.iter_mut() {
            *v = gray + (*v - gray) * seg.sat;
        }
    }
    if (style.saturation - 1.0).abs() > 0.001 {
        let gray = (f[0] + f[1] + f[2]) / 3.0;
        for v in f.iter_mut() {
            *v = gray + (*v - gray) * style.saturation;
        }
    }
    let k = seg.gain * style.brightness;
    // round rather than truncate: a 0.5 gain on 255 must land on 128, not 127
    [
        (f[0] * k).round().clamp(0.0, 255.0) as u8,
        (f[1] * k).round().clamp(0.0, 255.0) as u8,
        (f[2] * k).round().clamp(0.0, 255.0) as u8,
    ]
}

pub fn lerp(a: [u8; 3], b: [u8; 3], f: f32) -> [u8; 3] {
    let f = f.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round().clamp(0.0, 255.0) as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs_with(cpu: f32, duty: Option<f32>) -> Inputs {
        Inputs {
            temps: vec![("nct:PECI_0 (CPU)".into(), cpu)],
            duty: [duty, None, None, None, None, None, None],
            ..Default::default()
        }
    }

    fn seg(strategy: Strategy, leds: u16) -> Segment {
        let mut s = Segment::new("t", 0, leds);
        s.strategy = strategy;
        s
    }

    #[test]
    fn temp_color_ramps_between_the_ends() {
        let s = seg(
            Strategy::TempColor {
                source: "nct:PECI_0 (CPU)".into(),
                cold: [0, 0, 255],
                warm: [255, 0, 0],
                t_cold: 30.0,
                t_warm: 70.0,
            },
            4,
        );
        let (c, m) = render(&s, &inputs_with(30.0, None), &Style::default());
        assert_eq!(m, Missing::None);
        assert_eq!(c[0], [0, 0, 255]);
        let (c, _) = render(&s, &inputs_with(70.0, None), &Style::default());
        assert_eq!(c[0], [255, 0, 0]);
        let (c, _) = render(&s, &inputs_with(50.0, None), &Style::default());
        assert_eq!(c[0], [128, 0, 128]);
    }

    #[test]
    fn temp_color_reports_a_missing_source_and_stays_at_the_cold_end() {
        let s = seg(
            Strategy::TempColor {
                source: "gpu:0".into(),
                cold: [0, 0, 255],
                warm: [255, 0, 0],
                t_cold: 30.0,
                t_warm: 70.0,
            },
            2,
        );
        let (c, m) = render(&s, &inputs_with(50.0, None), &Style::default());
        assert_eq!(m, Missing::Source);
        assert_eq!(c[0], [0, 0, 255]);
    }

    #[test]
    fn temp_level_fills_from_position_zero() {
        let s = seg(
            Strategy::TempLevel {
                source: "nct:PECI_0 (CPU)".into(),
                cold: [0, 0, 0],
                warm: [255, 255, 255],
                t_min: 30.0,
                t_max: 70.0,
            },
            8,
        );
        let (c, _) = render(&s, &inputs_with(50.0, None), &Style::default());
        // half the range -> half the LEDs lit, starting at index 0
        assert_eq!(c.iter().filter(|x| **x == [255, 255, 255]).count(), 4);
        assert_eq!(c[0], [255, 255, 255]);
        assert_eq!(c[7], [0, 0, 0]);
    }

    #[test]
    fn temp_level_respects_direction_and_ring_phase() {
        let mut s = seg(
            Strategy::TempLevel {
                source: "nct:PECI_0 (CPU)".into(),
                cold: [0, 0, 0],
                warm: [255, 255, 255],
                t_min: 30.0,
                t_max: 70.0,
            },
            8,
        );
        s.reversed = true;
        let (c, _) = render(&s, &inputs_with(50.0, None), &Style::default());
        assert_eq!(c[7], [255, 255, 255]); // filled from the far end
        assert_eq!(c[0], [0, 0, 0]);

        // a phase of 3 moves the physical start three LEDs along, so the lit
        // run still begins at position 0 *of the ring*
        let mut s2 = s.clone();
        s2.reversed = false;
        s2.shape = Shape::Ring;
        s2.phase = 3;
        let (c2, _) = render(&s2, &inputs_with(50.0, None), &Style::default());
        assert_eq!(c2[3], [255, 255, 255]);
        assert_eq!(c2[2], [0, 0, 0]);
    }

    #[test]
    fn duty_color_follows_the_channel() {
        let s = seg(
            Strategy::DutyColor { channel: 0, low: [0, 0, 0], high: [0, 255, 0] },
            2,
        );
        let (c, m) = render(&s, &inputs_with(40.0, Some(50.0)), &Style::default());
        assert_eq!(m, Missing::None);
        assert_eq!(c[0], [0, 128, 0]);
        let (_, m) = render(&s, &inputs_with(40.0, None), &Style::default());
        assert_eq!(m, Missing::Duty);
    }

    #[test]
    fn status_prefers_alert_over_profile() {
        let s = seg(
            Strategy::Status {
                standard: [1, 1, 1],
                gaming: [2, 2, 2],
                server: [3, 3, 3],
                quiet: [4, 4, 4],
                full: [5, 5, 5],
                alert: [9, 9, 9],
            },
            1,
        );
        let mut i = inputs_with(40.0, None);
        i.profile = 1;
        assert_eq!(render(&s, &i, &Style::default()).0[0], [2, 2, 2]);
        i.quiet = true;
        assert_eq!(render(&s, &i, &Style::default()).0[0], [4, 4, 4]);
        i.full = true;
        assert_eq!(render(&s, &i, &Style::default()).0[0], [5, 5, 5]);
        i.alert = true;
        assert_eq!(render(&s, &i, &Style::default()).0[0], [9, 9, 9]);
    }

    #[test]
    fn guide_collapses_a_gradient_into_wide_blocks() {
        // a full sweep: as a smooth gradient it has 24 distinct values, through
        // a light guide it must read as a handful of blocks
        let mut s = Segment::new("t", 0, 24);
        s.strategy = Strategy::TempLevel {
            source: "nct:PECI_0 (CPU)".into(),
            cold: [0, 0, 0],
            warm: [255, 0, 0],
            t_min: 30.0,
            t_max: 70.0,
        };
        let (smooth, _) = render(&s, &inputs_with(45.0, None), &Style::default());
        assert_eq!(distinct(&smooth), 2); // a level is a hard edge, not a gradient
        s.diffusion = Diffusion::Guide;
        let (blocky, _) = render(&s, &inputs_with(45.0, None), &Style::default());
        assert!(distinct(&blocky) <= 2, "{blocky:?}");
    }

    #[test]
    fn solid_segments_get_exactly_one_colour() {
        let mut s = seg(
            Strategy::TempColor {
                source: "nct:PECI_0 (CPU)".into(),
                cold: [10, 20, 30],
                warm: [200, 100, 0],
                t_cold: 30.0,
                t_warm: 70.0,
            },
            4,
        );
        s.shape = Shape::Line;
        s.diffusion = Diffusion::Solid;
        let (c, _) = render(&s, &inputs_with(40.0, None), &Style::default());
        assert_eq!(distinct(&c), 1);
    }

    #[test]
    fn calibration_and_style_are_applied() {
        let mut s = seg(Strategy::Static { color: [200, 200, 200] }, 1);
        s.gain = 0.5;
        let (c, _) = render(&s, &inputs_with(40.0, None), &Style::default());
        assert_eq!(c[0], [100, 100, 100]);

        // saturation 0 greys a colour out, 1 leaves it alone
        let mut s2 = seg(Strategy::Static { color: [255, 0, 0] }, 1);
        s2.sat = 0.0;
        let (c2, _) = render(&s2, &inputs_with(40.0, None), &Style::default());
        let (r, g, b) = (c2[0][0] as i32, c2[0][1] as i32, c2[0][2] as i32);
        assert!((r - g).abs() <= 1 && (g - b).abs() <= 1, "{:?}", c2[0]);

        let mut s3 = seg(Strategy::Static { color: [255, 0, 0] }, 1);
        s3.white = [0.5, 1.0, 1.0];
        let (c3, _) = render(&s3, &inputs_with(40.0, None), &Style::default());
        assert_eq!(c3[0], [128, 0, 0]);
    }

    #[test]
    fn off_segments_render_black_but_stay_the_right_length() {
        let s = seg(Strategy::Off, 3);
        let (c, m) = render(&s, &inputs_with(40.0, None), &Style::default());
        assert_eq!(c.len(), 3);
        assert!(c.iter().all(|x| *x == [0, 0, 0]));
        assert_eq!(m, Missing::None);
    }

    fn distinct(c: &[[u8; 3]]) -> usize {
        let mut v: Vec<[u8; 3]> = c.to_vec();
        v.sort();
        v.dedup();
        v.len()
    }
}
