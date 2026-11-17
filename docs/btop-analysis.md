# btop: how it is written, everything it offers, and what istop takes from it

Studied: **btop v1.4.8**, `github.com/aristocratos/btop`, commit `5d03929` (2026-11-11), read from
source rather than from the README alone. Line counts are from that commit. Where the documentation
and the code disagree, the code is quoted.

Contents

1. [Architecture](#1-architecture)
2. [Data collection on Linux](#2-data-collection-on-linux)
3. [Drawing](#3-drawing)
4. [Every configuration option](#4-every-configuration-option)
5. [Command-line options](#5-command-line-options)
6. [Every key and mouse binding](#6-every-key-and-mouse-binding)
7. [Menus, themes and presets](#7-menus-themes-and-presets)
8. [Weaknesses found in the code](#8-weaknesses-found-in-the-code)
9. [Recent history](#9-recent-history)
10. [Decisions for istop](#10-decisions-for-istop)

---

## 1. Architecture

**Language and build.** C++23 (`fmt` for formatting), built with Make or CMake. About 14,500
lines in `src/` for Linux, plus one directory per platform: `linux/`, `freebsd/`, `netbsd/`,
`openbsd/`, `osx/`. No TUI library: btop writes ANSI escape sequences itself (`btop_tools`, namespace
`Term`/`Mv`/`Fx`), including the alternate screen, mouse reporting and synchronised output.

| File | Lines | Role |
|---|---:|---|
| `btop.cpp` | 1,189 | `main`, signal handlers, the **Runner** thread, the main loop |
| `btop_config.cpp` | 901 | option table with descriptions, defaults, load/save, presets |
| `btop_draw.cpp` | 2,551 | box outlines, `Meter` and `Graph` classes, per-box drawing, `calcSizes` |
| `btop_input.cpp` | 649 | key and mouse decoding, dispatch per box |
| `btop_menu.cpp` | 1,963 | main, options, help, signal, renice and size-error menus |
| `btop_theme.cpp` | 473 | theme files, 101-step colour gradients, 256-colour fallback |
| `btop_shared.cpp` | 360 | process sorting, tree sort, filter, container detection, renice |
| `btop_tools.cpp` | 756 | string, time, terminal helpers |
| `btop_cli.cpp` | 272 | argument parsing |
| `linux/btop_collect.cpp` | 3,656 | every Linux collector, GPU back-ends included |

**Threads.** Two. The **main thread** reads input and handles signals; a **Runner thread** does all
collection *and* drawing. The main thread wakes the runner through a `std::binary_semaphore`
(`thread_wait()`/`thread_trigger()`), passing which boxes to refresh (`"all"`, `"proc"`, …) and
whether to collect anew (`no_update`) or only redraw. An atomic `active` flag guards anything the
main thread must not touch mid-cycle; v1.4.7 and v1.4.8 fixed several data races and deadlocks
there (#1649, #1662). The runner blocks `SIGWINCH` etc. so handlers cannot deadlock it.

**Signals.** `SIGWINCH` resizes (deferred since #1674), `SIGTSTP`/`SIGCONT` implement Ctrl+Z,
`SIGUSR1` forces a redraw, `SIGUSR2` reloads the config (bound to Ctrl+R).

**Privilege.** `make setcap`/`make setuid` give the binary rights to read RAPL power and Intel GPU
PMU counters; the runner raises its effective UID only around those reads.

**Files.** Config `$XDG_CONFIG_HOME/btop/btop.conf` (else `~/.config/btop/`), written back on exit
by default; log `~/.local/state/btop.log`; themes in `../share/btop/themes` beside the binary or
`~/.config/btop/themes`.

## 2. Data collection on Linux

All from `/proc` and `/sys`, no external libraries except for GPUs.

| Area | Source | Notes |
|---|---|---|
| CPU % | `/proc/stat` | total and per core; `cpu_graph_upper/lower` can show user, system, iowait, irq, softirq, steal, guest… separately |
| CPU name | `/proc/cpuinfo` | trimmed of vendor boilerplate (`get_cpuName`) |
| Frequency | `scaling_cur_freq` | `freq_mode`: first core, range, lowest, highest, average |
| Temperature | `/sys/class/hwmon`, then `/sys/class/thermal` | any sensor selectable; per-core temperatures with a configurable `cpu_core_map` |
| CPU watts | `/sys/class/powercap/intel-rapl:0/energy_uj` | needs root on current kernels |
| Battery | `/sys/class/power_supply/*` | capacity, status, watts, time left |
| Effective CPUs | `/sys/fs/cgroup/cpuset.cpus.effective` | respects a container's CPU set (#1220) |
| Container | `/run/.containerenv`, `/.dockerenv`, `/run/systemd/container` | shows *btop's own* container engine (#1218) |
| Memory | `/proc/meminfo`, `/proc/spl/kstat/zfs/arcstats` | ARC optionally counted as cache (`zfs_arc_cached`) |
| Disks | `/proc/self/mounts` (or `/etc/fstab`), `/proc/filesystems`, `statvfs`, `/sys/block/*/stat` | ZFS pool IO from `/proc/spl/kstat/zfs/<pool>/objset-*` |
| Network | `getifaddrs()`, `/sys/class/net/*/statistics` | IPv4/IPv6 address shown; counter rollover handled |
| Processes | `/proc/[pid]/{stat,statm,cmdline,status}`, `/etc/passwd` | user map re-read when `/etc/passwd` changes |
| Process detail | `/proc/[pid]/{smaps,io}` | smaps optional: "very slow but more accurate" |
| GPUs | NVML and ROCm SMI by `dlopen`; Intel via vendored `intel_gpu_top` C code (i915 PMU); AMD APU sysfs (1.4.8); Apple IOReport | |

**Per-process CPU.** `cpu_p = (Δ utime+stime of the process) / (Δ total ticks in /proc/stat)`, so by
default a percentage **of the whole machine**, 0–100. `proc_per_core` multiplies by the core count
(top's convention). Measuring against `/proc/stat` ticks rather than wall time makes the figure
immune to the sampling loop's own jitter.

**"cpu lazy" sorting** (the default) sorts by `cpu_c` — *average CPU since the process started* —
then promotes up to ten processes whose current CPU exceeds a threshold (30 %, or the busiest of
the first six) to the front. A long-lived busy process stays put; a sudden burst still surfaces.
"cpu direct" sorts on the instantaneous figure and reshuffles every update.

**Tree view.** Built every update from `ppid`; children sorted by the current sort key; a stable
sort keeps siblings ordered; ASCII prefixes (`├─`, `└─`, `│`) generated recursively; collapsed
nodes can aggregate their children's CPU and memory (`proc_aggregate`); processes with many
children can start collapsed (`proc_tree_auto_collapse`, e.g. browsers).

**Network auto-scale.** The ceiling is recomputed once the speed has been above it, or far below it,
for five consecutive samples: 1.3× the mean of the last five readings when rising, 3× when
falling, never below 10 KiB/s. `net_sync` gives upload and download the same ceiling.

## 3. Drawing

* **Graphs** use **braille** by default: each character holds two samples side by side and four
  dot rows, so a 25-entry lookup table maps (previous, current) value pairs to a glyph. "block"
  uses quadrant blocks with the same two samples per cell at half the vertical resolution; "tty"
  uses three shades of `░▒█`. Graphs can be inverted (lower CPU graph, upload graph).
* **Meters** are rows of `■`, each coloured from a **101-step gradient** built from a theme's
  start/mid/end colours — the same idea as igtop's per-cell ramp.
* **Process list**: optional per-process mini CPU graph, colour by CPU, and a darkening gradient
  down the list (`proc_gradient`).
* **Layout** (`calcSizes`): CPU on top (or bottom), then memory and network stacked on one side and
  processes on the other (`proc_left`, `mem_below_net`). Any box can be hidden; a "terminal too
  small" message appears when the minimum cannot be met.
* **Output** is assembled as one string per cycle and written at once, wrapped in synchronised
  output sequences (`terminal_sync`) to avoid tearing.

## 4. Every configuration option

`btop.conf`, all options, with defaults from `btop_config.cpp`.

### Appearance and layout

| Option | Default | Meaning |
|---|---|---|
| `color_theme` | `Default` | theme file name; `TTY` built in |
| `theme_background` | `True` | paint the theme background; False for terminal transparency |
| `truecolor` | `True` | 24-bit colour; else 256-colour cube |
| `force_tty` | `False` | 16 colours, tty graph symbols |
| `disable_presets` | `Off` | Off / Default / Custom / All |
| `presets` | three presets | `box:position:graph_symbol` lists, up to 9 |
| `vim_keys` | `False` | h j k l g G move; help and kill move to shift |
| `disable_mouse` | `False` | ignore all mouse events |
| `rounded_corners` | `True` | |
| `terminal_sync` | `True` | synchronised output |
| `graph_symbol` | `braille` | braille, block, tty |
| `graph_symbol_cpu/gpu/mem/net/proc` | `default` | per-box override |
| `shown_boxes` | `cpu mem net proc` | plus `gpu0`…`gpu5` |
| `update_ms` | `2000` | |
| `clock_format` | `%X` | strftime, plus `/host`, `/user`, `/uptime` |
| `background_update` | `True` | keep updating behind menus |
| `save_config_on_exit` | `True` | |
| `log_level` | `WARNING` | |

### Processes

| Option | Default | Meaning |
|---|---|---|
| `proc_sorting` | `cpu lazy` | pid, program, arguments, threads, user, memory, cpu lazy, cpu direct |
| `proc_reversed` | `False` | |
| `proc_tree` | `False` | |
| `proc_colors` | `True` | colour figures by value |
| `proc_gradient` | `True` | darken towards the bottom of the list |
| `proc_per_core` | `False` | CPU % of one core instead of the machine |
| `proc_mem_bytes` | `True` | bytes instead of percent |
| `proc_cpu_graphs` | `True` | mini graph per process |
| `proc_info_smaps` | `False` | exact RSS in the detail box from smaps |
| `proc_left` | `False` | process box on the left |
| `proc_filter_kernel` | `False` | hide kernel threads |
| `proc_follow_detailed` | `True` | list follows the process whose detail is open |
| `proc_aggregate` | `False` | collapsed tree nodes sum their children |
| `proc_tree_auto_collapse` | `0` | collapse nodes with this many children |
| `keep_dead_proc_usage` | `False` | keep figures of processes that died while paused |

### CPU

| Option | Default | Meaning |
|---|---|---|
| `cpu_graph_upper` / `cpu_graph_lower` | `Auto` | which stat each half of the CPU graph shows |
| `cpu_invert_lower` | `True` | |
| `cpu_single_graph` | `False` | no lower graph |
| `cpu_bottom` | `False` | CPU box at the bottom |
| `show_uptime` | `True` | |
| `show_cpu_watts` | `True` | RAPL package power |
| `check_temp` | `True` | |
| `cpu_sensor` | `Auto` | choose any hwmon sensor |
| `show_coretemp` | `True` | per-core temperatures |
| `cpu_core_map` | `""` | `x:y` remapping of core temperatures |
| `temp_scale` | `celsius` | fahrenheit, kelvin, rankine |
| `show_cpu_freq` | `True` | |
| `freq_mode` | `first` | first, range, lowest, highest, average |
| `custom_cpu_name` | `""` | |
| `show_gpu_info` | `Auto` | GPU summary inside the CPU box |

### Memory and disks

| Option | Default | Meaning |
|---|---|---|
| `mem_graphs` | `True` | graphs rather than meters |
| `mem_below_net` | `False` | |
| `zfs_arc_cached` | `True` | count ARC as cache and available |
| `show_swap` | `True` | |
| `swap_disk` | `True` | swap as a disk row |
| `show_disks` | `True` | |
| `only_physical` | `True` | drop network and RAM filesystems |
| `use_fstab` | `True` | disk list from `/etc/fstab` |
| `zfs_hide_datasets` | `False` | pools only |
| `disk_free_priv` | `False` | free space including root's reserve |
| `disks_filter` | `""` | mount points, or `exclude=` list |
| `show_io_stat` | `True` | IO busy % in the disk list |
| `io_mode` | `False` | large read/write graphs per disk |
| `io_graph_combined` | `False` | |
| `io_graph_speeds` | `""` | per-mount graph ceilings, MiB/s |
| `base_10_sizes` | `False` | KB rather than KiB |

### Network, battery, GPU

| Option | Default | Meaning |
|---|---|---|
| `net_auto` | `True` | auto-scale (see §2) |
| `net_sync` | `True` | one scale for both directions |
| `net_download` / `net_upload` | `100` | fixed scales, Mebibits, when not auto |
| `net_iface` | `""` | interface to start on |
| `swap_upload_download` | `False` | upload graph on top |
| `base_10_bitrate` | `Auto` | |
| `show_battery` | `True` | |
| `selected_battery` | `Auto` | |
| `show_battery_watts` | `True` | |
| `nvml_measure_pcie_speeds`, `rsmi_measure_pcie_speeds` | `True` | |
| `gpu_mirror_graph` | `True` | |
| `shown_gpus` | `nvidia amd intel apple` | |
| `custom_gpu_name0…5` | `""` | |

Internal state also saved in the file: `proc_filter`, `selected_pid`, `detailed_pid`,
`pause_proc_list`, `follow_process`, and similar.

## 5. Command-line options

| Option | Meaning |
|---|---|
| `-c, --config <file>` | config file |
| `-d, --debug` | debug logging and timing |
| `-f, --filter <filter>` | initial process filter |
| `--force-utf` | ignore locale detection |
| `-l, --low-color` | 256 colours |
| `-p, --preset <0-9>` | start with a preset |
| `-t, --tty` / `--no-tty` | force or forbid tty mode |
| `--themes-dir <dir>` | |
| `-u, --update <ms>` | |
| `--default-config` | print the default config |
| `-h, --help`, `-V, --version` | |

btop has **no non-interactive output**: nothing like `--once` for scripts.

## 6. Every key and mouse binding

Global: `q`/Ctrl+C quit · `Esc`/`m` main menu · `F1`/`?`/`h` help · `F2`/`o` options ·
`1`–`4` toggle CPU, memory, network, process boxes (`5`, `0`, `6`… GPUs) · `p`/`P` next/previous
preset · Ctrl+Z suspend · Ctrl+R reload config · `+`/`-` ±100 ms (±1000 when held).

Processes: `↑ ↓ PgUp PgDn Home End` select · `←`/`→` sort column · `r` reverse · `e` tree ·
`E` collapse/expand all · `Space`/`+`/`-` expand/collapse selected · `C` toggle children ·
`f`/`/` filter (leading `!` = regex) · `Delete` clear filter · `Enter` detail box · `F` follow ·
`u` pause list · `c` per-core CPU · `%` memory as bytes/percent · `t` SIGTERM · `k` SIGKILL ·
`s` choose any signal · `N` renice.

Memory: `i` IO mode · `d` show/hide disks. Network: `b`/`n` previous/next interface · `z` reset
totals · `a` auto-scale · `y` sync scales.

Mouse: click buttons (every highlighted key in a title is clickable), select processes, scroll
lists and menus, drag the scrollbar.

## 7. Menus, themes and presets

* **Main menu** (Options, Help, Quit) with an animated banner; **Options menu** edits every option
  above live, with descriptions; **signal menu** lists all 31 signals by number and name;
  **renice** menu takes a nice value.
* **Themes**: bpytop/bashtop format, ~40 colour keys (`main_bg`, `hi_fg`, `cpu_start/mid/end`,
  `used_*`, `free_*`, `download_*`, `upload_*`, `process_*`, …), 49 themes shipped.
* **Presets** store box sets, positions and graph symbols; `p` cycles them.

## 8. Weaknesses found in the code

These shaped what istop does differently.

1. **RAPL wrap-around is not handled.** `get_cpuConsumptionWatts` subtracts two `energy_uj`
   readings without consulting `max_energy_range_uj`; the counter wraps every few minutes under
   load on a big part, giving one negative or absurd reading. Only package 0 (`intel-rapl:0`) is
   read, so a two-socket machine shows half its power.
2. **Per-process CPU is clamped** to `100 × cores`, and measured against `/proc/stat` ticks, which
   inside a container with lxcfs reflect the container's view while process times do not.
3. **No scripting output** (`--once`), so it cannot feed a check or a log.
4. **Container awareness is about btop itself**, not the processes it lists: on a Proxmox host
   every LXC and VM process shows as a plain host process.
5. **Saving the config on exit by default** means a toggle pressed once persists silently.
6. **C++ data races** between the input and runner threads have needed repeated fixes (v1.4.7,
   v1.4.8): the two-thread design with shared mutable config is fragile.
7. **Intel GPU support** is vendored C from `intel_gpu_top` and shows only utilisation, power and
   clock — the gaps igtop was written to fill.

## 9. Recent history

v1.4.6: renice, frequency modes, cgroup-effective CPUs, container-engine display, tree child
toggle. v1.4.7: process following, Apple Silicon GPU, `E` collapse-all, upload/download swap.
v1.4.8: AMD APU sysfs GPU back-end, ISO 8601 log timestamps, data-race and deadlock fixes,
deferred resize handling.

## 10. Decisions for istop

Principles: keep igtop's look (palette, per-cell gradient bars, rounded boxes, eighth-block
resolution); stay a single-threaded loop (collection is a few milliseconds; a second thread would
reintroduce exactly the races btop keeps fixing); **pure Rust, no `unsafe`**; adopt what makes a
real difference on this estate (Proxmox hosts, a laptop, ZFS).

| btop feature | istop | Reason |
|---|---|---|
| Process tree, expand/collapse, collapse all | **adopted** (`e`, Space/`+`/`-`, `E`) | genuinely useful on hosts with many LXC/QEMU process families |
| Detail box (`Enter`) | **adopted**, extended | adds IO rates, container, cgroup, nice, start time, CPU history |
| Renice (`N`) | **adopted** | |
| Full signal menu | **adopted** (`k`, twelve signals) | no one-key KILL: btop's `t`/`k` also confirm first, so a kill is always two keys |
| Pause process list (`u`) | **adopted** | |
| Per-core vs machine CPU (`c`) | **adopted** | istop defaults to per-thread like top; `c` switches |
| Box toggles (`1`–`4`) | **adopted** | presets reduce to this in practice |
| Previous interface (`b`) | **adopted**; `a` returns to automatic | `a` means auto-scale in btop; istop's scale is always automatic |
| Mouse: select, scroll, click to open | **adopted** | |
| Help overlay (`?`, F1) | **adopted** | replaces the long hint line |
| Clock in header | **adopted** | local time from `/etc/localtime`, parsed in Rust |
| Battery | **adopted** | laptops need it |
| CPU package watts (RAPL) | **adopted, corrected** | handles wrap-around and every package |
| Disk IO busy % | **adopted** | from `/proc/diskstats` io_ticks |
| Cumulative CPU time column and sort | **adopted** | stands in for "cpu lazy" with a figure one can read |
| Ctrl+Z suspend | **adopted** | |
| Synchronised output | **adopted** | |
| Config file, `--default-config`, `-c` | **adopted**, saved only on request (`W`), atomically | see weakness 5 |
| vim keys | **adopted** (option) | |
| Own container shown in header | **adopted** | |
| Regex filter (`!`) | not adopted | needs a regex engine dependency; substring filter covers the use |
| Themes, presets, options menu | not adopted | the design is fixed to igtop's on purpose; the config file covers options |
| Braille graphs | not adopted | eighth blocks give 8 levels per cell and match igtop; braille's extra horizontal resolution matters little at 1 s columns |
| IO mode with big disk graphs | not adopted | busy % and rates answer the usual question |
| GPU boxes | not adopted | igtop is the GPU monitor, and is more accurate on i915 than btop's |
| Per-core temperatures and core map | not adopted (yet) | needs topology mapping; package temperature answers the common question |
| Process following (`F`) | covered | istop's selection follows its process once moved; `Home` returns to the top row |
| `proc_gradient`, per-process graphs | not adopted | the per-row CPU bar does that job without the clutter |

Things istop does that btop does not: `--once` text output; **WHERE** column attributing
processes to Proxmox CTs and VMs, Docker and Podman; ZFS ARC shown as its own bar against its
ceiling; damped bars with redraws skipped when nothing visibly moves; no `unsafe` code.

### Implementation notes (istop 0.1.0, 2026-11-15)

* **Pure Rust.** `libc` replaced by `rustix` (already in the tree through crossterm), which issues
  `statvfs`, `kill`, `setpriority` and reads `AT_CLKTCK`/`AT_PAGESZ` directly, without the C
  library. `#![forbid(unsafe_code)]` holds for the whole crate. Rust's standard library itself, and
  crossterm's terminal layer, still use the platform C library underneath; no Rust program on Linux
  avoids that without a custom target.
* **Clock** without a date crate: `src/clock.rs` parses `/etc/localtime` (TZif v1–v4) including the
  POSIX footer rule that "slim" zone files rely on.
* **Key conflicts** resolved as btop does: in tree view, Space and `+`/`-` fold and unfold rather
  than pause and change the rate. `t` toggles kernel threads (btop: SIGTERM), because in istop the
  signal menu is one key away.
* **Cost** on a 22-thread VM (~400 processes): 1.3 % of one thread undamped, 2.8 %
  with damped bars.
* **Verified** in a terminal: tree, fold all, filter in tree, detail box, help, signal and renice
  menus, box toggles, `c`, `u`, `W`, Ctrl+Z/`fg`, mouse click/second click/wheel.
* **Verified on two Proxmox VE 9.2 hosts** (2026-11-15; a 22-core Xeon and an 8-core desktop CPU,
  both on ZFS): coretemp package temperature, RAPL package power against PL1, clock against its
  maximum, ZFS ARC, VM and CT attribution, container users. Battery remains untested (no battery).
* **ZFS, corrected after the host test.** `statvfs` on a ZFS dataset reports that dataset alone, so
  a pool holding 60 TB showed 256 KiB used — and btop, which uses the same call, has the same
  fault. istop now shows one row per pool: space from `zfs list -Hp -d 0` (every dataset,
  snapshot and zvol), I/O and busy % from the member disks found with `zpool list -vHP`, so VM
  zvol traffic is included. Confirmed with a 2 GiB direct read from an `rpool` member: 518 MB/s,
  98 % busy, on the right row. The zfs tools are run every 10 s (space) and 60 s (members).
* **Container users.** Processes in unprivileged CTs run as shifted host UIDs (100034); istop
  maps them back through `/proc/<pid>/uid_map` and names them from the container's own
  `/etc/passwd`, so they read `backup`, `postfix`, `www-data`. Names the kernel cut at 15
  characters are completed from argv[0] (`proxmox-backup-proxy`).
