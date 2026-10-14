# fan-tune

Windows fan controller for Nuvoton NCT6779D..NCT6799D SuperIO/EC chips, with a
temperature-curve engine, a closed-loop "budget" thermostat, and an ENE 6k7750
ARGB light controller.

Built for one specific machine (MSI/MAXSUN board, NCT6796D-R) and made public
because two parts of it are reusable: the curve engine and the light controller.

> **This writes to hardware fan control registers through a ring-0 driver.** It
> is not a general-purpose tool. Read [Which machines it drives](#which-machines-it-drives)
> before running it.

Full Chinese usage notes (per-field config reference, the logon autostart
task, troubleshooting): [`USAGE.zh-CN.md`](USAGE.zh-CN.md).


- **Sensors** — CPU temp/load, 7 fan channels (RPM + duty), board voltages, NVMe
  temps, GPU (temp/util/VRAM/power/fan), RAM usage
- **Control** — manual duty, draggable temperature curve, or BIOS auto
- **Budget mode** — a closed-loop thermostat: hold a temperature inside a band
  with the least duty that keeps it there, and probe downward to find the
  quietest sufficient level. This is the part worth reading; see
  [`docs/budget-mode-design.md`](docs/budget-mode-design.md)
- **Profiles** — Standard / Gaming (curves evaluated as if `+5 °C` warmer, so the
  ramp engages earlier while idle stays just as quiet) / Server (rate-limited,
  never oscillates, per-channel thermal guard)
- **Lights** — ENE 6k7750 ARGB controller over WinUSB, no ring-0 needed
- **Daemon / tray** — one process owns the hardware at all times; the GUI takes
  over on start and hands back on exit

## Which machines it drives

Chip detection covers the NCT6779D..NCT6799D family. The register tables come
from LibreHardwareMonitor (`refs/vendor/`).

Some chips are **recognised but deliberately not driven** — they report a reason
instead of being poked:

- NCT6683D / 6686D / 6687D — EC-space access, a different protocol
- NCT6796DS / 6799D — share an id/revision with a different chip; the fan
  registers differ and writing the wrong ones could hit another function
- NCT6771F / 6776F / 610XD — read-only, no writable fan registers

If your chip is not in the table, `detect` says so and exits. It will not guess.

## Building

Requires the **GNU** Rust toolchain on Windows (`windres` from mingw64, `ar`
from binutils) — `build.rs` embeds a `requireAdministrator` manifest, and fails
loudly if `ar` is missing rather than shipping an exe that silently cannot
reach the driver.

```sh
cargo build --release
```

Then place these next to `fan-tune.exe`:

- `WinRing0x64.sys` (from `driver/`) — only if you use the WinRing0 backend
- `LpcIO.bin`, `IntelMSR.bin` (from `driver/`) — PawnIO modules
- `fantune.json` — start from the one in the repo root

`rgb-tool.exe` is separate and does **not** need elevation.

### Ring-0 backends

Two are supported; both are attempted and the order is set by `backend` in
`fantune.json`.

- **PawnIO** (preferred) — a signed driver that runs signed modules. You install
  the driver yourself from <https://pawnio.eu>; fan-tune only loads the two
  modules above into it.
- **WinRing0** (fallback) — our own kernel service on `WinRing0x64.sys`. Note
  that Windows blocklists this driver and Defender flags it; HVCI / Smart App
  Control may refuse to load it.

## Safety notes

- Only one process may write the fan registers. The real guarantee is that the
  driver device is opened `FILE_SHARE_NONE`, plus an explicit lock file for the
  PawnIO backend (which opens with sharing).
- **Temperature source failure**: 5 consecutive passes with no usable reading —
  including a reading that has gone stale — puts every curve/budget channel at
  70% until the source recovers. `enabled: false` and `protected: true`
  channels are never touched.
- Channels are only driven up to the chip's actual fan count; writes above that
  are refused, because those registers belong to other functions.
- On exit, channels in Auto mode are handed back to the BIOS using the register
  values captured before the first override.

## Repository layout

| | |
|---|---|
| `src/algo.rs` | curve engine: EMA, hysteresis, slew limiting, budget loop |
| `src/control.rs` | one control pass — shared by the GUI and the daemon |
| `src/nct.rs` | chip register maps and banked EC access (testable: `PortIo`) |
| `src/driver.rs` | WinRing0 / PawnIO backends |
| `src/rgb/` | ENE 6k7750 controller (WinUSB, user-mode) |
| `src/ui/` | egui panel — specific to this machine, not meant to be reused |
| `docs/` | budget-mode design, hardware upgrade checklist |
| `refs/vendor/` | upstream sources the register maps were transcribed from |

## Tests

```sh
cargo test
```

Runs without elevation. `src/nct.rs` drives a simulated EC space through the
`PortIo` trait, so the bank cache, the BIOS hand-back and the out-of-range
guard are covered without a kernel driver.

## Third-party components

- Register maps and the PawnIO protocol transcribed from **LibreHardwareMonitor**
  (MPL-2.0) — `refs/vendor/LHM-LICENSE`
- PawnIO modules `LpcIO.bin` / `IntelMSR.bin` — LGPL-2.1, see
  `driver/PAWNIO-MODULES-LICENSE.txt`. The PawnIO driver itself is not
  redistributed here.
- `WinRing0x64.sys` — OpenLibSys 1.2.0, BSD-style (see `refs/vendor/OlsIoctl.h`)
- ARGB protocol transcribed from the community SignalRGB Maxsun plugin —
  `refs/vendor/Maxsun_ENE_Chip_6k7750.js`
- Geist Mono — SIL Open Font License, `src/ui/fonts/OFL.txt`

## License

MIT — see [LICENSE](LICENSE).
