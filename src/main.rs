//! istop — a system monitor in the manner of top and btop, drawn in igtop's
//! style: CPU, memory, disks, network and processes, from /proc and /sys.
//!
//! Written entirely in Rust with no `unsafe`: system calls go through rustix,
//! which makes them directly rather than through the C library.

#![forbid(unsafe_code)]

mod app;
mod clock;
mod config;
mod power;
mod procs;
mod sys;
mod theme;
mod ui;

use config::Config;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use procs::{Proc, Row, SortKey};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "\
istop — system monitor: CPU, memory, disks, network and processes

USAGE:
    istop [OPTIONS]

OPTIONS:
    -i, --interval <MS>   refresh interval in milliseconds (100-10000)
    -s, --sort <KEY>      process sort: pid where user name nice threads time cpu mem
    -r, --reverse         reverse the sort
    -f, --filter <TEXT>   initial process filter
        --tree            start in tree view
        --guests          Proxmox VMs and containers first, one row each
    -c, --config <FILE>   config file (default ~/.config/istop/istop.conf)
        --default-config  print the default config and exit
        --once            print a single plain-text snapshot and exit
        --no-smooth       show raw values, undamped
        --no-icons        plain ASCII markers instead of Nerd Font glyphs
        --damping <SEC>   seconds for a bar to fall ~90% (0.2-30)
        --fps <N>         animation frame rate (5-240)
    -V, --version         version
    -h, --help            this text

Settings come from the config file, then these options. Press ? in istop
for the keys, and Shift+W to save the current settings to the config file.

CPU:
    Busy is everything but idle and iowait. iowait is idle time with I/O
    outstanding, shown on its own bar. STEAL is time the hypervisor gave to
    someone else, and is only ever non-zero inside a VM. POWER is the CPU
    packages' draw from RAPL, which needs root.

PROCESSES:
    CPU % is of one thread by default, as top shows it: a process using four
    threads flat out reads 400. `c` switches to a share of the whole machine.
    Processes are rescanned once a second at most. WHERE is read from each
    process's cgroup: CT n and VM n on Proxmox, docker, podman, or host.
    TIME is CPU time used since the process started.
    `v` lists each Proxmox VM and container first, as one row with its
    processes added together (memory is the sum of their RSS); Enter on one
    lists its processes beneath it. `o` changes the guests' order.

DISKS:
    Local filesystems only, one row per device; network filesystems are left
    out deliberately (statvfs on an unreachable server blocks). On ZFS only
    pool roots. The busy figure is the share of time the device had I/O in
    flight.

MEMORY:
    USED is total minus MemAvailable. On a ZFS host the kernel counts the ARC
    in USED though most of it can be reclaimed; its own bar shows how much.
";

const INTERVAL_STEPS: [u64; 7] = [100, 250, 500, 1000, 2000, 5000, 10_000];

/// What the command line asked for, before it is laid over the config file.
#[derive(Default)]
struct Args {
    interval_ms: Option<u64>,
    sort: Option<SortKey>,
    reverse: bool,
    filter: String,
    tree: bool,
    guests: bool,
    config: Option<PathBuf>,
    default_config: bool,
    once: bool,
    no_smooth: bool,
    no_icons: bool,
    damping: Option<f64>,
    fps: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("istop {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-i" | "--interval" => {
                let v = it.next().ok_or("--interval needs a value")?;
                a.interval_ms = Some(v.parse::<u64>().map_err(|_| "--interval must be a number")?.clamp(100, 10_000));
            }
            "-s" | "--sort" => {
                let v = it.next().ok_or("--sort needs a key")?;
                a.sort = Some(SortKey::parse(&v).ok_or(format!("unknown sort key: {}", v))?);
            }
            "-r" | "--reverse" => a.reverse = true,
            "-f" | "--filter" => a.filter = it.next().ok_or("--filter needs a value")?,
            "--tree" => a.tree = true,
            "--guests" => a.guests = true,
            "-c" | "--config" => a.config = Some(PathBuf::from(it.next().ok_or("--config needs a file")?)),
            "--default-config" => a.default_config = true,
            "--once" => a.once = true,
            "--no-smooth" => a.no_smooth = true,
            "--no-icons" => a.no_icons = true,
            "--damping" => {
                let v = it.next().ok_or("--damping needs a value in seconds")?;
                a.damping = Some(
                    v.parse::<f64>()
                        .ok()
                        .filter(|d| d.is_finite())
                        .ok_or("--damping must be a finite number of seconds")?,
                );
            }
            "--fps" => {
                let v = it.next().ok_or("--fps needs a value")?;
                a.fps = Some(v.parse::<u64>().map_err(|_| "--fps must be a number")?.clamp(5, 240));
            }
            other => return Err(format!("unrecognised argument: {}", other)),
        }
    }
    Ok(a)
}

