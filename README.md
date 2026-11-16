# PROTEUS

A GPU terminal for Windows, built for people who direct AI agents and for the agents themselves. One self-contained exe.

The idea: every command a person or an agent runs passes through the terminal, so the terminal can be the system of record. It senses what happens (bytes, processes, files), remembers it (a journal of every session, a history of the working tree), and gives agents organs to see, drive, wait and undo. Nothing on screen or in sound is decorative: every mark and every note is a real event.

**Status: early.** It is one person's daily terminal, not a release. "PROTEUS" is a provisional name. Read [What is not done](#what-is-not-done) before trusting it with anything.

## What it does today

| | |
|---|---|
| **Draws nothing when nothing changes** | 0.00% of a core and no frames while idle, measured by the gate on every build |
| **Sessions outlive their window** | A daemon in the same exe holds sessions; a new window picks up where the last one left off |
| **Remembers everything printed** | Each session is journaled with timestamps; `proteus shot --journal F --until T` renders the screen as it stood at any moment |
| **Time travel** | Keyframes make any instant reachable in milliseconds; `proteus recall find TEXT` says when text was on screen; Ctrl+Shift+Left/Right scrubs in the window |
| **Sees every process** | A job object per session reports each process a session starts, with command line and exit code, with no hooks |
| **World view** | Ctrl+Shift+W shows the session's processes as a tree and a timeline; every mark is a recorded event |
| **Undo for the shell** | `proteus run -- CMD` records the working tree before and after; `proteus chronicle undo` puts it back |
| **Organs for agents** | `proteus mcp` is an MCP server: sessions, screen, shot, send, event-driven wait, lean reads |
| **Hears what happens** | Sound is computed from real events (no samples, nothing loops); the audio device sleeps between sounds |
| **Speaks the protocols** | Kitty keyboard protocol, synchronized output, colour queries, OSC 7501 program status (it answers the query, so agents that report their state start doing so) |
| **Checks itself** | `proteus shot` renders to PNG with no window, `proteus ears` renders sound to WAV; a gate script runs tests, a real-window key test, a benchmark and idle budgets before anything ships |

Numbers, with how each was measured, are in [`forge/ledger.json`](forge/ledger.json). The rule in this repository is that a claim without a measurement does not get written down.

## Build

Windows 11, Rust (MSVC toolchain).

```powershell
cargo build --release          # target\release\proteus.exe is the whole product
cargo test --release
pwsh forge\gate.ps1            # tests, runs-alone check, sound, screenshots, real keys, bench, idle budget
```

The first run creates `%LOCALAPPDATA%\proteus\config.json`.

## Use

```powershell
proteus                         # a window on a new or orphaned session
proteus sessions                # what the daemon is running
proteus shot --cmd "cmd /c ver" --out x.png
proteus run -- cargo test       # one undoable step
proteus chronicle undo          # dry run; add --apply to perform it
proteus mcp                     # serve agents over stdio
proteus licenses                # third-party notices
```

Register the MCP server with an agent harness, for example Claude Code:

```powershell
claude mcp add --scope user --transport stdio proteus -- "C:\path\to\proteus.exe" mcp
```

## What is not done

- **Window paths nobody has pressed.** The world view, time scrubbing and `--attach` were built and tested headless (through `proteus shot`). The gate types real keys into a real window, but those three views have not been driven by a person yet.
- **Undo covers files in a tracked tree only.** Not network, registry, database or anything in `.gitignore`. Restore is a dry run unless you pass `--apply`.
- **An agent's own tool commands are not seen as output.** Harnesses run them in separate processes. PROTEUS sees those processes, but lean reads, undo by command and receipts reach them only when commands are routed into PROTEUS sessions. That routing is not built.
- **Command boundaries are built but not switched on.** The scanner and the PowerShell integration exist and are tested through a real console host; sessions do not load the integration yet.
- **No colour emoji, no ligatures, no images.** Journals are plaintext and hold everything printed, including command lines.
- **Windows only.** No sandboxing: PROTEUS observes, it does not contain.
- **Nobody has tuned the sound or the light by ear and eye.**

The ranked plan, with a success metric and a kill criterion for each item, is in [`forge/roadmap.md`](forge/roadmap.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: measured claims only, nothing mock in tests, the gate must pass, and say plainly what you did not test.

## Licence

MIT for PROTEUS itself ([LICENSE](LICENSE)). It embeds Microsoft's ConPTY (MIT) and the Maple Mono NF font (SIL OFL 1.1); see `proteus licenses`.
