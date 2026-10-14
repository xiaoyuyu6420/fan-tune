//! Design tokens and hand-drawn primitives.
//!
//! The visual language is lifted from **RhinE** (a foobar2000 theme): Swiss
//! grid layout, a monochrome palette carrying exactly *one* accent colour,
//! monospace type with wide-tracked uppercase micro labels, 1 px hairlines and
//! zero corner radius. Everything is flat — no gradients, no shadows, no
//! rounded corners. State is expressed two ways only: an inverted solid block
//! (white-on-black ⇄ black-on-white) for "active", and an accent-coloured
//! outline plus corner crop marks for "selected".
//!
//! The fifteen colours per palette are RhinE's own semantic tokens (from its
//! `theme/tokens/tokens.json`), five schemes × light/dark. `warn`/`hot` are the
//! one addition: RhinE has no state colours because a music player has no
//! alarms, whereas a thermal monitor genuinely needs a danger signal. They are
//! kept deliberately scarce — healthy readings stay *grey and quiet*, the single
//! accent appears when something wants attention, red only at the edge.
//!
//! Fonts: Geist Mono (SIL OFL 1.1, see `src/ui/fonts/OFL.txt`) is embedded.
//! It has no CJK coverage, so a system CJK face is appended as fallback.

use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Pos2, Rangef, Rect, Rounding, Stroke, Vec2};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ---------------------------------------------------------------- palette ---

/// Compact hex literal → `Color32` (`0xRRGGBB`), usable in `const` context.
const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// One palette. Field names follow RhinE's token names so the two can be
/// diffed side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    /// page background — the deepest surface
    pub bg: Color32,
    /// default panel surface
    pub panel: Color32,
    /// lifted panel (hover)
    pub panel_hi: Color32,
    /// card / control body
    pub case: Color32,
    /// lifted card (hover / alternate rows)
    pub case_hi: Color32,
    /// inset well (text edit background, bar tracks)
    pub well: Color32,
    /// primary text
    pub fg: Color32,
    /// secondary text
    pub fg_soft: Color32,
    /// de-emphasised text
    pub muted: Color32,
    /// registration marks, rulers, tick labels
    pub mark: Color32,
    /// strong divider
    pub line_dim: Color32,
    /// weak divider
    pub line_faint: Color32,
    /// the 1 px hairline that boxes everything
    pub hair: Color32,
    /// the one accent colour
    pub accent: Color32,
    /// text/ink on top of an accent fill
    pub on_accent: Color32,
    /// attention state (== accent; kept as a named alias for call sites)
    pub warn: Color32,
    /// danger state — the only second hue in the system
    pub hot: Color32,
}

impl Tokens {
    /// Reading colour for a temperature. Four steps: soft grey → full ink →
    /// the accent → red. Readability round 4: the *primary* data of a monitor
    /// is never dimmer than the labels around it, so the floor moved up from
    /// `muted` to `fg_soft` — temperatures still stay grey while healthy, they
    /// are just grey you can read across the room.
    pub fn temp_color(&self, t: f32) -> Color32 {
        if t >= 80.0 {
            self.hot
        } else if t >= 62.0 {
            self.warn
        } else if t >= 45.0 {
            self.fg
        } else {
            self.fg_soft
        }
    }

    /// Accent when the value wants attention, quiet grey when it does not.
    pub fn load_color(&self, pct: f32) -> Color32 {
        if pct >= 90.0 {
            self.warn
        } else {
            self.fg_soft
        }
    }

    pub fn hover_lift(&self) -> Color32 {
        self.case_hi
    }
}

// --- the ten palettes (RhinE tokens.json, verbatim) ------------------------

/// ARCHIVE — paper, air, and the theme orange.
///
/// The grey ladder is lifted one step above RhinE's originals (readability
/// round 4): RhinE ships muted ≈5.6:1 on bg, which reads as smudge at 10 pt
/// instrument labels; here muted sits ≈7.3:1 and the surfaces step apart far
/// enough that the hairline grid is a reinforcement, not the only structure.
pub const ARCHIVE_DARK: Tokens = Tokens {
    bg: hex(0x0b0b0b), panel: hex(0x171716), panel_hi: hex(0x1e1e1d),
    case: hex(0x212120), case_hi: hex(0x31312f), well: hex(0x151514),
    fg: hex(0xf2f0ea), fg_soft: hex(0xc9c7c2), muted: hex(0x9e9c97),
    mark: hex(0x96958f), line_dim: hex(0x84837f), line_faint: hex(0x454543),
    hair: hex(0x4a4a48), accent: hex(0xff7a01), on_accent: hex(0x0b0b0b),
    warn: hex(0xff7a01), hot: hex(0xff5d52),
};
pub const ARCHIVE_LIGHT: Tokens = Tokens {
    bg: hex(0xeeede7), panel: hex(0xf4f3ee), panel_hi: hex(0xfaf9f4),
    case: hex(0xe4e2da), case_hi: hex(0xfbfaf5), well: hex(0xe3e2db),
    fg: hex(0x252821), fg_soft: hex(0x454742), muted: hex(0x575a51),
    mark: hex(0x81857b), line_dim: hex(0x7e7e78), line_faint: hex(0xc0c0b9),
    hair: hex(0xc4c3bd), accent: hex(0xff7a01), on_accent: hex(0x0b0b0b),
    warn: hex(0xd96400), hot: hex(0xcf2b1d),
};

/// HAZARD — black and yellow industrial.
pub const HAZARD_DARK: Tokens = Tokens {
    bg: hex(0x0d0d0b), panel: hex(0x191916), panel_hi: hex(0x20201d),
    case: hex(0x232320), case_hi: hex(0x333230), well: hex(0x171715),
    fg: hex(0xf2efe4), fg_soft: hex(0xcac6b9), muted: hex(0xa19d91),
    mark: hex(0x96958e), line_dim: hex(0x807d73), line_faint: hex(0x474643),
    hair: hex(0x4c4b48), accent: hex(0xffcf1a), on_accent: hex(0x0d0d0b),
    warn: hex(0xffcf1a), hot: hex(0xff5d52),
};
pub const HAZARD_LIGHT: Tokens = Tokens {
    bg: hex(0xebe8dc), panel: hex(0xf2efe2), panel_hi: hex(0xf8f5e8),
    case: hex(0xdfdcd1), case_hi: hex(0xf9f6e9), well: hex(0xdfdcd1),
    fg: hex(0x1a1a16), fg_soft: hex(0x3d3c35), muted: hex(0x535148),
    mark: hex(0x7c7b73), line_dim: hex(0x7e7b70), line_faint: hex(0xbbb9ae),
    hair: hex(0xc0bdb2), accent: hex(0xf2b800), on_accent: hex(0x1a1a16),
    warn: hex(0xb07f00), hot: hex(0xc4281a),
};