/// The command line laid over the config file.
fn settle(mut c: Config, a: &Args) -> Config {
    if let Some(v) = a.interval_ms {
        c.update_ms = v;
    }
    if let Some(v) = a.sort {
        c.sort = v;
    }
    c.reverse ^= a.reverse;
    c.tree |= a.tree;
    c.guests |= a.guests;
    c.smooth &= !a.no_smooth;
    c.icons &= !a.no_icons;
    if let Some(v) = a.damping {
        c.damping = v;
    }
    if let Some(v) = a.fps {
        c.fps = v;
    }
    c
}

/// Renders a string for a `key=value` pair in `--once` output: bare if it is a
/// single printable token, otherwise double-quoted with escapes. Process names
/// and arguments are chosen by whoever started the process.
fn quote(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| !c.is_control() && !c.is_whitespace() && c != '"' && c != '\\');
    if plain {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("istop: {}\n\n{}", e, USAGE);
            std::process::exit(2);
        }
    };
    if args.default_config {
        print!("{}", Config::default().render());
        return;
    }
    let path = args.config.clone().or_else(config::default_path);
    let (cfg, warnings) = match &path {
        Some(p) => match Config::load(p) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("istop: {}", e);
                std::process::exit(1);
            }
        },
        None => (Config::default(), Vec::new()),
    };
    for w in &warnings {
        eprintln!("istop: {}: {}", path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), w);
    }
    let cfg = settle(cfg, &args);

    let mut app = app::App::new();
    app.set_damping(cfg.damping);

    if args.once {
        snapshot(&mut app, &cfg, &args.filter);
        return;
    }
    let status = warnings.first().map(|w| format!("config {}", w));
    if let Err(e) = run_tui(&mut app, cfg, path, args.filter, status) {
        ratatui::restore();
        eprintln!("istop: {}", e);
        std::process::exit(1);
    }
}

/// Sample, wait, sample again: every headline figure is a rate, so without
/// this the first frame is a screen of zeros.
fn prime(app: &mut app::App, wait_ms: u64) {
    app.tick();
    app.scan_procs();
    std::thread::sleep(Duration::from_millis(wait_ms));
    app.tick();
    app.scan_procs();
}

