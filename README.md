# PS1e

A PlayStation 1 emulator written in Rust. Software-rasterized GPU, SPU with
reverb and XA audio, CD-ROM with mechanical timing, GTE, MDEC, memory
cards, an LLDB-compatible remote debugger, and a lockstep automation
interface.

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
ps1e [--bios <path>] [--disc <image>]
```

A disc image is either a `.cue` sheet or a raw image of 2352-byte sectors
(`.bin`, `.img`); only the `.cue` extension is treated specially.
2048-byte sector images are not supported — they would need sector
reconstruction.

Without `--disc`, the BIOS shell runs. The machine starts running as soon
as the window opens. The window shows the display, with a menu bar for run
control and a status bar underneath. Emulation covers run/pause, step,
hardware reset (a power cycle; the disc and memory card stay in, and the
machine keeps running), save/load state and screenshots; View toggles
fullscreen and the debug panels — CPU registers, the TTY console, the VRAM
viewer and the GPU command log — all of which start hidden. Audio holds the
master volume, and Help lists the current key bindings.

"Insert disc..." swaps the disc the way the console does: the drive opens,
the file picker comes up, and the drive closes on whatever was picked —
cancelling puts the old disc back. Emulation never stops, so this works
mid-game, and from the BIOS menu the console boots the disc on its own,
just as it does on hardware.

| Keys | |
|---|---|
| Arrows | D-pad |
| Z / X / S / D | Cross / Circle / Square / Triangle |
| W / R / E / U | L1 / R1 / L2 / R2 |
| V / C | Start / Select |
| F5 / F9 | Save / load state |
| F11 / F12 | Fullscreen (Esc leaves) / screenshot |

A gamepad, if one is connected, drives the same pad in parallel with the
keyboard: face buttons to Cross/Circle/Square/Triangle, shoulders and
triggers to L1/R1/L2/R2, D-pad and Start/Select as labelled. Analog
sticks do nothing yet — the emulated controller is a digital pad.

The pad and the save/load hotkeys are rebindable; see the `[keys]`, `[pad]`
and `[hotkeys]` tables under [Configuration](#configuration). Fullscreen
and screenshot are fixed.

A screenshot writes the displayed frame as `screenshot_<epoch>.bmp` in the
working directory, the same encoding as headless `--dump-frame`.

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
cross = "Z"
start = "V"

[pad]                       # gamepad; gilrs button names
cross = "South"
start = "Start"

[hotkeys]                   # frontend shortcuts
save_state = "F5"
load_state = "F9"
```

Key names are the ones egui reports: letters and digits as themselves
(`"X"`, `"1"`), arrows as `"Up"`/`"Down"`/`"Left"`/`"Right"`, plus
`"Enter"`, `"Backspace"`, `"Space"`, `"F1"`..`"F35"`. Omitted buttons keep
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

Runs without a window and prints a run summary (TTY output, pc, frame
count, audio and CD statistics). Useful flags:

| Flag | |
|---|---|
| `--cycles N` | CPU cycles to run (default 30,000,000) |
| `--input <file>` | Replay an input script: `<start-sec> <dur-sec> <BTN+BTN>` per line, `#` comments |
| `--mash-start` | Tap START/CROSS periodically to advance menus |
| `--dump-frame <p>` / `--dump-vram <p>` | Write the display frame / full VRAM as BMP |
| `--dump-wav <p>` | Write captured audio as WAV |
| `--log-gpu` | Decode every GP0/GP1 command to the log |
| `--peek <hex>` | Hex dump 96 bytes of RAM at the end of the run |

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
psxctl input clear            # release everything held
psxctl frame shot.bmp         # dump the current display frame
psxctl vram vram.bmp          # dump the full 1024x512 VRAM
psxctl peek 801ffc38 64       # hex dump memory (side-effect-free)
psxctl poke 80100000 deadbeef # write RAM
psxctl disc open              # open the drive lid
psxctl disc close game.cue    # close it on a new image (or bare: the old one)
psxctl tty                    # TTY output since the last call
psxctl savestate s.sst        # snapshot; loadstate restores it
psxctl state                  # pc, cycles, frames, held buttons, display
psxctl quit
```

`psxctl help` lists the full command set.

A typical agent loop: `press`/`run` → `frame`/`peek`/`tty` → decide →
repeat, with `savestate`/`loadstate` for branching exploration. The
control port and the debugger can be active simultaneously; execution
commands are refused while a debugger is attached.

## Limitations

- DMA channel 5 (expansion port / PIO) is not implemented. No retail
  software uses it; transfers on it are logged and ignored.
- Video timing follows the display region, resolution and display
  window, but is derived from the cycle count rather than real GPU
  scanout: interlaced fields are rounded to whole scanlines, and the
  dotclock counter does not drop the fractional dot at the end of a
  scanline the way the hardware does.
- The controller is a digital pad, so a gamepad's analog sticks are not
  read. Memory cards respond on slot 1 only; slot 2 is empty.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

MIT. See [LICENSE](LICENSE).
