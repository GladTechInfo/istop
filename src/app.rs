//! Machine state: joins the samplers, works out rates, keeps history and the
//! damped copies the bars are drawn from.

use crate::power::{self, Battery, Rapl};
use crate::procs::{Detail, Procs};
use crate::sys::{self, CpuShare, DiskIo, Mem, Mount, Stat, TempSensor};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// History columns, one per second of sampled time: ten minutes, whatever the
/// sampling rate.
pub const HISTORY: usize = 600;

/// Asymmetric damping, in the manner of a shock absorber: quick to compress,
/// slow to rebound. A rising edge reaches the bar almost at once so a spike is
/// never missed, whilst the fall is damped so the bar settles rather than
/// flickering. Symmetric smoothing would blunt the spikes too, and the spikes
/// are the one thing worth seeing.
///
/// These are **time constants in seconds**, not fixed per-sample weights, so
/// the feel is the same at any refresh rate. Converted per frame as
/// `alpha = 1 - exp(-dt / tau)`.
const ATTACK_RATIO: f64 = 3.0;
pub const DEFAULT_DAMPING_S: f64 = 4.0;

/// Percentage points below which a bar counts as having arrived.
const SETTLED: f64 = 0.1;

/// Seconds-to-90% converted to the time constant the filter actually uses.
pub fn damping_tau(seconds: f64) -> f64 {
    (seconds.clamp(0.2, 30.0)) / std::f64::consts::LN_10
}

/// A figure with a damped display copy. The raw value is never altered, so
/// `--once`, the history and every printed figure stay honest.
#[derive(Clone, Copy, Default, Debug)]
pub struct Damped {
    pub raw: f64,
    pub shown: f64,
}

impl Damped {
    pub fn get(&self, smooth: bool) -> f64 {
        if smooth {
            self.shown
        } else {
            self.raw
        }
    }

    /// One frame of the filter. Returns whether it is still visibly travelling;
    /// within `SETTLED` the gap is closed outright, so a settled bar reads
    /// exactly the sampled figure.
    fn glide(&mut self, dt: f64, attack: f64, decay: f64) -> bool {
        let tau = if self.raw > self.shown { attack } else { decay };
        self.shown += (self.raw - self.shown) * (1.0 - (-dt / tau).exp());
        if (self.raw - self.shown).abs() < SETTLED {
            self.shown = self.raw;
            false
        } else {
            true
        }
    }
}

/// One column per second of sampled time: each column is the time-weighted
/// mean of the samples in that second.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub ring: VecDeque<f64>,
    acc: f64,
    acc_t: f64,
}

impl History {
    pub fn add(&mut self, value: f64, dt: f64) {
        self.acc += value * dt;
        self.acc_t += dt;
        while self.acc_t >= 1.0 {
            let mean = self.acc / self.acc_t;
            if self.ring.len() == HISTORY {
                self.ring.pop_front();
            }
            self.ring.push_back(mean);
            // The remainder carries into the next column with the same mean,
            // which is exact for a value held across the boundary.
            self.acc_t -= 1.0;
            self.acc = mean * self.acc_t;
        }
    }

    pub fn last(&self, n: usize) -> impl Iterator<Item = f64> + '_ {
        self.ring.iter().skip(self.ring.len().saturating_sub(n)).copied()
    }
}

pub struct Disk {
    pub mount: Mount,
    /// Busy %, one column per second, for the row's histogram.
    pub busy_hist: History,
    pub read_bps: Option<f64>,
    pub write_bps: Option<f64>,
    /// Share of the interval the device had I/O in flight.
    pub busy_pct: Option<f64>,
}

pub struct Net {
    pub name: String,
    pub physical: bool,
    pub speed_mbit: Option<u64>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub rx_hist: History,
    pub tx_hist: History,
}

pub struct App {
    pub host: String,
    pub cpu_model: String,
    pub threads: usize,

    prev_stat: Option<Stat>,
    pub share: CpuShare,
    /// TOTAL, USER, SYSTEM, IOWAIT, STEAL.
    pub cpu_bars: [Damped; 5],
    pub cores: Vec<Damped>,
    pub cpu_hist: History,
    pub ctxt_per_s: f64,
    pub intr_per_s: f64,
    pub running: u64,
    pub blocked: u64,
    pub load: [f64; 3],
    pub kthreads: u64,
    pub uptime_s: f64,

