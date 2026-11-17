//! Rendering.
//!
//! Bars and graphs are written straight into the frame buffer, cell by cell,
//! as igtop does: a label, bar and figure share one row, and each cell of a
//! bar gets its own colour from the ramp — a stock gauge paints one flat colour.

use crate::app::{App, Damped, Disk, Net};
use crate::config::{self, Boxes};
use crate::procs::{self, Proc, Row, SortKey};
use crate::theme::*;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

/// A window drawn over everything else.
pub enum Popup<'a> {
    Signal(&'a Proc),
    Renice(&'a Proc, i32),
    Help { vim: bool },
    /// The disks menu: (name, filesystem, shown), in order, and the cursor.
    Disks { items: Vec<(String, String, bool)>, cursor: usize },
}

pub struct View<'a> {
    pub app: &'a App,
    pub smooth: bool,
    pub icons: bool,
    pub interval_ms: u64,
    pub paused: bool,
    /// Filtered and sorted (or arranged as a tree).
    pub rows: &'a [Row<'a>],
    pub selected: usize,
    pub offset: usize,
    pub sort: SortKey,
    pub desc: bool,
    pub tree: bool,
    pub per_core: bool,
    pub filter: &'a str,
    pub editing: bool,
    pub net: Option<usize>,
    /// The chosen network graph scale in Mbit/s, before the link-speed cap.
    pub net_scale: Option<u64>,
    pub popup: Option<Popup<'a>>,
    pub status: Option<(&'a str, bool)>,
    pub kernel_threads: bool,
    pub list_paused: bool,
    pub boxes: Boxes,
    pub clock: Option<String>,
    /// The disks box's rows: shown ones only, in the configured order.
    pub disks: Vec<&'a Disk>,
}

/// Where things landed on screen, for the mouse and for paging.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hit {
    /// The process table's data rows: first row's area, one line per row.
    pub rows: Rect,
}

// ─────────────────────────── formatting ───────────────────────────

/// Compact byte formatting: the value column is narrow and `5.94 GiB` reads no
/// better than `5.94G` at a glance.
pub fn short_bytes(b: u64) -> String {
    const U: [(u64, &str); 5] = [(1 << 40, "T"), (1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K"), (1, "B")];
    for (div, suf) in U {
        if b >= div {
            let v = b as f64 / div as f64;
            return if v >= 100.0 || div == 1 {
                format!("{:.0}{}", v, suf)
            } else if v >= 10.0 {
                format!("{:.1}{}", v, suf)
            } else {
                format!("{:.2}{}", v, suf)
            };
        }
    }
    "0B".into()
}

pub fn rate(bps: f64) -> String {
    format!("{}/s", short_bytes(bps.max(0.0) as u64))
}

pub fn duration(s: f64) -> String {
    let s = s.max(0.0) as u64;
    let (d, h, m) = (s / 86_400, s / 3600 % 24, s / 60 % 60);
    if d > 0 {
        format!("{}d {}h", d, h)
    } else if h > 0 {
        format!("{}h {:02}m", h, m)
    } else {
        format!("{}m {:02}s", m, s % 60)
    }
}

/// CPU time as top's TIME+ column reads it, compacted past a day.
pub fn cpu_time(s: f64) -> String {
    let cs = (s.max(0.0) * 100.0) as u64;
    let secs = cs / 100;
    if secs >= 86_400 {
        format!("{}d{:02}h", secs / 86_400, secs / 3600 % 24)
    } else if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, secs / 60 % 60)
    } else {
        format!("{}:{:02}.{:02}", secs / 60, secs % 60, cs % 100)
    }
}

fn si(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if v >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{:.0}", v)
    }
}

/// Keeps the end of a path, which is the part that tells mounts apart.
fn tail(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        s.to_string()
    } else {
        let keep: String = s.chars().skip(n + 1 - w).collect();
        format!("…{}", keep)
    }
}

// ─────────────────────────── primitives ───────────────────────────

/// Writes `s` at (x, y), clipped to `w` cells. Returns the x after it.
fn put(buf: &mut Buffer, x: u16, y: u16, w: usize, s: &str, style: Style) -> u16 {
    if w == 0 {
        return x;
    }
    buf.set_stringn(x, y, s, w, style).0
}

/// Spans laid end to end, right-aligned so the last ends at `right`.
fn put_right(buf: &mut Buffer, right: u16, y: u16, parts: &[(String, Color)]) {
    let total: usize = parts.iter().map(|(s, _)| s.chars().count()).sum();
    let mut x = right.saturating_sub(total as u16);
    for (s, c) in parts {
        x = put(buf, x, y, right.saturating_sub(x) as usize, s, Style::default().fg(*c));
    }
}

/// The ramp colour of every cell along a bar `width` cells long.
fn ramp_cells(width: usize) -> Vec<Color> {
    let span = (width.saturating_sub(1)).max(1) as f64;
    (0..width).map(|i| ramp(i as f64 / span)).collect()
}

/// A gradient bar `colours.len()` cells long, the tip an eighth-block for
/// sub-cell resolution, the rest a dotted track.
fn bar(buf: &mut Buffer, x: u16, y: u16, pct: f64, colours: &[Color], room: usize) {
    let bw = colours.len();
    let filled = (pct / 100.0 * bw as f64 * 8.0).round().max(0.0) as usize;
    let full = (filled / 8).min(bw);
    let rem = filled % 8;
    for (i, &colour) in colours.iter().enumerate().take(room) {
        let (sym, fg) = if i < full {
            ("█", colour)
        } else if i == full && rem > 0 {
            (EIGHTHS[rem - 1], colour)
        } else {
            (TRACK_CH, TRACK)
        };
        if let Some(c) = buf.cell_mut((x + i as u16, y)) {
            c.set_symbol(sym).set_fg(fg);
        }
    }
}

/// A bar row: glyph, label, name, bar, right-aligned value.
struct BarRow {
    icon: &'static str,
    icon_colour: Color,
    class: String,
    name: String,
    pct: f64,
    /// Drawn instead of the bar: a one-line history, 0-100, oldest first.
    hist: Option<Vec<f64>>,
    value: Vec<(String, Color)>,
}

/// Column widths shared by every row of one block, so bars line up.
struct Cols {
    class: usize,
    name: usize,
    value: usize,
}

impl Cols {
    fn bar_width(&self, area: Rect) -> usize {
        (area.width as usize).saturating_sub(4 + self.class + self.name + self.value + 2).max(6)
    }
}

