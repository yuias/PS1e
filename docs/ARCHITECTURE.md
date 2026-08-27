# PS1e Architecture

PS1e is a PlayStation 1 emulator written in Rust, aiming to be lightweight and
fast while modeling the real hardware as independent components.

## Decisions

| Topic | Decision |
|---|---|
| Language / Graphics | Rust + wgpu |
| GPU rasterization | Software rasterizer inside the core; wgpu only presents the VRAM framebuffer. A hardware (wgpu) renderer with upscaling may be added later behind the same GPU command interface. |
| BIOS | Boot retail BIOS images from `assets/` (LLE). An original BIOS binary is developed in a **separate project**; this emulator treats it as just another 512 KiB ROM image and serves as its primary bring-up/verification tool (rich logging). No HLE hooks in the execution path. |
| Debug UI | egui embedded in the native frontend (registers, VRAM viewer, TTY console, log filtering). |
| Timing accuracy | Cycle-accuracy oriented: model memory wait states, DMA bus contention, load/branch delay slots, and per-instruction timing as faithfully as practical. Speed optimizations must not change observable timing. |
| Debugger | `psx-debug`: remote debug stub speaking the gdb-remote serial protocol (the wire protocol LLDB uses), with **LLDB as the primary client**: the LLDB-specific extensions (`qHostInfo`, `qProcessInfo`, `qRegisterInfo`, `target.xml`) are implemented. Plain GDB works as a byproduct. Enabled with `--debug-port <port>` (GUI and headless); `--wait-debugger` holds execution at the reset vector until attach. Validated against LLDB 22.1.8 on Windows (attach, registers via `target.xml`, memory read/write, address breakpoints, instruction stepping); `disassemble` requires an LLVM build that includes the Mips target, which official Windows release binaries omit. |
| Save states | Full-machine snapshots via serde + postcard (`PsxSystem::save_state`/`load_state`), versioned with a format header and BIOS fingerprint. The BIOS, disc image and memory card are deliberately excluded and carried over on load — rolling back the card would corrupt real saves. GUI: F5/F9 and toolbar buttons (slot file next to the memory card image); automation: `psxctl savestate/loadstate <path>`. |
| Web (future) | `psx-core` is platform-independent and compiles to `wasm32`; a thin HTML5/JS frontend drives it. |

## Workspace layout

```
PS1e/
├── crates/
│   ├── psx-core/     # Emulator core. No windowing, no GPU API, no I/O — wasm-safe.
│   ├── psx-debug/    # LLDB-first gdb-remote debug stub (TCP).
│   └── psx-app/      # Native frontend: eframe (egui + wgpu), audio, input.
├── assets/           # BIOS images (gitignored).
└── docs/
```

Planned crates: `psx-wasm` (wasm bindings).

## Core design (`psx-core`)

Each hardware component is an independent module owning its own state, mirroring
the real machine:

- `cpu`    — R3000A interpreter, COP0 (SCC), COP2 (GTE).
- `bus`    — Memory map: RAM (2 MiB, mirrored), scratchpad, BIOS ROM, MMIO dispatch,
             memory-control registers, wait-state accounting.
- `dma`    — 7-channel DMA controller.
- `gpu`    — GP0/GP1 command FIFO + software rasterizer into a 1 MiB VRAM buffer.
- `spu`    — Sound processing unit.
- `cdrom`  — CD-ROM controller + disc image (BIN/CUE) backend.
- `sio`    — Controllers and memory cards.
- `timers` — The three root counters.
- `irq`    — Interrupt controller (I_STAT / I_MASK).
- `scheduler` — Event-driven scheduler (see below).

### Timing model

The CPU is the master clock (33.8688 MHz). Instead of ticking every component
every cycle, components register *events* (e.g. "timer 1 overflows at cycle N",
"DMA block completes at cycle N") in the scheduler. The CPU runs until the next
event deadline, then due events fire. Components must be able to compute their
state lazily from the current cycle count ("catch-up on register access").

This keeps the emulator fast while remaining cycle-accurate: accuracy comes from
*when* events are scheduled (wait states, DMA stalls, GTE completion cycles),
not from per-cycle polling.

### Logging

