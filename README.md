# istop

A terminal system monitor — CPU, memory, disks, network and processes — in the style of
[igtop](https://github.com/GladTechInfo/igtop): pastel gradient bars coloured cell by cell, rounded
boxes, damped motion. Written entirely in Rust, with no `unsafe` code.

![istop](docs/screenshot.png)

How it compares with btop, and why each btop feature was or was not taken up:
[docs/btop-analysis.md](docs/btop-analysis.md).

## What it shows

* **CPU** — busy, user, system, iowait and steal; temperature, package power (RAPL, root) and clock
  where the hardware reports them; a bar per thread; ten minutes of history; load, context
  switches and interrupts.
* **Memory** — used, cache, swap, and the ZFS ARC against its ceiling on ZFS hosts.
* **Disks** — local filesystems: space, read and write rates, and how busy each device is. ZFS is
  shown per pool: space as `zfs list` counts it, I/O from the pool's member disks.
* **Network** — receive and send graphs for one interface on a shared scale; follows the busiest
  physical port unless you choose one.
* **Processes** — flat list or tree; sortable by PID, container, user, name, nice, threads, CPU
  time, CPU or memory; filter; detail box with CPU history, disk I/O, cgroup and command line;
  signals and renice. The **WHERE** column names the Proxmox CT or VM, Docker or Podman container
  each process runs in; users inside unprivileged containers are shown by their own names.
* **Header** — clock, uptime, battery, and the container istop itself runs in.

`istop --once` prints a plain-text snapshot for scripts.

## Keys

Press `?` in istop for the full list.

| Key | Action |
|---|---|
| `q` `Esc` | quit (Esc first clears a filter or closes the detail box) |
| `↑ ↓ PgUp PgDn Home End` | select a process |
| `← →`, `r` | sort column, reverse |
| `/` `f` | filter |
| `Enter` | detail box |
| `e`, `E` | tree view; fold or unfold every branch |
| `Space` `+` `-` | pause, refresh rate — in tree view: fold and unfold |
| `k`, `N` | signal menu, renice |
| `u`, `c`, `t` | pause the list; CPU % of a thread or the machine; kernel threads |
| `1`–`4` | show or hide the CPU, memory, network and process boxes |
| `d` | disks menu: choose which disks and pools are shown, and in what order |
| `n` `b`, `a` | next or previous interface; back to automatic |
| `s`, `i` | damped or raw bars; icons or ASCII |
| `W` | save the current settings |
| `Ctrl+Z` | suspend |

Mouse: click selects a process, a second click opens its detail box, the wheel scrolls.

## Configuration

`~/.config/istop/istop.conf` (or `-c FILE`); `istop --default-config` prints every option with
its description. Command-line options override the file. Settings are written back only when you
press `W`.

The disks menu (`d`): `↑ ↓` select, `Space` shows or hides, `Shift+↑ ↓` (or `+ -`) moves the
selected row, `r` returns to the default order with everything shown. The choice is kept as
`disk_order` and `disk_hidden` (mount points or ZFS pool names, comma-separated); a disk listed in
neither appears after the ordered ones, so a new mount is never silently lost. `--once` still
reports every disk.

## Building

```sh
cargo build --release           # for this machine
./build.sh                      # static musl binary for any x86-64 Linux, AVX-512 checked
```

Needs a Nerd Font for the glyphs (`--no-icons` otherwise). Run as root to see CPU power and every
process's disk I/O.