fn render_bar_rows(buf: &mut Buffer, area: Rect, rows: &[BarRow], cols: &Cols) {
    let bw = cols.bar_width(area);
    let colours = ramp_cells(bw);
    let right = area.right();
    let room = |x: u16| right.saturating_sub(x) as usize;
    for (row, r) in rows.iter().enumerate() {
        let y = area.y + row as u16;
        if y >= area.bottom() {
            break;
        }
        let x = area.x + 2;
        let x = put(buf, x, y, room(x), r.icon, Style::default().fg(r.icon_colour)) + 1;
        let x = put(
            buf,
            x,
            y,
            room(x),
            &format!("{:<w$}", r.class, w = cols.class),
            Style::default().fg(LABEL).add_modifier(Modifier::BOLD),
        );
        let x = put(buf, x, y, room(x), &format!("{:<w$}", r.name, w = cols.name), Style::default().fg(DIM));
        let x = x + 1;
        match &r.hist {
            Some(h) => {
                // The empty track first, so the history reads as a bar's twin.
                bar(buf, x, y, 0.0, &colours, room(x));
                let width = bw.min(room(x)) as u16;
                let data: Vec<f64> = h.iter().map(|v| v / 100.0).collect();
                graph(buf, Rect { x, y, width, height: 1 }, &data, true);
            }
            None => bar(buf, x, y, r.pct, &colours, room(x)),
        }
        put_right(buf, right.saturating_sub(1), y, &r.value);
    }
}

/// A filled area graph, newest column at the right edge, growing leftwards
/// until full. Each column is coloured by its own height from the bar ramp,
/// so a spike is the colour a bar would be at that level.
///
/// `up` grows from the bottom; otherwise from the top, for the mirrored half
/// of the network graph. A partial cell hanging from the top is drawn as the
/// complementary lower block in reverse video: the glyph takes the terminal's
/// background and the rest of the cell the colour.
fn graph(buf: &mut Buffer, area: Rect, data: &[f64], up: bool) {
    let h = area.height as usize;
    if h == 0 || area.width == 0 {
        return;
    }
    let n = data.len().min(area.width as usize);
    let data = &data[data.len() - n..];
    let x0 = area.right() - n as u16;
    for (i, &v) in data.iter().enumerate() {
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let mut total = (v * h as f64 * 8.0).round() as usize;
        // Anything at all shows as a hairline: an idle link and a quiet one
        // should not look the same.
        if v > 0.0 {
            total = total.max(1);
        }
        let colour = ramp(v);
        let x = x0 + i as u16;
        for r in 0..h {
            let fill = total.saturating_sub(r * 8).min(8);
            if fill == 0 {
                break;
            }
            let y = if up { area.bottom() - 1 - r as u16 } else { area.y + r as u16 };
            let Some(c) = buf.cell_mut((x, y)) else { continue };
            if fill == 8 {
                c.set_symbol("█").set_fg(colour);
            } else if up {
                c.set_symbol(VEIGHTHS[fill - 1]).set_fg(colour);
            } else {
                c.set_symbol(VEIGHTHS[7 - fill])
                    .set_style(Style::default().fg(colour).bg(Color::Reset).add_modifier(Modifier::REVERSED));
            }
        }
    }
}

fn boxed<'a>(title: Vec<Span<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CHROME))
        .title(Line::from(title))
}

fn rule<'a>(title: Vec<Span<'a>>) -> Block<'a> {
    Block::default().borders(Borders::TOP).border_style(Style::default().fg(TRACK)).title(Line::from(title))
}

fn divider(buf: &mut Buffer, area: Rect) {
    let w = (area.width as usize).saturating_sub(4);
    put(buf, area.x + 2, area.y, w, &"╌".repeat(w), Style::default().fg(TRACK));
}

fn dim(s: impl Into<String>) -> (String, Color) {
    (s.into(), DIM)
}

