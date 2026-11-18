//! `istop.conf`: `key = value` lines, `#` comments.
//!
//! Read at start-up from `$XDG_CONFIG_HOME/istop/istop.conf` (else
//! `~/.config/istop/istop.conf`), or the file given with `-c`. Command-line
//! options override it. Unlike btop, nothing is written back on exit: a key
//! pressed once does not silently become the new default. `W` (Shift+W) saves the
//! current settings on purpose.

use crate::procs::{GuestOrder, SortKey};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub update_ms: u64,
    pub sort: SortKey,
    /// Sort the opposite way to the key's natural order.
    pub reverse: bool,
    pub tree: bool,
    /// Proxmox VMs and containers first, one row each, their processes added
    /// together.
    pub guests: bool,
    pub guest_order: GuestOrder,
    /// Process CPU as a share of one thread (top) rather than of the machine.
    pub per_core: bool,
    pub kernel_threads: bool,
    pub smooth: bool,
    pub damping: f64,
    pub fps: u64,
    pub icons: bool,
    pub vim_keys: bool,
    pub mouse: bool,
    pub clock: bool,
    pub boxes: Boxes,
    pub net_iface: String,
    /// The network graph's full height. Never above the interface's link
    /// speed when drawn.
    pub net_scale: NetScale,
    /// Disks box order, by mount point or ZFS pool name; anything not listed
    /// follows in the usual order, so a new mount still appears.
    pub disk_order: Vec<String>,
    /// Kept out of the disks box.
    pub disk_hidden: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boxes {
    pub cpu: bool,
    pub mem: bool,
    pub net: bool,
    pub proc: bool,
}

impl Boxes {
    pub const ALL: Boxes = Boxes { cpu: true, mem: true, net: true, proc: true };

    fn parse(s: &str) -> Result<Boxes, String> {
        let mut b = Boxes { cpu: false, mem: false, net: false, proc: false };
        for w in s.split_whitespace() {
            match w {
                "cpu" => b.cpu = true,
                "mem" => b.mem = true,
                "net" => b.net = true,
                "proc" => b.proc = true,
                other => return Err(format!("unknown box \"{}\" (cpu mem net proc)", other)),
            }
        }
        if b.any() {
            Ok(b)
        } else {
            Err("shown_boxes must name at least one box".into())
        }
    }

    pub fn any(&self) -> bool {
        self.cpu || self.mem || self.net || self.proc
    }

    fn render(&self) -> String {
        [("cpu", self.cpu), ("mem", self.mem), ("net", self.net), ("proc", self.proc)]
            .iter()
            .filter(|(_, on)| *on)
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            update_ms: 1000,
            sort: SortKey::Cpu,
            reverse: false,
            tree: false,
            guests: false,
            guest_order: GuestOrder::Vmid,
            per_core: true,
            kernel_threads: true,
            smooth: true,
            damping: crate::app::DEFAULT_DAMPING_S,
            fps: 30,
            icons: true,
            vim_keys: false,
            mouse: true,
            clock: true,
            boxes: Boxes::ALL,
            net_iface: String::new(),
            net_scale: NetScale::Link,
            disk_order: Vec::new(),
            disk_hidden: Vec::new(),
        }
    }
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("istop").join("istop.conf"))
}

/// The fixed network graph scales, in Mbit/s, that `m` steps through.
pub const NET_SCALES: [u64; 6] = [100, 1_000, 10_000, 25_000, 40_000, 100_000];

/// `100M`, `1G`, `2.5G`: a scale or link speed in Mbit/s, short.
pub fn scale_name(mbit: u64) -> String {
    if mbit >= 1000 {
        format!("{}G", mbit as f64 / 1000.0)
    } else {
        format!("{}M", mbit)
    }
}

/// The network graph's full height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetScale {
    /// The interface's link speed, whatever the port; automatic where the
    /// driver reports none.
    Link,
    /// Fitted to the traffic in view.
    Auto,
    /// Fixed, in Mbit/s.
    Fixed(u64),
}

impl NetScale {
    fn parse(v: &str) -> Result<NetScale, String> {
        if v.eq_ignore_ascii_case("link") {
            return Ok(NetScale::Link);
        }
        if v.eq_ignore_ascii_case("auto") {
            return Ok(NetScale::Auto);
        }
        NET_SCALES
            .iter()
            .copied()
            .find(|&s| scale_name(s).eq_ignore_ascii_case(v))
            .map(NetScale::Fixed)
            .ok_or(format!("\"{}\" is not link auto 100M 1G 10G 25G 40G 100G", v))
    }

    fn render(self) -> String {
        match self {
            NetScale::Link => "link".into(),
            NetScale::Auto => "auto".into(),
            NetScale::Fixed(s) => scale_name(s),
        }
    }

    /// The scale drawn, in Mbit/s, or `None` for fitted to the traffic. A
    /// fixed one is capped at the link speed, so a 40G choice shows a 10G
    /// port at its own full height.
    pub fn for_link(self, link: Option<u64>) -> Option<u64> {
        match self {
            NetScale::Link => link,
            NetScale::Auto => None,
            NetScale::Fixed(s) => Some(link.map_or(s, |l| s.min(l))),
        }
    }