fn snapshot(app: &mut app::App, cfg: &Config, filter: &str) {
    use std::fmt::Write as _;
    prime(app, cfg.update_ms.max(200));
    let mut o = String::new();
    let s = &app.share;
    let na = |o: Option<String>| o.unwrap_or_else(|| "n/a".into());
    let _ = writeln!(o, "host              {}", app.host);
    if let Some(c) = &app.own_container {
        let _ = writeln!(o, "own_container     {}", c);
    }
    let _ = writeln!(o, "cpu_model         {}", app.cpu_model);
    let _ = writeln!(o, "threads           {}", app.threads);
    let _ = writeln!(o, "cpu_busy_pct      {:.1}", s.busy);
    let _ = writeln!(o, "cpu_user_pct      {:.1}", s.user);
    let _ = writeln!(o, "cpu_system_pct    {:.1}", s.system);
    let _ = writeln!(o, "cpu_iowait_pct    {:.1}", s.iowait);
    let _ = writeln!(o, "cpu_steal_pct     {:.1}", s.steal);
    let cores: Vec<String> = app.cores.iter().map(|c| format!("{:.0}", c.raw)).collect();
    let _ = writeln!(o, "cpu_cores_pct     {}", cores.join(" "));
    let _ = writeln!(o, "load              {:.2} {:.2} {:.2}", app.load[0], app.load[1], app.load[2]);
    let _ = writeln!(o, "temp_c            {}", na(app.temp_c.map(|t| format!("{:.1}", t))));
    if let Some(l) = app.temp_label() {
        let _ = writeln!(o, "temp_sensor       {}", l);
    }
    let _ = writeln!(o, "cpu_watts         {}", na(app.cpu_watts.map(|w| format!("{:.1}", w))));
    let _ = writeln!(o, "cpu_limit_w       {}", na(app.cpu_limit_w.map(|w| format!("{:.0}", w))));
    let _ = writeln!(o, "freq_mhz          {}", na(app.freq_mhz.map(|f| format!("{:.0}", f))));
    let _ = writeln!(o, "freq_max_mhz      {}", na(app.freq_max_mhz.map(|f| format!("{:.0}", f))));
    let _ = writeln!(o, "uptime_s          {:.0}", app.uptime_s);
    if let Some(b) = &app.battery {
        let _ = writeln!(
            o,
            "battery           pct={:.0} status={} watts={} secs={}",
            b.pct,
            quote(&b.status),
            na(b.watts.map(|w| format!("{:.2}", w))),
            na(b.secs.map(|s| format!("{:.0}", s)))
        );
    }
    let m = &app.mem;
    let _ = writeln!(o, "mem_total_b       {}", m.total);
    let _ = writeln!(o, "mem_used_b        {}", m.used());
    let _ = writeln!(o, "mem_available_b   {}", m.available);
    let _ = writeln!(o, "mem_cache_b       {}", m.cache);
    let _ = writeln!(o, "swap_total_b      {}", m.swap_total);
    let _ = writeln!(o, "swap_used_b       {}", m.swap_used());
    if let Some((size, max)) = m.arc {
        let _ = writeln!(o, "zfs_arc_b         {}", size);
        let _ = writeln!(o, "zfs_arc_max_b     {}", max);
    }
    let num = |v: Option<f64>| na(v.map(|b| format!("{:.0}", b)));
    for d in &app.disks {
        let _ = writeln!(
            o,
            "disk path={} fs={} used_b={} total_b={} read_bps={} write_bps={} busy_pct={}",
            quote(&d.mount.path),
            d.mount.fstype,
            d.mount.used,
            d.mount.total,
            num(d.read_bps),
            num(d.write_bps),
            num(d.busy_pct),
        );
    }
    for n in &app.nets {
        let _ = writeln!(
            o,
            "net iface={} physical={} rx_bps={:.0} tx_bps={:.0} rx_b={} tx_b={}",
            quote(&n.name),
            n.physical,
            n.rx_bps,
            n.tx_bps,
            n.rx_total,
            n.tx_total
        );
    }
    let desc = cfg.sort.default_desc() ^ cfg.reverse;
    let keep = |p: &Proc| procs::matches(p, filter);
    if cfg.guests {
        let none = HashSet::new();
        let list = &app.procs.list;
        for r in procs::guests(list, keep, filter, cfg.guest_order, &app.procs.guest_info, cfg.sort, desc, &none) {
            let p = &*r.p;
            let _ = writeln!(
                o,
                "guest where={} name={} procs={} threads={} time_s={:.2} cpu={:.1}% rss_b={} mem={:.1}%",
                quote(p.container.as_deref().unwrap_or("")),
                quote(&p.comm),
                r.children,
                p.threads,
                p.cpu_time_s,
                p.cpu_pct,
                p.rss,
                p.mem_pct
            );
        }
    }
    let host = |p: &Proc| !(cfg.guests && procs::in_guest(p)) && keep(p);
    let rows = procs::flat(&app.procs.list, host, cfg.sort, desc);
    for r in rows.iter().take(25) {
        let p = &*r.p;
        let _ = writeln!(
            o,
            "proc pid={} ppid={} where={} user={} comm={} nice={} threads={} time_s={:.2} cpu={:.1}% rss_b={} mem={:.1}%",
            p.pid,
            p.ppid,
            quote(p.container.as_deref().unwrap_or("host")),
            quote(&p.user),
            quote(&p.comm),
            p.nice,
            p.threads,
            p.cpu_time_s,
            p.cpu_pct,
            p.rss,
            p.mem_pct
        );
    }
    // Rust ignores SIGPIPE, so a closed pipe (`istop --once | head`) is a
    // write error here, and the right response is to stop quietly.
    let _ = io::stdout().lock().write_all(o.as_bytes());
}

/// What the keyboard is doing.
#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Normal,
    Filter,
    /// Choosing a signal for this PID.
    Signal(i32),
    /// Choosing a nice value for this PID.
    Renice(i32, i32),
    Help,
    /// The disks menu, cursor on this row of `Ui::disk_items`.
    Disks(usize),
}

struct Ui {
    cfg: Config,
    path: Option<PathBuf>,
    desc: bool,
    filter: String,
    mode: Mode,
    /// Once moved, the selection follows a process, not a row, so it stays on
    /// it while the table re-sorts underneath. Until then — and after Home —
    /// it stays on the top row, so the busiest process is always in view.
    follow: bool,
    selected_pid: Option<i32>,
    selected: usize,
    offset: usize,
    collapsed: HashSet<i32>,
    /// Guest rows listing their processes beneath them, by the row's PID.
    open_guests: HashSet<i32>,
    list_paused: bool,
    /// Chosen with n / b; `None` follows the busiest interface.
    net: Option<usize>,
    status: Option<(String, bool, Instant)>,
    /// The disks menu's rows, in order, taken when it opens.
    disk_items: Vec<String>,
}

impl Ui {
    fn say(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), err, Instant::now()));
    }

    fn select(&mut self, to: usize, len: usize) {
        self.selected = to.min(len.saturating_sub(1));
        self.selected_pid = None;
        self.follow = to > 0;
    }
}