/// FLARE — raw orange; the only scheme whose accent inverts to white.
pub const FLARE_DARK: Tokens = Tokens {
    bg: hex(0x140a05), panel: hex(0x20160e), panel_hi: hex(0x271c17),
    case: hex(0x2a1f1a), case_hi: hex(0x3a2f28), well: hex(0x1e140f),
    fg: hex(0xffe9da), fg_soft: hex(0xffc7a5), muted: hex(0xe09c6e),
    mark: hex(0xa18f86), line_dim: hex(0xab6b46), line_faint: hex(0x4f433c),
    hair: hex(0x544437), accent: hex(0xff6b1a), on_accent: hex(0x140a05),
    warn: hex(0xff6b1a), hot: hex(0xff5d52),
};
pub const FLARE_LIGHT: Tokens = Tokens {
    bg: hex(0xff6b1a), panel: hex(0xff6e1b), panel_hi: hex(0xff711b),
    case: hex(0xf16519), case_hi: hex(0xff721c), well: hex(0xf16519),
    fg: hex(0x060403), fg_soft: hex(0x331707), muted: hex(0x3e1a04),
    mark: hex(0x7b2d03), line_dim: hex(0x6e2c04), line_faint: hex(0xc64d0a),
    hair: hex(0xce4e09), accent: hex(0xffffff), on_accent: hex(0x060403),
    warn: hex(0x3b1f0f), hot: hex(0x3d0700),
};

/// FIELD — green paper, green ink.
pub const FIELD_DARK: Tokens = Tokens {
    bg: hex(0x090d09), panel: hex(0x151913), panel_hi: hex(0x1b201b),
    case: hex(0x1e231e), case_hi: hex(0x2e322d), well: hex(0x131713),
    fg: hex(0xe7f1e3), fg_soft: hex(0xc0cebb), muted: hex(0x98a895),
    mark: hex(0x8f968c), line_dim: hex(0x7d8676), line_faint: hex(0x424741),
    hair: hex(0x474c46), accent: hex(0x4fdc76), on_accent: hex(0x090d09),
    warn: hex(0x4fdc76), hot: hex(0xff5d52),
};
pub const FIELD_LIGHT: Tokens = Tokens {
    bg: hex(0xedf1e6), panel: hex(0xf4f7ed), panel_hi: hex(0xfafef3),
    case: hex(0xe1e5da), case_hi: hex(0xfbfff4), well: hex(0xe1e5da),
    fg: hex(0x1c261a), fg_soft: hex(0x3b4838), muted: hex(0x4d5a4e),
    mark: hex(0x7c857a), line_dim: hex(0x798577), line_faint: hex(0xbdc3b7),
    hair: hex(0xc2c6bb), accent: hex(0x1fa84f), on_accent: hex(0x1c261a),
    warn: hex(0x157a39), hot: hex(0xc4281a),
};

/// COLD FRONT — the cold-wind blue.
pub const FRONT_DARK: Tokens = Tokens {
    bg: hex(0x070a0f), panel: hex(0x13161b), panel_hi: hex(0x191d22),
    case: hex(0x1c2025), case_hi: hex(0x2c2f35), well: hex(0x111419),
    fg: hex(0xe5edf6), fg_soft: hex(0xc2d0df), muted: hex(0x95a3b1),
    mark: hex(0x8d9299), line_dim: hex(0x707e8c), line_faint: hex(0x404449),
    hair: hex(0x45494e), accent: hex(0x3d8bff), on_accent: hex(0x070a0f),
    warn: hex(0x3d8bff), hot: hex(0xff5d52),
};
pub const FRONT_LIGHT: Tokens = Tokens {
    bg: hex(0xe9eef3), panel: hex(0xf0f5f9), panel_hi: hex(0xf6fbff),
    case: hex(0xdde2e8), case_hi: hex(0xf7fcff), well: hex(0xdde2e8),
    fg: hex(0x15202b), fg_soft: hex(0x36424f), muted: hex(0x4a5763),
    mark: hex(0x777f87), line_dim: hex(0x78848f), line_faint: hex(0xb9bfc5),
    hair: hex(0xbdc3c9), accent: hex(0x1f6fff), on_accent: hex(0xe9eef3),
    warn: hex(0x0f52c8), hot: hex(0xc4281a),
};

/// Scheme name + its two modes, in the order index 0,1 / 2,3 / …
pub struct Scheme {
    pub id: &'static str,
    pub label: &'static str,
    pub dark: Tokens,
    pub light: Tokens,
}

pub const SCHEMES: [Scheme; 5] = [
    Scheme { id: "archive", label: "ARCHIVE", dark: ARCHIVE_DARK, light: ARCHIVE_LIGHT },
    Scheme { id: "hazard", label: "HAZARD", dark: HAZARD_DARK, light: HAZARD_LIGHT },
    Scheme { id: "flare", label: "FLARE", dark: FLARE_DARK, light: FLARE_LIGHT },
    Scheme { id: "field", label: "FIELD", dark: FIELD_DARK, light: FIELD_LIGHT },
    Scheme { id: "front", label: "COLD FRONT", dark: FRONT_DARK, light: FRONT_LIGHT },
];

/// Flat lookup: `index / 2` = scheme, `index % 2` = 0 dark / 1 light.
pub const ALL: [Tokens; 10] = [
    ARCHIVE_DARK, ARCHIVE_LIGHT,
    HAZARD_DARK, HAZARD_LIGHT,
    FLARE_DARK, FLARE_LIGHT,
    FIELD_DARK, FIELD_LIGHT,
    FRONT_DARK, FRONT_LIGHT,
];

pub const DEFAULT_THEME: &str = "archive-dark";

/// `"archive-dark"` → flat index. Returns `None` for an unknown id.
pub fn index_of(id: &str) -> Option<usize> {
    let (scheme, mode) = id.rsplit_once('-')?;
    let si = SCHEMES.iter().position(|s| s.id == scheme)?;
    let dark = match mode {
        "dark" => true,
        "light" => false,
        _ => return None,
    };
    Some(si * 2 + if dark { 0 } else { 1 })
}

/// Flat index → `"archive-dark"`.
pub fn id_of(index: usize) -> String {
    let si = (index / 2).min(SCHEMES.len() - 1);
    format!("{}-{}", SCHEMES[si].id, if index % 2 == 0 { "dark" } else { "light" })
}

/// Every `(id, label)` the UI can offer, e.g. `("archive-dark", "ARCHIVE DARK")`.
pub fn theme_ids() -> Vec<(String, String)> {
    let mut v = Vec::with_capacity(10);
    for (i, s) in SCHEMES.iter().enumerate() {
        v.push((format!("{}-dark", s.id), format!("{} · DARK", s.label)));
        v.push((format!("{}-light", s.id), format!("{} · LIGHT", s.label)));
        let _ = i;
    }
    v
}

// --- the active palette ----------------------------------------------------

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// RhinE's "Reduce motion" switch: kills the scan transition and the rotating
/// fan icons. Grain is separately controllable and cheap enough to keep.
pub static REDUCE_MOTION: AtomicBool = AtomicBool::new(false);
/// Grain overlay (a single static textured quad).
pub static GRAIN: AtomicBool = AtomicBool::new(true);

