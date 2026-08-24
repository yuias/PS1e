# PS1e

A PlayStation 1 emulator written in Rust. Software-rasterized GPU, SPU with
reverb and XA audio, CD-ROM with mechanical timing, GTE, memory cards, an
LLDB-compatible remote debugger, and a lockstep automation interface.

A PlayStation BIOS image (512 KiB, e.g. SCPH-1000) is required and not
included.

## Build

Requires a recent stable Rust toolchain.

```
cargo build --release
```

Produces `ps1e` (emulator) and `psxctl` (automation client) in
`target/release/`.

## Run

```
ps1e [--bios <path>] [--disc <image.bin|image.cue>]
```

Without `--disc`, the BIOS shell runs. The GUI shows the display, CPU
registers, a VRAM viewer, and a TTY console, and can pick a disc image at
runtime ("Open disc…"). Opening one resets the machine: the drive's
shell-open event is not modeled, so a running game would never notice a
swap.

| Keys | |
|---|---|
| Arrows | D-pad |
| X / C / S / D | Cross / Circle / Square / Triangle |
| Q / E / 1 / 3 | L1 / R1 / L2 / R2 |
| Enter / Backspace | Start / Select |
| F5 / F9 | Save / load state |

A gamepad, if one is connected, drives the same pad in parallel with the
keyboard: face buttons to Cross/Circle/Square/Triangle, shoulders and
triggers to L1/R1/L2/R2, D-pad and Start/Select as labelled. Analog
sticks do nothing yet — the emulated controller is a digital pad.

All of these are rebindable; see the `[keys]`, `[pad]` and `[hotkeys]`
tables under [Configuration](#configuration).

Save states snapshot the full machine to `state0.sst` next to the memory
card image. The BIOS, disc image and memory card are not part of a state
and carry over on load.

## Configuration

`config.toml` is searched at `<exe_dir>/config/config.toml`, then
`~/.config/PS1e/config.toml`. A commented template is generated on first
run. CLI flags override the file.

```toml
bios = "path/to/bios.bin"   # required to run
volume = 0.5                # master volume, 0.0..1.0
memcard = "memcard0.mcr"    # created and formatted automatically

[keys]                      # digital pad; egui key names
cross = "X"
start = "Enter"

[pad]                       # gamepad; gilrs button names
cross = "South"
start = "Start"

[hotkeys]                   # frontend shortcuts
save_state = "F5"
load_state = "F9"
```

Key names are the ones egui reports: letters and digits as themselves
(`"X"`, `"1"`), arrows as `"Up"`/`"Down"`/`"Left"`/`"Right"`, plus
`"Enter"`, `"Backspace"`, `"Space"`, `"F1"`..`"F20"`. Omitted buttons keep
their defaults, and an unrecognized name falls back to the default with a
warning in the log.

Gamepad names are the `gilrs::Button` variants: `"South"`, `"East"`,
`"North"`, `"West"`, `"DPadUp"`..`"DPadRight"`, `"LeftTrigger"`,
`"LeftTrigger2"`, `"RightTrigger"`, `"RightTrigger2"`, `"Start"`,
`"Select"`, `"LeftThumb"`, `"RightThumb"`, `"Mode"`, `"C"`, `"Z"`.

## Headless mode

```
ps1e --headless [--cycles N] [--disc <image>] ...
```

Runs without a window and prints a run summary (TTY output, pc, audio and
CD statistics). Useful flags:

| Flag | |
|---|---|
| `--cycles N` | CPU cycles to run (default 30,000,000) |
| `--input <file>` | Replay an input script: `<start-sec> <dur-sec> <BTN+BTN>` per line |
| `--mash-start` | Tap START/CROSS periodically to advance menus |
| `--dump-frame <p>` / `--dump-vram <p>` | Write the display frame / full VRAM as BMP |
| `--dump-wav <p>` | Write captured audio as WAV |
| `--log-gpu` | Decode every GP0/GP1 command to the log |

## Debugger (LLDB / GDB)

```
ps1e --headless --debug-port 9001 --wait-debugger
```

The stub speaks the gdb-remote serial protocol with LLDB as the primary
client (`target.xml`, `qHostInfo`, `qRegisterInfo`); plain GDB works too.
`--wait-debugger` holds execution at the reset vector until a client
attaches. Also available in the GUI.

```
(lldb) gdb-remote localhost:9001
(lldb) breakpoint set --address 0x80010000
(lldb) continue
```

Registers, memory read/write, software breakpoints, single-stepping and
interrupt are supported. Memory reads are side-effect-free (MMIO is not
dispatched). Disassembly requires an LLVM build that includes the Mips
target.

## Automation (LLM / scripting)

```
ps1e --headless --control-port 9002 [--disc <image>]
```

The emulator runs in lockstep: it advances only when commanded, so every
observation is deterministic. `psxctl` sends one command per invocation
over TCP:

```
psxctl run 20s                # advance (frames by default; s/c suffixes)
psxctl press START 30         # hold buttons for 30 frames, then release
psxctl input set UP           # hold until changed; applied during run
psxctl frame shot.bmp         # dump the current display frame
psxctl peek 801ffc38 64       # hex dump memory (side-effect-free)
psxctl poke 80100000 deadbeef # write RAM
psxctl tty                    # TTY output since the last call
psxctl savestate s.sst        # snapshot; loadstate restores it
psxctl state                  # pc, cycles, frames, held buttons, display
psxctl quit
```

A typical agent loop: `press`/`run` → `frame`/`peek`/`tty` → decide →
repeat, with `savestate`/`loadstate` for branching exploration. The
control port and the debugger can be active simultaneously; execution
commands are refused while a debugger is attached.

## Limitations

- DMA channel 5 (expansion port / PIO) is not implemented. No retail
  software uses it; transfers on it are logged and ignored.
- Video timing follows the display region (NTSC/PAL), but the dotclock
  timer source is fixed at the 320-pixel divider and the blanking windows
  the counter synchronization modes gate on are nominal rather than taken
  from the configured display range. Interlaced fields are rounded to
  whole scanlines.
- The controller is a digital pad, so a gamepad's analog sticks are not
  read. Memory cards respond on slot 1 only; slot 2 is empty.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

To be decided.