fn title_span<'a>(icon: &'a str, name: &'a str) -> [Span<'a>; 2] {
    [
        Span::styled(format!(" {} ", icon), Style::default().fg(TITLE)),
        Span::styled(name, Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
    ]
}

// ───────────────────────────── frame ─────────────────────────────

pub fn draw(f: &mut Frame, v: &View) -> Hit {
    let area = f.area();
    let b = v.boxes;

    let mem_rows = mem_rows(v).len() as u16;
    let disks = v.disks.len().min(6) as u16;
    let mid_h = ((3 + mem_rows) + (2 + disks.max(1))).max(12);
    let mid = b.mem || b.net;

    // The last box shown takes whatever height is left.
    let mut cons = vec![Constraint::Length(1)];
    let last_is = |which: u8| match which {
        0 => !mid && !b.proc,
        1 => !b.proc,
        _ => true,
    };
    if b.cpu {
        cons.push(if last_is(0) { Constraint::Min(0) } else { Constraint::Length(cpu_height(v, area.width)) });
    }
    if mid {
        cons.push(if last_is(1) { Constraint::Min(0) } else { Constraint::Length(mid_h) });
    }
    if b.proc {
        cons.push(Constraint::Min(6));
    }
    let parts = Layout::vertical(cons).split(area);
    draw_header(f, parts[0], v);

    let mut i = 1;
    if b.cpu {
        draw_cpu(f, parts[i], v);
        i += 1;
    }
    if mid {
        let cols: Vec<Rect> = match (b.mem, b.net) {
            (true, true) => Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(parts[i])
                .to_vec(),
            _ => vec![parts[i]],
        };
        if b.mem {
            let left = Layout::vertical([Constraint::Length(3 + mem_rows), Constraint::Min(3)]).split(cols[0]);
            draw_mem(f, left[0], v);
            draw_disks(f, left[1], v);
        }
        if b.net {
            draw_net(f, *cols.last().unwrap(), v);
        }
        i += 1;
    }
    let mut hit = Hit::default();
    if b.proc {
        hit = draw_procs(f, parts[i], v);
    }

    match &v.popup {
        Some(Popup::Signal(p)) => draw_signal(f, area, p),
        Some(Popup::Renice(p, n)) => draw_renice(f, area, p, *n),
        Some(Popup::Help { vim }) => draw_help(f, area, *vim),
        Some(Popup::Disks { items, cursor }) => draw_disk_menu(f, area, items, *cursor),
        None => {}
    }
    hit
}

fn draw_header(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let mut spans = vec![
        Span::styled(" istop", Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", a.host), Style::default().fg(TEXT)),
    ];
    if let Some(c) = &a.own_container {
        spans.push(Span::styled(format!(" ({})", c), Style::default().fg(BLUE)));
    }
    if let Some(t) = &v.clock {
        spans.push(Span::styled(format!("  {} ", ic.clock), Style::default().fg(DIM)));
        spans.push(Span::styled(t.clone(), Style::default().fg(TEXT)));
    }
    spans.push(Span::styled(format!("  {} ", ic.uptime), Style::default().fg(DIM)));
    spans.push(Span::styled(format!("up {}", duration(a.uptime_s)), Style::default().fg(TEXT)));
    spans.push(Span::styled(format!("  {} ms", v.interval_ms), Style::default().fg(DIM)));
    if let Some(bat) = &a.battery {
        let charging = bat.status == "Charging" || bat.status == "Full";
        spans.push(Span::styled(
            format!("  {} ", if charging { ic.plug } else { ic.battery }),
            Style::default().fg(if charging { MINT } else { AMBER }),
        ));
        // Coloured as a gauge that is good when full: the ramp, reversed.
        spans.push(Span::styled(format!("{:.0}%", bat.pct), Style::default().fg(value_colour(100.0 - bat.pct))));
        if let Some(w) = bat.watts {
            spans.push(Span::styled(format!(" {:.1} W", w), Style::default().fg(DIM)));
        }
        if let Some(s) = bat.secs.filter(|s| *s > 60.0 && *s < 30.0 * 86_400.0) {
            spans.push(Span::styled(format!(" {}", duration(s)), Style::default().fg(DIM)));
        }
    }
    if !v.smooth {
        spans.push(Span::styled("  raw", Style::default().fg(AMBER)));
    }
    if v.paused {
        spans.push(Span::styled("  paused", Style::default().fg(WARN).add_modifier(Modifier::BOLD)));
    }
    if let Some((msg, err)) = v.status {
        spans.push(Span::styled(
            format!("  {}", msg),
            Style::default().fg(if err { WARN } else { MINT }).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("   q quit  ? help", Style::default().fg(CHROME)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ────────────────────────────── CPU ──────────────────────────────

const CORE_CELL_MIN: usize = 18;
const HISTORY_H: u16 = 5;

fn cpu_rows(v: &View) -> (Vec<BarRow>, usize) {
    let a = v.app;
    let ic = icons(v.icons);
    let [total, user, system, iowait, steal] = a.cpu_bars.map(|d| d.get(v.smooth));
    let s = &a.share;
    let pct_row = |icon, colour, class: &str, shown: f64, raw: f64| BarRow {
        icon,
        icon_colour: colour,
        class: class.into(),
        name: String::new(),
        pct: shown,
        hist: None,
        value: vec![(format!("{:.1} %", raw), value_colour(raw))],
    };
    let mut rows = vec![
        pct_row(ic.cpu, TITLE, "CPU", total, s.busy),
        pct_row(ic.user, MINT, "USER", user, s.user),
        pct_row(ic.system, VIOLET, "SYSTEM", system, s.system),
        pct_row(ic.iowait, AMBER, "IOWAIT", iowait, s.iowait),
        pct_row(ic.steal, BLUE, "STEAL", steal, s.steal),
    ];
    let breakdown = rows.len();
    if let Some(t) = a.temp_c {
        rows.push(BarRow {
            icon: ic.temp,
            icon_colour: MINT,
            class: "TEMP".into(),
            name: String::new(),
            pct: a.temp_bar.get(v.smooth),
            hist: None,
            value: vec![(format!("{:.0} / {:.0} °C", t, a.temp_crit_c), TEXT)],
        });
    }
    if let (Some(w), Some(l)) = (a.cpu_watts, a.cpu_limit_w) {
        rows.push(BarRow {
            icon: ic.power,
            icon_colour: AMBER,
            class: "POWER".into(),
            name: String::new(),
            pct: a.watts_bar.get(v.smooth),
            hist: None,
            value: vec![(format!("{:.1} / {:.0} W", w, l), TEXT)],
        });
    }
    if let (Some(fq), Some(max)) = (a.freq_mhz, a.freq_max_mhz) {
        rows.push(BarRow {
            icon: ic.clock,
            icon_colour: TITLE,
            class: "CLOCK".into(),
            name: String::new(),
            pct: a.freq_bar.get(v.smooth),
            hist: None,
            value: vec![(format!("{:.0} / {:.0} MHz", fq, max), TEXT)],
        });
    }
    (rows, breakdown)
}

/// Rows the bar column needs: the breakdown, and the capacity gauges under a
/// divider when there are any.
fn cpu_bar_lines(v: &View) -> usize {
    let (rows, breakdown) = cpu_rows(v);
    if rows.len() > breakdown {
        rows.len() + 1
    } else {
        rows.len()
    }
}

/// Columns and rows of the per-core grid in a pane `width` wide, aiming to be
/// no taller than the bar column beside it.
fn core_grid(n: usize, width: usize, target_rows: usize) -> (usize, usize) {
    if n == 0 {
        return (1, 0);
    }
    let max_cols = (width / CORE_CELL_MIN).max(1);
    let cols = n.div_ceil(target_rows.max(1)).clamp(1, max_cols);
    (cols, n.div_ceil(cols))
}

/// The bars sit beside the core grid on a wide pane, above it on a narrow one.
fn cpu_split(width: u16) -> bool {
    width >= 110
}

fn cpu_body_lines(v: &View, width: u16) -> usize {
    let inner_w = width.saturating_sub(2) as usize;
    let lines = cpu_bar_lines(v);
    let n = v.app.cores.len();
    if cpu_split(width) {
        let (_, rows) = core_grid(n, inner_w * 45 / 100, lines);
        lines.max(rows)
    } else {
        let (_, rows) = core_grid(n, inner_w, 4);
        lines + 1 + rows
    }
}

fn cpu_height(v: &View, width: u16) -> u16 {
    2 + 2 + cpu_body_lines(v, width) as u16 + 1 + HISTORY_H
}

fn draw_cpu(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let mut title = title_span(ic.cpu, &a.cpu_model).to_vec();
    title.push(Span::styled(format!(" · {} threads ", a.threads), Style::default().fg(DIM)));
    let block = boxed(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 || inner.width < 30 {
        return;
    }

    let body = cpu_body_lines(v, area.width) as u16;
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(body),
        // The history takes whatever is left, so a CPU box shown alone
        // becomes mostly graph.
        Constraint::Min(0),
    ])
    .split(inner);
    draw_cpu_summary(f, parts[0], v);

    let lines = cpu_bar_lines(v);
    let (bars_area, cores_area) = if cpu_split(area.width) {
        let s = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(parts[1]);
        (s[0], s[1])
    } else {
        let s = Layout::vertical([Constraint::Length(lines as u16 + 1), Constraint::Min(0)]).split(parts[1]);
        (s[0], s[1])
    };

    let (rows, breakdown) = cpu_rows(v);
    let cols = Cols { class: 8, name: 0, value: 17 };
    let buf = f.buffer_mut();
    render_bar_rows(buf, Rect { height: breakdown as u16, ..bars_area }, &rows[..breakdown], &cols);
    if rows.len() > breakdown && bars_area.height as usize > breakdown + 1 {
        divider(buf, Rect { y: bars_area.y + breakdown as u16, height: 1, ..bars_area });
        let rest = Rect {
            y: bars_area.y + breakdown as u16 + 1,
            height: bars_area.height.saturating_sub(breakdown as u16 + 1),
            ..bars_area
        };
        render_bar_rows(buf, rest, &rows[breakdown..], &cols);
    }
    draw_cores(buf, cores_area, &a.cores, v.smooth, lines);

    if parts[2].height > 1 {
        draw_cpu_history(f, parts[2], v);
    }
}

fn draw_cpu_summary(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let figure = |label: &str, value: String, colour: Color| {
        [Span::styled(label.to_string(), Style::default().fg(DIM)), Span::styled(value, Style::default().fg(colour))]
    };
    // Load is judged per thread: 8.0 is idle on 64 threads and saturated on 8.
    let per = |l: f64| value_colour(l / a.threads as f64 * 100.0);
    let mut spans = vec![Span::raw("  "), Span::styled(format!("{} ", ic.load), Style::default().fg(DIM))];
    spans.extend(figure("load ", format!("{:.2}", a.load[0]), per(a.load[0])));
    spans.push(Span::styled(format!(" {:.2}", a.load[1]), Style::default().fg(per(a.load[1]))));
    spans.push(Span::styled(format!(" {:.2}", a.load[2]), Style::default().fg(per(a.load[2]))));
    spans.extend(figure("   run ", a.running.to_string(), TEXT));
    spans.extend(figure("   blocked ", a.blocked.to_string(), if a.blocked > 0 { AMBER } else { TEXT }));
    spans.extend(figure("   procs ", a.procs.list.len().to_string(), TEXT));
    spans.extend(figure("   threads ", a.kthreads.to_string(), TEXT));
    spans.extend(figure("   ctx ", format!("{}/s", si(a.ctxt_per_s)), TEXT));
    spans.extend(figure("   irq ", format!("{}/s", si(a.intr_per_s)), TEXT));
    // Figures that have no ceiling to draw a bar against.
    if a.freq_max_mhz.is_none() {
        if let Some(fq) = a.freq_mhz {
            spans.extend(figure("   clock ", format!("{:.0} MHz", fq), TEXT));
        }
    }
    if a.cpu_limit_w.is_none() {
        if let Some(w) = a.cpu_watts {
            spans.extend(figure("   power ", format!("{:.1} W", w), TEXT));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_cores(buf: &mut Buffer, area: Rect, cores: &[Damped], smooth: bool, target_rows: usize) {
    let n = cores.len();
    if n == 0 || area.width < 12 || area.height == 0 {
        return;
    }
    let width = area.width.saturating_sub(2) as usize;
    let (cols, rows) = core_grid(n, width, target_rows);
    let cell_w = width / cols;
    let label_w = (n - 1).to_string().len();
    // Label, space, bar, space, "100%", gap.
    let bw = cell_w.saturating_sub(label_w + 1 + 1 + 4 + 2).max(3);
    let colours = ramp_cells(bw);
    for (i, d) in cores.iter().enumerate() {
        // Down the columns first, so CPU numbers run down the page as lscpu
        // lists them.
        let (col, row) = (i / rows, i % rows);
        if row >= area.height as usize {
            continue;
        }
        let x = area.x + 2 + (col * cell_w) as u16;
        let y = area.y + row as u16;
        let x = put(buf, x, y, label_w, &format!("{:>w$}", i, w = label_w), Style::default().fg(DIM)) + 1;
        bar(buf, x, y, d.get(smooth), &colours, bw);
        let raw = d.raw;
        put(buf, x + bw as u16 + 1, y, 4, &format!("{:>3.0}%", raw), Style::default().fg(value_colour(raw)));
    }
}

fn draw_cpu_history(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let w = area.width.saturating_sub(2) as usize;
    let data: Vec<f64> = a.cpu_hist.last(w).collect();
    let peak = data.iter().copied().fold(0.0, f64::max);
    let secs = data.len();
    let span = if secs >= 60 { format!("{}m{:02}s", secs / 60, secs % 60) } else { format!("{}s", secs) };
    let block = rule(vec![
        Span::styled(format!(" {} ", ic.history), Style::default().fg(DIM)),
        Span::styled("cpu", Style::default().fg(LABEL)),
        Span::styled(format!("  peak {:.0}%  · last {}  ", peak, span), Style::default().fg(DIM)),
    ]);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let norm: Vec<f64> = data.iter().map(|v| v / 100.0).collect();
    graph(f.buffer_mut(), Rect { width: inner.width.saturating_sub(1), ..inner }, &norm, true);
}

// ─────────────────────────── memory ───────────────────────────

fn mem_rows(v: &View) -> Vec<BarRow> {
    let a = v.app;
    let m = &a.mem;
    let ic = icons(v.icons);
    let pair = |used: u64, total: u64| vec![(format!("{} / {}", short_bytes(used), short_bytes(total)), TEXT)];
    let mut rows = vec![
        BarRow {
            icon: ic.memory,
            icon_colour: BLUE,
            class: "USED".into(),
            name: String::new(),
            pct: a.mem_bars[0].get(v.smooth),
            hist: None,
            value: pair(m.used(), m.total),
        },
        BarRow {
            icon: ic.cache,
            icon_colour: TITLE,
            class: "CACHE".into(),
            name: String::new(),
            pct: a.mem_bars[1].get(v.smooth),
            hist: None,
            value: pair(m.cache, m.total),
        },
    ];
    if let Some((size, max)) = m.arc {
        rows.push(BarRow {
            icon: ic.arc,
            icon_colour: VIOLET,
            class: "ZFS ARC".into(),
            name: String::new(),
            pct: a.mem_bars[2].get(v.smooth),
            hist: None,
            value: pair(size, max),
        });
    }
    if m.swap_total > 0 {
        rows.push(BarRow {
            icon: ic.swap,
            icon_colour: AMBER,
            class: "SWAP".into(),
            name: String::new(),
            pct: a.mem_bars[3].get(v.smooth),
            hist: None,
            value: pair(m.swap_used(), m.swap_total),
        });
    }
    rows
}

fn draw_mem(f: &mut Frame, area: Rect, v: &View) {
    let m = &v.app.mem;
    let ic = icons(v.icons);
    let mut title = title_span(ic.memory, "memory").to_vec();
    title.push(Span::styled(format!(" · {} ", short_bytes(m.total)), Style::default().fg(DIM)));
    let block = boxed(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let figure = |label: &str, b: u64| {
        [Span::styled(label.to_string(), Style::default().fg(DIM)), Span::styled(short_bytes(b), Style::default().fg(TEXT))]
    };
    let mut spans = vec![Span::raw("  ")];
    spans.extend(figure("available ", m.available));
    spans.extend(figure("   free ", m.free));
    spans.extend(figure("   shared ", m.shmem));
    spans.extend(figure("   dirty ", m.dirty));
    f.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..inner });
    let rest = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
    render_bar_rows(f.buffer_mut(), rest, &mem_rows(v), &Cols { class: 8, name: 0, value: 17 });
}

fn draw_disks(f: &mut Frame, area: Rect, v: &View) {
    let ic = icons(v.icons);
    let mut title = title_span(ic.disk, "disks").to_vec();
    title.push(Span::styled(" · local filesystems · io busy % ", Style::default().fg(DIM)));
    let block = boxed(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let disks = &v.disks;
    if disks.is_empty() {
        let why = if v.app.disks.is_empty() { "   no local filesystems" } else { "   every disk hidden · d to choose" };
        f.render_widget(Paragraph::new(Span::styled(why, Style::default().fg(DIM))), inner);
        return;
    }
    let class_w = disks.iter().map(|d| d.mount.path.chars().count()).max().unwrap_or(4).clamp(4, 16) + 1;
    let fmt_io = |r: Option<f64>| r.map(|b| short_bytes(b as u64)).unwrap_or_else(|| "-".into());
    let rows: Vec<BarRow> = disks
        .iter()
        .map(|d| {
            let io = |b: Option<f64>| if b.unwrap_or(0.0) >= 1.0 { TEXT } else { DIM };
            let busy = d.busy_pct.map(|b| (format!("{:>4.0}%", b), value_colour(b))).unwrap_or_else(|| dim("    -"));
            BarRow {
                icon: ic.disk,
                icon_colour: BLUE,
                class: tail(&d.mount.path, class_w - 1),
                name: d.mount.fstype.chars().take(6).collect(),
                pct: 0.0,
                hist: Some(d.busy_hist.ring.iter().copied().collect()),
                value: vec![
                    dim("r "),
                    (format!("{:>6}", fmt_io(d.read_bps)), io(d.read_bps)),
                    dim("  w "),
                    (format!("{:>6}", fmt_io(d.write_bps)), io(d.write_bps)),
                    busy,
                    (format!("  {:>6} / {:<6}", short_bytes(d.mount.used), short_bytes(d.mount.total)), TEXT),
                ],
            }
        })
        .collect();
    render_bar_rows(f.buffer_mut(), inner, &rows, &Cols { class: class_w, name: 7, value: 41 });
}

// ─────────────────────────── network ───────────────────────────

/// Which interface the graph follows when none is chosen: the busiest
/// physical port over the visible window, else the busiest of any.
pub fn default_net(nets: &[Net]) -> Option<usize> {
    let busy = |n: &Net| n.rx_hist.last(120).chain(n.tx_hist.last(120)).sum::<f64>() + n.rx_bps + n.tx_bps;
    let best = |phys: bool| {
        nets.iter()
            .enumerate()
            .filter(|(_, n)| !phys || n.physical)
            .max_by(|a, b| busy(a.1).total_cmp(&busy(b.1)).then(b.1.name.cmp(&a.1.name)))
            .map(|(i, _)| i)
    };
    best(true).or_else(|| best(false))
}

fn draw_net(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let net = v.net.and_then(|i| a.nets.get(i));
    let mut title = title_span(ic.net, "network").to_vec();
    let fixed = net.and_then(|n| config::net_scale_for(v.net_scale, n.speed_mbit));
    if let Some(n) = net {
        title.push(Span::styled(" · ", Style::default().fg(DIM)));
        title.push(Span::styled(n.name.clone(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)));
        if let Some(s) = n.speed_mbit {
            let speed = if s >= 1000 { format!("{} Gbit/s", s as f64 / 1000.0) } else { format!("{} Mbit/s", s) };
            title.push(Span::styled(format!(" · {}", speed), Style::default().fg(DIM)));
        }
        let scale = fixed.map_or("auto".into(), config::scale_name);
        title.push(Span::styled(format!(" · scale {}", scale), Style::default().fg(DIM)));
        title.push(Span::styled(
            format!("  {}/{}  b n m ", v.net.unwrap_or(0) + 1, a.nets.len()),
            Style::default().fg(CHROME),
        ));
    }
    let block = boxed(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(n) = net else {
        f.render_widget(Paragraph::new(Span::styled("   no interfaces", Style::default().fg(DIM))), inner);
        return;
    };
    if inner.height < 4 || inner.width < 20 {
        return;
    }

    let w = inner.width.saturating_sub(3) as usize;
    let rx: Vec<f64> = n.rx_hist.last(w).collect();
    let tx: Vec<f64> = n.tx_hist.last(w).collect();
    // One scale for both halves, so a download and an upload of the same rate
    // are the same height. Floored at 1 KiB/s, or an idle link's background
    // chatter would fill the graph. A fixed scale (m) is in bits, decimal, as
    // link speeds are; above it the graph is simply full.
    let scale = match fixed {
        Some(mbit) => mbit as f64 * 1e6 / 8.0,
        None => rx.iter().chain(tx.iter()).copied().fold(1024.0, f64::max),
    };

    let half = (inner.height.saturating_sub(2)) / 2;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(half),
        Constraint::Length(half),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);

    let label = |icon: &str, colour: Color, name: &str, now: f64, hist: &[f64], total: u64| {
        let peak = hist.iter().copied().fold(0.0, f64::max);
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{} ", icon), Style::default().fg(colour)),
            Span::styled(format!("{:<4}", name), Style::default().fg(LABEL).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:>10}", rate(now)), Style::default().fg(value_colour(now / scale * 100.0))),
            Span::styled("   peak ", Style::default().fg(DIM)),
            Span::styled(rate(peak), Style::default().fg(TEXT)),
            Span::styled("   total ", Style::default().fg(DIM)),
            Span::styled(short_bytes(total), Style::default().fg(TEXT)),
        ])
    };
    f.render_widget(Paragraph::new(label(ic.down, MINT, "RX", n.rx_bps, &rx, n.rx_total)), rows[0]);
    let buf = f.buffer_mut();
    let g = |r: Rect| Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r };
    graph(buf, g(rows[1]), &rx.iter().map(|b| b / scale).collect::<Vec<_>>(), true);
    graph(buf, g(rows[2]), &tx.iter().map(|b| b / scale).collect::<Vec<_>>(), false);
    f.render_widget(Paragraph::new(label(ic.up, AMBER, "TX", n.tx_bps, &tx, n.tx_total)), rows[3]);
}

// ────────────────────────── processes ──────────────────────────

struct Col {
    key: Option<SortKey>,
    head: &'static str,
    width: usize,
    right: bool,
}

const CPU_BAR: usize = 5;

fn columns() -> [Col; 10] {
    [
        Col { key: Some(SortKey::Pid), head: "PID", width: 8, right: true },
        Col { key: Some(SortKey::Where), head: "WHERE", width: 9, right: false },
        Col { key: Some(SortKey::User), head: "USER", width: 10, right: false },
        // PROCESS takes whatever is left.
        Col { key: Some(SortKey::Name), head: "PROCESS", width: 0, right: false },
        Col { key: Some(SortKey::Nice), head: "NI", width: 3, right: true },
        Col { key: Some(SortKey::Threads), head: "THR", width: 5, right: true },
        Col { key: Some(SortKey::Time), head: "TIME", width: 9, right: true },
        Col { key: Some(SortKey::Cpu), head: "CPU%", width: CPU_BAR + 8, right: true },
        Col { key: Some(SortKey::Mem), head: "MEM", width: 8, right: true },
        Col { key: None, head: "MEM%", width: 6, right: true },
    ]
}

/// Height of the detail box inside the process box.
const DETAIL_H: u16 = 9;

fn draw_procs(f: &mut Frame, area: Rect, v: &View) -> Hit {
    let ic = icons(v.icons);
    let mut title = title_span(ic.procs, "processes").to_vec();
    title.push(Span::styled(format!(" · {} ", v.rows.len()), Style::default().fg(DIM)));
    if v.tree {
        title.push(Span::styled("· tree ", Style::default().fg(TITLE)));
    }
    title.push(Span::styled(
        if v.per_core { "· cpu % of one thread " } else { "· cpu % of the machine " },
        Style::default().fg(DIM),
    ));
    if !v.kernel_threads {
        title.push(Span::styled("· kernel threads hidden ", Style::default().fg(DIM)));
    }
    if v.list_paused {
        title.push(Span::styled("· list paused ", Style::default().fg(WARN).add_modifier(Modifier::BOLD)));
    }
    if v.editing || !v.filter.is_empty() {
        title.push(Span::styled(format!("{} ", ic.filter), Style::default().fg(AMBER)));
        title.push(Span::styled(
            format!("{}{} ", v.filter, if v.editing { "▏" } else { "" }),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ));
    }
    let block = boxed(title);
    let mut inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 || inner.width < 40 {
        return Hit::default();
    }

    if let Some(d) = v.app.detail.as_ref() {
        if inner.height > DETAIL_H + 4 {
            draw_detail(f, Rect { height: DETAIL_H, ..inner }, d, v);
            inner = Rect { y: inner.y + DETAIL_H, height: inner.height - DETAIL_H, ..inner };
        }
    }

    let cols = columns();
    let gaps = cols.len() - 1;
    let fixed: usize = cols.iter().map(|c| c.width).sum::<usize>() + gaps + 2;
    let flex = (inner.width as usize).saturating_sub(fixed).max(8);
    let widths: Vec<usize> = cols.iter().map(|c| if c.width == 0 { flex } else { c.width }).collect();
    let xs: Vec<u16> = widths
        .iter()
        .scan(inner.x + 1, |x, w| {
            let at = *x;
            *x += *w as u16 + 1;
            Some(at)
        })
        .collect();

    let buf = f.buffer_mut();
    let y = inner.y;
    for (i, c) in cols.iter().enumerate() {
        let sorted = c.key == Some(v.sort);
        let head = if sorted { format!("{}{}", c.head, if v.desc { "▼" } else { "▲" }) } else { c.head.to_string() };
        let s = if c.right { format!("{:>w$}", head, w = widths[i]) } else { format!("{:<w$}", head, w = widths[i]) };
        let style = if sorted { Style::default().fg(TITLE).add_modifier(Modifier::BOLD) } else { Style::default().fg(DIM) };
        put(buf, xs[i], y, widths[i], &s, style);
    }

    let page = (inner.height - 1) as usize;
    let hit = Hit { rows: Rect { y: inner.y + 1, height: page as u16, ..inner } };
    let colours = ramp_cells(CPU_BAR);
    let threads = v.app.threads.max(1) as f64;
    for (row, r) in v.rows.iter().skip(v.offset).take(page).enumerate() {
        let p = r.p;
        let y = inner.y + 1 + row as u16;
        let selected = v.offset + row == v.selected;
        if selected {
            buf.set_style(Rect { x: inner.x, y, width: inner.width, height: 1 }, Style::default().bg(SELECT));
        }
        let bold = |s: Style| if selected { s.add_modifier(Modifier::BOLD) } else { s };
        let right = |i: usize, s: String| format!("{:>w$}", s, w = widths[i]);
        // A collapsed node stands for its subtree: its figures are the sums.
        let (cpu_raw, rss) = r.sub.unwrap_or((p.cpu_pct, p.rss));
        let cpu = if v.per_core { cpu_raw } else { cpu_raw / threads };
        let mem_pct = if r.sub.is_some() && v.app.mem.total > 0 {
            rss as f64 / v.app.mem.total as f64 * 100.0
        } else {
            p.mem_pct
        };

        put(buf, xs[0], y, widths[0], &right(0, p.pid.to_string()), Style::default().fg(DIM));
        put(
            buf,
            xs[1],
            y,
            widths[1],
            p.container.as_deref().unwrap_or("host"),
            Style::default().fg(if p.container.is_some() { BLUE } else { DIM }),
        );
        let user_colour = if p.user == "root" { AMBER } else { LABEL };
        put(buf, xs[2], y, widths[2], &p.user, Style::default().fg(user_colour));

        // Tree connectors, a fold marker, then the name in full colour and
        // the arguments dimmed after it; kernel threads in brackets, as ps
        // shows them.
        let mut x = xs[3];
        let end = xs[3] + widths[3] as u16;
        if !r.prefix.is_empty() {
            x = put(buf, x, y, (end - x) as usize, &r.prefix, Style::default().fg(TRACK));
        }
        if v.tree && r.children > 0 {
            let mark = if r.collapsed { format!("+{} ", r.children) } else { "- ".into() };
            x = put(buf, x, y, (end - x) as usize, &mark, Style::default().fg(if r.collapsed { AMBER } else { CHROME }));
        }
        let name = if p.kernel { format!("[{}]", p.comm) } else { p.comm.clone() };
        let name_colour = if p.kernel {
            DIM
        } else {
            match p.state {
                'R' => MINT,
                'D' => AMBER,
                'Z' | 'T' => WARN,
                _ => TEXT,
            }
        };
        x = put(buf, x, y, (end.saturating_sub(x)) as usize, &name, bold(Style::default().fg(name_colour)));
        if !p.args.is_empty() && x + 1 < end {
            put(buf, x + 1, y, (end - x - 1) as usize, &p.args, Style::default().fg(DIM));
        }

        let ni_colour = match p.nice {
            n if n < 0 => AMBER,
            n if n > 0 => BLUE,
            _ => DIM,
        };
        put(buf, xs[4], y, widths[4], &right(4, p.nice.to_string()), Style::default().fg(ni_colour));
        put(buf, xs[5], y, widths[5], &right(5, p.threads.to_string()), Style::default().fg(DIM));
        put(buf, xs[6], y, widths[6], &right(6, cpu_time(p.cpu_time_s)), Style::default().fg(DIM));

        // A small bar, against one thread or the machine, and the figure.
        bar(buf, xs[7], y, cpu.min(100.0), &colours, CPU_BAR);
        let text = if cpu >= 99.95 { format!("{:.0}", cpu) } else { format!("{:.1}", cpu) };
        put(
            buf,
            xs[7] + CPU_BAR as u16,
            y,
            widths[7] - CPU_BAR,
            &format!("{:>w$}", text, w = widths[7] - CPU_BAR),
            bold(Style::default().fg(value_colour(cpu.min(100.0)))),
        );
        put(buf, xs[8], y, widths[8], &right(8, short_bytes(rss)), Style::default().fg(TEXT));
        put(buf, xs[9], y, widths[9], &right(9, format!("{:.1}", mem_pct)), Style::default().fg(value_colour(mem_pct)));
    }
    if v.rows.is_empty() {
        put(buf, inner.x + 3, inner.y + 1, inner.width as usize, "no process matches", Style::default().fg(DIM));
    }
    hit
}

fn draw_detail(f: &mut Frame, area: Rect, d: &procs::Detail, v: &View) {
    let p = &d.last;
    let ic = icons(v.icons);
    let halves = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);

    // Left: this process's CPU over time, against one thread.
    let w = halves[0].width.saturating_sub(4) as usize;
    let data: Vec<f64> = d.cpu_hist.iter().skip(d.cpu_hist.len().saturating_sub(w)).copied().collect();
    let peak = data.iter().copied().fold(0.0, f64::max);
    // A multi-threaded process can exceed one thread; the graph scales to it.
    let top = peak.max(100.0);
    let gblock = rule(vec![
        Span::styled(format!(" {} ", ic.history), Style::default().fg(DIM)),
        Span::styled(format!("{} ", p.comm), Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" peak {:.0}% · scale {:.0}%  ", peak, top), Style::default().fg(DIM)),
    ]);
    let ginner = gblock.inner(Rect { x: halves[0].x + 1, width: halves[0].width.saturating_sub(2), ..halves[0] });
    f.render_widget(gblock, Rect { x: halves[0].x + 1, width: halves[0].width.saturating_sub(2), ..halves[0] });
    graph(f.buffer_mut(), ginner, &data.iter().map(|c| c / top).collect::<Vec<_>>(), true);

    // Right: the facts.
    let k = |s: &str| Span::styled(format!("{:<9}", s), Style::default().fg(DIM));
    let t = |s: String| Span::styled(s, Style::default().fg(TEXT));
    let gap = || Span::raw("   ");
    let uptime = v.app.uptime_s;
    let started = (uptime - p.start as f64 / v.app.procs.clk_tck).max(0.0);
    let status = if d.alive {
        Span::styled(procs::state_name(p.state), Style::default().fg(if p.state == 'R' { MINT } else { TEXT }))
    } else {
        Span::styled("exited", Style::default().fg(WARN).add_modifier(Modifier::BOLD))
    };
    let io = match (d.io, d.io_rate) {
        (Some((r, w)), rates) => {
            let (rr, wr) = rates.unwrap_or((0.0, 0.0));
            format!("read {} ({})   written {} ({})", short_bytes(r), rate(rr), short_bytes(w), rate(wr))
        }
        (None, _) => "not readable — needs root".into(),
    };
    let lines = vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{} ", p.pid), Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
            Span::styled(p.comm.clone(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            status,
            Span::styled("   Enter closes", Style::default().fg(CHROME)),
        ]),
        Line::from(vec![
            Span::raw(" "),
            k("parent"),
            t(format!("{} {}", p.ppid, d.parent.as_deref().unwrap_or("?"))),
            gap(),
            k("user"),
            t(p.user.clone()),
            gap(),
            k("where"),
            t(p.container.clone().unwrap_or_else(|| "host".into())),
        ]),
        Line::from(vec![
            Span::raw(" "),
            k("running"),
            t(duration(started)),
            gap(),
            k("cpu time"),
            t(cpu_time(p.cpu_time_s)),
            gap(),
            k("nice"),
            t(p.nice.to_string()),
            gap(),
            k("threads"),
            t(p.threads.to_string()),
        ]),
        Line::from(vec![
            Span::raw(" "),
            k("memory"),
            t(format!("{} ({:.1}%)", short_bytes(p.rss), p.mem_pct)),
            gap(),
            k("cpu"),
            Span::styled(format!("{:.1}%", p.cpu_pct), Style::default().fg(value_colour(p.cpu_pct.min(100.0)))),
        ]),
        Line::from(vec![Span::raw(" "), k("disk io"), t(io)]),
        Line::from(vec![Span::raw(" "), k("cgroup"), Span::styled(d.cgroup.clone(), Style::default().fg(LABEL))]),
        Line::from(vec![
            Span::raw(" "),
            k("command"),
            Span::styled(d.exe.clone().unwrap_or_else(|| p.comm.clone()), Style::default().fg(TEXT)),
            Span::styled(format!(" {}", p.args), Style::default().fg(DIM)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), Rect { height: area.height.saturating_sub(1), ..halves[1] });
    divider(f.buffer_mut(), Rect { y: area.bottom() - 1, height: 1, ..area });
}

// ──────────────────────────── popups ────────────────────────────

fn popup(f: &mut Frame, area: Rect, w: u16, h: u16, title: &str, colour: Color) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(colour))
        .title(Span::styled(format!(" {} ", title), Style::default().fg(colour).add_modifier(Modifier::BOLD)));
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

fn who(p: &Proc) -> Line<'static> {
    Line::from(vec![
        Span::styled("  PID ", Style::default().fg(DIM)),
        Span::styled(p.pid.to_string(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(p.comm.clone(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", p.user), Style::default().fg(DIM)),
    ])
}

fn draw_signal(f: &mut Frame, area: Rect, p: &Proc) {
    let inner = popup(f, area, 64, 11, "send signal", WARN);
    let mut lines = vec![Line::from(""), who(p), Line::from("")];
    for row in procs::SIGNALS.chunks(4) {
        let mut spans = vec![Span::raw("  ")];
        for (key, _, name, num) in row {
            spans.push(Span::styled(key.to_string(), Style::default().fg(TITLE).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(" {:<6}", name), Style::default().fg(LABEL)));
            spans.push(Span::styled(format!("{:<4}", num), Style::default().fg(DIM)));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  any other key cancels", Style::default().fg(DIM))));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_renice(f: &mut Frame, area: Rect, p: &Proc, n: i32) {
    let inner = popup(f, area, 56, 9, "renice", AMBER);
    let lines = vec![
        Line::from(""),
        who(p),
        Line::from(""),
        Line::from(vec![
            Span::styled("  nice ", Style::default().fg(DIM)),
            Span::styled(format!("{}", p.nice), Style::default().fg(TEXT)),
            Span::styled("  →  ", Style::default().fg(DIM)),
            Span::styled(format!("{:+}", n), Style::default().fg(AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("   (-20 highest … 19 lowest)", Style::default().fg(DIM)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  ↑ ↓ or + -  change    Enter  apply    Esc  cancel",
            Style::default().fg(DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_disk_menu(f: &mut Frame, area: Rect, items: &[(String, String, bool)], cursor: usize) {
    let name_w = items.iter().map(|i| i.0.chars().count()).max().unwrap_or(4).clamp(12, 40);
    let w = (name_w as u16 + 22).max(66);
    // Two lines of help and a blank above and below the list.
    let room = area.height.saturating_sub(7).max(1) as usize;
    let h = items.len().min(room) as u16 + 7;
    let inner = popup(f, area, w, h, "disks", TITLE);
    let top = cursor.saturating_sub(room.saturating_sub(1));
    let mut lines = vec![Line::from("")];
    for (i, (name, fs, shown)) in items.iter().enumerate().skip(top).take(room) {
        let sel = i == cursor;
        let mark = if *shown { "[x]" } else { "[ ]" };
        let fg = if *shown { TEXT } else { DIM };
        let mut name_style = Style::default().fg(fg);
        if sel {
            name_style = name_style.bg(SELECT).add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(if sel { "  ▶ " } else { "    " }, Style::default().fg(AMBER)),
            Span::styled(format!("{} ", mark), Style::default().fg(if *shown { MINT } else { DIM })),
            Span::styled(format!(" {:<w$} ", tail(name, name_w), w = name_w), name_style),
            Span::styled(format!(" {}", fs), Style::default().fg(DIM)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑ ↓  select    space  show / hide    Shift+↑ ↓ or + -  move",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  r  default order, all shown    Esc d q  close    W  save",
        Style::default().fg(DIM),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

/// The help window's text: (keys, what they do). A blank key starts a section.
pub fn help_lines(vim: bool) -> Vec<(&'static str, &'static str)> {
    let mut v = vec![
        ("", "general"),
        ("q  Ctrl+C", "quit"),
        ("?  F1", "this help"),
        ("space", "pause sampling (in tree view: fold)"),
        ("+  -", "faster / slower refresh (in tree view: unfold / fold)"),
        ("1 2 3 4", "show or hide the cpu, memory, network, process box"),
        ("d", "disks menu: choose and order the disks shown"),
        ("s", "damped or raw bars"),
        ("i", "Nerd Font icons or ASCII"),
        ("W", "save current settings to the config file"),
        ("Ctrl+Z", "suspend"),
        ("", "processes"),
        ("↑ ↓ PgUp PgDn", "select"),
        ("Home End", "first / last"),
        ("← →", "sort column"),
        ("r", "reverse the sort"),
        ("/  f", "filter; Enter keeps it, Esc clears it"),
        ("Enter", "detail box for the selected process"),
        ("e", "tree view"),
        ("space + -", "in the tree: fold / unfold the selected branch"),
        ("E", "fold / unfold every branch"),
        ("u", "pause the process list"),
        ("c", "cpu % of one thread or of the machine"),
        ("t", "show or hide kernel threads"),
        ("k", "signal menu (K with vim keys)"),
        ("N", "renice"),
        ("mouse", "click selects, click again opens details, wheel scrolls"),
        ("", "network"),
        ("n  b", "next / previous interface"),
        ("a", "back to the busiest interface"),
        ("m", "graph scale: auto, 100M … 100G, up to the link speed"),
    ];
    if vim {
        v.push(("", "vim keys"));
        v.push(("j k  g G", "down, up, first, last"));
        v.push(("h l", "sort column"));
    }
    v
}

fn draw_help(f: &mut Frame, area: Rect, vim: bool) {
    let items = help_lines(vim);
    let h = (items.len() as u16 + 3).min(area.height);
    let inner = popup(f, area, 74, h, "help", TITLE);
    let lines: Vec<Line> = items
        .into_iter()
        .map(|(k, what)| {
            if k.is_empty() {
                Line::from(Span::styled(format!(" {}", what), Style::default().fg(TITLE).add_modifier(Modifier::BOLD)))
            } else {
                Line::from(vec![
                    Span::styled(format!("   {:<16}", k), Style::default().fg(AMBER)),
                    Span::styled(what, Style::default().fg(TEXT)),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_compactly() {
        assert_eq!(short_bytes(0), "0B");
        assert_eq!(short_bytes(512), "512B");
        assert_eq!(short_bytes(1536), "1.50K");
        assert_eq!(short_bytes(15 << 30), "15.0G");
        assert_eq!(short_bytes(300 << 20), "300M");
    }

    #[test]
    fn cpu_time_reads_like_top() {
        assert_eq!(cpu_time(0.0), "0:00.00");
        assert_eq!(cpu_time(83.25), "1:23.25");
        assert_eq!(cpu_time(7300.0), "2h01m");
        assert_eq!(cpu_time(200_000.0), "2d07h");
    }

    #[test]
    fn grid_aims_for_the_bar_column_height() {
        // 22 threads beside 8 bar lines on a 90-wide pane: 3 columns of 8.
        assert_eq!(core_grid(22, 90, 8), (3, 8));
        // Too narrow for that many columns: grows taller instead.
        assert_eq!(core_grid(128, 90, 8), (5, 26));
    }

    #[test]
    fn graph_grows_from_the_right_and_mirrors() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
        graph(&mut buf, Rect::new(0, 0, 10, 2), &[1.0, 0.5, 0.0], true);
        assert_eq!(buf[(7, 1)].symbol(), "█");
        assert_eq!(buf[(7, 0)].symbol(), "█");
        assert_eq!(buf[(8, 1)].symbol(), "█");
        assert_eq!(buf[(8, 0)].symbol(), " ");
        assert_eq!(buf[(9, 1)].symbol(), " ");
        assert_eq!(buf[(6, 1)].symbol(), " ");

        let mut down = Buffer::empty(Rect::new(0, 0, 4, 1));
        graph(&mut down, Rect::new(0, 0, 4, 1), &[0.25], false);
        // A quarter hanging from the top: the lower three quarters, reversed.
        assert_eq!(down[(3, 0)].symbol(), "▆");
        assert!(down[(3, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn paths_keep_their_end() {
        assert_eq!(tail("/var/lib/vz", 20), "/var/lib/vz");
        assert_eq!(tail("/mnt/pve/backup-store", 10), "…kup-store");
    }
}