    temp_sensor: Option<TempSensor>,
    pub temp_c: Option<f64>,
    pub temp_crit_c: f64,
    pub temp_bar: Damped,
    pub freq_mhz: Option<f64>,
    pub freq_max_mhz: Option<f64>,
    pub freq_bar: Damped,

    pub mem: Mem,
    /// USED, CACHE, ARC, SWAP.
    pub mem_bars: [Damped; 4],

    pub disks: Vec<Disk>,
    prev_io: HashMap<String, DiskIo>,

    rapl: Option<Rapl>,
    pub cpu_watts: Option<f64>,
    pub cpu_limit_w: Option<f64>,
    pub watts_bar: Damped,
    pub battery: Option<Battery>,
    /// The container istop itself runs in.
    pub own_container: Option<String>,

    zfs: Option<sys::Zfs>,

    /// The process whose detail box is open.
    pub detail: Option<Detail>,

    pub nets: Vec<Net>,

    pub procs: Procs,

    last: Option<Instant>,
    decay_tau: f64,
}

impl App {
    pub fn new() -> Self {
        let model = sys::cpu_model();
        let st = sys::read_stat();
        let threads = st.as_ref().map(|s| s.cores.iter().flatten().count()).unwrap_or(1).max(1);
        let ncores = st.as_ref().map(|s| s.cores.len()).unwrap_or(0);
        let sensor = sys::find_temp_sensor();
        App {
            host: sys::hostname(),
            cpu_model: model,
            threads,
            prev_stat: None,
            share: CpuShare::default(),
            cpu_bars: [Damped::default(); 5],
            cores: vec![Damped::default(); ncores],
            cpu_hist: History::default(),
            ctxt_per_s: 0.0,
            intr_per_s: 0.0,
            running: 0,
            blocked: 0,
            load: [0.0; 3],
            kthreads: 0,
            uptime_s: 0.0,
            temp_crit_c: sensor.as_ref().map(|s| s.crit_c).unwrap_or(100.0),
            temp_sensor: sensor,
            temp_c: None,
            temp_bar: Damped::default(),
            freq_mhz: None,
            freq_max_mhz: None,
            freq_bar: Damped::default(),
            mem: Mem::default(),
            mem_bars: [Damped::default(); 4],
            disks: Vec::new(),
            prev_io: HashMap::new(),
            cpu_limit_w: None,
            rapl: Rapl::discover(),
            cpu_watts: None,
            watts_bar: Damped::default(),
            battery: power::battery(),
            own_container: sys::own_container(),
            detail: None,
            zfs: sys::Zfs::new(),
            nets: Vec::new(),
            procs: Procs::new(),
            last: None,
            decay_tau: damping_tau(DEFAULT_DAMPING_S),
        }
    }

    pub fn set_damping(&mut self, seconds: f64) {
        self.decay_tau = damping_tau(seconds);
    }

    pub fn temp_label(&self) -> Option<&str> {
        self.temp_sensor.as_ref().map(|s| s.label.as_str())
    }

    /// Every figure is a rate over the time since the last sample. After a
    /// pause the first one would average the whole pause, so start afresh.
    pub fn reset_rates(&mut self) {
        self.prev_stat = None;
        self.prev_io.clear();
        self.last = None;
        self.procs.reset_rates();
        for n in self.nets.iter_mut() {
            n.rx_total = 0;
            n.tx_total = 0;
        }
    }

    /// Takes one sample of everything except processes, which are scanned
    /// separately and more slowly: see `scan_procs`.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64()).filter(|d| *d > 0.0);
        self.last = Some(now);

        // CPU.
        if let Some(st) = sys::read_stat() {
            if let (Some(prev), Some(dt)) = (&self.prev_stat, dt) {
                let s = sys::share(&prev.total, &st.total);
                self.share = s;
                for (b, v) in self.cpu_bars.iter_mut().zip([s.busy, s.user, s.system, s.iowait, s.steal]) {
                    b.raw = v;
                }
                if self.cores.len() < st.cores.len() {
                    self.cores.resize(st.cores.len(), Damped::default());
                }
                for (i, cur) in st.cores.iter().enumerate() {
                    let v = match (prev.cores.get(i).copied().flatten(), cur) {
                        (Some(p), Some(c)) => sys::share(&p, c).busy,
                        _ => 0.0,
                    };
                    self.cores[i].raw = v;
                }
                self.ctxt_per_s = st.ctxt.saturating_sub(prev.ctxt) as f64 / dt;
                self.intr_per_s = st.intr.saturating_sub(prev.intr) as f64 / dt;
                self.cpu_hist.add(s.busy, dt);
            }
            self.running = st.running;
            self.blocked = st.blocked;
            self.threads = st.cores.iter().flatten().count().max(1);
            self.prev_stat = Some(st);
        }
        let (load, kthreads) = sys::loadavg();
        self.load = load;
        self.kthreads = kthreads;
        self.uptime_s = sys::uptime_s();

