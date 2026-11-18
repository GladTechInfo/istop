//! The process table, from `/proc/<pid>/stat`.
//!
//! CPU is the change in a process's utime + stime between two scans, against
//! the wall time between them, as a share of **one** CPU — the convention top
//! and ps use, so a process running four threads flat out reads 400%. The
//! view can divide by the thread count instead (`c`), as btop does by default.
//!
//! Per process, the slow-moving facts — command line, owner, container — are
//! read once and cached under (pid, start time), so a recycled PID is never
//! mistaken for the process that had it before.

use rustix::process::{Pid, Signal};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    /// Arguments after argv[0], joined by spaces. Empty for kernel threads.
    pub args: String,
    pub kernel: bool,
    pub user: String,
    /// "CT 101", "VM 105", "docker", "podman" — or `None` on the host.
    pub container: Option<String>,
    pub state: char,
    pub nice: i32,
    pub threads: u64,
    pub rss: u64,
    /// Seconds of CPU used since the process started, all threads together.
    pub cpu_time_s: f64,
    /// Start time, in clock ticks after boot.
    pub start: u64,
    pub cpu_pct: f64,
    pub mem_pct: f64,
}

struct Cached {
    start: u64,
    /// The full program name, when the kernel's 15-character `comm` cut it.
    name: Option<String>,
    args: String,
    user: String,
    container: Option<String>,
}

pub struct Procs {
    pub list: Vec<Proc>,
    /// pid → (start time, cumulative utime + stime), from the last scan.
    prev: HashMap<i32, (u64, u64)>,
    cache: HashMap<i32, Cached>,
    users: HashMap<u32, String>,
    /// Each container's own users, read from inside it once.
    guest_users: HashMap<String, HashMap<u32, String>>,
    /// Each Proxmox guest's name, read once while it has processes running.
    pub guest_names: HashMap<String, Option<String>>,
    passwd_mtime: Option<std::time::SystemTime>,
    last: Option<Instant>,
    pub clk_tck: f64,
    page: u64,
}

fn passwd_mtime() -> Option<std::time::SystemTime> {
    fs::metadata("/etc/passwd").and_then(|m| m.modified()).ok()
}

impl Procs {
    pub fn new() -> Self {
        let clk_tck = rustix::param::clock_ticks_per_second();
        let page = rustix::param::page_size();
        Procs {
            list: Vec::new(),
            prev: HashMap::new(),
            cache: HashMap::new(),
            users: read_passwd("/etc/passwd"),
            guest_users: HashMap::new(),
            guest_names: HashMap::new(),
            passwd_mtime: passwd_mtime(),
            last: None,
            clk_tck: if clk_tck > 0 { clk_tck as f64 } else { 100.0 },
            page: if page > 0 { page as u64 } else { 4096 },
        }
    }

    /// Forgets the CPU baseline, so the next scan is not an average over a pause.
    pub fn reset_rates(&mut self) {
        self.prev.clear();
        self.last = None;
    }

    pub fn scan(&mut self, mem_total: u64) {
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64()).filter(|d| *d > 0.0);
        self.last = Some(now);

        // A user added since start-up would otherwise show as a bare UID
        // until restart. Cached names are dropped with the map.
        let mtime = passwd_mtime();
        if mtime != self.passwd_mtime {
            self.passwd_mtime = mtime;
            self.users = read_passwd("/etc/passwd");
            self.guest_users.clear();
            self.cache.clear();
        }

        let mut list = Vec::with_capacity(self.list.len().max(256));
        let mut prev = HashMap::with_capacity(self.prev.len().max(256));
        let Ok(dir) = fs::read_dir("/proc") else { return };
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            // A process can exit between the directory listing and the read;
            // that is normal and it is simply skipped.
            let Some(st) = read_stat(pid) else { continue };
            let ticks = st.utime + st.stime;
            let cpu_pct = match (dt, self.prev.get(&pid)) {
                (Some(dt), Some(&(start, t))) if start == st.start => {
                    ticks.saturating_sub(t) as f64 / self.clk_tck / dt * 100.0
                }
                _ => 0.0,
            };
            prev.insert(pid, (st.start, ticks));

