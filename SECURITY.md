# Security

PROTEUS is a terminal: it runs programs with the rights of whoever launched it, records what they print, and can be driven by agents. Treat it accordingly.

## What to know

- **Journals are plaintext.** Everything a session prints is written to `%LOCALAPPDATA%\proteus\journal`, including command lines of the processes it starts. Keyboard input is never journaled.
- **No sandbox.** PROTEUS observes sessions; it does not contain them. A session inherits the token of the process that launched PROTEUS, including an elevated one.
- **The MCP server** may read any session but, by default, may only type into or end sessions it started itself. `--drive-all` lifts that; use it deliberately.
- **Undo** (`proteus chronicle restore` / `undo`) overwrites and deletes files inside the tracked tree. It is a dry run unless `--apply` is given, never follows links out of the tree, and snapshots the current state first.
- **Marks printed by programs** (shell integration, status reports) are untrusted unless they carry the session's nonce.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository (Security tab, "Report a vulnerability"). Please include the build id from `proteus --version` and the steps. You will get an answer; this is a small project, so allow a few days.