/// The palette every drawing call reads. Lock-free: `Tokens` is `Copy` and the
/// table is `const`, so this is an array index.
#[inline]
pub fn t() -> Tokens {
    ALL[ACTIVE.load(Ordering::Relaxed) % ALL.len()]
}

#[inline]
pub fn theme_id() -> String {
    id_of(ACTIVE.load(Ordering::Relaxed) % ALL.len())
}

/// Select a palette by id. Returns `true` if the id was recognised.
pub fn set_theme(id: &str) -> bool {
    match index_of(id) {
        Some(i) => {
            ACTIVE.store(i, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

#[inline]
pub fn reduce_motion() -> bool {
    REDUCE_MOTION.load(Ordering::Relaxed)
}

#[inline]
pub fn grain_on() -> bool {
    GRAIN.load(Ordering::Relaxed)
}

// ------------------------------------------------------------------ scale ---

/// Type scale. Seven steps, deliberately few.
pub mod size {
    /// wide-tracked uppercase micro label
    pub const MICRO: f32 = 11.0;
    /// small label / list row
    pub const LABEL: f32 = 11.5;
    /// body text
    pub const BODY: f32 = 13.5;
    /// inline numeric readout
    pub const NUM: f32 = 16.5;
    /// panel title
    pub const TITLE: f32 = 20.0;
    /// the big readout on an instrument card
    pub const DISPLAY: f32 = 30.0;
    /// hero / ghost type
    pub const HERO: f32 = 48.0;
}

/// Spacing. An 8 dp module, 24 dp page gutter — RhinE's own numbers.
pub mod space {
    pub const GUTTER: f32 = 24.0;
    pub const COL: f32 = 16.0;
    pub const UNIT: f32 = 8.0;
    pub const HALF: f32 = 4.0;
    /// list row height
    pub const ROW: f32 = 22.0;
    /// 1 px hairline
    pub const HAIR: f32 = 1.0;
    /// 2 px heavy rule
    pub const HEAVY: f32 = 2.0;
    /// top HUD bar height
    pub const HEADER: f32 = 73.0;
    /// bottom status bar height
    pub const STATUS: f32 = 28.0;
}

/// Extra letter spacing, in pixels. RhinE works in em; these are the em values
/// pre-multiplied at the sizes above, which is easier to reason about in egui.
pub mod track {
    pub const MICRO: f32 = 1.2;
    pub const LABEL: f32 = 1.3;
    pub const TITLE: f32 = 0.3;
    /// the wordmark is *negative* tracked, as in RhinE
    pub const WORDMARK: f32 = -0.3;
}

// ------------------------------------------------------------------ fonts ---

pub const FAMILY_LIGHT: &str = "geist-light";
pub const FAMILY_REG: &str = "geist";
pub const FAMILY_MED: &str = "geist-medium";
pub const FAMILY_SEMI: &str = "geist-semibold";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Light,
    Regular,
    Medium,
    SemiBold,
}

impl Weight {
    pub fn family(self) -> FontFamily {
        let name = match self {
            Weight::Light => FAMILY_LIGHT,
            Weight::Regular => FAMILY_REG,
            Weight::Medium => FAMILY_MED,
            Weight::SemiBold => FAMILY_SEMI,
        };
        FontFamily::Name(name.into())
    }

    pub fn font(self, size: f32) -> FontId {
        FontId::new(size, self.family())
    }
}

/// Mono body text — the default voice of the whole UI.
pub fn body(size: f32) -> FontId {
    Weight::Regular.font(size)
}

/// Register the embedded Geist Mono faces plus a system CJK fallback, then
/// point *both* `Proportional` and `Monospace` at Geist so nothing can escape
/// into a proportional face by accident.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        FAMILY_LIGHT.to_owned(),
        egui::FontData::from_static(include_bytes!("fonts/GeistMono-Light.ttf")).into(),
    );
    fonts.font_data.insert(
        FAMILY_REG.to_owned(),
        egui::FontData::from_static(include_bytes!("fonts/GeistMono-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        FAMILY_MED.to_owned(),
        egui::FontData::from_static(include_bytes!("fonts/GeistMono-Medium.ttf")).into(),
    );
    fonts.font_data.insert(
        FAMILY_SEMI.to_owned(),
        egui::FontData::from_static(include_bytes!("fonts/GeistMono-SemiBold.ttf")).into(),
    );

    // CJK: Geist Mono covers Latin/Greek/Cyrillic but not Han. Read a system
    // face at runtime (zero binary cost, and it follows the user's OS).
    let cjk = ["C:\\Windows\\Fonts\\msyh.ttc", "C:\\Windows\\Fonts\\simhei.ttf"]
        .iter()
        .find_map(|p| std::fs::read(p).ok().map(|b| (*p, b)));
    let cjk_name = cjk.as_ref().map(|(p, bytes)| {
        let name = "cjk".to_owned();
        fonts.font_data.insert(name.clone(), egui::FontData::from_owned(bytes.clone()).into());
        let _ = p;
        name
    });

    // Every family gets the CJK face appended as fallback.
    let mut families: Vec<FontFamily> = vec![
        FontFamily::Name(FAMILY_LIGHT.into()),
        FontFamily::Name(FAMILY_REG.into()),
        FontFamily::Name(FAMILY_MED.into()),
        FontFamily::Name(FAMILY_SEMI.into()),
        FontFamily::Proportional,
        FontFamily::Monospace,
    ];
    for fam in families.drain(..) {
        let entry = fonts
            .families
            .entry(fam)
            .or_insert_with(|| vec![FAMILY_REG.to_owned()]);
        // Geist first (unless the family already leads with its own weight).
        if entry.is_empty() {
            entry.push(FAMILY_REG.to_owned());
        }
        if let Some(c) = &cjk_name {
            entry.push(c.clone());
        }
    }
    // Both default families lead with the regular Geist face; the named weight
    // families lead with their own face.
    for fam in [FontFamily::Proportional, FontFamily::Monospace] {
        if let Some(list) = fonts.families.get_mut(&fam) {
            list.retain(|n| n != FAMILY_REG);
            list.insert(0, FAMILY_REG.to_owned());
        }
    }
    for (fam, face) in [
        (FAMILY_LIGHT, FAMILY_LIGHT),
        (FAMILY_MED, FAMILY_MED),
        (FAMILY_SEMI, FAMILY_SEMI),
    ] {
        if let Some(list) = fonts.families.get_mut(&FontFamily::Name(fam.into())) {
            list.retain(|n| n != face && n != FAMILY_REG);
            list.insert(0, face.to_owned());
        }
    }

    ctx.set_fonts(fonts);
}

// ------------------------------------------------------------------ style ---

/// Apply the active palette to egui's `Visuals`: zero radius, no shadows,
/// hairlines everywhere, accent used for hover/selection.
pub fn install_style(ctx: &egui::Context) {
    let tk = t();
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;

    v.dark_mode = index_of(&theme_id()).map(|i| i % 2 == 0).unwrap_or(true);
    v.panel_fill = tk.bg;
    v.window_fill = tk.bg;
    v.faint_bg_color = tk.panel;
    v.extreme_bg_color = tk.well;
    v.override_text_color = Some(tk.fg);
    v.window_stroke = Stroke::new(space::HAIR, tk.hair);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.window_rounding = Rounding::ZERO;
    v.menu_rounding = Rounding::ZERO;

    v.widgets.noninteractive.bg_fill = tk.panel;
    v.widgets.noninteractive.weak_bg_fill = tk.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(space::HAIR, tk.hair);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, tk.fg_soft);

    v.widgets.inactive.bg_fill = tk.case;
    v.widgets.inactive.weak_bg_fill = tk.case;
    v.widgets.inactive.bg_stroke = Stroke::new(space::HAIR, tk.hair);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, tk.fg);

    v.widgets.hovered.bg_fill = tk.case_hi;
    v.widgets.hovered.weak_bg_fill = tk.case_hi;
    v.widgets.hovered.bg_stroke = Stroke::new(space::HAIR, tk.accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, tk.fg);

    // Pressed = inverted block. The stamp of this design language.
    v.widgets.active.bg_fill = tk.fg;
    v.widgets.active.weak_bg_fill = tk.fg;
    v.widgets.active.bg_stroke = Stroke::new(space::HAIR, tk.fg);
    v.widgets.active.fg_stroke = Stroke::new(1.0, tk.bg);

    v.widgets.open.bg_fill = tk.panel_hi;
    v.widgets.open.weak_bg_fill = tk.panel_hi;
    v.widgets.open.bg_stroke = Stroke::new(space::HAIR, tk.hair);
    v.widgets.open.fg_stroke = Stroke::new(1.0, tk.fg);

    // Selected = inverted block. `selection.stroke.color` is also the text
    // colour egui uses on top of `selection.bg_fill`, hence the swap.
    v.selection.bg_fill = tk.fg;
    v.selection.stroke = Stroke::new(1.0, tk.bg);

    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = Rounding::ZERO;
        w.expansion = 0.0;
    }

    style.spacing.item_spacing = Vec2::new(space::UNIT, space::HALF);
    style.spacing.button_padding = Vec2::new(space::UNIT + 2.0, 3.0);
    style.spacing.menu_margin = egui::Margin::same(space::HAIR);
    style.spacing.slider_width = 110.0;
    style.spacing.interact_size.y = space::ROW;
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = false;

    style.text_styles = [
        (egui::TextStyle::Heading, Weight::SemiBold.font(size::TITLE)),
        (egui::TextStyle::Body, body(size::BODY)),
        (egui::TextStyle::Small, body(size::LABEL)),
        (egui::TextStyle::Button, Weight::Medium.font(size::LABEL)),
        (egui::TextStyle::Monospace, body(size::BODY)),
    ]
    .into_iter()
    .collect();

    ctx.set_style(style);
}