fn rows<'a>(ps: &'a procs::Procs, ui: &Ui) -> Vec<Row<'a>> {
    let list = &ps.list;
    let kt = ui.cfg.kernel_threads;
    let keep = |p: &Proc| (kt || !p.kernel) && procs::matches(p, &ui.filter);
    // With guests first, the rest are the host's own processes.
    let grouped = ui.cfg.guests;
    let host = |p: &Proc| !(grouped && procs::in_guest(p)) && keep(p);
    let mut out = if grouped {
        let c = &ui.cfg;
        procs::guests(list, keep, &ui.filter, c.guest_order, &ps.guest_info, c.sort, ui.desc, &ui.open_guests)
    } else {
        Vec::new()
    };
    out.extend(if ui.cfg.tree {
        procs::tree(list, host, ui.cfg.sort, ui.desc, &ui.collapsed)
    } else {
        procs::flat(list, host, ui.cfg.sort, ui.desc)
    });
    out
}

type Term = ratatui::DefaultTerminal;

fn start_terminal(mouse: bool) -> io::Result<Term> {
    let t = ratatui::init();
    if mouse {
        crossterm::execute!(io::stdout(), EnableMouseCapture)?;
    }
    Ok(t)
}

fn stop_terminal(mouse: bool) {
    if mouse {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
}

fn run_tui(
    app: &mut app::App,
    cfg: Config,
    path: Option<PathBuf>,
    filter: String,
    status: Option<String>,
) -> io::Result<()> {
    let mut terminal = start_terminal(cfg.mouse)?;
    let mut ui = Ui {
        desc: cfg.sort.default_desc() ^ cfg.reverse,
        cfg,
        path,
        filter,
        mode: Mode::Normal,
        follow: false,
        selected_pid: None,
        selected: 0,
        offset: 0,
        collapsed: HashSet::new(),
        open_guests: HashSet::new(),
        list_paused: false,
        net: None,
        status: None,
        disk_items: Vec::new(),
    };
    if let Some(s) = status {
        ui.say(s, true);
    }
    let zone = clock::Zone::local();
    let mut paused = false;

    prime(app, 250);
    if !ui.cfg.net_iface.is_empty() {
        ui.net = app.nets.iter().position(|n| n.name == ui.cfg.net_iface);
    }
    let mut last_tick = Instant::now();
    let mut last_scan = Instant::now();
    let mut last_frame = Instant::now();
    let mut last_clock = String::new();
    let frame_budget = Duration::from_millis((1000 / ui.cfg.fps).max(1));
    // `dirty`: something changed that must reach the screen. `moving`: a bar
    // was still gliding on the previous pass. A gliding frame is drawn only if
    // a bar has moved by a visible step; see `App::motion_signature`.
    let mut dirty = true;
    let mut moving = false;
    let mut last_sig: Vec<i64> = Vec::new();
    // From the last frame drawn, for the keys and the mouse.
    let mut hit = ui::Hit::default();
    let mut len = 0;
    let mut net: Option<usize> = None;

    loop {
        let now = Instant::now();
        let tick_every = Duration::from_millis(ui.cfg.update_ms);
        if !paused && now.duration_since(last_tick) >= tick_every {
            app.tick();
            // Process rates are held to ~1 Hz no matter how fast the rest is
            // sampled: a full /proc walk is the expensive part, and a faster
            // scan only adds noise.
            if !ui.list_paused && now.duration_since(last_scan) >= Duration::from_millis(1000) {
                app.scan_procs();
                last_scan = now;
            }
            last_tick = now;
            dirty = true;
        }
        if ui.status.as_ref().is_some_and(|(_, _, t)| now.duration_since(*t) > Duration::from_secs(5)) {
            ui.status = None;
            dirty = true;
        }
        let clock = ui.cfg.clock.then(|| zone.now_hms());
        if clock.as_deref().is_some_and(|c| c != last_clock) {
            last_clock = clock.clone().unwrap_or_default();
            dirty = true;
        }

        // Animation runs on wall time, independent of the sampling interval
        // and the frame rate. Coming out of rest, motion restarts from one
        // frame's worth rather than the whole idle wait.
        let frame_dt = if moving { now.duration_since(last_frame) } else { frame_budget };
        last_frame = now;
        moving = false;
        if !paused {
            if ui.cfg.smooth {
                moving = app.animate(frame_dt.as_secs_f64());
            } else {
                app.snap();
            }
        }

        let size = terminal.size()?;
        // A gliding frame that moves no bar by a visible step is not drawn,
        // and nothing for it is built either.
        let sig = app.motion_signature(size.width);
        if dirty || (moving && sig != last_sig) {
            last_sig = sig;
            dirty = false;
            let list = rows(&app.procs, &ui);
            if let Some(pid) = ui.selected_pid.filter(|_| ui.follow) {
                if let Some(i) = list.iter().position(|r| r.p.pid == pid) {
                    ui.selected = i;
                }
            }
            ui.selected = ui.selected.min(list.len().saturating_sub(1));
            ui.selected_pid = list.get(ui.selected).map(|r| r.p.pid);
            net = ui.net.filter(|i| *i < app.nets.len()).or_else(|| ui::default_net(&app.nets));
            let find = |pid: i32| app.procs.list.iter().find(|p| p.pid == pid);
            let popup = match ui.mode {
                Mode::Signal(pid) => find(pid).map(ui::Popup::Signal),
                Mode::Renice(pid, n) => find(pid).map(|p| ui::Popup::Renice(p, n)),
                Mode::Help => Some(ui::Popup::Help { vim: ui.cfg.vim_keys }),
                Mode::Disks(cursor) => Some(ui::Popup::Disks {
                    items: ui
                        .disk_items
                        .iter()
                        .map(|n| {
                            let fs = app.disks.iter().find(|d| &d.mount.path == n).map(|d| d.mount.fstype.clone());
                            (n.clone(), fs.unwrap_or_else(|| "gone".into()), ui.cfg.disk_shown(n))
                        })
                        .collect(),
                    cursor,
                }),
                _ => None,
            };
            let names: Vec<&str> = app.disks.iter().map(|d| d.mount.path.as_str()).collect();
            let disks = ui
                .cfg
                .disk_rank(&names)
                .into_iter()
                .map(|i| &app.disks[i])
                .filter(|d| ui.cfg.disk_shown(&d.mount.path))
                .collect();
            // Scroll just enough to keep the selection on screen, using the
            // table height from the last frame.
            let page = (hit.rows.height as usize).max(1);
            if ui.selected < ui.offset {
                ui.offset = ui.selected;
            } else if ui.selected >= ui.offset + page {
                ui.offset = ui.selected + 1 - page;
            }
            ui.offset = ui.offset.min(list.len().saturating_sub(page));
            let view = ui::View {
                app,
                smooth: ui.cfg.smooth,
                icons: ui.cfg.icons,
                interval_ms: ui.cfg.update_ms,
                paused,
                rows: &list,
                selected: ui.selected,
                offset: ui.offset,
                sort: ui.cfg.sort,
                desc: ui.desc,
                tree: ui.cfg.tree,
                guests: ui.cfg.guests,
                per_core: ui.cfg.per_core,
                filter: &ui.filter,
                editing: ui.mode == Mode::Filter,
                net,
                net_scale: ui.cfg.net_scale,
                popup,
                status: ui.status.as_ref().map(|(s, e, _)| (s.as_str(), *e)),
                kernel_threads: ui.cfg.kernel_threads,
                list_paused: ui.list_paused,
                boxes: ui.cfg.boxes,
                clock: clock.clone(),
                disks,
            };
            // Synchronised output: the terminal shows the frame whole or not
            // at all, so a large redraw never tears. Ignored where unsupported.
            crossterm::queue!(io::stdout(), BeginSynchronizedUpdate)?;
            let mut new_hit = hit;
            terminal.draw(|f| new_hit = ui::draw(f, &view))?;
            crossterm::execute!(io::stdout(), EndSynchronizedUpdate)?;
            let grew = new_hit.rows.height != hit.rows.height;
            hit = new_hit;
            len = list.len();
            // The table's height decides the scroll; if it changed (a box
            // toggled, the detail box opened), draw again with the new one.
            dirty |= grew;
        }

        let wait = if dirty {
            Duration::ZERO
        } else if moving {
            frame_budget
        } else if paused {
            Duration::from_millis(500)
        } else {
            (last_tick + tick_every)
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500))
                .max(Duration::from_millis(1))
        };
        if !event::poll(wait)? {
            continue;
        }
        let ev = event::read()?;
        dirty = true;
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                let page = (hit.rows.height as usize).max(1);
                match handle_key(k, &mut ui, app, &mut paused, len, page, net) {
                    Flow::Quit => break,
                    Flow::Suspend => {
                        stop_terminal(ui.cfg.mouse);
                        // Stops here until `fg`, then carries on.
                        let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::Tstp);
                        terminal = start_terminal(ui.cfg.mouse)?;
                        terminal.clear()?;
                    }
                    Flow::Resume => {
                        app.reset_rates();
                        prime(app, ui.cfg.update_ms.min(250));
                        last_tick = Instant::now();
                        last_scan = last_tick;
                    }
                    Flow::Go => {}
                }
            }
            Event::Mouse(m) if ui.mode == Mode::Normal => {
                let r = hit.rows;
                let inside = m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom();
                match m.kind {
                    MouseEventKind::ScrollDown => ui.select(ui.selected + 3, len),
                    MouseEventKind::ScrollUp => ui.select(ui.selected.saturating_sub(3), len),
                    MouseEventKind::Down(MouseButton::Left) if inside => {
                        let row = ui.offset + (m.row - r.y) as usize;
                        if row < len {
                            if row == ui.selected {
                                toggle_detail(&mut ui, app);
                            } else {
                                ui.select(row, len);
                                ui.follow = true;
                            }
                        }
                    }
                    _ => dirty = false,
                }
            }
            Event::Resize(..) => {}
            _ => dirty = false,
        }
    }

    stop_terminal(ui.cfg.mouse);
    Ok(())
}

