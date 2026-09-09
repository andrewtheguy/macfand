# macfand

A fan control daemon for Intel MacBooks that steers on every sensor on the board, not just the CPU.

`mbpfan` and `macfanctld` read the CPU package temperature and nothing else. Everything else the SMC exposes — the PCH die, memory, the chassis skin — can sit at whatever temperature it likes without the fan ever noticing. macfand runs a PI controller per configured sensor and commands whichever one is asking for the most air.

## Install

Prebuilt binaries are published for Linux on amd64. There is no arm64 build, because there is no arm64 machine with an Apple SMC.

**Disable any other fan daemon first.** Two of them fighting over `fan1_output` will do exactly what you would expect, so this comes before macfand is started, not after:

```sh
sudo systemctl disable --now mbpfan      # or macfanctld, if that is what you run
```

Then install the binary:

```sh
curl -fsSL https://raw.githubusercontent.com/andrewtheguy/macfand/main/install.sh | sh
```

Or build from source with a recent Rust toolchain, and put it where the unit expects it:

```sh
cargo install --git https://github.com/andrewtheguy/macfand
sudo install -m 755 ~/.cargo/bin/macfand /usr/local/bin/macfand
```

Both of those install the binary and nothing else, so fetch the unit separately — it is what makes the daemon safe to run unattended:

```sh
sudo curl -fsSL -o /etc/systemd/system/macfand.service \
  https://raw.githubusercontent.com/andrewtheguy/macfand/main/systemd/macfand.service

# Optional. The defaults are built in, so the daemon runs correctly without it.
sudo curl -fsSL -o /etc/macfand.toml \
  https://raw.githubusercontent.com/andrewtheguy/macfand/main/macfand.toml.example

sudo systemctl daemon-reload
sudo systemctl enable --now macfand
```

From a clone of the repository, `sudo cp systemd/macfand.service /etc/systemd/system/` and `sudo cp macfand.toml.example /etc/macfand.toml` do the same thing.

## Use

```sh
macfand --show      # every sensor and fan this machine exposes, with readings
macfand --restore   # hand the fans back to the SMC
```

`--show` is how you find sensor labels to put in the config, and it does not need root:

```
applesmc (/sys/devices/platform/applesmc.768)
  TC0J              1.5 C  (implausible — likely not populated)
  TC0P             69.0 C  (steering on this)
  TPCD             88.0 C  (steering on this)
  Ts0S             52.8 C  (steering on this)
```

Sensors are named by label, never by the `tempN_input` file they happen to live in — the numbering is assigned in probe order and is not stable across kernels, so a config naming `temp18_input` would quietly start steering from the wrong sensor after an upgrade.

## How it steers

Each sensor gets a `target`, a `critical`, and PI gains. Every poll, each sensor's controller produces a demanded speed, and **the fan runs at the highest of them**. Taking the maximum rather than an average is the whole design: a daemon that averages sits at a comfortable speed while one sensor bakes.

The integral term is what `mbpfan` lacks. A proportional-only response settles as soon as its fixed contribution balances, which is how a sensor ends up parked several degrees above target indefinitely. The integral keeps pushing until the error is actually gone.

Three things bound it:

- **`integral_limit_rpm`** caps how much one sensor's integral may contribute. This exists for sensors airflow barely moves (see below) — without it, such a sensor winds all the way up and pins the fan at maximum forever in exchange for nothing.
- **Anti-windup**: the integral is clamped to the usable speed range, so a sensor that sits above target for an hour cannot bank a term that then takes an hour to unwind.
- **Slew limiting**: `ramp_up_rpm_per_s` is near-instant by design; `ramp_down_rpm_per_s` is slow, because a fan that chases every dip in load is audible in a way a steady one is not.

`critical` bypasses the controller entirely and goes straight to maximum, unaffected by any cap and unaffected by the upward slew limit — an emergency gets the air it is asking for on the poll it is detected, not several seconds later. A failed sensor is treated the same way.

At startup macfand takes the fan over at whatever speed the SMC is already running it, not at the floor, so taking control of a machine that is already hot does not begin by slowing its fan down.

## Failing safe

A daemon holding the SMC in manual mode is holding a loaded gun: the SMC keeps running whatever speed it was last told, forever, so a daemon that dies without clearing the flag strands the fan — stuck at maximum after a hot spell, or stuck at idle in the middle of a compile.