`tracing` with per-component targets (`psx_core::cpu`, `psx_core::gpu`, …).
Debug builds log generously — BIOS call tracing, MMIO access logging — to make
verifying the original BIOS reimplementation easy. Release builds compile the
verbose levels out (`tracing`'s static max-level features).

### TTY

The kernel `putchar` entry points (A0h:3Ch / B0h:3Dh) are observed (PC watch)
to mirror TTY output into the log and debug UI. This is observation only — it
never alters execution — so it stays LLE-safe.

## Frontend (`psx-app`)

eframe (egui) with the wgpu backend. The emulated VRAM/framebuffer is uploaded
as a texture each frame. Debug panels: CPU registers, disassembly (future),
VRAM viewer, TTY console, MMIO/log filter. A `--headless` mode runs the core
without a window for CI and quick BIOS bring-up tests.

`--headless --control-port <port>` switches to *lockstep control mode*,
designed for scripted or LLM-driven game analysis: the emulator only advances
on command, so every observation is deterministic. The bundled `psxctl` client
sends one command per invocation (`run`, `press`, `input set`, `frame`,
`peek`/`poke`, `tty`, …), letting a shell or tool-using agent drive an
interact→observe→decide loop statelessly. Coexists with the debug stub; while
a debugger is attached, execution commands are refused (observation still
works).

## Original BIOS (separate project)

An original BIOS binary (LLE, real MIPS code) is developed in a separate
project. From this emulator's point of view it is just another 512 KiB ROM
image loaded at `0xBFC00000`, selectable via CLI/UI. The emulator's rich
logging (MMIO trace, TTY, PC history) is the primary bring-up tool for it.

## Known accuracy gaps (polish backlog)

Timing. These are one cluster: together they let control reach the game
slightly earlier than on real hardware (observed with SLPS-01770).

- Read timing matches the figures measured on hardware per region, and for
  the external bus follows the delay registers the BIOS programs. Still
  missing: the load shadow (a slow load partly overlaps the instructions
  after it, so a load with independent work behind it costs about 3 cycles
  rather than the full 5), and the DRAM refresh collisions that push main
  RAM a little above its nominal 7 cycles.
- The I-cache is modeled as an unconditional hit for cached segments: there
  are no cache lines, so no misses and no line fills.
- Stores cost only their issue cycle. The write queue is not modeled, so a
  full queue never stalls the CPU.
- The multiplier and divider retire instantly; nothing stalls on HI/LO.
- DMA still moves a whole transfer the moment CHCR starts it, but now
  charges what it would have taken at the measured per-word rates. The
  interleaving is missing: hardware keeps the CPU running out of cache,
  scratchpad, COP0 and GTE for the duration, and chopping hands the bus
  back at intervals, so a cache-resident loop is charged more here than it
  costs on the console. Channel priority and the chopping windows are not
  modeled at all.
- CD-ROM command latencies follow the measured averages, including the ones
  that depend on drive state (Pause and Stop are far quicker with nothing to
  wind down, and the drive acknowledges sooner while the motor is stopped).
  Seek time is still a coarse distance model rather than the drive's own
  coarse/fine stepping, and the second responses of Init and ReadTOC are
  guesses — hardware figures for those have not been published.
- GPU draw commands complete instantly, so GPUSTAT's ready flags stay
  pinned to ready and the 16-word GP0 FIFO never fills. Modeling this
  properly is blocked rather than merely unfinished: how long the hardware
  takes to render a primitive has never been measured, and the ready bits
  are what games poll, so inventing a figure risks hanging them. The
  scanline the GPU reports drawing (bit 31) is accurate.
- GTE commands take their documented time, and the CPU runs on until an
  instruction touches the GTE, as on hardware. Not modeled: the shorter
  delay before a written COP2 register is readable, and the LZCS/LZCR
  timing, which is undocumented.

Component coverage.

- XA audio resamples linearly to 44100 Hz; hardware uses the ZigZag
  interpolation tables.
- MDEC 24-bit output ordering unverified against hardware captures.
- SIO1 is a stub: the status register reports an idle port and writes are
  only kept for read-back.
- The emulated controller is a digital pad; there is no analog mode, so the
  frontend has nothing to map the sticks onto.

## Milestones

1. **CPU bring-up** — workspace, bus, R3000A interpreter; retail BIOS executes,
   TTY prints the kernel banner.
2. **GPU** — software rasterizer, VRAM viewer, boot logo renders; DMA (OTC/GPU),
   timers, vblank IRQ.
3. **Game boot** — CD-ROM controller, BIN/CUE loading, SIO controllers, GTE.
4. **Sound** — SPU, audio output (cpal), CD-DA/XA.
5. **Accuracy & speed** — wait states, DMA contention, GTE timing; profiling.
   *(current)*
6. **Platform reach** — LLDB-first remote debug stub *(done)*; wasm build
   still to come.