            let fresh = self.cache.get(&pid).is_none_or(|c| c.start != st.start);
            if fresh {
                let uid = entry.metadata().map(|m| m.uid()).unwrap_or(0);
                let container = read_container(pid);
                let user = self.user_name(pid, uid, container.as_deref());
                let (argv0, args) = read_args(pid);
                // comm is cut at 15 bytes; argv[0] has the whole name.
                let name = (st.comm.len() == 15)
                    .then(|| argv0.rsplit('/').next().unwrap_or("").to_string())
                    .filter(|n| n.len() > 15 && n.starts_with(&st.comm));
                self.cache.insert(pid, Cached { start: st.start, name, args, user, container });
            }
            let c = &self.cache[&pid];
            let rss = st.rss_pages * self.page;
            // Kernel threads are children of kthreadd (PID 2), and have no
            // command line of their own.
            let kernel = pid == 2 || st.ppid == 2;
            list.push(Proc {
                pid,
                ppid: st.ppid,
                comm: c.name.clone().unwrap_or(st.comm),
                args: c.args.clone(),
                kernel,
                user: c.user.clone(),
                container: c.container.clone(),
                state: st.state,
                nice: st.nice,
                threads: st.threads,
                rss,
                cpu_time_s: ticks as f64 / self.clk_tck,
                start: st.start,
                cpu_pct,
                mem_pct: if mem_total > 0 { rss as f64 / mem_total as f64 * 100.0 } else { 0.0 },
            });
        }
        self.cache.retain(|pid, _| prev.contains_key(pid));
        // Read again after the guest stops, so a rename shows on its restart.
        let running: HashSet<&str> = list.iter().filter_map(|p| p.container.as_deref()).collect();
        self.guest_names.retain(|g, _| running.contains(g.as_str()));
        for g in running {
            if guest_of(g).is_some() && !self.guest_names.contains_key(g) {
                self.guest_names.insert(g.to_string(), read_guest_name(g));
            }
        }
        self.prev = prev;
        self.list = list;
    }
}

impl Procs {
    /// A name for `uid` as the process itself would see it. A process in an
    /// unprivileged container runs as a shifted host UID (100034 for the
    /// container's 34): that is mapped back through the process's
    /// `uid_map` and named from the container's own `/etc/passwd`.
    fn user_name(&mut self, pid: i32, uid: u32, container: Option<&str>) -> String {
        if let Some(ct) = container.filter(|c| c.starts_with("CT ")) {
            if let Some(inner) = inner_uid(pid, uid) {
                let users = self
                    .guest_users
                    .entry(ct.to_string())
                    .or_insert_with(|| read_passwd(&format!("/proc/{}/root/etc/passwd", pid)));
                if let Some(n) = users.get(&inner) {
                    return n.clone();
                }
                return inner.to_string();
            }
        }
        self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string())
    }
}

/// The UID inside the process's user namespace for host UID `uid`, if the
/// namespace maps it differently from the host.
fn inner_uid(pid: i32, uid: u32) -> Option<u32> {
    let map = fs::read_to_string(format!("/proc/{}/uid_map", pid)).ok()?;
    map_uid(&map, uid)
}

pub fn map_uid(map: &str, uid: u32) -> Option<u32> {
    for line in map.lines() {
        let v: Vec<u64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if let [inside, outside, count] = v[..] {
            if inside == outside {
                // The identity map: the initial namespace, nothing to undo.
                continue;
            }
            let u = uid as u64;
            if u >= outside && u < outside + count {
                return Some((u - outside + inside) as u32);
            }
        }
    }
    None
}

struct RawStat {
    comm: String,
    state: char,
    ppid: i32,
    utime: u64,
    stime: u64,
    nice: i32,
    threads: u64,
    start: u64,
    rss_pages: u64,
}

fn read_stat(pid: i32) -> Option<RawStat> {
    parse_stat(&fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?)
}

/// The name sits in parentheses and may itself contain spaces and ")", so
/// the fields are counted from the *last* ")".
fn parse_stat(s: &str) -> Option<RawStat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let f: Vec<&str> = s.get(close + 1..)?.split_ascii_whitespace().collect();
    let n = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    Some(RawStat {
        comm,
        state: f.first()?.chars().next().unwrap_or('?'),
        ppid: f.get(1).and_then(|x| x.parse().ok()).unwrap_or(0),
        utime: n(11),
        stime: n(12),
        nice: f.get(16).and_then(|x| x.parse().ok()).unwrap_or(0),
        threads: n(17),
        start: n(19),
        rss_pages: n(21),
    })
}

