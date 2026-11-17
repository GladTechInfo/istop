//! Readers for the kernel's own accounting: `/proc` and `/sys`, nothing else.
//!
//! Every function here is a plain read of current counters. Rates are worked
//! out in `app`, from two readings and the wall time between them.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn read_num<T: std::str::FromStr>(path: impl AsRef<Path>) -> Option<T> {
    read(path)?.trim().parse().ok()
}

// ───────────────────────────── CPU ─────────────────────────────

/// One `cpu` line of `/proc/stat`, in USER_HZ ticks.
#[derive(Clone, Copy, Default, Debug)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// Guest time is already inside user and nice, so it is not added again.
    pub fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }
}

/// Shares of one interval, in percent.
#[derive(Clone, Copy, Default, Debug)]
pub struct CpuShare {
    pub busy: f64,
    pub user: f64,
    pub system: f64,
    pub iowait: f64,
    pub steal: f64,
}

pub fn share(prev: &CpuTimes, cur: &CpuTimes) -> CpuShare {
    let d = |a: u64, b: u64| b.saturating_sub(a) as f64;
    let total = d(prev.total(), cur.total());
    if total <= 0.0 {
        return CpuShare::default();
    }
    let pct = |x: f64| (x / total * 100.0).clamp(0.0, 100.0);
    let idle = d(prev.idle, cur.idle);
    let iowait = d(prev.iowait, cur.iowait);
    CpuShare {
        // iowait is idle time with I/O outstanding: the CPU could have run
        // something else, so it does not count as busy.
        busy: pct(total - idle - iowait),
        user: pct(d(prev.user, cur.user) + d(prev.nice, cur.nice)),
        system: pct(d(prev.system, cur.system) + d(prev.irq, cur.irq) + d(prev.softirq, cur.softirq)),
        iowait: pct(iowait),
        steal: pct(d(prev.steal, cur.steal)),
    }
}

#[derive(Clone, Default, Debug)]
pub struct Stat {
    pub total: CpuTimes,
    /// Indexed by CPU number. An offline CPU has no line and stays `None`.
    pub cores: Vec<Option<CpuTimes>>,
    pub ctxt: u64,
    pub intr: u64,
    pub running: u64,
    pub blocked: u64,
}

pub fn read_stat() -> Option<Stat> {
    parse_stat(&read("/proc/stat")?)
}

pub fn parse_stat(s: &str) -> Option<Stat> {
    let mut st = Stat::default();
    let mut seen_total = false;
    for line in s.lines() {
        let mut it = line.split_ascii_whitespace();
        let key = it.next().unwrap_or("");
        if let Some(rest) = key.strip_prefix("cpu") {
            let v: Vec<u64> = it.take(8).map(|x| x.parse().unwrap_or(0)).collect();
            let g = |i: usize| v.get(i).copied().unwrap_or(0);
            let t = CpuTimes {
                user: g(0),
                nice: g(1),
                system: g(2),
                idle: g(3),
                iowait: g(4),
                irq: g(5),
                softirq: g(6),
                steal: g(7),
            };
            if rest.is_empty() {
                st.total = t;
                seen_total = true;
            } else if let Ok(n) = rest.parse::<usize>() {
                if st.cores.len() <= n {
                    st.cores.resize(n + 1, None);
                }
                st.cores[n] = Some(t);
            }
            continue;
        }
        let first = || it.clone().next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        match key {
            "ctxt" => st.ctxt = first(),
            "intr" => st.intr = first(),
            "procs_running" => st.running = first(),
            "procs_blocked" => st.blocked = first(),
            _ => {}
        }
    }
    seen_total.then_some(st)
}

pub fn cpu_model() -> String {
    let info = read("/proc/cpuinfo").unwrap_or_default();
    for line in info.lines() {
        // x86 says "model name"; arm64 has no model name, only "Hardware" on
        // some boards, and nothing on most.
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if k == "model name" || k == "Hardware" {
                return tidy_model(v.trim());
            }
        }
    }
    "CPU".into()
}

