---
name: proteus-terminal
description: Use when a task needs an interactive or long-running Windows terminal program (a dev server, a REPL, an installer that asks questions, a full-screen TUI, a build to watch) or when a person should be able to watch what the agent runs. Uses the PROTEUS MCP tools instead of one-shot shell commands.
---

# Working in PROTEUS terminal sessions

The `proteus` MCP server gives you real sessions on a Windows pseudo-console. A person can watch any of them live with `proteus --attach N`, and every session is journaled, so nothing you run is lost.

- **Start:** `spawn` with a command (or none for the person's shell) and a `cwd`. Keep the session number.
- **Type:** `send` with `text`, named `keys` (Enter, Tab, Escape, Up, Down, Ctrl+C ...) and `enter: true`. It returns a cursor into the output.
- **Wait, do not poll:** `wait` with `until` = a `regex`, `quiet_ms`, or `exit`, and a `timeout_ms`. It returns when that happens. Never loop on `read` with sleeps.
- **Read lean:** `read` with the cursor from `send` and `mode: "lean"`: escape codes stripped, progress bars collapsed to their final state, repeated lines folded with counts, error and warning lines always kept. It reports raw and lean byte counts; ask for `raw` only when you need exact bytes.
- **Look:** `screen` returns what a person sees as text; `shot` returns a PNG. Both take `at` (seconds into the session) to look at an earlier moment.
- **Clean up:** `kill` sessions you started when you are done. You can only drive or kill sessions you started unless the server was started with `--drive-all`.

Prefer one long-lived session per task (environment, working directory and activated tools persist between commands) over a new process per command.