/// argv[0], and the arguments after it joined by spaces.
fn read_args(pid: i32) -> (String, String) {
    let raw = fs::read(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
    let mut parts = raw.split(|b| *b == 0).filter(|p| !p.is_empty());
    // Control characters are dropped: a command line is chosen by whoever
    // started the process.
    let clean = |s: String| -> String { s.chars().filter(|c| !c.is_control()).take(400).collect() };
    let argv0 = parts.next().map(|p| clean(String::from_utf8_lossy(p).into_owned())).unwrap_or_default();
    let joined: Vec<String> = parts.map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    (argv0, clean(joined.join(" ")))
}

fn read_cgroup(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()
}

/// Which container or VM a process belongs to, read from its cgroup path:
/// LXC on Proxmox (`/lxc/101/…`, `/lxc.payload.101/…`), a Proxmox VM's QEMU
/// (`/qemu.slice/105.scope`), Docker and Podman.
pub fn read_container(pid: i32) -> Option<String> {
    container_of(&read_cgroup(pid)?)
}

pub fn container_of(cgroup: &str) -> Option<String> {
    for line in cgroup.lines() {
        let path = line.splitn(3, ':').nth(2).unwrap_or("");
        for seg in path.split('/') {
            if let Some(id) = seg.strip_prefix("lxc.payload.").or_else(|| seg.strip_prefix("lxc.monitor.")) {
                return Some(format!("CT {}", id));
            }
        }
        if let Some(rest) = path.split("/lxc/").nth(1) {
            return Some(format!("CT {}", rest.split('/').next().unwrap_or("?")));
        }
        if let Some(rest) = path.split("qemu.slice/").nth(1) {
            let id = rest.split('/').next().unwrap_or("").trim_end_matches(".scope");
            return Some(format!("VM {}", id));
        }
        if path.contains("/docker-") || path.contains("/docker/") {
            return Some("docker".into());
        }
        if path.contains("libpod-") {
            return Some("podman".into());
        }
    }
    None
}

fn read_passwd(path: &str) -> HashMap<u32, String> {
    let s = fs::read_to_string(path).unwrap_or_default();
    s.lines()
        .filter_map(|l| {
            let v: Vec<&str> = l.split(':').collect();
            Some((v.get(2)?.parse().ok()?, v.first()?.to_string()))
        })
        .collect()
}

// ───────────────────────── signals and nice ─────────────────────────

/// The signals offered in the signal menu: (key, signal, name, number).
pub const SIGNALS: [(char, Signal, &str, i32); 12] = [
    ('t', Signal::Term, "TERM", 15),
    ('k', Signal::Kill, "KILL", 9),
    ('i', Signal::Int, "INT", 2),
    ('h', Signal::Hup, "HUP", 1),
    ('q', Signal::Quit, "QUIT", 3),
    ('s', Signal::Stop, "STOP", 19),
    ('c', Signal::Cont, "CONT", 18),
    ('1', Signal::Usr1, "USR1", 10),
    ('2', Signal::Usr2, "USR2", 12),
    ('a', Signal::Alarm, "ALRM", 14),
    ('w', Signal::Winch, "WINCH", 28),
    ('p', Signal::Tstp, "TSTP", 20),
];

fn pid_of(pid: i32) -> Result<Pid, String> {
    if pid <= 1 {
        return Err("refusing to act on PID 1 or lower".into());
    }
    Pid::from_raw(pid).ok_or_else(|| "invalid PID".into())
}

/// Sends `sig` to `pid`. The error is the OS's own, for the status line.
pub fn signal(pid: i32, sig: Signal) -> Result<(), String> {
    rustix::process::kill_process(pid_of(pid)?, sig).map_err(|e| e.to_string())
}

/// Sets the nice value. Lowering it (raising priority) needs root.
pub fn renice(pid: i32, nice: i32) -> Result<(), String> {
    rustix::process::setpriority_process(Some(pid_of(pid)?), nice.clamp(-20, 19)).map_err(|e| e.to_string())
}

// ───────────────────────────── sorting ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Pid,
    Where,
    User,
    Name,
    Nice,
    Threads,
    Time,
    Cpu,
    Mem,
}

impl SortKey {
    pub const ALL: [SortKey; 9] = [
        SortKey::Pid,
        SortKey::Where,
        SortKey::User,
        SortKey::Name,
        SortKey::Nice,
        SortKey::Threads,
        SortKey::Time,
        SortKey::Cpu,
        SortKey::Mem,
    ];

    pub fn step(self, by: isize) -> SortKey {
        let i = Self::ALL.iter().position(|k| *k == self).unwrap_or(0) as isize;
        let n = Self::ALL.len() as isize;
        Self::ALL[((i + by) % n + n) as usize % n as usize]
    }

    /// Numbers read best largest first; names alphabetically.
    pub fn default_desc(self) -> bool {
        matches!(self, SortKey::Cpu | SortKey::Mem | SortKey::Threads | SortKey::Time)
    }

    pub fn name(self) -> &'static str {
        match self {
            SortKey::Pid => "pid",
            SortKey::Where => "where",
            SortKey::User => "user",
            SortKey::Name => "name",
            SortKey::Nice => "nice",
            SortKey::Threads => "threads",
            SortKey::Time => "time",
            SortKey::Cpu => "cpu",
            SortKey::Mem => "mem",
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        let s = s.trim().to_lowercase();
        Some(match s.as_str() {
            "process" | "program" | "command" => SortKey::Name,
            "memory" => SortKey::Mem,
            "thr" => SortKey::Threads,
            "container" => SortKey::Where,
            "ni" => SortKey::Nice,
            _ => return Self::ALL.iter().copied().find(|k| k.name() == s),
        })
    }
}