// ------------------------------------------------------- text primitives ---

/// Uppercasing allocates, and this UI builds ~200 labels per frame. Almost
/// all of them are already-uppercase Latin or CJK (which has no case), so
/// only pay for the allocation when there is something to uppercase.
fn upper(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(char::is_lowercase) {
        std::borrow::Cow::Owned(s.to_uppercase())
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Wide tracking is a Latin habit: CJK glyphs are already square and
/// full-width, and pushing them apart reads as broken typesetting, not as
/// instrument labels. Any label that contains CJK gets its tracking damped,
/// so `micro()` still works for mixed strings like `"温度 — 04"`.
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        let u = c as u32;
        matches!(u,
            0x1100..=0x11FF       // Hangul Jamo
            | 0x2E80..=0xA4CF     // CJK radicals, ideographs, kana, Yi
            | 0xAC00..=0xD7AF     // Hangul syllables
            | 0xF900..=0xFAFF     // CJK compatibility ideographs
            | 0xFE30..=0xFE4F     // CJK compatibility forms
            | 0xFF00..=0xFFEF     // full-width forms
            | 0x20000..=0x2FA1F   // CJK extensions B..F
        )
    })
}

/// A tracked text run. This is how every label in the UI is built: RhinE's
/// instrument look comes from monospace type with the letters pushed apart.
pub fn tracked(text: &str, size: f32, weight: Weight, color: Color32, tracking: f32) -> LayoutJob {
    // Damp, don't drop: a mixed string keeps a little air between its parts.
    let tracking = if has_cjk(text) { tracking * 0.45 } else { tracking };
    let mut job = LayoutJob::default();
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: weight.font(size),
            color,
            extra_letter_spacing: tracking,
            ..Default::default()
        },
    );
    job
}

/// Uppercase + wide tracking + muted grey — the workhorse label.
pub fn micro(text: &str, color: Color32) -> LayoutJob {
    tracked(&upper(text), size::MICRO, Weight::Medium, color, track::MICRO)
}

