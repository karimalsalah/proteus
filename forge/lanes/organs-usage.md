# proteus mcp: the terminal an agent would ask for

`proteus mcp` is a Model Context Protocol server on standard input and output.
It gives an agent sessions in the PROTEUS daemon to start, type into, wait on,
read and look at. The same executable; nothing to install.

## Register it with Claude Code

Run this once, in PowerShell, with a PROTEUS.exe built from a tree that
contains `src/mcp.rs` (an older build does not know `mcp` and opens a window
instead):

```powershell
claude mcp add --scope user --transport stdio proteus -- "$env:USERPROFILE\Desktop\PROTEUS.exe" mcp
```

`--scope user` makes it available in every project; leave it out to register
it for the current project only. Check with `claude mcp list`, remove with
`claude mcp remove proteus`. Nothing in this repository runs that command or
touches Claude's configuration.

Two switches go after `mcp`:

| Switch | Effect |
|---|---|
| `--drive-all` | Also type into and end sessions this server did not spawn (your own windows' sessions). Off by default. |
| `--linger` | Sessions it spawned keep running after the server stops. By default they end with it. |

## The tools

| Tool | What it does |
|---|---|
| `sessions` | What the daemon is running: number, program, size, age, journal file, whether this server may drive it. Sessions it knew that have ended are listed too and can still be read. |
| `spawn {command?, args?, cwd?, cols?, rows?}` | Start a program on a real terminal (default 120 x 32; no command starts your shell). Returns the session number. |
| `send {session, text?, keys?, enter?}` | Type: text as given (a newline presses Enter), then named keys (`Enter`, `Tab`, `Escape`, `Up`, `F5`, `Ctrl+C`, `Alt+x`, ...). Returns the output position at that moment. |
| `wait {session, until: {regex?, quiet_ms?, exit?}, timeout_ms?, cursor?}` | Block until the pattern appears, the output has been quiet that long, or the program exits. Returns `reason`, the match and its line, `elapsed_ms`, a cursor. |
| `read {session, mode?, cursor?, max_bytes?, head?, tail?, lines?}` | Output since the last read. `lean` (default), `plain` (every final line, no escape codes) or `raw` (the bytes). |
| `screen {session, at?}` | The screen as text: rows, size, cursor position, title. With `at`, as it stood that many seconds into the session. |
| `shot {session, at?, scale?, exact?}` | The same as a PNG, drawn by the window's renderer. |
| `kill {session}` | End it. |

The usual loop: `spawn`, `wait` for the prompt, `send` a command with
`enter: true`, `wait` for a pattern or the exit, `read`.

A person can watch a session an agent is driving: `proteus --attach N` opens a
window on session N. Closing that window leaves the session running.

## What lean means, exactly

The bytes are run through the terminal emulator the window uses, and the lines
are read back as they finally stood. So a progress bar redrawn 200 times is
one line, a command line edited key by key is the command that ran, and
nothing depends on guessing at escape codes. Then, in order:

1. Trailing blanks are trimmed; a run of blank lines becomes one.
2. Three or more identical lines in a row become the line and
   `[... N more identical lines ...]`.
3. Four or more lines in a row that are identical once every run of digits is
   replaced by `#` become the first, `[... N similar lines ...]`, and the last.
   Error and warning lines never take part.
4. If more than `head` + `tail` entries remain (20 + 40), the first `head` and
   the last `tail` are kept, and of the rest only error and warning lines, each
   with the line after it when that line is indented further. Each gap becomes
   `[... N lines omitted (a-b) ...]`.
5. An error line contains one of: error, err!, fatal, panic, fail, exception,
   traceback, denied, abort, rejected, conflict, invalid, unexpected, missing,
   not found, not recognized, no such, cannot, could not, unable to,
   segmentation fault, timed out, refused, unresolved, undefined (any case, not
   directly after a letter), or `Error` / `Exception` inside a longer name
   (`ValueError`). A warning line: warn, deprecat, or `Warning` inside a name.
   These lines are never left out and never merged with merely similar ones.
   (What the program itself erased or wrote over before the read is gone, as
   it is on screen: lean reads the screen as it ended.)
6. A line over 1,000 characters keeps its first 750 and last 250 around
   `[... N chars ...]`. In an error or warning line the word that makes it
   one is never in the part left out: if it lies in between, the line keeps
   its first 500, 250 around that word, and its last 250.

Every result starts with one line of JSON: `raw_bytes` (what the call
consumed), `lean_bytes` (what it returned), `saved_pct`, `lines`,
`lines_shown`, `errors`, `warnings`, `cursor`, `more_bytes`, `exited`.

To see what a gap hides: `read {session, cursor: <the result's "from">, mode:
"plain", lines: [a, b]}`. To get everything: `mode: "plain"` or `mode: "raw"`
from the same cursor. A read returns about `max_bytes` (24,000, roughly 6,000
tokens of ordinary text) and leaves the rest for the next call (`more_bytes`
says how much is waiting). Claude Code warns above 10,000 tokens of tool
output and cuts at 25,000 by default (`MAX_MCP_OUTPUT_TOKENS`).

## Safety

- The server may look at any session. It types into and ends only sessions it
  spawned itself, and what proves that is the connection it spawned them on,
  never a session number. `--drive-all` lifts this.
- It never answers a program's questions to its terminal and never resizes a
  session. Its emulators have no channel back to the program.
- Sessions it spawned are marked in the daemon: a plain window launch never
  adopts one, and one that nobody is connected to any more is ended.
- Keyboard input is not journaled, by the server or by the daemon.

## Measured (2026-11-16, this machine; commands in `forge/lanes/organs.json`)

Every figure, with the command that produced it, is in
`forge/lanes/organs.json`: the builder's runs under `measured`, a second
agent's first pass under `measured.review`, and its second pass, on the final
code, under `measured.review2`. In short, for sessions the server spawned
(second pass, final code; the builder's own figure in brackets):

| What | Measured |
|---|---|
| A line printed until `wait` returns it | 0.51 ms median, 1.10 ms worst of 40 (0.44, 1.39) |
| CPU while a `wait` sleeps for 30 s | 0.46 ms in the server, 0.002% of a core (0.42 ms) |
| `send` | 0.11 ms median (0.15) |
| `screen` on 1 MB of history | 12.5 ms the first time, 0.32 ms after (21 and 0.45) |
| `screen` on 20 MB of history | 117 ms the first time, 0.83 ms after (213 and 1.2) |
| `shot` (120 x 32, full of text) | 48 ms, 383 KB; 208 ms the first time (49; 246) |
| Start of a session until its prompt | 3,072 ms (3,062; see Limits) |
| The server leaves after its input closes | 55 to 100 ms, 60 times |

Lean against raw, on output captured here: a compiler run with 25 warnings and
one error 10,457 to 3,696 bytes (64.7% saved); `git log -p` of this repository
3,141,604 to 119,329 bytes in five reads (96.2%; the builder measured 98.0% on
a history a tenth shorter and with half as many lines that say error); a
progress bar redrawn 200 times 15,209 to 93 bytes (99.4%). On captures a
script of the review made: a compiler run with 42 warning and 3 error lines
16,114 to 5,235 bytes (67.5%), and a 400-step progress bar with two warnings
in between 38,846 to 179 bytes (99.5%), both warnings kept.

## Limits to know

- A new session prints nothing for about 3 seconds: the console host waits
  that long for a terminal to answer its start-up questions, and by the rule
  above nothing answers. Wait for the prompt with a regex, not for quiet.
- A `wait` on a session this server did not spawn looks at its journal four
  times a second, so it can lag up to a quarter of a second, and with a daemon
  older than this build the journal itself can lag further.
- `proteus --attach` has not been run with a real window in this lane (the
  lane was built while a person was typing on the machine).
- With `--drive-all`, the server stays attached to a session once it has
  typed into it, and the daemon counts it as a window. A session of yours that
  has no window is then not picked up by your next plain launch (which starts
  a new shell) until the server has left; `proteus --attach N` reaches it.
- A tool argument of the wrong type, or with a name the tool does not know, is
  refused with its name. (`5.0` for 5, one key as a string, and `null` for
  "not given" are accepted.) A message over 64 MB is refused.
- A read that stops early stops where a line ends, so its cursor can be given
  to any later read, by this server or another. It runs up to 8 KB past
  `max_bytes` to get there.
- One lean read holds every line since its cursor in memory: reading 52 MB in
  one call took 1.8 s (556 MB at its peak, in the first pass). Page with
  `max_bytes` in plain mode, or wait for a pattern, when the output is that
  large.
- A `wait` never returns before its timeout. It returns up to 16 ms after it
  on a quiet machine, and later when the machine stalls.
- No pattern holds a `wait` past its timeout by more than one search of its
  window: 81 ms for the slowest pattern found, 380 ms measured for a 300 ms
  timeout. The window is set by how large the pattern compiles. An ordinary
  pattern is searched 65,536 characters back. A heavy one (counted repeats of
  `\w` and the like) reaches less, down to 51 characters for the heaviest
  that compile; a match longer than its reach can be missed, and the answer
  then carries `pattern_reach` and says how to make the pattern lighter
  (`(?-u:\w)`, smaller counts).
- Lean keeps an error line and the one more-indented line under it. What a
  compiler says under that (what was expected, the help) may be in the gap:
  ask for it with `lines`.
- Calling `screen` or `read` in a loop on a session this server did not spawn
  costs that session something: 1,550 screens a second slowed a flood from
  2.8 to 2.0 MB/s. One `wait` did not.

## Checking it

```powershell
cargo test --release --test mcp                                                          # nine end-to-end runs over real pipes
cargo test --release --test mcp -- --ignored --nocapture --test-threads=1 measure        # the measurements
```

Both use a data folder, a socket and a daemon of their own under `%TEMP%`, so
they never touch your sessions, and each test ends its own daemon when it is
done. The scripts a second agent attacked the server with are in
`forge/attack/organs` (the list, and how to run them, is at the top of
`mcpc.py`); they are isolated the same way. To try the server by hand the same way, set
`PROTEUS_DATA` and `PROTEUS_SOCKET` to a fresh folder first.

Protocol: MCP revisions 2024-11-05 to 2025-11-25 (the `initialize` handshake)
and 2026-07-28 (stateless: `server/discover`, version and capabilities in each
request's `_meta`). Tools only: no resources, prompts or sampling.