pub fn compare(a: &Proc, b: &Proc, key: SortKey, desc: bool) -> std::cmp::Ordering {
    let o = match key {
        SortKey::Pid => a.pid.cmp(&b.pid),
        SortKey::Where => a.container.cmp(&b.container),
        SortKey::User => a.user.cmp(&b.user),
        SortKey::Name => a.comm.to_lowercase().cmp(&b.comm.to_lowercase()),
        SortKey::Nice => a.nice.cmp(&b.nice),
        SortKey::Threads => a.threads.cmp(&b.threads),
        SortKey::Time => a.cpu_time_s.total_cmp(&b.cpu_time_s),
        SortKey::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
        SortKey::Mem => a.rss.cmp(&b.rss),
    };
    let o = if desc { o.reverse() } else { o };
    // Stable tie-break on PID, so equal rows do not shuffle every scan.
    o.then(a.pid.cmp(&b.pid))
}

pub fn matches(p: &Proc, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let n = needle.to_lowercase();
    p.comm.to_lowercase().contains(&n)
        || p.args.to_lowercase().contains(&n)
        || p.user.to_lowercase().contains(&n)
        || p.container.as_deref().is_some_and(|c| c.to_lowercase().contains(&n))
        || p.pid.to_string() == n
}

// ─────────────────────────── list and tree ───────────────────────────

/// One line of the process table.
pub struct Row<'a> {
    /// A process from the list, or for a guest row one made up of the
    /// guest's processes added together.
    pub p: Cow<'a, Proc>,
    /// Tree connectors drawn before the name; empty in the flat list.
    pub prefix: String,
    pub children: usize,
    pub collapsed: bool,
    /// A collapsed node's whole subtree, itself included: CPU and memory.
    pub sub: Option<(f64, u64)>,
    /// A whole VM or container, not one process.
    pub guest: bool,
}

impl<'a> Row<'a> {
    fn of(p: &'a Proc) -> Self {
        Row { p: Cow::Borrowed(p), prefix: String::new(), children: 0, collapsed: false, sub: None, guest: false }
    }
}

pub fn flat<'a>(list: &'a [Proc], keep: impl Fn(&Proc) -> bool, key: SortKey, desc: bool) -> Vec<Row<'a>> {
    let mut v: Vec<&Proc> = list.iter().filter(|p| keep(p)).collect();
    v.sort_by(|a, b| compare(a, b, key, desc));
    v.into_iter().map(Row::of).collect()
}

// ─────────────────────────── guest rows ───────────────────────────

/// The order of the guest rows. Within a type, by VMID, as Proxmox's
/// resource tree sorts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestOrder {
    /// By VMID alone, VMs and containers mixed.
    Vmid,
    /// VMs, then containers.
    VmFirst,
    /// Containers, then VMs: the Proxmox tree's default ("Group Guest Types").
    CtFirst,
}

impl GuestOrder {
    pub const ALL: [GuestOrder; 3] = [GuestOrder::Vmid, GuestOrder::VmFirst, GuestOrder::CtFirst];

    pub fn name(self) -> &'static str {
        match self {
            GuestOrder::VmFirst => "vm-first",
            GuestOrder::CtFirst => "ct-first",
            GuestOrder::Vmid => "vmid",
        }
    }

    pub fn parse(s: &str) -> Option<GuestOrder> {
        let s = s.trim().to_lowercase();
        Self::ALL.iter().copied().find(|o| o.name() == s)
    }

    pub fn next(self) -> GuestOrder {
        let i = Self::ALL.iter().position(|o| *o == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    fn rank(self, vm: bool, id: u32) -> (u8, u32) {
        let first = match self {
            GuestOrder::VmFirst => !vm as u8,
            GuestOrder::CtFirst => vm as u8,
            GuestOrder::Vmid => 0,
        };
        (first, id)
    }
}

/// ("VM" or "CT", VMID) for a Proxmox guest's label. An unreadable VMID
/// sorts last.
fn guest_of(container: &str) -> Option<(bool, u32)> {
    let (kind, id) = container.split_once(' ')?;
    let vm = match kind {
        "VM" => true,
        "CT" => false,
        _ => return None,
    };
    Some((vm, id.parse().unwrap_or(u32::MAX)))
}

/// Whether the process belongs to a Proxmox VM or container.
pub fn in_guest(p: &Proc) -> bool {
    p.container.as_deref().and_then(guest_of).is_some()
}

/// A guest's name from its Proxmox config: `name` for a VM, `hostname` for
/// a container. Only the current settings, not a snapshot's section.
pub fn read_guest_name(container: &str) -> Option<String> {
    let (vm, id) = guest_of(container)?;
    let path = if vm { format!("/etc/pve/qemu-server/{}.conf", id) } else { format!("/etc/pve/lxc/{}.conf", id) };
    guest_name_in(&fs::read_to_string(path).ok()?, if vm { "name" } else { "hostname" })
}

fn guest_name_in(conf: &str, key: &str) -> Option<String> {
    conf.lines()
        .take_while(|l| !l.starts_with('['))
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()))
        .filter(|n| !n.is_empty())
}

