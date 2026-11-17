//! `istop.conf`: `key = value` lines, `#` comments.
//!
//! Read at start-up from `$XDG_CONFIG_HOME/istop/istop.conf` (else
//! `~/.config/istop/istop.conf`), or the file given with `-c`. Command-line
//! options override it. Unlike btop, nothing is written back on exit: a key
//! pressed once does not silently become the new default. `W` saves the
//! current settings on purpose.

use crate::procs::SortKey;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub update_ms: u64,
    pub sort: SortKey,
    /// Sort the opposite way to the key's natural order.
    pub reverse: bool,
    pub tree: bool,
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

    pub fn render(&self) -> String {
        format!(
            "\
# istop configuration. `key = value`; `#` starts a comment.
# Command-line options override these. Saved only when W is pressed.

# Sampling interval in milliseconds, 100-10000.
update_ms = {}

# Process sort: pid where user name nice threads time cpu mem.
sort = {}
# Sort the opposite way to the key's natural order (numbers largest first).
reverse = {}
# Show processes as a tree.
tree = {}
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
",
            self.update_ms,
            self.sort.name(),
            self.reverse,
            self.tree,
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
}