/// A tracked single-line job clamped to `max_w` with an ellipsis.
///
/// [`egui::CollapsingHeader`] headers do not wrap, so one long line (the algo
/// knobs list) used to widen the whole card rack past the window — which drags
/// a black band across the stage and clips everything on the right. Anything
/// long, single-line and width-driving must go through this.
pub fn tracked_fitted(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    weight: Weight,
    color: Color32,
    tracking: f32,
    max_w: f32,
) -> LayoutJob {
    let width_of = |job: &LayoutJob| {
        ui.fonts(|f| f.layout_job(job.clone()).size().x)
    };
    let full = tracked(text, size, weight, color, tracking);
    if width_of(&full) <= max_w || text.is_empty() {
        return full;
    }
    // longest prefix that still fits once "…" is appended
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let probe: String = chars[..mid].iter().collect::<String>() + "…";
        if width_of(&tracked(&probe, size, weight, color, tracking)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let cut: String = chars[..lo].iter().collect::<String>() + "…";
    tracked(&cut, size, weight, color, tracking)
}

/// Slightly larger tracked label, sentence case preserved.
pub fn label(text: &str, color: Color32) -> LayoutJob {
    tracked(text, size::LABEL, Weight::Regular, color, track::LABEL)
}

/// Tracked numeric readout.
pub fn num(text: &str, size: f32, weight: Weight, color: Color32) -> LayoutJob {
    tracked(text, size, weight, color, 0.0)
}

// ---------------------------------------------------- painter primitives ---

/// Snap a coordinate to the device pixel grid so a 1 px line lands on exactly
/// one physical pixel row instead of smearing across two.
#[inline]
fn snap(v: f32, ppp: f32) -> f32 {
    (v * ppp).round() / ppp
}

/// 1 px horizontal hairline.
pub fn hline(p: &egui::Painter, x: Rangef, y: f32, color: Color32) {
    let ppp = p.ctx().pixels_per_point();
    p.hline(x, snap(y, ppp), Stroke::new(space::HAIR, color));
}

/// 1 px vertical hairline.
pub fn vline(p: &egui::Painter, x: f32, y: Rangef, color: Color32) {
    let ppp = p.ctx().pixels_per_point();
    p.vline(snap(x, ppp), y, Stroke::new(space::HAIR, color));
}

/// Hairline box, zero radius.
pub fn box_line(p: &egui::Painter, rect: Rect, color: Color32) {
    p.rect_stroke(rect, Rounding::ZERO, Stroke::new(space::HAIR, color));
}

/// The four corner crop marks that mean "selected" in this language. Drawn just
/// *outside* the box so they read as registration marks, not as a border.
pub fn corner_marks(p: &egui::Painter, rect: Rect, len: f32, color: Color32) {
    let s = Stroke::new(space::HAIR, color);
    let v = 2.0; // how far the marks stand off the box
    let (l, t, r, b) = (
        rect.left() - v,
        rect.top() - v,
        rect.right() + v,
        rect.bottom() + v,
    );
    p.line_segment([Pos2::new(l, t), Pos2::new(l + len, t)], s);
    p.line_segment([Pos2::new(l, t), Pos2::new(l, t + len)], s);
    p.line_segment([Pos2::new(r, t), Pos2::new(r - len, t)], s);
    p.line_segment([Pos2::new(r, t), Pos2::new(r, t + len)], s);
    p.line_segment([Pos2::new(l, b), Pos2::new(l + len, b)], s);
    p.line_segment([Pos2::new(l, b), Pos2::new(l, b - len)], s);
    p.line_segment([Pos2::new(r, b), Pos2::new(r - len, b)], s);
    p.line_segment([Pos2::new(r, b), Pos2::new(r, b - len)], s);
}

/// A "+" registration cross, the frame-corner idiom of the reference.
pub fn reg_cross(p: &egui::Painter, at: Pos2, arm: f32, color: Color32) {
    let s = Stroke::new(space::HAIR, color);
    let g = arm * 0.35; // central gap
    p.line_segment([Pos2::new(at.x - arm, at.y), Pos2::new(at.x - g, at.y)], s);
    p.line_segment([Pos2::new(at.x + g, at.y), Pos2::new(at.x + arm, at.y)], s);
    p.line_segment([Pos2::new(at.x, at.y - arm), Pos2::new(at.x, at.y - g)], s);
    p.line_segment([Pos2::new(at.x, at.y + g), Pos2::new(at.x, at.y + arm)], s);
}

/// A ruler: `ticks` minor divisions, every `major` one longer. Used under
/// gauges and along the curve plot.
pub fn ruler(
    p: &egui::Painter,
    x: Rangef,
    y_top: f32,
    height: f32,
    ticks: usize,
    major: usize,
    color: Color32,
) {
    if ticks < 2 {
        return;
    }
    let s = Stroke::new(space::HAIR, color);
    let span = x.max - x.min;
    for i in 0..ticks {
        let f = i as f32 / (ticks - 1) as f32;
        let x = snap(x.min + span * f, p.ctx().pixels_per_point());
        let h = if major > 0 && i % major == 0 { height } else { height * 0.42 };
        p.line_segment([Pos2::new(x, y_top), Pos2::new(x, y_top + h)], s);
    }
}

/// Diagonal hazard stripes, clipped to `rect` — the warning idiom.
pub fn hazard_stripes(p: &egui::Painter, rect: Rect, width: f32, on: Color32, off: Color32) {
    p.rect_filled(rect, Rounding::ZERO, off);
    let mut clip = p.clone();
    clip.set_clip_rect(rect);
    let step = width * 2.0;
    let mut x = rect.left() - rect.height();
    while x < rect.right() + rect.height() {
        clip.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(x, rect.bottom()),
                Pos2::new(x + width, rect.bottom()),
                Pos2::new(x + width + rect.height(), rect.top()),
                Pos2::new(x + rect.height(), rect.top()),
            ],
            on,
            Stroke::NONE,
        ));
        x += step;
    }
}

/// Oversized, very low contrast type — the ghost wordmark of the reference.
pub fn ghost_text(p: &egui::Painter, at: Pos2, anchor: egui::Align2, text: &str, size: f32, color: Color32) {
    p.text(
        at,
        anchor,
        text,
        Weight::Light.font(size),
        color,
    );
}

// ------------------------------------------------------ scene layer ---

/// The stage's graph-paper underlay: a fine 24 pt ruling with every fifth line
/// heavier, plus small crosses on the major intersections. Drawn first so
/// every widget lands on top of it. Pure static paint — it asks for nothing.
///
/// This is the "lab deck" of the reference, translated from a 3D render into
/// the vector language this UI can afford.
pub fn blueprint_grid(p: &egui::Painter, rect: Rect) {
    let tk = t();
    let minor = tk.line_faint.gamma_multiply(0.45);
    let major = tk.line_faint;
    let ppp = p.ctx().pixels_per_point();
    let step = 24.0;
    let mut i = 0;
    let mut x = rect.left() + step;
    while x < rect.right() {
        let sx = snap(x, ppp);
        let c = if i % 5 == 0 { major } else { minor };
        p.line_segment(
            [Pos2::new(sx, rect.top()), Pos2::new(sx, rect.bottom())],
            Stroke::new(space::HAIR, c),
        );
        x += step;
        i += 1;
    }
    let mut j = 0;
    let mut y = rect.top() + step;
    while y < rect.bottom() {
        let sy = snap(y, ppp);
        let c = if j % 5 == 0 { major } else { minor };
        p.line_segment(
            [Pos2::new(rect.left(), sy), Pos2::new(rect.right(), sy)],
            Stroke::new(space::HAIR, c),
        );
        if j % 5 == 0 {
            // registration crosses on the major nodes, the drawing-board idiom
            let mut gx = rect.left() + step;
            let mut gi = 0;
            while gx < rect.right() {
                let cx = Pos2::new(snap(gx, ppp), sy);
                p.line_segment(
                    [Pos2::new(cx.x - 2.0, cx.y), Pos2::new(cx.x + 2.0, cx.y)],
                    Stroke::new(space::HAIR, tk.line_faint),
                );
                p.line_segment(
                    [Pos2::new(cx.x, cx.y - 2.0), Pos2::new(cx.x, cx.y + 2.0)],
                    Stroke::new(space::HAIR, tk.line_faint),
                );
                gx += step;
                gi += 1;
            }
            let _ = gi;
        }
        y += step;
        j += 1;
    }
}

/// Polyline arc helper (centre/radius/angles in radians) — hand-rolled so the
/// reactor never depends on `Painter::arc`'s stroke type changing under us.
fn arc_line(p: &egui::Painter, center: Pos2, radius: f32, a0: f32, a1: f32, stroke: Stroke) {
    let steps = ((a1 - a0).abs() / 0.22).ceil().max(2.0) as usize;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|k| {
            let a = a0 + (a1 - a0) * k as f32 / steps as f32;
            Pos2::new(
                center.x + radius * a.cos(),
                center.y + radius * a.sin(),
            )
        })
        .collect();
    p.add(egui::Shape::line(pts, stroke));
}