/// Strips the marketing boilerplate that pushes the useful part of the name
/// off a narrow title: "(R)", "(TM)", "CPU", "Processor" and "@ 3.00GHz".
fn tidy_model(s: &str) -> String {
    let s = s.split(" @ ").next().unwrap_or(s);
    let mut out = s.replace("(R)", "").replace("(TM)", "").replace("(tm)", "");
    for w in [" CPU", " Processor", " processor", " with Radeon Graphics"] {
        out = out.replace(w, "");
    }
    for w in ["-Core", " Core"] {
        if let Some(i) = out.find(w) {
            // "12-Core" / "8 Core": drop the count that precedes it as well.
            let head = out[..i].trim_end();
            if let Some(j) = head.rfind(' ') {
                if head[j + 1..].chars().all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit()) {
                    out = format!("{}{}", &head[..j], &out[i + w.len()..]);
                }
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Mean current clock across online CPUs, and the highest rated maximum, MHz.
/// Without cpufreq (most VMs) the mean falls back to `/proc/cpuinfo` and there
/// is no maximum to measure it against.
pub fn cpu_freq(cores: usize) -> (Option<f64>, Option<f64>) {
    let mut sum = 0.0;
    let mut n = 0;
    let mut max: Option<f64> = None;
    for i in 0..cores {
        let base = format!("/sys/devices/system/cpu/cpu{}/cpufreq", i);
        if let Some(khz) = read_num::<f64>(format!("{}/scaling_cur_freq", base)) {
            sum += khz / 1000.0;
            n += 1;
        }
        if let Some(khz) = read_num::<f64>(format!("{}/cpuinfo_max_freq", base)) {
            max = Some(max.map_or(khz / 1000.0, |m: f64| m.max(khz / 1000.0)));
        }
    }
    if n > 0 {
        return (Some(sum / n as f64), max);
    }
    let info = read("/proc/cpuinfo").unwrap_or_default();
    let mhz: Vec<f64> = info
        .lines()
        .filter(|l| l.starts_with("cpu MHz"))
        .filter_map(|l| l.split_once(':')?.1.trim().parse().ok())
        .collect();
    if mhz.is_empty() {
        (None, max)
    } else {
        (Some(mhz.iter().sum::<f64>() / mhz.len() as f64), max)
    }
}

/// Where the package temperature lives, found once at start-up.
#[derive(Clone, Debug)]
pub struct TempSensor {
    pub inputs: Vec<PathBuf>,
    pub crit_c: f64,
    pub label: String,
}

/// The CPU package sensor: Intel's coretemp "Package id N", AMD's k10temp
/// Tctl (or zenpower's Tdie), or an ARM SoC's cpu_thermal. On a multi-socket
/// host every package is read and the hottest shown.
pub fn find_temp_sensor() -> Option<TempSensor> {
    let mut found: Option<TempSensor> = None;
    let dirs = fs::read_dir("/sys/class/hwmon").ok()?;
    let mut dirs: Vec<PathBuf> = dirs.flatten().map(|d| d.path()).collect();
    dirs.sort();
    for dir in dirs {
        let name = read(dir.join("name")).unwrap_or_default().trim().to_string();
        let wanted: &[&str] = match name.as_str() {
            "coretemp" => &["Package id"],
            "k10temp" => &["Tctl", "Tdie"],
            "zenpower" => &["Tdie", "Tctl"],
            "cpu_thermal" | "soc_thermal" => &[""],
            _ => continue,
        };
        for i in 1..64 {
            let input = dir.join(format!("temp{}_input", i));
            if !input.exists() {
                continue;
            }
            let label = read(dir.join(format!("temp{}_label", i))).unwrap_or_default().trim().to_string();
            if !wanted.iter().any(|w| label.starts_with(w)) {
                continue;
            }
            let crit = read_num::<f64>(dir.join(format!("temp{}_crit", i)))
                .or_else(|| read_num::<f64>(dir.join(format!("temp{}_max", i))))
                .map(|m| m / 1000.0)
                .filter(|c| *c > 20.0 && *c < 150.0)
                // k10temp publishes no limit; 95 °C is where Zen parts throttle.
                .unwrap_or(if name == "coretemp" { 100.0 } else { 95.0 });
            let s = found.get_or_insert_with(|| TempSensor {
                inputs: Vec::new(),
                crit_c: crit,
                label: if label.is_empty() { name.clone() } else { format!("{} {}", name, label) },
            });
            s.inputs.push(input);
            s.crit_c = s.crit_c.min(crit);
            // k10temp has both Tctl and Tdie on some parts; one is enough.
            if name != "coretemp" {
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    found
}

pub fn read_temp(s: &TempSensor) -> Option<f64> {
    s.inputs
        .iter()
        .filter_map(read_num::<f64>)
        .map(|m| m / 1000.0)
        .fold(None, |a: Option<f64>, t| Some(a.map_or(t, |a| a.max(t))))
}

pub fn loadavg() -> ([f64; 3], u64) {
    let s = read("/proc/loadavg").unwrap_or_default();
    let v: Vec<&str> = s.split_whitespace().collect();
    let f = |i: usize| v.get(i).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    // Fourth field is "running/total" scheduling entities: threads, not processes.
    let threads = v.get(3).and_then(|x| x.split('/').nth(1)).and_then(|x| x.parse().ok()).unwrap_or(0);
    ([f(0), f(1), f(2)], threads)
}

pub fn uptime_s() -> f64 {
    read("/proc/uptime")
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

// ─────────────────────────── Memory ────────────────────────────

#[derive(Clone, Default, Debug)]
pub struct Mem {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    /// Page cache, buffers and reclaimable slab: memory the kernel gives back
    /// on demand.
    pub cache: u64,
    pub shmem: u64,
    pub dirty: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    /// ZFS ARC size and ceiling, when the zfs module is loaded. The ARC is
    /// counted as *used* by MemAvailable, though most of it is reclaimable,
    /// so on a ZFS host it explains a "full" machine.
    pub arc: Option<(u64, u64)>,
}

impl Mem {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

pub fn read_mem() -> Mem {
    let s = read("/proc/meminfo").unwrap_or_default();
    let mut m: HashMap<&str, u64> = HashMap::new();
    for line in s.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let kb = v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
            m.insert(k, kb * 1024);
        }
    }
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    Mem {
        total: g("MemTotal"),
        available: g("MemAvailable"),
        free: g("MemFree"),
        cache: g("Buffers") + g("Cached") + g("SReclaimable"),
        shmem: g("Shmem"),
        dirty: g("Dirty"),
        swap_total: g("SwapTotal"),
        swap_free: g("SwapFree"),
        arc: read_arc(),
    }
}

fn read_arc() -> Option<(u64, u64)> {
    let s = read("/proc/spl/kstat/zfs/arcstats")?;
    let mut size = None;
    let mut max = None;
    for line in s.lines() {
        let v: Vec<&str> = line.split_whitespace().collect();
        if v.len() == 3 {
            match v[0] {
                "size" => size = v[2].parse().ok(),
                "c_max" => max = v[2].parse().ok(),
                _ => {}
            }
        }
    }
    Some((size?, max?))
}

// ──────────────────────────── Disks ────────────────────────────

/// Local filesystems only. Network and automounted filesystems are left out
/// on purpose: `statvfs` on a CIFS or NFS mount whose server is down blocks,
/// and on an autofs trigger it mounts the share.
const LOCAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "vfat", "exfat", "f2fs", "ntfs", "ntfs3",
    "bcachefs", "jfs", "reiserfs", "nilfs2",
];

#[derive(Clone, Debug)]
pub struct Mount {
    pub source: String,
    pub path: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    /// The block devices under it, as `/proc/diskstats` names them: one for
    /// an ordinary filesystem, every member disk for a ZFS pool.
    pub devices: Vec<String>,
}

/// `/proc/mounts` escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let s = rustix::fs::statvfs(path).ok()?;
    let total = s.f_blocks * s.f_frsize;
    // Used as df counts it: blocks not free, against blocks not reserved.
    let used = s.f_blocks.saturating_sub(s.f_bfree) * s.f_frsize;
    Some((total, used))
}

pub fn read_mounts() -> Vec<Mount> {
    let s = read("/proc/mounts").unwrap_or_default();
    let mut out: Vec<Mount> = Vec::new();
    for line in s.lines() {
        let v: Vec<&str> = line.split_whitespace().collect();
        if v.len() < 3 || !LOCAL_FS.contains(&v[2]) {
            continue;
        }
        let (source, path, fstype) = (unescape(v[0]), unescape(v[1]), v[2].to_string());
        // ZFS is shown per pool, from `zfs list` (see `Zfs`): statvfs on a
        // dataset reports that dataset's own data only, so a pool holding
        // 60 TB read as 256 KiB used.
        if fstype == "zfs" {
            continue;
        }
        // Bind mounts and btrfs subvolumes repeat one device: the first
        // mount (normally the shortest path) speaks for it.
        if out.iter().any(|m| m.source == source) {
            continue;
        }
        if path.starts_with("/snap/") || path.starts_with("/var/lib/docker/") {
            continue;
        }
        let Some((total, used)) = statvfs(&path) else { continue };
        if total == 0 {
            continue;
        }
        let devices = source.strip_prefix("/dev/").map(|_| kernel_name(&source)).into_iter().collect();
        out.push(Mount { source, path, fstype, total, used, devices });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// /dev/mapper/x and /dev/disk/by-*/… are symlinks to /dev/dm-N, sdX: the
/// name `/proc/diskstats` uses.
fn kernel_name(dev: &str) -> String {
    fs::canonicalize(dev)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| dev.trim_start_matches("/dev/").to_string())
}

// ───────────────────────────── ZFS ─────────────────────────────

/// ZFS pools, one row each. Space is the pool's root dataset as `zfs list`
/// reports it — used includes every dataset, snapshot and zvol, avail is
/// what can still be written — which is what a pool's "full" means. I/O is
/// the member disks' own counters, so it is physical I/O, VM zvols and
/// parity included. Both come from running the zfs tools, so they are
/// refreshed slowly: space every 10 s, membership every minute.
pub struct Zfs {
    zfs: Option<PathBuf>,
    zpool: Option<PathBuf>,
    pools: Vec<(String, u64, u64)>,
    members: HashMap<String, Vec<String>>,
    pools_at: Option<std::time::Instant>,
    members_at: Option<std::time::Instant>,
}

fn find_tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/usr/local/sbin", "/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
}

fn run(tool: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `zfs list -Hp -d 0 -o name,used,avail`: name, used, used + avail.
pub fn parse_zfs_list(s: &str) -> Vec<(String, u64, u64)> {
    s.lines()
        .filter_map(|l| {
            let v: Vec<&str> = l.split('\t').collect();
            let used = v.get(1)?.parse::<u64>().ok()?;
            let avail = v.get(2)?.parse::<u64>().ok()?;
            Some((v[0].to_string(), used, used + avail))
        })
        .collect()
}

/// `zpool list -vHP -o name`: a pool name at the margin, then its vdevs
/// indented, member devices as full paths.
pub fn parse_zpool_members(s: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut pool = String::new();
    for line in s.lines() {
        if !line.starts_with('\t') && !line.starts_with(' ') {
            pool = line.trim().to_string();
            continue;
        }
        let dev = line.trim();
        if dev.starts_with("/dev/") {
            out.entry(pool.clone()).or_default().push(dev.to_string());
        }
    }
    out
}

impl Zfs {
    pub fn new() -> Option<Zfs> {
        if !Path::new("/proc/spl/kstat/zfs").exists() {
            return None;
        }
        Some(Zfs {
            zfs: find_tool("zfs"),
            zpool: find_tool("zpool"),
            pools: Vec::new(),
            members: HashMap::new(),
            pools_at: None,
            members_at: None,
        })
    }

    pub fn mounts(&mut self) -> Vec<Mount> {
        let now = std::time::Instant::now();
        let due = |at: Option<std::time::Instant>, secs| at.is_none_or(|t| now.duration_since(t).as_secs() >= secs);
        if due(self.pools_at, 10) {
            if let Some(t) = &self.zfs {
                if let Some(s) = run(t, &["list", "-Hp", "-d", "0", "-o", "name,used,avail"]) {
                    self.pools = parse_zfs_list(&s);
                }
            }
            self.pools_at = Some(now);
        }
        if due(self.members_at, 60) {
            if let Some(t) = &self.zpool {
                if let Some(s) = run(t, &["list", "-vHP", "-o", "name"]) {
                    self.members = parse_zpool_members(&s)
                        .into_iter()
                        .map(|(p, devs)| (p, devs.iter().map(|d| kernel_name(d)).collect()))
                        .collect();
                }
            }
            self.members_at = Some(now);
        }
        self.pools
            .iter()
            .filter(|(_, _, total)| *total > 0)
            .map(|(name, used, total)| Mount {
                source: format!("zfs:{}", name),
                path: name.clone(),
                fstype: "zpool".into(),
                total: *total,
                used: *used,
                devices: self.members.get(name).cloned().unwrap_or_default(),
            })
            .collect()
    }
}

/// Cumulative counters for one block device.
#[derive(Clone, Copy, Default, Debug)]
pub struct DiskIo {
    pub read_b: u64,
    pub write_b: u64,
    /// Milliseconds the device had I/O in flight: its busy time.
    pub busy_ms: u64,
}

/// By device name. Sectors are always 512 bytes here, whatever the device's
/// own sector size.
pub fn read_diskstats() -> HashMap<String, DiskIo> {
    let s = read("/proc/diskstats").unwrap_or_default();
    let mut out = HashMap::new();
    for line in s.lines() {
        let v: Vec<&str> = line.split_whitespace().collect();
        if v.len() >= 13 {
            let n = |i: usize| v[i].parse::<u64>().unwrap_or(0);
            out.insert(v[2].to_string(), DiskIo { read_b: n(5) * 512, write_b: n(9) * 512, busy_ms: n(12) });
        }
    }
    out
}

/// The container istop itself runs in, if any, as btop shows it: Podman and
/// Docker mark their containers with a file; systemd-nspawn and LXC set
/// `/run/systemd/container`.
pub fn own_container() -> Option<String> {
    if Path::new("/run/.containerenv").exists() {
        return Some("podman".into());
    }
    if Path::new("/.dockerenv").exists() {
        return Some("docker".into());
    }
    read("/run/systemd/container").map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ─────────────────────────── Network ───────────────────────────

/// Bytes received and sent, by interface.
pub fn read_netdev() -> Vec<(String, u64, u64)> {
    let s = read("/proc/net/dev").unwrap_or_default();
    let mut out = Vec::new();
    for line in s.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let v: Vec<u64> = rest.split_whitespace().map(|x| x.parse().unwrap_or(0)).collect();
        if v.len() >= 9 {
            out.push((name.trim().to_string(), v[0], v[8]));
        }
    }
    out
}

/// A physical port: one with a device behind it. Bridges, taps, veths,
/// bonds and tunnels have none.
pub fn is_physical(iface: &str) -> bool {
    Path::new("/sys/class/net").join(iface).join("device").exists()
}

/// Negotiated link speed in Mbit/s, where the driver reports one.
pub fn link_speed(iface: &str) -> Option<u64> {
    read_num::<i64>(Path::new("/sys/class/net").join(iface).join("speed"))
        .filter(|s| *s > 0)
        .map(|s| s as u64)
}

pub fn hostname() -> String {
    read("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|| "localhost".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parses_cores_and_gaps() {
        let s = "cpu  10 0 5 80 5 0 0 0 0 0\ncpu0 5 0 2 40 3 0 0 0\ncpu2 5 0 3 40 2 0 0 0\nctxt 99\nprocs_running 3\n";
        let st = parse_stat(s).unwrap();
        assert_eq!(st.total.total(), 100);
        assert_eq!(st.cores.len(), 3);
        assert!(st.cores[1].is_none());
        assert_eq!(st.ctxt, 99);
        assert_eq!(st.running, 3);
    }

    #[test]
    fn share_excludes_iowait_from_busy() {
        let a = CpuTimes::default();
        let b = CpuTimes { user: 20, system: 10, idle: 60, iowait: 10, ..Default::default() };
        let s = share(&a, &b);
        assert!((s.busy - 30.0).abs() < 1e-9);
        assert!((s.iowait - 10.0).abs() < 1e-9);
    }

    #[test]
    fn model_is_tidied() {
        assert_eq!(tidy_model("Intel(R) Xeon(R) CPU E5-2690 v3 @ 2.60GHz"), "Intel Xeon E5-2690 v3");
        assert_eq!(tidy_model("AMD Ryzen 7 5800X 8-Core Processor"), "AMD Ryzen 7 5800X");
    }

    #[test]
    fn zfs_tool_output_parses() {
        let l = parse_zfs_list("tank\t4000000000000\t2000000000000\nrpool\t20000000000\t480000000000\n");
        assert_eq!(l[0], ("tank".into(), 4000000000000, 4000000000000 + 2000000000000));
        let m = parse_zpool_members(
            "fast\n\tmirror-0\n\t/dev/disk/by-id/ata-A-part1\n\t/dev/disk/by-id/ata-B-part1\nrpool\n\tmirror-0\n\t/dev/sda3\n",
        );
        assert_eq!(m["fast"].len(), 2);
        assert_eq!(m["rpool"], vec!["/dev/sda3".to_string()]);
    }

    #[test]
    fn mount_paths_unescape() {
        assert_eq!(unescape("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape("/plain"), "/plain");
    }
}
