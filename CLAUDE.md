# PROTEUS

A GPU-native terminal engine for people and AI agents. One standalone Windows exe. This file is the working agreement for anyone (person or coding agent) changing this repository.

## Rules

- **Measured claims only.** Every performance number comes from `--bench`, `stats.json`, a test or a counter, with the command beside it. Never write "fast" where a number belongs.
- **Nothing decorative.** Every visual or sound is driven by a real event (keystroke, output, exit code, process, load). No fake telemetry, no idle animation.
- **Zero frames when idle.** A frame is drawn only for an event or while an animation settles. Check idle CPU after any change to the frame loop (the gate does).
- **One file.** The product is a single exe with everything inside it (fonts, icon, console host, licence texts). Never add a file that must sit beside it; embed it and unpack at run time (`src/runtime.rs`). The gate proves it runs alone.
- **It looks after itself.** Setup, daemon start, hand-over between builds, journal pruning, cleanup and updates happen inside the program, not in scripts the user must run.
- **Never disturb a terminal you are working in.** If you run PROTEUS while developing PROTEUS, test from copies in `dist\` with `PROTEUS_DATA` and `PROTEUS_SOCKET` pointing at a temp folder and `PROTEUS_SOUND=off`. Never kill PROTEUS processes by name.
- **Look at what you draw.** `proteus shot` renders to a PNG with no window; read the PNG after any renderer change. `proteus ears` does the same for sound.
- **Nothing mock in tests.** Real processes, real files in temp directories, real journals through the real console host. Generated input says so.
- Keyboard input is never journaled (passwords). Command text comes from the shell's own report, never from keystrokes.
- Restore and undo delete and overwrite files: their tests run only in temp directories.

## Layout

| Area | Files |
|---|---|
| Window, input, frames | `app.rs`, `input.rs`, `keys.rs` (kitty keyboard protocol, pure), `probe.rs` (`proteus keys`) |
| Rendering | `renderer.rs`, `renderer/scene.rs`, `shader.wgsl`, `atlas.rs`, `fonts.rs`, `builtin.rs` (engine-drawn box, block, braille), `deco.rs` (underlines), `theme.rs`, `gpu.rs`, `icon.rs` |
| Terminal state | `term.rs` (alacritty_terminal, wake-on-change), `marks.rs` (OSC 133/633/7/9/7501 scanner, status records, PowerShell integration), `conform.rs` |
| Sessions | `session.rs` (pseudo-console child, journal), `daemon.rs`, `remote.rs`, `proto.rs` (sessions outlive windows) |
| Memory | `journal.rs`, `recall.rs`, `recall_cli.rs`, `scrub.rs` (keyframes, seek, search, scrubbing), `shot.rs` (headless render, time travel) |
| Process senses | `procs.rs`, `procs_win.rs` (job object), `procs_cli.rs`, `procs_measure.rs` |
| World view | `world.rs`, `world_shot.rs` |
| Tree history and undo | `chronicle.rs`, `watch.rs` (`proteus chronicle`, `proteus run`) |
| Agents | `mcp.rs` (MCP server), `lean.rs` (lean reader) |
| Sound | `synth.rs` (pure), `sound.rs` (device), `ears.rs` (`proteus ears`) |
| Self-containment and updates | `runtime.rs`, `update.rs` (signed self-update), `config.rs`, `paths.rs`, `build.rs` |
| Bundled | `vendor/conpty/` (Microsoft ConPTY), `assets/fonts/` (Maple Mono NF, OFL 1.1), `assets/THIRD-PARTY.txt` |
| Forge | `forge/gate.ps1` (the gate), `forge/ship.ps1`, `forge/release.ps1` (signed release), `forge/licenses.ps1`, `forge/sign/` (release signing tool), `forge/ledger.json` (state with evidence), `forge/roadmap.md`, `forge/samples/` |

## Commands

```powershell
cargo test --release                      # unit and integration tests
pwsh forge\gate.ps1 -SkipBench            # headless gate: opens no window
pwsh forge\gate.ps1                       # full gate: test windows (never take the keyboard), bench, idle budget
proteus shot --vt forge\samples\basic.vt --out x.png [--reveal-ms 110]
proteus shot --journal F.prtj --until SECS [--world | --scrub] --out x.png
proteus ears --out x.wav
proteus --bench                           # writes bench.json in the data folder
```

Env: `PROTEUS_DATA`, `PROTEUS_SOCKET` (isolate a test run), `PROTEUS_SOUND=off|0..1`, `PROTEUS_NO_FOCUS=1` (test windows never take the keyboard), `PROTEUS_BACKEND=dx12|vulkan|auto`, `PROTEUS_PRESENT=vsync|immediate`, `PROTEUS_SHELL=<exe>`. Data: `%LOCALAPPDATA%\proteus\` (log, stats, bench, journals, config).

## State

`forge/ledger.json` is the source of truth for what is done, with evidence, and what is known to be wrong. Update it in the same commit as the change. `forge/roadmap.md` ranks what comes next, each item with a success metric and a kill criterion. API notes: wgpu 27, winit 0.30, alacritty_terminal 0.25. Check `~/.cargo/registry/src` before assuming an API.