        self.temp_c = self.temp_sensor.as_ref().and_then(sys::read_temp);
        if let Some(t) = self.temp_c {
            self.temp_bar.raw = (t / self.temp_crit_c * 100.0).clamp(0.0, 100.0);
        }
        if let Some(r) = self.rapl.as_mut() {
            self.cpu_watts = r.sample();
            self.cpu_limit_w = r.limit_w;
            if let (Some(w), Some(l)) = (self.cpu_watts, r.limit_w) {
                self.watts_bar.raw = (w / l * 100.0).clamp(0.0, 100.0);
            }
        }
        self.battery = power::battery();

        let (f, fmax) = sys::cpu_freq(self.cores.len());
        self.freq_mhz = f;
        self.freq_max_mhz = fmax;
        if let (Some(f), Some(m)) = (f, fmax) {
            self.freq_bar.raw = (f / m * 100.0).clamp(0.0, 100.0);
        }

        // Memory.
        self.mem = sys::read_mem();
        let m = &self.mem;
        let pct = |a: u64, b: u64| if b > 0 { (a as f64 / b as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 };
        self.mem_bars[0].raw = pct(m.used(), m.total);
        self.mem_bars[1].raw = pct(m.cache, m.total);
        self.mem_bars[2].raw = m.arc.map(|(s, max)| pct(s, max)).unwrap_or(0.0);
        self.mem_bars[3].raw = pct(m.swap_used(), m.swap_total);

        // Disks: the list is re-read every sample, since mounts come and go;
        // each mount keeps its damped bar across re-reads.
        let io = sys::read_diskstats();
        let mut disks = Vec::new();
        let mut old = std::mem::take(&mut self.disks);
        let mut mounts = sys::read_mounts();
        if let Some(z) = self.zfs.as_mut() {
            mounts.extend(z.mounts());
        }
        for mount in mounts {
            let mut busy_hist = old
                .iter_mut()
                .find(|d| d.mount.path == mount.path)
                .map(|d| std::mem::take(&mut d.busy_hist))
                .unwrap_or_default();
            // Summed over the devices underneath; busy is the busiest of
            // them, since one saturated disk holds up the whole pool.
            let (mut r, mut w, mut busy): (Option<f64>, Option<f64>, Option<f64>) = (None, None, None);
            for dev in &mount.devices {
                if let (Some(cur), Some(prev), Some(dt)) = (io.get(dev), self.prev_io.get(dev), dt) {
                    *r.get_or_insert(0.0) += cur.read_b.saturating_sub(prev.read_b) as f64 / dt;
                    *w.get_or_insert(0.0) += cur.write_b.saturating_sub(prev.write_b) as f64 / dt;
                    let b = (cur.busy_ms.saturating_sub(prev.busy_ms) as f64 / 10.0 / dt).clamp(0.0, 100.0);
                    busy = Some(busy.map_or(b, |x: f64| x.max(b)));
                }
            }
            if let (Some(b), Some(dt)) = (busy, dt) {
                busy_hist.add(b, dt);
            }
            disks.push(Disk { mount, busy_hist, read_bps: r, write_bps: w, busy_pct: busy });
        }
        self.disks = disks;
        self.prev_io = io;

        // Network.
        let mut seen = Vec::new();
        for (name, rx, tx) in sys::read_netdev() {
            if name == "lo" {
                continue;
            }
            seen.push(name.clone());
            let n = match self.nets.iter_mut().position(|n| n.name == name) {
                Some(i) => &mut self.nets[i],
                None => {
                    self.nets.push(Net {
                        physical: sys::is_physical(&name),
                        speed_mbit: None,
                        name,
                        rx_bps: 0.0,
                        tx_bps: 0.0,
                        rx_total: 0,
                        tx_total: 0,
                        rx_hist: History::default(),
                        tx_hist: History::default(),
                    });
                    self.nets.last_mut().unwrap()
                }
            };
            if let Some(dt) = dt.filter(|_| n.rx_total > 0 || n.tx_total > 0) {
                // A counter that went backwards was reset (driver reload);
                // that interval is read as zero rather than as a huge spike.
                n.rx_bps = rx.saturating_sub(n.rx_total) as f64 / dt;
                n.tx_bps = tx.saturating_sub(n.tx_total) as f64 / dt;
                n.rx_hist.add(n.rx_bps, dt);
                n.tx_hist.add(n.tx_bps, dt);
            }
            n.rx_total = rx;
            n.tx_total = tx;
            n.speed_mbit = sys::link_speed(&n.name);
        }
        self.nets.retain(|n| seen.contains(&n.name));
    }