/// The reactor — the stage's resident instrument, the vector stand-in for the
/// reference's Möbius ring. `heat` (0..1) sweeps a 270° gauge; `phase` rotates
/// three turbine blades (the caller bakes the speed in, advancing phase with
/// `spin_t`, so the whole thing freezes with the `animating` gate). Pure paint.
pub fn reactor(p: &egui::Painter, center: Pos2, r: f32, heat: f32, phase: f32) {
    let tk = t();
    let heat = heat.clamp(0.0, 1.0);
    p.circle_stroke(center, r, Stroke::new(space::HAIR, tk.hair));
    p.circle_stroke(center, r * 0.60, Stroke::new(space::HAIR, tk.line_faint));
    // 12 ticks on the outer ring
    for k in 0..12 {
        let a = k as f32 * std::f32::consts::TAU / 12.0;
        p.line_segment(
            [
                Pos2::new(center.x + (r - 3.0) * a.cos(), center.y + (r - 3.0) * a.sin()),
                Pos2::new(center.x + r * a.cos(), center.y + r * a.sin()),
            ],
            Stroke::new(space::HAIR, tk.line_faint),
        );
    }
    // heat gauge: 270° from the top, clockwise
    let start = -std::f32::consts::FRAC_PI_2;
    let sweep = 1.5 * std::f32::consts::PI * heat;
    if sweep > 0.03 {
        let col = if heat >= 0.80 { tk.hot } else { tk.accent };
        arc_line(p, center, r * 0.80, start, start + sweep, Stroke::new(2.5, col));
    }
    // three turbine blades, rotating
    for k in 0..3 {
        let a0 = phase + k as f32 * std::f32::consts::TAU / 3.0;
        arc_line(p, center, r * 0.40, a0, a0 + 0.55, Stroke::new(1.5, tk.mark));
    }
    p.circle_filled(center, 1.6, tk.fg);
}

// -------------------------------------------------------- widget helpers ---

/// A small outlined tag. `on` inverts it to a solid block, which is how this
/// language says "current".
///
/// The response comes from a single `allocate_exact_size(.., Sense::click())`
/// registration, so egui assigns a unique auto id per call. Deriving the id
/// from the label text instead was a real bug: the mode buttons ("曲线",
/// "应用", …) appear once per channel card, and seven same-id widgets in one
/// frame share interaction state — clicks landed on the wrong card or
/// nowhere at all.
pub fn chip(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    let tk = t();
    let text_up = upper(text);
    let galley = ui.fonts(|f| {
        f.layout_job(tracked(
            &text_up,
            size::MICRO,
            Weight::Medium,
            tk.fg,
            track::MICRO,
        ))
    });
    let pad = Vec2::new(space::UNIT, 2.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::click());
    let p = ui.painter();
    if on {
        p.rect_filled(rect, Rounding::ZERO, tk.fg);
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            &text_up,
            Weight::Medium.font(size::MICRO),
            tk.bg,
        );
    } else {
        let border = if resp.hovered() { tk.line_dim } else { tk.hair };
        box_line(p, rect, border);
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            &text_up,
            Weight::Medium.font(size::MICRO),
            if resp.hovered() { tk.fg } else { tk.fg_soft },
        );
    }
    resp
}

/// Chip whose label is not uppercased (for ids, numbers, CJK).
pub fn chip_raw(ui: &mut egui::Ui, text: &str, on: bool, color: Color32) -> egui::Response {
    let tk = t();
    let galley = ui.fonts(|f| {
        f.layout_job(tracked(text, size::MICRO, Weight::Medium, color, track::MICRO))
    });
    let pad = Vec2::new(space::UNIT, 2.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::click());
    let p = ui.painter();
    if on {
        p.rect_filled(rect, Rounding::ZERO, color);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, text,
               Weight::Medium.font(size::MICRO), tk.bg);
    } else {
        box_line(p, rect, if resp.hovered() { tk.line_dim } else { tk.hair });
        p.text(rect.center(), egui::Align2::CENTER_CENTER, text,
               Weight::Medium.font(size::MICRO), color);
    }
    resp
}

/// A section header: `TITLE — 03`, tracked and muted, with a hairline filling
/// the remaining width. Returns the row rect.
pub fn section(ui: &mut egui::Ui, title: &str, count: Option<&str>) -> Rect {
    let tk = t();
    let text = match count {
        Some(c) => format!("{} — {}", upper(title), c),
        None => upper(title).into_owned(),
    };
    let job = tracked(&text, size::MICRO, Weight::SemiBold, tk.muted, track::MICRO);
    let galley = ui.fonts(|f| f.layout_job(job));
    let h = (galley.size().y + space::UNIT).max(space::ROW * 0.8);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), egui::Sense::hover());
    let p = ui.painter();
    let ty = rect.center().y;
    p.galley(
        Pos2::new(rect.left(), ty - galley.size().y / 2.0),
        galley.clone(),
        tk.muted,
    );
    let x0 = rect.left() + galley.size().x + space::UNIT;
    if x0 < rect.right() - space::UNIT {
        hline(p, Rangef::new(x0, rect.right()), ty, tk.hair);
    }
    rect
}

/// An inset panel body: `case` fill, hairline, 12/8 padding.
pub fn panel_frame() -> egui::Frame {
    let tk = t();
    egui::Frame::none()
        .fill(tk.case)
        .stroke(Stroke::new(space::HAIR, tk.hair))
        .inner_margin(egui::Margin::symmetric(space::GUTTER / 2.0, space::UNIT + 2.0))
}

/// A readout tile: micro label above, oversized number below. Returns the rect
/// so the caller can add a ruler or a sparkline underneath.
pub fn readout(
    ui: &mut egui::Ui,
    label_text: &str,
    value: &str,
    unit: &str,
    value_color: Color32,
    width: f32,
) -> Rect {
    let tk = t();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 62.0), egui::Sense::hover());
    let p = ui.painter();
    let job = micro(label_text, tk.muted);
    let galley = ui.fonts(|f| f.layout_job(job));
    p.galley(Pos2::new(rect.left(), rect.top() + 3.0), galley.clone(), tk.muted);

    let vjob = tracked(value, size::DISPLAY, Weight::Light, value_color, 0.0);
    let vgalley = ui.fonts(|f| f.layout_job(vjob));
    let unit_w = if unit.is_empty() {
        0.0
    } else {
        let ujob = tracked(unit, size::LABEL, Weight::Regular, tk.muted, track::LABEL);
        let ug = ui.fonts(|f| f.layout_job(ujob));
        let w = ug.size().x;
        p.galley(
            Pos2::new(rect.left() + vgalley.size().x + 4.0, rect.top() + 56.0 - ug.size().y),
            ug,
            tk.muted,
        );
        w + 4.0
    };
    p.galley(
        Pos2::new(rect.left(), rect.top() + 56.0 - vgalley.size().y),
        vgalley,
        value_color,
    );
    let _ = unit_w;
    hline(p, Rangef::new(rect.left(), rect.right()), rect.bottom() - 1.0, tk.hair);
    rect
}