enum Flow {
    Go,
    Quit,
    Suspend,
    /// Sampling resumed after a pause: rates must restart.
    Resume,
}

fn toggle_detail(ui: &mut Ui, app: &mut app::App) {
    if let Some(g) = ui.selected_pid.filter(|p| *p < 0) {
        if !ui.open_guests.remove(&g) {
            ui.open_guests.insert(g);
        }
        ui.follow = true;
        return;
    }
    match (app.detail.as_ref().map(|d| d.pid), ui.selected_pid) {
        (Some(d), Some(s)) if d == s => app.detail = None,
        (_, Some(s)) => app.open_detail(s),
        _ => app.detail = None,
    }
}

fn handle_key(
    k: KeyEvent,
    ui: &mut Ui,
    app: &mut app::App,
    paused: &mut bool,
    len: usize,
    page: usize,
    net: Option<usize>,
) -> Flow {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('c') {
        return Flow::Quit;
    }
    if ctrl && k.code == KeyCode::Char('z') {
        return Flow::Suspend;
    }

    match ui.mode {
        Mode::Filter => {
            match k.code {
                KeyCode::Enter => ui.mode = Mode::Normal,
                KeyCode::Esc => {
                    ui.filter.clear();
                    ui.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    ui.filter.pop();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    ui.filter.push(c);
                    ui.select(0, len);
                }
                _ => {}
            }
            return Flow::Go;
        }
        Mode::Help => {
            ui.mode = Mode::Normal;
            return Flow::Go;
        }
        Mode::Signal(pid) => {
            if let KeyCode::Char(c) = k.code {
                if let Some((_, sig, name, _)) = procs::SIGNALS.iter().find(|s| s.0 == c) {
                    match procs::signal(pid, *sig) {
                        Ok(()) => ui.say(format!("sent {} to {}", name, pid), false),
                        Err(e) => ui.say(format!("{} to {}: {}", name, pid, e), true),
                    }
                }
            }
            ui.mode = Mode::Normal;
            return Flow::Go;
        }
        Mode::Renice(pid, n) => {
            match k.code {
                KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => ui.mode = Mode::Renice(pid, (n + 1).min(19)),
                KeyCode::Down | KeyCode::Char('-') => ui.mode = Mode::Renice(pid, (n - 1).max(-20)),
                KeyCode::Enter => {
                    match procs::renice(pid, n) {
                        Ok(()) => ui.say(format!("nice {} for {}", n, pid), false),
                        Err(e) => ui.say(format!("renice {}: {}", pid, e), true),
                    }
                    ui.mode = Mode::Normal;
                }
                _ => ui.mode = Mode::Normal,
            }
            return Flow::Go;
        }
        Mode::Disks(c) => {
            let n = ui.disk_items.len();
            let shift = k.modifiers.contains(KeyModifiers::SHIFT);
            let vim = ui.cfg.vim_keys;
            match k.code {
                KeyCode::Up if shift => move_disk(ui, c, -1),
                KeyCode::Down if shift => move_disk(ui, c, 1),
                KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('K') => move_disk(ui, c, -1),
                KeyCode::Char('-') | KeyCode::Char('J') => move_disk(ui, c, 1),
                KeyCode::Up => ui.mode = Mode::Disks(c.saturating_sub(1)),
                KeyCode::Char('k') if vim => ui.mode = Mode::Disks(c.saturating_sub(1)),
                KeyCode::Down => ui.mode = Mode::Disks((c + 1).min(n.saturating_sub(1))),
                KeyCode::Char('j') if vim => ui.mode = Mode::Disks((c + 1).min(n.saturating_sub(1))),
                KeyCode::Home => ui.mode = Mode::Disks(0),
                KeyCode::End => ui.mode = Mode::Disks(n.saturating_sub(1)),
                KeyCode::Char(' ') | KeyCode::Enter => {
                    if let Some(name) = ui.disk_items.get(c).cloned() {
                        if ui.cfg.disk_shown(&name) {
                            ui.cfg.disk_hidden.push(name);
                        } else {
                            ui.cfg.disk_hidden.retain(|h| *h != name);
                        }
                    }
                }
                KeyCode::Char('r') => {
                    ui.cfg.disk_order.clear();
                    ui.cfg.disk_hidden.clear();
                    let names: Vec<&str> = app.disks.iter().map(|d| d.mount.path.as_str()).collect();
                    ui.disk_items = names.iter().map(|s| s.to_string()).collect();
                    ui.mode = Mode::Disks(0);
                }
                KeyCode::Char('W') => save(ui),
                KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('q') => ui.mode = Mode::Normal,
                _ => {}
            }
            return Flow::Go;
        }
        Mode::Normal => {}
    }

    let vim = ui.cfg.vim_keys;
    let signal_key = if vim { 'K' } else { 'k' };
    let selected_proc = || ui.selected_pid.and_then(|pid| app.procs.list.iter().find(|p| p.pid == pid).cloned());
    match k.code {
        KeyCode::Char('q') => return Flow::Quit,
        KeyCode::Esc if !ui.filter.is_empty() => ui.filter.clear(),
        KeyCode::Esc if app.detail.is_some() => app.detail = None,
        KeyCode::Esc => return Flow::Quit,
        KeyCode::Char('?') | KeyCode::F(1) => ui.mode = Mode::Help,

        KeyCode::Up => ui.select(ui.selected.saturating_sub(1), len),
        KeyCode::Down => ui.select(ui.selected + 1, len),
        KeyCode::Char('k') if vim => ui.select(ui.selected.saturating_sub(1), len),
        KeyCode::Char('j') if vim => ui.select(ui.selected + 1, len),
        KeyCode::PageUp => ui.select(ui.selected.saturating_sub(page), len),
        KeyCode::PageDown => ui.select(ui.selected + page, len),
        KeyCode::Home => ui.select(0, len),
        KeyCode::Char('g') if vim => ui.select(0, len),
        KeyCode::End => ui.select(usize::MAX, len),
        KeyCode::Char('G') if vim => ui.select(usize::MAX, len),
        KeyCode::Left | KeyCode::Right => step_sort(ui, if k.code == KeyCode::Left { -1 } else { 1 }),
        KeyCode::Char('h') if vim => step_sort(ui, -1),
        KeyCode::Char('l') if vim => step_sort(ui, 1),

        KeyCode::Char('r') => ui.desc = !ui.desc,
        KeyCode::Char('/') | KeyCode::Char('f') => ui.mode = Mode::Filter,
        KeyCode::Delete => ui.filter.clear(),
        KeyCode::Enter => toggle_detail(ui, app),
        KeyCode::Char('e') => {
            ui.cfg.tree = !ui.cfg.tree;
            ui.follow = ui.selected > 0;
        }
        KeyCode::Char('E') if ui.cfg.tree => {
            if ui.collapsed.is_empty() {
                ui.collapsed = procs::parents(&app.procs.list);
            } else {
                ui.collapsed.clear();
            }
        }
        KeyCode::Char(' ') | KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') if ui.cfg.tree => {
            if let Some(pid) = ui.selected_pid {
                let fold = match k.code {
                    KeyCode::Char('-') => true,
                    KeyCode::Char(' ') => !ui.collapsed.contains(&pid),
                    _ => false,
                };
                if fold {
                    ui.collapsed.insert(pid);
                } else {
                    ui.collapsed.remove(&pid);
                }
                ui.follow = true;
            }
        }
        KeyCode::Char('v') => {
            ui.cfg.guests = !ui.cfg.guests;
            ui.follow = ui.selected > 0;
        }
        KeyCode::Char('o') if ui.cfg.guests => {
            ui.cfg.guest_order = ui.cfg.guest_order.next();
            ui.say(format!("guest order: {}", ui.cfg.guest_order.name()), false);
        }
        KeyCode::Char('u') => ui.list_paused = !ui.list_paused,
        KeyCode::Char('c') => ui.cfg.per_core = !ui.cfg.per_core,
        KeyCode::Char('t') => ui.cfg.kernel_threads = !ui.cfg.kernel_threads,
        KeyCode::Char(c) if (c == signal_key || c == 'N') && ui.selected_pid.is_some_and(|p| p < 0) => {
            ui.say("a guest row is all its processes: Enter lists them, then choose one", true);
        }
        KeyCode::Char(c) if c == signal_key => {
            if let Some(pid) = ui.selected_pid {
                ui.mode = Mode::Signal(pid);
            }
        }
        KeyCode::Char('N') => {
            if let Some(p) = selected_proc() {
                ui.mode = Mode::Renice(p.pid, p.nice);
            }
        }

        KeyCode::Char('n') | KeyCode::Char('b') => {
            let n = app.nets.len().max(1);
            let cur = net.unwrap_or(0);
            ui.net = Some(if k.code == KeyCode::Char('n') { (cur + 1) % n } else { (cur + n - 1) % n });
        }
        KeyCode::Char('m') => {
            let link = net.and_then(|i| app.nets.get(i)).and_then(|n| n.speed_mbit);
            ui.cfg.net_scale = ui.cfg.net_scale.next(link);
            let name = match (ui.cfg.net_scale, ui.cfg.net_scale.for_link(link)) {
                (_, None) => "auto".into(),
                (config::NetScale::Link, Some(s)) => format!("link speed, {}bit/s", config::scale_name(s)),
                (_, Some(s)) => format!("{}bit/s", config::scale_name(s)),
            };
            ui.say(format!("network graph scale: {}", name), false);
        }
        KeyCode::Char('a') => {
            ui.net = None;
            ui.say("network: following the busiest interface", false);
        }

        KeyCode::Char(d @ '1'..='4') => {
            let mut b = ui.cfg.boxes;
            match d {
                '1' => b.cpu = !b.cpu,
                '2' => b.mem = !b.mem,
                '3' => b.net = !b.net,
                _ => b.proc = !b.proc,
            }
            if b.any() {
                ui.cfg.boxes = b;
            } else {
                ui.say("at least one box stays shown", true);
            }
        }
        KeyCode::Char('d') => {
            let names: Vec<&str> = app.disks.iter().map(|d| d.mount.path.as_str()).collect();
            ui.disk_items = ui.cfg.disk_rank(&names).into_iter().map(|i| names[i].to_string()).collect();
            if ui.disk_items.is_empty() {
                ui.say("no local filesystems", true);
            } else {
                ui.mode = Mode::Disks(0);
            }
        }
        KeyCode::Char('s') => ui.cfg.smooth = !ui.cfg.smooth,
        KeyCode::Char('i') => ui.cfg.icons = !ui.cfg.icons,
        KeyCode::Char('W') => save(ui),
        KeyCode::Char(' ') => {
            *paused = !*paused;
            if !*paused {
                return Flow::Resume;
            }
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            let i = ui.cfg.update_ms;
            ui.cfg.update_ms = INTERVAL_STEPS.iter().rev().copied().find(|&s| s < i).unwrap_or(i);
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            let i = ui.cfg.update_ms;
            ui.cfg.update_ms = INTERVAL_STEPS.iter().copied().find(|&s| s > i).unwrap_or(i);
        }
        _ => {}
    }
    Flow::Go
}