/// A VM's name from QEMU's `-name NAME,debug-threads=on`, where the config
/// cannot be read.
fn qemu_name(args: &str) -> Option<String> {
    let rest = args.split_once("-name ")?.1;
    let v = rest.split(' ').next()?.split(',').next()?;
    (!v.is_empty()).then(|| v.to_string())
}

/// One row per Proxmox VM and container, its processes added together, in
/// `order`. A guest is listed if any of its processes passes `keep`, or its
/// name holds `filter`. Guests in `open` have their processes listed under
/// them, sorted by the sort key; the figures are always the guest's whole.
#[allow(clippy::too_many_arguments)]
pub fn guests<'a>(
    list: &'a [Proc],
    keep: impl Fn(&Proc) -> bool,
    filter: &str,
    order: GuestOrder,
    names: &HashMap<String, Option<String>>,
    key: SortKey,
    desc: bool,
    open: &HashSet<i32>,
) -> Vec<Row<'a>> {
    let mut groups: HashMap<&str, Vec<&'a Proc>> = HashMap::new();
    for p in list {
        if let Some(c) = p.container.as_deref().filter(|c| guest_of(c).is_some()) {
            groups.entry(c).or_default().push(p);
        }
    }
    let mut labels: Vec<&str> = groups.keys().copied().collect();
    labels.sort_by_key(|l| guest_of(l).map(|(vm, id)| order.rank(vm, id)));

    let needle = filter.to_lowercase();
    let mut out = Vec::new();
    for label in labels {
        let members = &groups[label];
        let (vm, id) = guest_of(label).unwrap_or((false, u32::MAX));
        let name = names
            .get(label)
            .cloned()
            .flatten()
            .or_else(|| vm.then(|| members.iter().find_map(|p| qemu_name(&p.args))).flatten());
        let kept: Vec<&'a Proc> = members.iter().copied().filter(|p| keep(p)).collect();
        let named = !needle.is_empty() && name.as_deref().is_some_and(|n| n.to_lowercase().contains(&needle));
        if kept.is_empty() && !named {
            continue;
        }
        // A negative PID: never a real process, so the signal and renice
        // keys can refuse it, and the selection can follow it.
        let pid = if id == u32::MAX { i32::MIN + 1 } else { -(id.min(i32::MAX as u32) as i32) };
        let sum = Proc {
            pid,
            ppid: 0,
            comm: name.unwrap_or_else(|| label.to_string()),
            args: String::new(),
            kernel: false,
            user: String::new(),
            container: Some(label.to_string()),
            state: if members.iter().any(|p| p.state == 'R') { 'R' } else { 'S' },
            nice: 0,
            threads: members.iter().map(|p| p.threads).sum(),
            rss: members.iter().map(|p| p.rss).sum(),
            cpu_time_s: members.iter().map(|p| p.cpu_time_s).sum(),
            start: 0,
            cpu_pct: members.iter().map(|p| p.cpu_pct).sum(),
            mem_pct: members.iter().map(|p| p.mem_pct).sum(),
        };
        let expanded = open.contains(&pid);
        out.push(Row {
            p: Cow::Owned(sum),
            prefix: String::new(),
            children: members.len(),
            collapsed: !expanded,
            sub: None,
            guest: true,
        });
        if expanded {
            let mut shown = if kept.is_empty() { members.clone() } else { kept };
            shown.sort_by(|a, b| compare(a, b, key, desc));
            let n = shown.len();
            out.extend(shown.into_iter().enumerate().map(|(k, p)| Row {
                prefix: (if k + 1 == n { "└─" } else { "├─" }).into(),
                ..Row::of(p)
            }));
        }
    }
    out
}