    /// `m`: the next scale after the one drawn, for an interface of this
    /// link speed. Fixed steps below the link speed, then the link speed
    /// (2.5G say, when it is not one of the fixed ones), then automatic.
    pub fn next(self, link: Option<u64>) -> NetScale {
        let steps = NET_SCALES.iter().copied().filter(|&s| link.is_none_or(|l| s < l));
        let after = |now: u64| steps.clone().find(|&s| s > now);
        match (self.for_link(link), link) {
            (None, _) => steps.clone().next().map_or(NetScale::Link, NetScale::Fixed),
            (Some(now), Some(l)) if now >= l => NetScale::Auto,
            (Some(now), _) => after(now).map_or(if link.is_some() { NetScale::Link } else { NetScale::Auto }, NetScale::Fixed),
        }
    }
}

/// Comma-separated names; a name holding a comma cannot be listed.
fn parse_list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("\"{}\" is not true or false", v)),
    }
}

impl Config {
    /// Parses a config file's text over the defaults. Every problem is
    /// reported, with its line number; the rest of the file still applies.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut c = Config::default();
        let mut errors = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                errors.push(format!("line {}: expected key = value", n + 1));
                continue;
            };
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            let r: Result<(), String> = (|| {
                match k {
                    "update_ms" => {
                        c.update_ms = v.parse::<u64>().map_err(|_| "not a number")?.clamp(100, 10_000)
                    }
                    "sort" => c.sort = SortKey::parse(v).ok_or(format!("unknown sort key \"{}\"", v))?,
                    "reverse" => c.reverse = parse_bool(v)?,
                    "tree" => c.tree = parse_bool(v)?,
                    "guests" => c.guests = parse_bool(v)?,
                    "guest_order" => {
                        c.guest_order =
                            GuestOrder::parse(v).ok_or(format!("\"{}\" is not vm-first ct-first vmid", v))?
                    }
                    "per_core" => c.per_core = parse_bool(v)?,
                    "kernel_threads" => c.kernel_threads = parse_bool(v)?,
                    "smooth" => c.smooth = parse_bool(v)?,
                    "damping" => {
                        c.damping = v.parse::<f64>().ok().filter(|d| d.is_finite()).ok_or("not a number")?
                    }
                    "fps" => c.fps = v.parse::<u64>().map_err(|_| "not a number")?.clamp(5, 240),
                    "icons" => c.icons = parse_bool(v)?,
                    "vim_keys" => c.vim_keys = parse_bool(v)?,
                    "mouse" => c.mouse = parse_bool(v)?,
                    "clock" => c.clock = parse_bool(v)?,
                    "shown_boxes" => c.boxes = Boxes::parse(v)?,
                    "net_iface" => c.net_iface = v.to_string(),
                    "net_scale" => c.net_scale = NetScale::parse(v)?,
                    "disk_order" => c.disk_order = parse_list(v),
                    "disk_hidden" => c.disk_hidden = parse_list(v),
                    _ => return Err(format!("unknown option \"{}\"", k)),
                }
                Ok(())
            })();
            if let Err(e) = r {
                errors.push(format!("line {}: {}: {}", n + 1, k, e));
            }
        }
        (c, errors)
    }

    pub fn load(path: &std::path::Path) -> Result<(Config, Vec<String>), String> {
        match std::fs::read_to_string(path) {
            Ok(t) => Ok(Config::parse(&t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), Vec::new())),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        }
    }

    /// Indices into `names` in the disks box's order: listed names first, as
    /// listed, then the rest as they came. Hidden ones are included.
    pub fn disk_rank(&self, names: &[&str]) -> Vec<usize> {
        let pos = |n: &str| self.disk_order.iter().position(|o| o == n).unwrap_or(usize::MAX);
        let mut idx: Vec<usize> = (0..names.len()).collect();
        // Stable, so the unlisted keep their own order.
        idx.sort_by_key(|&i| pos(names[i]));
        idx
    }

    pub fn disk_shown(&self, name: &str) -> bool {
        !self.disk_hidden.iter().any(|h| h == name)
    }

    pub fn render(&self) -> String {
        format!(
            "\
# istop configuration. `key = value`; `#` starts a comment.
# Command-line options override these. Saved only when W (Shift+W) is pressed.

# Sampling interval in milliseconds, 100-10000.
update_ms = {}

# Process sort: pid where user name nice threads time cpu mem.
sort = {}
# Sort the opposite way to the key's natural order (numbers largest first).
reverse = {}
# Show processes as a tree.
tree = {}
# Proxmox VMs and containers first, one row each with its processes added together.
guests = {}
# Guest rows: vmid (by VMID, VMs and containers mixed), or each type by VMID:
# vm-first, ct-first (the Proxmox tree's default).
guest_order = {}
# Process CPU % of one thread, as top shows it (true), or of the whole machine (false).
per_core = {}
# List kernel threads.
kernel_threads = {}

# Damped bars: quick to rise, slow to fall.
smooth = {}
# Seconds for a damped bar to fall ~90%, 0.2-30.
damping = {}
# Animation frame rate while bars move, 5-240.
fps = {}
# Nerd Font glyphs; false for plain ASCII markers.
icons = {}
# h j k l g G move in the process list; signals move to K, help to ?.
vim_keys = {}
# Mouse: click to select, scroll to move, click a selected row for details.
mouse = {}
# Clock in the header.
clock = {}

# Boxes shown: cpu mem net proc.
shown_boxes = {}
# Network interface to start on; empty follows the busiest physical port.
net_iface = {}
# Network graph full height: link (the interface's link speed; auto where none
# is reported), auto (fits the traffic), or 100M 1G 10G 25G 40G 100G, capped at
# the link speed, so 40G draws a 10G port at 10G.
net_scale = {}

# Disks box (d arranges it): mount points or ZFS pool names, comma-separated.
# Order: these first, as listed; any other follows in the usual order.
disk_order = {}
# Never shown.
disk_hidden = {}
",
            self.update_ms,
            self.sort.name(),
            self.reverse,
            self.tree,
            self.guests,
            self.guest_order.name(),
            self.per_core,
            self.kernel_threads,
            self.smooth,
            self.damping,
            self.fps,
            self.icons,
            self.vim_keys,
            self.mouse,
            self.clock,
            self.boxes.render(),
            self.net_iface,
            self.net_scale.render(),
            self.disk_order.join(", "),
            self.disk_hidden.join(", "),
        )
    }

    /// Writes the file whole, through a temporary file and a rename, so an
    /// interrupted save never leaves half a config.
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        }
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, self.render()).map_err(|e| format!("{}: {}", tmp.display(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {}", path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_default_parses_back_cleanly() {
        let d = Config::default();
        let (c, errors) = Config::parse(&d.render());
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(c.render(), d.render());
    }

    #[test]
    fn errors_are_reported_and_the_rest_applies() {
        let (c, errors) = Config::parse("tree = yes\nsort = bogus\nnonsense\nshown_boxes = cpu proc\nfps = 999 # clamped");
        assert!(c.tree);
        assert_eq!(c.fps, 240);
        assert_eq!(c.boxes, Boxes { cpu: true, mem: false, net: false, proc: true });
        assert_eq!(errors.len(), 2);
        assert!(errors[0].starts_with("line 2"));
    }

    #[test]
    fn disks_listed_first_then_the_rest_in_order() {
        let (c, errors) = Config::parse("disk_order = pool0, /home ,\ndisk_hidden = /boot");
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(c.disk_rank(&["/", "/boot", "/home", "/srv", "pool0"]), vec![4, 2, 0, 1, 3]);
        assert!(!c.disk_shown("/boot") && c.disk_shown("/"));
        let (back, _) = Config::parse(&c.render());
        assert_eq!(back.disk_order, ["pool0", "/home"]);
        assert_eq!(back.disk_hidden, ["/boot"]);
    }

    #[test]
    fn net_scale_steps_stop_at_the_link_speed() {
        use NetScale::*;
        // From the default, one full turn of m, as drawn.
        let walk = |link: Option<u64>| {
            let mut s = Config::default().net_scale;
            let mut seen = vec![s.for_link(link)];
            for _ in 0..8 {
                s = s.next(link);
                seen.push(s.for_link(link));
                if s == Link || (link.is_none() && s == Auto) {
                    break;
                }
            }
            seen
        };
        assert_eq!(walk(Some(10_000)), [Some(10_000), None, Some(100), Some(1_000), Some(10_000)]);
        assert_eq!(walk(Some(2_500)), [Some(2_500), None, Some(100), Some(1_000), Some(2_500)]);
        // No link speed: the default is automatic, and every step is offered.
        let mut none = vec![None];
        none.extend(NET_SCALES.map(Some));
        none.push(None);
        assert_eq!(walk(None), none);
        // A 40G choice made on a faster port draws a 10G port at 10G, and the
        // next step from there is automatic, not 25G.
        assert_eq!(Fixed(40_000).for_link(Some(10_000)), Some(10_000));
        assert_eq!(Fixed(40_000).next(Some(10_000)), Auto);
        assert_eq!(Fixed(40_000).next(Some(100_000)), Link);
        assert_eq!(Auto.next(Some(10)), Link);
    }

    #[test]
    fn net_scale_parses_and_renders() {
        let (c, errors) = Config::parse("net_scale = 25g");
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(c.net_scale, NetScale::Fixed(25_000));
        assert_eq!(Config::parse(&c.render()).0.net_scale, NetScale::Fixed(25_000));
        assert_eq!(Config::parse("net_scale = auto").0.net_scale, NetScale::Auto);
        assert_eq!(Config::parse("").0.net_scale, NetScale::Link);
        assert_eq!(Config::parse("net_scale = 2.5G").1.len(), 1);
        assert_eq!(scale_name(2_500), "2.5G");
    }
}
