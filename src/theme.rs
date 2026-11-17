//! Palette and glyphs — igtop's, unchanged, so the two tools sit side by side
//! in one terminal and read as a pair.
//!
//! The bar ramp is the load-bearing idea: colour is interpolated **per
//! character along the track**, so a bar encodes its magnitude twice — once by
//! length and once by hue. A 60% bar is visibly warmer at the tip than a 40%
//! one even before the number is read.
//!
//! The ramp is pastel: high lightness, restrained saturation. Saturated
//! terminal colours vibrate against a dark ground and tire the eye on a panel
//! that is meant to be glanced at all day.

use ratatui::style::Color;

/// Anchors for the gradient, as (position 0..1, RGB). Interpolated between.
const RAMP: [(f64, (u8, u8, u8)); 5] = [
    (0.00, (167, 230, 170)), // mint
    (0.35, (214, 230, 158)), // chartreuse
    (0.60, (242, 216, 158)), // amber
    (0.80, (243, 186, 152)), // peach
    (1.00, (238, 155, 155)), // soft red
];

fn lerp(a: u8, b: u8, k: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * k).round().clamp(0.0, 255.0) as u8
}

/// Colour at fractional position `t` along the track.
pub fn ramp(t: f64) -> Color {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    for w in RAMP.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t <= t1 {
            let k = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return Color::Rgb(lerp(c0.0, c1.0, k), lerp(c0.1, c1.1, k), lerp(c0.2, c1.2, k));
        }
    }
    let l = RAMP[RAMP.len() - 1].1;
    Color::Rgb(l.0, l.1, l.2)
}

/// Colour for a bare number, matched to where that value sits on the ramp so
/// figures and bars agree.
pub fn value_colour(pct: f64) -> Color {
    if pct <= 0.5 {
        DIM
    } else {
        ramp(pct / 100.0)
    }
}

pub const TRACK: Color = Color::Rgb(58, 66, 80);
pub const CHROME: Color = Color::Rgb(68, 78, 92);
pub const DIM: Color = Color::Rgb(108, 120, 136);
pub const LABEL: Color = Color::Rgb(150, 163, 180);
pub const TEXT: Color = Color::Rgb(201, 211, 223);
pub const TITLE: Color = Color::Rgb(158, 216, 224); // pastel cyan
pub const BLUE: Color = Color::Rgb(168, 197, 235); // pastel blue
pub const MINT: Color = Color::Rgb(167, 230, 170);
pub const WARN: Color = Color::Rgb(238, 155, 155);
pub const AMBER: Color = Color::Rgb(242, 216, 158);
pub const VIOLET: Color = Color::Rgb(198, 180, 236);
/// Background of the selected process row: one step above the ground, below
/// the track, so the row is found at a glance without shouting.
pub const SELECT: Color = Color::Rgb(40, 47, 60);

pub const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
/// Vertical eighths, for the graphs.
pub const VEIGHTHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
pub const TRACK_CH: &str = "░";

/// Nerd Font glyphs, all from the Font Awesome range that Nerd Fonts has
/// carried since v1, so they survive most patched fonts.
///
/// Every bar row carries exactly one glyph. That matters: if a terminal renders
/// these as double-width, all rows shift by the same amount and the columns
/// still line up. A row without a glyph would break that.
#[derive(Clone, Copy)]
pub struct Icons {
    pub cpu: &'static str,
    pub user: &'static str,
    pub system: &'static str,
    pub iowait: &'static str,
    pub steal: &'static str,
    pub clock: &'static str,
    pub temp: &'static str,
    pub memory: &'static str,
    pub cache: &'static str,
    pub arc: &'static str,
    pub swap: &'static str,
    pub disk: &'static str,
    pub net: &'static str,
    pub down: &'static str,
    pub up: &'static str,
    pub procs: &'static str,
    pub filter: &'static str,
    pub history: &'static str,
    pub load: &'static str,
    pub uptime: &'static str,
    pub power: &'static str,
    pub battery: &'static str,
    pub plug: &'static str,
}

pub const NERD: Icons = Icons {
    cpu: "\u{f2db}",
    user: "\u{f007}",
    system: "\u{f085}",
    iowait: "\u{f251}",
    steal: "\u{f0ec}",
    clock: "\u{f017}",
    temp: "\u{f2c9}",
    memory: "\u{f1b2}",
    cache: "\u{f1c0}",
    arc: "\u{f1c0}",
    swap: "\u{f0ec}",
    disk: "\u{f0a0}",
    net: "\u{f0ac}",
    down: "\u{f063}",
    up: "\u{f062}",
    procs: "\u{f0ae}",
    filter: "\u{f0b0}",
    history: "\u{f1da}",
    load: "\u{f0e4}",
    uptime: "\u{f251}",
    power: "\u{f0e7}",
    battery: "\u{f240}",
    plug: "\u{f1e6}",
};

/// Fallback for a terminal without a patched font. Same column width, so the
/// layout does not move when icons are switched off.
pub const PLAIN: Icons = Icons {
    cpu: "*",
    user: "u",
    system: "s",
    iowait: "w",
    steal: "x",
    clock: "F",
    temp: "T",
    memory: "M",
    cache: "C",
    arc: "Z",
    swap: "S",
    disk: "D",
    net: "N",
    down: "v",
    up: "^",
    procs: "P",
    filter: "/",
    history: "-",
    load: "L",
    uptime: "U",
    power: "W",
    battery: "B",
    plug: "+",
};

pub fn icons(enabled: bool) -> Icons {
    if enabled {
        NERD
    } else {
        PLAIN
    }
}
