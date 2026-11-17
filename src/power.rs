//! Power: the CPU package's draw from RAPL, and the battery.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

// ────────────────────────────── RAPL ──────────────────────────────

/// One package's energy counter.
struct Zone {
    energy: PathBuf,
    /// The counter wraps to zero after this many microjoules.
    range_uj: u64,
    last: Option<(u64, Instant)>,
}

/// CPU package power from the powercap RAPL counters, which Intel and AMD
/// (Zen, through the same `intel-rapl` driver) both publish.
///
/// Two things btop does not do: every package is summed, so a two-socket
/// host reads its whole draw, and a counter that has wrapped since the last
/// reading is unwrapped with `max_energy_range_uj` rather than producing one
/// absurd figure. The counters are root-only on kernels since 5.10
/// (CVE-2020-8694); without root there is simply no power row.
pub struct Rapl {
    zones: Vec<Zone>,
    /// The packages' long-term limits (PL1) added together, as the ceiling
    /// the bar is drawn against.
    pub limit_w: Option<f64>,
}

impl Rapl {
    pub fn discover() -> Option<Rapl> {
        let mut zones = Vec::new();
        let mut limit = 0.0;
        let mut dirs: Vec<PathBuf> = fs::read_dir("/sys/class/powercap").ok()?.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // Packages are intel-rapl:N; their sub-zones (core, uncore, dram)
            // are intel-rapl:N:M and are already inside the package figure.
            if !name.starts_with("intel-rapl:") || name.matches(':').count() != 1 {
                continue;
            }
            if !read_str(d.join("name")).unwrap_or_default().starts_with("package") {
                continue;
            }
            let energy = d.join("energy_uj");
            if read_u64(&energy).is_none() {
                continue;
            }
            let range_uj = read_u64(d.join("max_energy_range_uj")).unwrap_or(u64::MAX);
            if let Some(uw) = read_u64(d.join("constraint_0_power_limit_uw")) {
                limit += uw as f64 / 1e6;
            }
            zones.push(Zone { energy, range_uj, last: None });
        }
        (!zones.is_empty()).then_some(Rapl { zones, limit_w: (limit > 0.0).then_some(limit) })
    }

    /// Watts since the previous call; `None` on the first.
    pub fn sample(&mut self) -> Option<f64> {
        let now = Instant::now();
        let mut total = 0.0;
        let mut complete = true;
        for z in self.zones.iter_mut() {
            let Some(e) = read_u64(&z.energy) else {
                complete = false;
                continue;
            };
            match z.last {
                Some((prev, at)) => {
                    let dt = now.duration_since(at).as_secs_f64();
                    let used = unwrap_counter(prev, e, z.range_uj);
                    if dt > 0.0 {
                        total += used as f64 / 1e6 / dt;
                    }
                }
                None => complete = false,
            }
            z.last = Some((e, now));
        }
        complete.then_some(total)
    }
}

/// Energy used between two readings of a counter that wraps at `range`.
fn unwrap_counter(prev: u64, cur: u64, range: u64) -> u64 {
    if cur >= prev {
        cur - prev
    } else {
        range.saturating_sub(prev).saturating_add(cur)
    }
}

// ───────────────────────────── battery ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Battery {
    pub pct: f64,
    pub status: String,
    pub watts: Option<f64>,
    /// To empty when discharging, to full when charging.
    pub secs: Option<f64>,
}

/// The system battery, if there is one. Peripherals — a wireless mouse
/// reports itself as a battery too — have `scope` Device and are skipped.
pub fn battery() -> Option<Battery> {
    let mut dirs: Vec<PathBuf> =
        fs::read_dir("/sys/class/power_supply").ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    let d = dirs.into_iter().find(|d| {
        read_str(d.join("type")).as_deref() == Some("Battery")
            && read_str(d.join("scope")).as_deref() != Some("Device")
            && d.join("capacity").exists()
    })?;
    let pct = read_u64(d.join("capacity"))? as f64;
    let status = read_str(d.join("status")).unwrap_or_else(|| "Unknown".into());

    // Drivers report either energy (µWh, µW) or charge (µAh, µA, µV).
    let power_uw = read_u64(d.join("power_now")).or_else(|| {
        let i = read_u64(d.join("current_now"))?;
        let v = read_u64(d.join("voltage_now"))?;
        Some((i as u128 * v as u128 / 1_000_000) as u64)
    });
    let watts = power_uw.filter(|w| *w > 0).map(|w| w as f64 / 1e6);
    let (now_wh, full_wh) = match (read_u64(d.join("energy_now")), read_u64(d.join("energy_full"))) {
        (Some(n), Some(f)) => (Some(n as f64 / 1e6), Some(f as f64 / 1e6)),
        _ => {
            let v = read_u64(d.join("voltage_now")).map(|v| v as f64 / 1e6);
            let n = read_u64(d.join("charge_now")).zip(v).map(|(c, v)| c as f64 / 1e6 * v);
            let f = read_u64(d.join("charge_full")).zip(v).map(|(c, v)| c as f64 / 1e6 * v);
            (n, f)
        }
    };
    let secs = match (status.as_str(), watts, now_wh, full_wh) {
        ("Discharging", Some(w), Some(n), _) => Some(n / w * 3600.0),
        ("Charging", Some(w), Some(n), Some(f)) => Some((f - n).max(0.0) / w * 3600.0),
        _ => None,
    };
    Some(Battery { pct, status, watts, secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_wraps() {
        assert_eq!(unwrap_counter(100, 250, 1000), 150);
        // Wrapped: 900 → 1000 (100) then 0 → 50 (50).
        assert_eq!(unwrap_counter(900, 50, 1000), 150);
    }
}