/// Moves the menu's row `c` up or down one place and makes the menu's order
/// the configured one. Listed names not mounted now keep their place at the end.
fn move_disk(ui: &mut Ui, c: usize, by: isize) {
    let to = c as isize + by;
    if c >= ui.disk_items.len() || to < 0 || to as usize >= ui.disk_items.len() {
        return;
    }
    ui.disk_items.swap(c, to as usize);
    let absent: Vec<String> = ui.cfg.disk_order.iter().filter(|o| !ui.disk_items.contains(o)).cloned().collect();
    ui.cfg.disk_order = ui.disk_items.iter().cloned().chain(absent).collect();
    ui.mode = Mode::Disks(to as usize);
}

fn step_sort(ui: &mut Ui, by: isize) {
    ui.cfg.sort = ui.cfg.sort.step(by);
    ui.desc = ui.cfg.sort.default_desc();
}

/// `W` (Shift+W): the current settings, written to the config file on request only.
fn save(ui: &mut Ui) {
    let mut c = ui.cfg.clone();
    c.reverse = ui.desc != c.sort.default_desc();
    let Some(path) = ui.path.clone() else {
        ui.say("no config path: HOME is not set", true);
        return;
    };
    match c.save(&path) {
        Ok(()) => ui.say(format!("saved {}", path.display()), false),
        Err(e) => ui.say(format!("save failed: {}", e), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_quoting() {
        assert_eq!(quote("nginx"), "nginx");
        assert_eq!(quote("CT 101"), "\"CT 101\"");
        assert_eq!(quote("x\x1b[2J"), "\"x\\x1b[2J\"");
    }

    #[test]
    fn command_line_overrides_config() {
        let (c, _) = Config::parse("tree = false\nupdate_ms = 2000\nsmooth = true");
        let a = Args { tree: true, interval_ms: Some(500), no_smooth: true, ..Default::default() };
        let c = settle(c, &a);
        assert!(c.tree);
        assert_eq!(c.update_ms, 500);
        assert!(!c.smooth);
    }
}