/// A bar painted at an exact rect — the inline gauge of the sensor rail, where
/// a row's sub-bar must not swallow the row's width the way [`gauge`] would.
pub fn bar_at(p: &egui::Painter, rect: Rect, pct: f32, fill: Color32) {
    let tk = t();
    p.rect_filled(rect, Rounding::ZERO, tk.well);
    box_line(p, rect, tk.hair);
    let f = (pct / 100.0).clamp(0.0, 1.0);
    let w = (rect.width() - 2.0) * f;
    if w > 0.5 {
        p.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.left() + 1.0, rect.top() + 1.0),
                Vec2::new(w, rect.height() - 2.0),
            ),
            Rounding::ZERO,
            fill,
        );
    }
}

/// A compact polyline over `values`, scaled into [vmin, vmax]. Static paint:
/// it redraws whatever the caller already has, asks for nothing, and costs a
/// single line shape per frame.
pub fn spark(
    p: &egui::Painter,
    rect: Rect,
    values: &[f32],
    vmin: f32,
    vmax: f32,
    color: Color32,
) {
    if values.len() < 2 || vmax <= vmin {
        return;
    }
    let pts: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32;
            let f = ((v - vmin) / (vmax - vmin)).clamp(0.0, 1.0);
            Pos2::new(x, rect.bottom() - f * rect.height())
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(space::HAIR, color)));
}

/// A horizontal gauge: inset track, flat fill, optional target tick and ruler.
/// Returns the rect it used.
pub fn gauge(ui: &mut egui::Ui, value_pct: f32, target_pct: Option<f32>, fill: Color32, height: f32) -> Rect {
    let tk = t();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, Rounding::ZERO, tk.well);
    box_line(p, rect, tk.hair);
    let f = (value_pct / 100.0).clamp(0.0, 1.0);
    let w = (rect.width() - 2.0) * f;
    if w > 0.0 {
        let fill_rect = Rect::from_min_size(
            Pos2::new(rect.left() + 1.0, rect.top() + 1.0),
            Vec2::new(w, rect.height() - 2.0),
        );
        p.rect_filled(fill_rect, Rounding::ZERO, fill);
    }
    if let Some(tg) = target_pct {
        if (0.0..=100.0).contains(&tg) {
            let x = rect.left() + 1.0 + (rect.width() - 2.0) * tg / 100.0;
            vline(p, x, Rangef::new(rect.top(), rect.bottom()), tk.fg);
        }
    }
    rect
}

// --------------------------------------------------------------- grain -----

/// How strongly the grain tints the page, 0..255. The reference overlay moves
/// the background by about two grey levels out of 255; anything stronger reads
/// as dirt on the screen rather than as a finish.
const GRAIN_STRENGTH: u8 = 9;

/// Build the seamless noise tile. Generated once, then drawn as a single static
/// textured quad per frame — no per-frame work, no repaint pressure.
///
/// The tile is *opaque* mid-grey noise; [`paint_grain`] scales it down with the
/// mesh vertex colour. Baking the weakness into the texture's alpha instead
/// would be wrong: `Color32::from_rgba_premultiplied` stores premultiplied
/// channels, so full-brightness RGB with a tiny alpha is nearly opaque white,
/// not a faint veil.
pub fn make_grain(ctx: &egui::Context) -> egui::TextureHandle {
    const N: usize = 192;
    let mut img = egui::ColorImage::new([N, N], Color32::from_gray(128));
    // Deterministic LCG: the texture must be identical on every run, and
    // identical across schemes, so only the strength is palette-dependent.
    let mut state: u32 = 0x9E37_79B9;
    for px in img.pixels.iter_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = ((state >> 16) & 0xFF) as i32 - 128; // -128..127, mean 0
        *px = Color32::from_gray((128 + n / 2).clamp(0, 255) as u8);
    }
    ctx.load_texture("grain", img, egui::TextureOptions::NEAREST_REPEAT)
}

/// Lay the grain over `rect` (the whole viewport), tiled 1:1 in logical points.
///
/// The tint is white at a few percent alpha, so it lightens by the same amount
/// whatever the palette underneath — the reference grain shifts every surface by
/// about two grey levels either way.
pub fn paint_grain(p: &egui::Painter, rect: Rect, tex: &egui::TextureHandle) {
    const N: f32 = 192.0;
    let mut mesh = egui::Mesh::with_texture(tex.id());
    let uv = Vec2::new(rect.width() / N, rect.height() / N);
    mesh.add_rect_with_uv(
        rect,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(uv.x, uv.y)),
        Color32::from_rgba_premultiplied(GRAIN_STRENGTH, GRAIN_STRENGTH, GRAIN_STRENGTH, GRAIN_STRENGTH),
    );
    p.add(egui::Shape::mesh(mesh));
}

// -------------------------------------------------------------- switcher ---

