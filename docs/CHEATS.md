# Cheats

PS1e applies GameShark / Pro Action Replay codes once per frame, at the
vblank edge, which is where the real cartridge got control.

Codes are read from a `.cht` file beside the disc image — `Crash.cue`
takes its cheats from `Crash.cht`. The format is the one PCSX-Reloaded
and DuckStation write: `[Name]` section headers, one code per line, `#`
and `;` comments, blank lines ignored. A leading `*` on a section name
means that cheat is enabled, and it is the only record of that — there is
no per-disc enable map in the config file.

Codes get into that file two ways: by hand, and from the memory scanner
on the pane's Memory page. A scanner hit has a "+" beside it that appends
a cheat holding that value at that address — a `30`/`80` line, or a pair
of `80` lines for a 32-bit hit, since PS1 codes have no 32-bit write. The
cheat is named after the address and width, so pressing "+" again on the
same hit updates its value rather than adding a second section. From then
on it is an ordinary entry in the file, enabled and disabled from the
Cheats page like any other. The list has no delete: removing one means
editing the `.cht`.

Nothing applies until cheats are switched on. `cheats` in the config file
is the master switch and defaults to off, so a `.cht` left beside an
image does not change how a game runs until it is asked for. In the GUI
the switch is at the top of the pane's Cheats page; over the control port
it is `cheat apply on|off`.

## Code types

The reference is the psx-spx page "Cheat Devices — Datel Cheat Code
Format". Codes are `TTaaaaaa vvvv`: a type byte, a 24-bit address and a
16-bit operand.

| Type | Line | Effect |
| --- | --- | --- |
| `30` | `30aaaaaa 00dd` | 8-bit write |
| `80` | `80aaaaaa dddd` | 16-bit write |
| `10` / `11` | `1xaaaaaa dddd` | 16-bit increment / decrement |
| `20` / `21` | `2xaaaaaa 00dd` | 8-bit increment / decrement |
| `D0`–`D3` | `Dxaaaaaa dddd` | 16-bit compare; runs the next code if it holds |
| `E0`–`E3` | `Exaaaaaa 00dd` | 8-bit compare; runs the next code if it holds |
| `D4` | `D4000000 dddd` | runs the next code while these buttons are held |
| `50` | `5000nnbb dddd` + `aaaaaaaa ??ee` | slide: `nn` writes stepping the address by `bb` and the value by `ee` |
| `C2` | `C2ssssss nnnn` + `80tttttt 0000` | copy `nnnn` bytes from `ssssss` to `tttttt` |

A conditional gates exactly the code after it. There is nothing to nest:
chaining conditionals nests them by construction, because each one either
runs or skips the single code that follows.

Every write goes through the same side-effect-free accessor the debugger
and the control port use, so a code naming ROM or a hardware register
does nothing rather than, say, draining a FIFO. The 24-bit address field
only reaches the low 16 MiB of the bus in any case, of which only RAM
decodes, so in practice codes can only write RAM.

## Details the reference states loosely

Four points are worth spelling out, because psx-spx gives them briefly or
hedges them. Each is pinned by a named test in
`crates/psx-core/src/cheats.rs`, so a correction changes a test rather
than being discovered by accident.

- **Comparison direction.** A comparison puts the code's own operand on
  the left: `D2` runs the next code when `dddd` < `[aaaaaa]`.
- **Button operand.** `D4` compares against the pad halfword as the
  hardware presents it, which is active low: nothing held is `FFFF`, and
  Cross alone is `BFFF`. Published codes use values of that shape.
- **Slide width.** The slide's writes are 16-bit, matching the width of
  its value field.
- **Copy length.** For `C2`, the layout names the length `nnnn` while the
  prose calls it `ssss`; the operand word is the field that holds it.

## Types that are not applied

`C0`, `C1`, `D5` and `D6` are parsed and kept — a file containing them
still loads, and the affected cheat is marked in the UI — but they do
nothing. So are the code types psx-spx attributes to the Caetla release
notes rather than to Datel: the `C3`/`9100` indirect writes and the
`12`/`22` 32-bit increments.

```
C0aaaaaa dddd   If dddd=[aaaaaa] then turn on all codes
C1000000 nnnn   Delay activation of codes by nnnn
D5000000 dddd   If dddd=JoypadButtons then turn on all codes
D6000000 dddd   If dddd=JoypadButtons then turn off all codes
```

Parsing these is trivial; what stops them is that all four change state
that has to outlive the frame they run in, and the reference does not say
what that state means. Implementing them is a design decision, not a
missing feature:

- **What "all codes" covers.** Cheats here are independent named entries
  that the user turns on and off one at a time. A `D5` inside one entry
  arming every other entry in the file is a defensible reading of "all
  codes", and it is also a surprising thing for a checkbox to do.
- **How anything gets turned back on.** After `D6` disables everything,
  `D5` and `C0` are themselves codes. If "off" means no code runs, then
  nothing can ever re-enable them. The arming codes therefore have to be
  exempt from the state they control — a rule the reference never states,
  and one whose exact shape decides whether a disabled cheat's `D5` can
  revive the others.
- **A third switch.** PS1e already has two: the `*` marker per cheat, and
  the master switch. These codes would add a runtime-only third, and the
  three would have to compose in some order that a user can predict from
  the UI.
- **What `C1` counts.** The page gives "nnnn (4000-5000 = 20-30 sec)".
  That is not frames: 4000 frames is over a minute at 60 Hz. It works out
  near 20 seconds only around 200 Hz, which is not a video rate, so the
  unit is some other tick that would have to be measured on hardware
  rather than read off the page.

None of the four appears often in published PS1 code lists — they exist
to toggle cheats with a button combination mid-game, not to change what a
game does — so the cost of guessing wrong is paid against very little
benefit.

## Automation

The control port exposes the whole feature, which is also how it is
tested end to end:

```
cheat list            cheats from the disc's .cht, with their enable state
cheat apply on|off    master switch
cheat on|off <n>      toggle cheat n and write the marker back to the file
cheat reload          re-read the .cht for the disc in the drive
```

Toggling rewrites the `.cht` in place. Comments in the original file are
not preserved.