    pub fn scan_procs(&mut self) {
        self.procs.scan(self.mem.total);
        if let Some(d) = self.detail.as_mut() {
            d.update(&self.procs.list);
        }
    }

    pub fn open_detail(&mut self, pid: i32) {
        if let Some(p) = self.procs.list.iter().find(|p| p.pid == pid) {
            self.detail = Some(Detail::new(p, &self.procs.list));
        }
    }

    /// Advances the damped values toward the latest sample. Called every
    /// frame; returns whether any bar is still moving.
    pub fn animate(&mut self, dt: f64) -> bool {
        let decay = self.decay_tau;
        let attack = decay / ATTACK_RATIO;
        let mut moving = false;
        for d in self.all_damped() {
            moving |= d.glide(dt, attack, decay);
        }
        moving
    }

    /// Puts every damped value on its target at once.
    pub fn snap(&mut self) {
        for d in self.all_damped() {
            d.shown = d.raw;
        }
    }

    /// What the animated bars would show, reduced to eighth-cells. Two frames
    /// with the same signature differ on screen only in what the sample
    /// changed, and between samples only the damped bars move — so a gliding
    /// frame that leaves this unchanged is not drawn. Bar lengths are
    /// estimated from the terminal width, generously, so the estimate errs
    /// towards drawing.
    pub fn motion_signature(&self, width: u16) -> Vec<i64> {
        let w = width as f64;
        // Beside the core grid, the main bars get about half the width; on a
        // narrow terminal, all of it. Core bars are a fraction of the grid.
        let main = if width >= 110 { w * 0.55 - 25.0 } else { w - 25.0 }.max(8.0);
        let core = (w * 0.45 / 2.0).max(4.0);
        let step = |d: &Damped, cells: f64| (d.shown / 100.0 * cells * 8.0).round() as i64;
        self.cpu_bars
            .iter()
            .chain([&self.temp_bar, &self.freq_bar, &self.watts_bar])
            .chain(self.mem_bars.iter())
            .map(|d| step(d, main))
            .chain(self.cores.iter().map(|d| step(d, core)))
            .collect()
    }

    fn all_damped(&mut self) -> impl Iterator<Item = &mut Damped> {
        self.cpu_bars
            .iter_mut()
            .chain(self.cores.iter_mut())
            .chain(std::iter::once(&mut self.temp_bar))
            .chain(std::iter::once(&mut self.freq_bar))
            .chain(std::iter::once(&mut self.watts_bar))
            .chain(self.mem_bars.iter_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_one_column_per_second() {
        let mut h = History::default();
        for _ in 0..10 {
            h.add(50.0, 0.25);
        }
        // 2.5 s sampled: two whole columns, half a second carried.
        assert_eq!(h.ring.len(), 2);
        h.add(50.0, 0.5);
        assert_eq!(h.ring.len(), 3);
        assert!(h.ring.iter().all(|v| (*v - 50.0).abs() < 1e-9));
    }

    #[test]
    fn damping_is_quick_up_and_slow_down() {
        let tau = damping_tau(DEFAULT_DAMPING_S);
        let mut d = Damped { raw: 100.0, shown: 0.0 };
        d.glide(0.5, tau / ATTACK_RATIO, tau);
        let up = d.shown;
        let mut e = Damped { raw: 0.0, shown: 100.0 };
        e.glide(0.5, tau / ATTACK_RATIO, tau);
        assert!(up > 100.0 - e.shown);
    }
}