/// The theme picker: one column per scheme — an accent swatch, the name chip,
/// and (when the scheme is current) corner crop marks. Returns `Some(new_id)`
/// on the frame the user picked something. The DARK/LIGHT half of the target
/// id follows whatever mode is currently shown, so switching schemes keeps
/// your light/dark preference.
pub fn theme_switcher(ui: &mut egui::Ui, current: &str) -> Option<String> {
    let tk = t();
    let light_now = current.ends_with("light");
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(2.0, 2.0);
        for s in SCHEMES.iter() {
            let target = format!("{}-{}", s.id, if light_now { "light" } else { "dark" });
            let is_sel = current == target;
            let label = if s.id == "front" { "COLD FRONT" } else { s.label };
            let col = ui.vertical(|ui| {
                // Accent swatch above the name so the row reads as a palette.
                let (sw, _) = ui.allocate_exact_size(Vec2::new(46.0, 3.0), egui::Sense::hover());
                ui.painter().rect_filled(sw, Rounding::ZERO, s.dark.accent);
                chip(ui, label, is_sel)
            });
            let r = col.inner;
            if is_sel {
                corner_marks(ui.painter(), r.rect.expand(3.0), 4.0, tk.accent);
            }
            if r.clicked() {
                picked = Some(target);
            }
        }
    });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless UI harness: run `body` inside a real egui context with the
    /// given input events, returning what the body recorded. This exercises
    /// the actual layout + interaction machinery, not just compile.
    struct Headless {
        ctx: egui::Context,
        time: f64,
        screen: egui::Rect,
    }

    impl Headless {
        fn new() -> Self {
            let ctx = egui::Context::default();
            install_fonts(&ctx);
            Self {
                ctx,
                time: 0.0,
                screen: egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0)),
            }
        }

        fn frame(
            &mut self,
            events: Vec<egui::Event>,
            body: impl FnOnce(&mut egui::Ui),
        ) -> egui::FullOutput {
            let raw = egui::RawInput {
                screen_rect: Some(self.screen),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            self.time += 1.0 / 30.0;
            // `run` wants FnMut but callers hand a fresh FnOnce body per
            // frame; park it in an Option and take it inside.
            let mut body = Some(Box::new(body) as Box<dyn FnOnce(&mut egui::Ui)>);
            self.ctx.run(raw, move |ctx| {
                if let Some(body) = body.take() {
                    egui::CentralPanel::default().show(ctx, |ui| body(ui));
                }
            })
        }
    }

    fn click(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn clicked_none(flags: &[bool]) -> bool {
        flags.iter().all(|c| !*c)
    }

    #[test]
    fn same_label_chips_are_distinct_targets_and_click_independently() {
        // The channel rack renders seven cards, each carrying chips labelled
        // 曲线 / 预算 / 手动 / 自动 / 应用. Same-label chips in one frame used
        // to share one interaction id, so clicks landed on the wrong card.
        // This replays that exact scenario with real pointer events.
        let mut h = Headless::new();

        // frame 1: lay out seven same-label chips, record rects + ids
        let (rects, ids, _) = {
            let mut rects = Vec::new();
            let mut ids = Vec::new();
            let mut clicks = Vec::new();
            h.frame(vec![], |ui| {
                for _ in 0..7 {
                    let r = chip(ui, "曲线", false);
                    ids.push(r.id);
                    rects.push(r.rect);
                    clicks.push(r.clicked());
                }
            });
            assert!(clicks.iter().all(|c| !*c), "no clicks without input");
            (rects, ids, clicks)
        };
        assert_eq!(rects.len(), 7);
        // ids must be pairwise distinct — the actual regression
        for (i, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(i + 1) {
                assert_ne!(a, b, "chip {i} shares its id with another chip");
            }
        }
        // and every chip must have a real, click-sensing rect
        for r in &rects {
            assert!(r.width() > 10.0 && r.height() > 8.0, "degenerate chip rect {r:?}");
        }

        // frame 2: press chip #3 — the rack must be rendered again (immediate
        // mode: a widget only receives a press in the frame it exists)
        let target = rects[3].center();
        let pressed = {
            let mut clicks = Vec::new();
            h.frame(vec![click(target, true)], |ui| {
                for _ in 0..7 {
                    clicks.push(chip(ui, "曲线", false).clicked());
                }
            });
            clicks
        };
        assert!(clicked_none(&pressed), "a press alone must not count as a click");

        // frame 3: release over chip #3 — exactly that one reports clicked
        let clicked = {
            let mut clicks = Vec::new();
            h.frame(vec![click(target, false)], |ui| {
                for _ in 0..7 {
                    clicks.push(chip(ui, "曲线", false).clicked());
                }
            });
            clicks
        };
        for (i, c) in clicked.iter().enumerate() {
            assert_eq!(
                *c,
                i == 3,
                "chip {i} {}respond to the click on chip #3",
                if *c { "must not " } else { "must " }
            );
        }
    }

    #[test]
    fn raw_chips_and_hovered_border_track_the_pointer() {
        let mut h = Headless::new();
        // frame 1: two same-label raw chips (temperature sources do this)
        let (rects, ids) = {
            let mut rects = Vec::new();
            let mut ids = Vec::new();
            h.frame(vec![], |ui| {
                for _ in 0..2 {
                    let r = chip_raw(ui, "nct:PECI_0 (CPU)", false, t().fg_soft);
                    ids.push(r.id);
                    rects.push(r.rect);
                }
            });
            assert_ne!(ids[0], ids[1], "same-label chip_raw ids collide");
            (rects, ids)
        };
        // frame 2: pointer rests over chip #0 — it must report hovered
        let hovered = {
            let mut flags = Vec::new();
            h.frame(
                vec![egui::Event::PointerMoved(rects[0].center())],
                |ui| {
                    for _ in 0..2 {
                        flags.push(chip_raw(ui, "nct:PECI_0 (CPU)", false, t().fg_soft).hovered());
                    }
                },
            );
            flags
        };
        assert!(hovered[0], "chip #0 should be hovered");
        assert!(!hovered[1], "chip #1 should not be hovered");
    }

    #[test]
    fn every_scheme_id_round_trips() {
        for s in SCHEMES.iter() {
            for mode in ["dark", "light"] {
                let id = format!("{}-{}", s.id, mode);
                let i = index_of(&id).unwrap_or_else(|| panic!("{id} not recognised"));
                assert_eq!(id_of(i), id);
            }
        }
    }

    #[test]
    fn theme_ids_are_all_resolvable_and_unique() {
        let ids = theme_ids();
        assert_eq!(ids.len(), 10);
        let mut seen = std::collections::HashSet::new();
        for (id, _) in &ids {
            assert!(seen.insert(id.clone()), "duplicate theme id {id}");
            assert!(index_of(id).is_some(), "{id} unresolvable");
        }
        assert!(index_of(DEFAULT_THEME).is_some());
    }

    #[test]
    fn unknown_ids_are_rejected() {
        assert!(index_of("archive").is_none());
        assert!(index_of("nope-dark").is_none());
        assert!(index_of("archive-blue").is_none());
        assert!(index_of("").is_none());
    }

    #[test]
    fn palettes_are_legible() {
        // Text must actually stand off its background in all ten palettes; a
        // copy-paste slip in the table would otherwise ship an unreadable UI.
        let lum = |c: Color32| {
            0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32
        };
        for (i, tk) in ALL.iter().enumerate() {
            let d = (lum(tk.fg) - lum(tk.bg)).abs();
            assert!(d > 90.0, "{} fg/bg contrast too low: {d:.0}", id_of(i));
            let d2 = (lum(tk.muted) - lum(tk.bg)).abs();
            assert!(d2 > 25.0, "{} muted/bg contrast too low: {d2:.0}", id_of(i));
            assert_ne!(tk.accent, tk.bg, "{} accent invisible on bg", id_of(i));
            let d3 = (lum(tk.accent) - lum(tk.bg)).abs();
            assert!(d3 > 30.0, "{} accent/bg contrast too low: {d3:.0}", id_of(i));
        }
    }

    #[test]
    fn cjk_labels_get_damped_tracking() {
        // Latin labels carry the full instrument tracking; labels containing
        // CJK must be damped (square glyphs pushed apart read as broken
        // typesetting), without dropping the air entirely for mixed strings.
        let t_of = |j: LayoutJob| j.sections[0].format.extra_letter_spacing;
        let latin = tracked("DUTY 45", 10.0, Weight::Medium, Color32::WHITE, 1.7);
        let mixed = tracked("温度 - 04", 10.0, Weight::Medium, Color32::WHITE, 1.7);
        assert!((t_of(latin) - 1.7).abs() < 1e-4);
        let c = t_of(mixed);
        assert!(
            (c - 1.7 * 0.45).abs() < 1e-4,
            "CJK tracking should be damped to 45%, got {c}"
        );
    }

    #[test]
    fn temp_ramp_is_monotonic_in_severity() {
        let tk = ARCHIVE_DARK;
        // readability round 4: the floor is fg_soft, never muted — the
        // primary data outranks the labels around it
        assert_eq!(tk.temp_color(30.0), tk.fg_soft);
        assert_eq!(tk.temp_color(50.0), tk.fg);
        assert_eq!(tk.temp_color(70.0), tk.warn);
        assert_eq!(tk.temp_color(95.0), tk.hot);
        assert_ne!(tk.temp_color(30.0), tk.temp_color(50.0), "warm step must differ from ambient");
    }
}
