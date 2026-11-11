# air-conditioning

Run your Mac's fans at max for a while, then hand them back to macOS.

macOS keeps fans quiet until the chip gets hot, and lets them settle early. That's the right
default, but sometimes you'd rather have it loud and cool: after a long build, during a render,
or when the laptop is warm on your lap. `air-conditioning` takes the fans for a set time, or
until the CPU and GPU cool to a target, then gives control back.

## Install

From a clone of this repo:

```sh
cargo install --path . --locked
```

Needs macOS and Rust 1.89+. Changing fan speeds requires root, so the tool re-runs itself
under `sudo` and asks for your password when it needs it.

## Usage

```sh
air-conditioning                # max fans for 30 minutes
air-conditioning 10             # max fans for 10 minutes
air-conditioning --target 55    # max until the hottest CPU/GPU sensor holds under 55°C for a minute (30 min at most)
air-conditioning off            # hand the fans back to macOS now, ending any run in progress
```

```
$ air-conditioning 20 --target 55
fans at max for up to 20 min, or until the hottest sensor holds under 55°C (now 71.3°C)
held under 55°C, fans back on auto
```

The fans go back to macOS when the time is up, when the target is reached, and on Ctrl-C,
`kill`, errors and panics. For the deaths a process can't clean up after itself, like
`kill -9` or `Ctrl-\`, each run starts a small watchdog process that waits for the run to end
and then hands the fans back. Only killing both at once (say, `pkill -9 air-conditioning`) gets
past it; `air-conditioning off` or a reboot puts that right. Only one run holds the fans at a
time.

## How it works

The fans are controlled by the System Management Controller (SMC). A run:

1. puts each fan in manual mode (`F<n>Md`) and sets its target speed (`F<n>Tg`) to the
   fan's own reported maximum (`F<n>Mx`), so it never asks for more than the hardware says
   it can do;
2. reads every write back until it takes, because the SMC applies writes asynchronously;
3. with `--target`, watches the hottest CPU/GPU die sensor every 5 seconds. Sensors are found
   by listing the SMC's keys at startup, since their names change with each chip generation;
4. switches each fan back to automatic when it ends.

On M3 and M4 Macs, the SMC reportedly ignores manual fan modes until a test flag (`Ftst`) is
set, so the tool sets it while it holds the fans and clears it afterward. Chips without that
key skip the step.

## Compatibility

| Mac | Status |
|---|---|
| M1 Max (MacBookPro18,4), macOS 15 | Tested |
| Other M1 and M2 | Expected to work, untested |
| M3, M4 | Uses the `Ftst` unlock other fan tools describe; untested |
| M5 and later | Untested |
| Intel | Unsupported: its SMC uses different number formats for fan speeds |

Reports from other Macs are welcome.

## Neutral engineering notes

`air-conditioning` does one thing and gets out of the way. No network calls, no telemetry, no
config files, nothing left running once a run is over. The only thing it leaves on disk is an empty
lock file, `/var/run/air-conditioning.lock`. The code forbids `unsafe`; the SMC calls go through
[`smc-lib`](https://crates.io/crates/smc-lib), pinned to an exact version that was read
before use because it runs as root.

## License

MIT