- A sensor that stops reading, or reads outside `plausible_range_c`, is ridden out on its last good value for `sensor_grace_polls` and then **demands maximum**, never minimum. `applesmc` reports sensors that are not populated on a given board as 0 or 1 degrees, which a naive daemon reads as "very cold" and holds the fan down for.
- A configured sensor that is missing or implausible **at startup is a hard error**, not a warning. Starting up having quietly dropped half its inputs is worse than not starting.
- The fans are handed back to the SMC on a clean exit, on `SIGTERM`/`SIGINT`/`SIGHUP`, and on a panic.
- `SIGKILL` cannot be caught, so the unit's `ExecStopPost` runs `macfand --restore` to cover it.
- Only one macfand drives the fans. An `flock` on `/run/macfand.lock` is taken **before** anything is switched into manual mode: two daemons would overwrite each other's commands from two different configurations, and the first of them to exit would hand the fans back to the SMC while the other went on believing it was in control. A file lock rather than a pidfile, so `SIGKILL` releases it too.
- Handover starts from the speed the fans are already running at, so a machine that is already hot is not briefly slowed down. If any fan's tachometer will not read, macfand takes over at **maximum** rather than assuming the machine is idle — the first poll's `dt` is nearly zero, so the slew limit would take several seconds to walk a wrong guess back.
- Fan writes that fail persistently — `MAX_WRITE_FAILURES` consecutive — **exit the daemon** rather than being logged forever. The fans are in manual mode for as long as macfand runs, so a daemon that cannot write is holding them at a stale speed; exiting hands them back to the SMC. One-off failures are ridden out, because `applesmc` returns `EBUSY` often enough on this hardware to matter.
- The unit is `Type=notify` with `WatchdogSec=30s`. If the control loop wedges, systemd kills and restarts it — which also runs `ExecStopPost`. A wedged fan daemon that nobody notices is the worst outcome available. The watchdog is fed on its own schedule rather than once per poll, so `poll_interval_ms` can be set longer than `WatchdogSec` without systemd killing a daemon that is only being slow on purpose — and it is withheld entirely while fan writes are failing, so a daemon that has lost the fan never reports itself healthy.

## The defaults, and where they come from

The shipped defaults were measured on a MacBookPro9,2 (13-inch, mid-2012, Debian 13), not guessed. Two of its sensors turn out to be nearly uncontrollable, which is worth knowing before you set a target on them:

| condition | package | TC0P | **TPCD** | **Ts0S** | fan |
|---|---|---|---|---|---|
| light load | 70 | 64 | **85** | **51** | 3900 |
| light load, fan pinned at maximum | 69 | 62 | **82** | **50** | 6200 |
| 4 threads flat out | 86–95 | 78 | **88** | **52** | 6200 |
| sustained hot session | 77 | 70 | **91** | **54** | 2900 |

TPCD (the PCH die) moved **3 °C for a 59% increase in airflow**, and **3 °C between idle and full load**. Ts0S (the palm rest skin) moved **one degree** across all of it. On this machine the PCH is a bare die conducting into the logic board, not under the heatsink the fan blows through — so the fan simply cannot reach it.

Over a longer hot session TPCD drifts further, to 91 °C. That is the full range it occupies: **82–91 °C, almost independently of anything the fan does.**

So `TPCD` ships with `target = 90.0` — at the top of that range, where it contributes only when the PCH is at its hottest, and `integral_limit_rpm = 1500` bounds that contribution to +1500 rpm over the floor no matter how long it stays there. Without the cap, a sensor that airflow cannot reach winds its integral all the way up and pins the fan at maximum permanently. Drop the target below about 85 and you get that outcome deliberately, in exchange for roughly 3 °C.

**If your complaint is that the chassis feels hot, a fan daemon is not the fix.** The measurements above are what that conclusion rests on. The lever that does work is reducing heat generation — on this machine the RAPL long-term package limit ships programmed at 100 W on a part whose thermal design power is 35 W.

## Configuration

`/etc/macfand.toml`, or `--config PATH`. Every key is optional and an empty file is valid; see `macfand.toml.example` for the annotated defaults.

| Key | Default | Description |
|---|---|---|
| `poll_interval_ms` | `1000` | Sample and re-command interval |
| `ramp_up_rpm_per_s` | `3000.0` | Ceiling on how fast the speed may rise; critical and failed sensors bypass it |
| `ramp_down_rpm_per_s` | `200.0` | Ceiling on how fast it may fall |
| `deadband_rpm` | `40` | Don't rewrite the fan for a change smaller than this |
| `min_rpm` / `max_rpm` | hardware | Narrow the SMC's range; cannot widen it |
| `plausible_range_c` | `[5.0, 125.0]` | Outside this, a reading is a misreport, not a temperature |
| `sensor_grace_polls` | `5` | Bad samples ridden out before a sensor is failed |

Per sensor:

| Key | Description |
|---|---|
| `source` | `applesmc` or `coretemp` |
| `key` | The sensor's label, e.g. `TPCD`, `Package id 0` |
| `target` | The temperature it is steered towards |
| `critical` | Above this, straight to maximum |
| `kp` | rpm per degree over target |
| `ki` | rpm per degree-second |
| `integral_limit_rpm` | Ceiling on the integral's contribution; unset means the full range |
| `optional` | A missing optional sensor is skipped rather than a startup error |

## Requirements

Linux on an Intel Mac, with `applesmc` and `coretemp` loaded. Driving the fan needs root; `--show` does not.
