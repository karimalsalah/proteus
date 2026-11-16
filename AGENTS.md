# AGENTS.md

Instructions for AI coding agents (Codex, Claude Code, Cursor, Copilot, Aider and others) working in this repository. The full working agreement is in [CLAUDE.md](CLAUDE.md); this is the short version.

- **Build and test:** `cargo build --release`, `cargo test --release`, `pwsh forge\gate.ps1 -SkipBench` (headless; opens no window).
- **Never open a PROTEUS window from a test or script** unless `PROTEUS_NO_FOCUS=1` is set, and always set `PROTEUS_DATA` and `PROTEUS_SOCKET` to a temp folder and `PROTEUS_SOUND=off`, so a person's own sessions are never touched.
- **Never kill processes by name.** The person may be running PROTEUS itself.
- **Measured claims only.** Any number you write (in code comments, docs, the ledger, a pull request) comes from a command you ran; put the command beside it.
- **Nothing mock.** Tests use real processes, real files in temp directories, real journals.
- **Look at what you draw:** render with `proteus shot ... --out x.png` and read the PNG after any renderer change.
- **Update `forge/ledger.json`** in the same change: what is done with its evidence, what is known to be wrong.
- **Restore and undo delete files.** Their tests run only in temp directories.

Using PROTEUS as a tool rather than changing it: `proteus mcp` is an MCP server (sessions, spawn, send, wait, read, screen, shot, kill). See the README section "For AI agents".