/// The process tree. Siblings are ordered by the sort key. A process is shown
/// if it passes `keep` or anything beneath it does, so filtering keeps the
/// path down to every match. A collapsed node hides its subtree and carries
/// the subtree's totals instead.
pub fn tree<'a>(
    list: &'a [Proc],
    keep: impl Fn(&Proc) -> bool,
    key: SortKey,
    desc: bool,
    collapsed: &HashSet<i32>,
) -> Vec<Row<'a>> {
    let index: HashMap<i32, usize> = list.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (i, p) in list.iter().enumerate() {
        if p.ppid != p.pid && index.contains_key(&p.ppid) {
            children.entry(p.ppid).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    let by = |a: &usize, b: &usize| compare(&list[*a], &list[*b], key, desc);
    roots.sort_by(by);
    for c in children.values_mut() {
        c.sort_by(by);
    }

    // Post-order: which nodes have a kept process at or below them.
    fn mark(
        i: usize,
        list: &[Proc],
        children: &HashMap<i32, Vec<usize>>,
        keep: &dyn Fn(&Proc) -> bool,
        w: &mut [bool],
    ) -> bool {
        let mut any = keep(&list[i]);
        if let Some(cs) = children.get(&list[i].pid) {
            for &c in cs {
                any |= mark(c, list, children, keep, w);
            }
        }
        w[i] = any;
        any
    }
    let mut wanted = vec![false; list.len()];
    for &r in &roots {
        mark(r, list, &children, &keep, &mut wanted);
    }

    fn subtree(i: usize, list: &[Proc], children: &HashMap<i32, Vec<usize>>) -> (f64, u64) {
        let mut acc = (list[i].cpu_pct, list[i].rss);
        if let Some(cs) = children.get(&list[i].pid) {
            for &c in cs {
                let s = subtree(c, list, children);
                acc.0 += s.0;
                acc.1 += s.1;
            }
        }
        acc
    }

    struct Walk<'b, 'a> {
        list: &'a [Proc],
        children: &'b HashMap<i32, Vec<usize>>,
        wanted: &'b [bool],
        collapsed: &'b HashSet<i32>,
        out: Vec<Row<'a>>,
    }
    impl Walk<'_, '_> {
        fn visit(&mut self, i: usize, lead: &str, last: bool, root: bool) {
            let p = &self.list[i];
            let kids: Vec<usize> = self
                .children
                .get(&p.pid)
                .map(|v| v.iter().copied().filter(|c| self.wanted[*c]).collect())
                .unwrap_or_default();
            let folded = self.collapsed.contains(&p.pid) && !kids.is_empty();
            let prefix = if root { String::new() } else { format!("{}{}", lead, if last { "└─" } else { "├─" }) };
            self.out.push(Row {
                prefix,
                children: kids.len(),
                collapsed: folded,
                sub: folded.then(|| subtree(i, self.list, self.children)),
                ..Row::of(p)
            });
            if folded {
                return;
            }
            let next = if root { String::new() } else { format!("{}{}", lead, if last { "  " } else { "│ " }) };
            let n = kids.len();
            for (k, c) in kids.into_iter().enumerate() {
                self.visit(c, &next, k + 1 == n, false);
            }
        }
    }
    let mut w = Walk { list, children: &children, wanted: &wanted, collapsed, out: Vec::new() };
    for r in roots {
        if wanted[r] {
            w.visit(r, "", true, true);
        }
    }
    w.out
}

/// Every PID with children, for collapsing the whole tree at once.
pub fn parents(list: &[Proc]) -> HashSet<i32> {
    let pids: HashSet<i32> = list.iter().map(|p| p.pid).collect();
    list.iter().filter(|p| p.ppid != p.pid && pids.contains(&p.ppid)).map(|p| p.ppid).collect()
}

// ─────────────────────────── detail view ───────────────────────────

pub const DETAIL_HISTORY: usize = 300;

/// What the detail box shows for one process, kept up to date at each scan.
/// It outlives the process: once it exits the last figures stay, marked dead.
pub struct Detail {
    pub pid: i32,
    start: u64,
    pub last: Proc,
    pub alive: bool,
    pub parent: Option<String>,
    pub cgroup: String,
    pub exe: Option<String>,
    pub cpu_hist: VecDeque<f64>,
    /// Cumulative bytes read and written to storage, when readable: other
    /// users' processes need root.
    pub io: Option<(u64, u64)>,
    pub io_rate: Option<(f64, f64)>,
    io_at: Instant,
}

impl Detail {
    pub fn new(p: &Proc, list: &[Proc]) -> Self {
        let mut d = Detail {
            pid: p.pid,
            start: p.start,
            last: p.clone(),
            alive: true,
            parent: None,
            cgroup: read_cgroup(p.pid)
                .and_then(|s| s.lines().find_map(|l| l.strip_prefix("0::").map(str::to_string)))
                .unwrap_or_default(),
            exe: fs::read_link(format!("/proc/{}/exe", p.pid)).ok().map(|p| p.to_string_lossy().into_owned()),
            cpu_hist: VecDeque::with_capacity(DETAIL_HISTORY),
            io: read_io(p.pid),
            io_rate: None,
            io_at: Instant::now(),
        };
        d.update(list);
        d
    }

    pub fn update(&mut self, list: &[Proc]) {
        let Some(p) = list.iter().find(|p| p.pid == self.pid && p.start == self.start) else {
            self.alive = false;
            return;
        };
        self.last = p.clone();
        self.parent = list.iter().find(|q| q.pid == p.ppid).map(|q| q.comm.clone());
        if self.cpu_hist.len() == DETAIL_HISTORY {
            self.cpu_hist.pop_front();
        }
        self.cpu_hist.push_back(p.cpu_pct);
        let now = Instant::now();
        let io = read_io(self.pid);
        let dt = now.duration_since(self.io_at).as_secs_f64();
        if let (Some((r0, w0)), Some((r1, w1))) = (self.io, io) {
            if dt > 0.0 {
                self.io_rate = Some((r1.saturating_sub(r0) as f64 / dt, w1.saturating_sub(w0) as f64 / dt));
            }
        }
        self.io = io;
        self.io_at = now;
    }
}

fn read_io(pid: i32) -> Option<(u64, u64)> {
    let s = fs::read_to_string(format!("/proc/{}/io", pid)).ok()?;
    let get = |k: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.trim_start_matches(':').trim().parse::<u64>().ok())
    };
    Some((get("read_bytes")?, get("write_bytes")?))
}

pub fn state_name(c: char) -> &'static str {
    match c {
        'R' => "running",
        'S' => "sleeping",
        'D' => "disk sleep",
        'Z' => "zombie",
        'T' => "stopped",
        't' => "traced",
        'I' => "idle",
        'X' => "dead",
        'P' => "parked",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_awkward_name() {
        let s = "1234 (a b) c)) S 1 1234 1234 0 -1 4194560 100 0 0 0 7 3 0 0 20 5 5 0 4242 1000 25 18446744073709551615";
        let st = parse_stat(s).unwrap();
        assert_eq!(st.comm, "a b) c)");
        assert_eq!(st.state, 'S');
        assert_eq!(st.ppid, 1);
        assert_eq!(st.utime + st.stime, 10);
        assert_eq!(st.nice, 5);
        assert_eq!(st.threads, 5);
        assert_eq!(st.start, 4242);
        assert_eq!(st.rss_pages, 25);
    }

    #[test]
    fn containers_from_cgroups() {
        assert_eq!(container_of("0::/lxc.payload.101/system.slice/x.service").as_deref(), Some("CT 101"));
        assert_eq!(container_of("0::/lxc/104/ns/init.scope").as_deref(), Some("CT 104"));
        assert_eq!(container_of("0::/qemu.slice/105.scope").as_deref(), Some("VM 105"));
        assert_eq!(container_of("0::/system.slice/docker-0123abcd.scope").as_deref(), Some("docker"));
        assert_eq!(container_of("0::/user.slice/user-1000.slice/session-3.scope"), None);
    }

    #[test]
    fn container_uids_map_back() {
        let unpriv = "         0     100000      65536\n";
        assert_eq!(map_uid(unpriv, 100034), Some(34));
        assert_eq!(map_uid(unpriv, 1000), None);
        assert_eq!(map_uid("         0          0 4294967295\n", 0), None);
    }

    #[test]
    fn sort_key_wraps_and_parses() {
        assert_eq!(SortKey::Pid.step(-1), SortKey::Mem);
        assert_eq!(SortKey::Mem.step(1), SortKey::Pid);
        assert_eq!(SortKey::parse("memory"), Some(SortKey::Mem));
        assert_eq!(SortKey::parse("TIME"), Some(SortKey::Time));
        assert_eq!(SortKey::parse("bogus"), None);
    }

    fn p(pid: i32, ppid: i32, cpu: f64) -> Proc {
        Proc {
            pid,
            ppid,
            comm: format!("p{}", pid),
            args: String::new(),
            kernel: false,
            user: "u".into(),
            container: None,
            state: 'S',
            nice: 0,
            threads: 1,
            rss: 10,
            cpu_time_s: 0.0,
            start: 0,
            cpu_pct: cpu,
            mem_pct: 0.0,
        }
    }

    #[test]
    fn tree_orders_draws_and_collapses() {
        let list = vec![p(1, 0, 0.0), p(10, 1, 1.0), p(11, 10, 5.0), p(12, 10, 9.0), p(20, 1, 3.0)];
        let rows = tree(&list, |_| true, SortKey::Cpu, true, &HashSet::new());
        let order: Vec<i32> = rows.iter().map(|r| r.p.pid).collect();
        // Siblings by CPU, descending: 20 (3) before 10 (1); 12 before 11.
        assert_eq!(order, vec![1, 20, 10, 12, 11]);
        assert_eq!(rows[1].prefix, "├─");
        assert_eq!(rows[2].prefix, "└─");
        assert_eq!(rows[3].prefix, "  ├─");
        assert_eq!(rows[4].prefix, "  └─");

        let folded: HashSet<i32> = [10].into();
        let rows = tree(&list, |_| true, SortKey::Cpu, true, &folded);
        assert_eq!(rows.len(), 3);
        let r = rows.iter().find(|r| r.p.pid == 10).unwrap();
        assert!(r.collapsed);
        assert_eq!(r.sub, Some((15.0, 30)));
    }

    fn guest(pid: i32, at: &str, cpu: f64) -> Proc {
        Proc { container: Some(at.into()), ..p(pid, 1, cpu) }
    }

    #[test]
    fn guests_add_up_in_proxmox_order() {
        let list = vec![
            p(1, 0, 1.0),
            guest(10, "CT 200", 2.0),
            guest(11, "CT 200", 3.0),
            guest(20, "VM 205", 50.0),
            guest(30, "CT 104", 4.0),
            guest(40, "VM 103", 1.0),
            Proc { container: Some("docker".into()), ..p(50, 1, 9.0) },
        ];
        let names: HashMap<String, Option<String>> = [("CT 104".to_string(), Some("media".to_string()))].into();
        let none = HashSet::new();
        let at = |order| -> Vec<String> {
            guests(&list, |_| true, "", order, &names, SortKey::Cpu, true, &none)
                .iter()
                .map(|r| r.p.container.clone().unwrap())
                .collect()
        };
        assert_eq!(at(GuestOrder::VmFirst), ["VM 103", "VM 205", "CT 104", "CT 200"]);
        assert_eq!(at(GuestOrder::CtFirst), ["CT 104", "CT 200", "VM 103", "VM 205"]);
        assert_eq!(at(GuestOrder::Vmid), ["VM 103", "CT 104", "CT 200", "VM 205"]);

        let rows = guests(&list, |_| true, "", GuestOrder::Vmid, &names, SortKey::Cpu, true, &none);
        let ct = rows.iter().find(|r| r.p.container.as_deref() == Some("CT 200")).unwrap();
        assert!(ct.guest && ct.collapsed);
        assert_eq!((ct.p.cpu_pct, ct.p.rss, ct.children), (5.0, 20, 2));
        assert!(ct.p.pid < 0);
        assert_eq!(rows[1].p.comm, "media");
        // Docker and the host are not guests.
        assert!(!in_guest(&list[0]) && !in_guest(&list[6]) && in_guest(&list[1]));

        // Opened: the processes beneath it, by the sort key.
        let open: HashSet<i32> = [ct.p.pid].into();
        let rows = guests(&list, |_| true, "", GuestOrder::Vmid, &names, SortKey::Cpu, true, &open);
        let pids: Vec<i32> = rows.iter().map(|r| r.p.pid).collect();
        assert_eq!(pids[2..5], [-200, 11, 10]);
        assert_eq!(rows[4].prefix, "└─");

        // Filtered: a guest stays if a process or its name matches.
        let rows = guests(&list, |q| q.pid == 20, "med", GuestOrder::Vmid, &names, SortKey::Cpu, true, &none);
        let kept: Vec<i32> = rows.iter().map(|r| r.p.pid).collect();
        assert_eq!(kept, [-104, -205]);
    }

    #[test]
    fn guest_names_from_config_and_qemu() {
        let conf = "boot: order=scsi0\nname: files\n\n[snap1]\nname: Old\n";
        assert_eq!(guest_name_in(conf, "name").as_deref(), Some("files"));
        assert_eq!(guest_name_in("arch: amd64\nhostname: media\n", "hostname").as_deref(), Some("media"));
        assert_eq!(guest_name_in("[snap]\nname: x\n", "name"), None);
        assert_eq!(qemu_name("-id 105 -name web,debug-threads=on -no-shutdown").as_deref(), Some("web"));
        assert_eq!(GuestOrder::parse("CT-first"), Some(GuestOrder::CtFirst));
        assert_eq!(GuestOrder::CtFirst.next(), GuestOrder::Vmid);
    }

    #[test]
    fn tree_filter_keeps_the_path_to_a_match() {
        let list = vec![p(1, 0, 0.0), p(10, 1, 1.0), p(11, 10, 5.0), p(20, 1, 3.0)];
        let rows = tree(&list, |q| q.pid == 11, SortKey::Pid, false, &HashSet::new());
        let order: Vec<i32> = rows.iter().map(|r| r.p.pid).collect();
        assert_eq!(order, vec![1, 10, 11]);
    }
}
