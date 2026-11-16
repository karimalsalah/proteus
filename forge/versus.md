# VERSUS: PROTEUS against the terminals on this machine

Status: **provisional, not publishable.** Four things stand in the way, all measured:

1. **Load.** Every run so far was taken while about ten other agents were building on the same machine.
   Machine-wide CPU was 69-88% busy where recorded. The first campaign (`forge/out/versus/full1`) also ran
   with free RAM falling to 678 MB and was stopped part-way.
2. **Throughput moves with conditions nobody controls here.** The same PROTEUS exe read 1.7-3.8 MB/s in
   full1 (19:31) and 14.4-26.0 MB/s 14 minutes later (19:45). At 20:23 every terminal read about 4-10x
   higher again at once (section E), with the machine busier (87%), the session unlocked and recent input.
   The cause is not known. Only figures from one interleaved campaign are compared with each other.
3. **The benchmarked windows were covered.** The runner samples a 9x9 grid over each terminal's window. In
   every idle run the window was fully covered (uncovered share 0.0) except while it held the foreground.
   The same is assumed of the throughput runs (sampling was added after them). A covered window may draw
   differently from a visible one.
4. **Test windows took the foreground.** In 5 of 18 idle runs (`idle`), and again in 4 of 18 after a guard
   that marks the windows `WS_EX_NOACTIVATE` within 200 ms (`idle2`), a process in the test job owned the
   foreground window. That includes PROTEUS with `PROTEUS_NO_FOCUS=1`. While that happens, a person's
   keystrokes can land in a test window. Such runs are dropped from the medians, but the risk is to the
   person. **Run VERSUS only when nobody is typing on the machine**, and with the test windows visible.

When those hold, the final table is:

```powershell
pwsh forge\versus.ps1 -Stages          # opens windows (see "Windows"); duration not yet timed for all rows
```

## Results at a glance

Raw data: `forge/versus/data/<campaign>.json`. Every run's workload and runner record is there, with local
paths replaced by `%WORKTREE%`, `%LOCALAPPDATA%`, `%TEMP%` and `%USERPROFILE%`. Each record carries its own
conditions: machine CPU busy %, least free RAM, whether the terminal took the foreground and, from E on, the
window's uncovered share. All PROTEUS runs are 0.5.4 from source commit 1ce14cd: `proteus-versus-20261116-191139.exe`
in A-D and the gate's copy `proteus-20261116-201918.exe` in E-F, built from the same source.

| campaign | when (2026-11-16) | runs | machine CPU busy | least free RAM | windows |
|---|---|---|---|---|---|
| D `full1` | 19:28-19:31+, stopped part-way | 3 (ascii), 1-2 (sgr) | not recorded (heavy builds) | fell to 678 MB | not recorded |
| B `stage-proteus` | 19:45:15-19:45:59 | 3 | 69-70% | 3.8 GB | not recorded |
| C `sink-bufsize` | 19:44:34-19:44:55 | 3 per buffer | 28% at start | 4.1 GB | none (no window) |
| A `ascii-stage` | 19:46:22-19:49:51 | 3 | 72-74% | 3.2 GB | not recorded |
| E `hop9` | 20:22:54-20:24:22 | 9 | 87-88% | 4.6 GB | not recorded |
| `idle` | 20:25-20:34 | 3 (2 kept) | 71-88% | 1.9 GB | covered |
| F `idle2` | 20:36-20:45 | 3 (1-3 kept) | 65-88% | 15 MB | covered except while foreground |

In `idle2`, free RAM fell to 15 MB during conhost's third run, and below 1 GB in PROTEUS run 3, WezTerm run 3
and conhost run 2. Working sets under that pressure stayed within 2% of the same terminal's other runs
(PROTEUS 197-204 MB, WT 146-148 in every run), except that conhost run (22.5 against 25.1-25.2 MB).

**Losses, each holding inside one interleaved campaign**

| loss | PROTEUS | best other | where | condition |
|---|---|---|---|---|
| peak tree memory, 16 MiB flood | 361 MB / 347 MB | WT 154 / 156 MB | A / E | 72-74% / 87-88% busy |
| tree memory at idle | 200 MB | conhost 22, WT 146, Ghostty 152 MB | F | covered windows |
| payload CPU per 16 MiB vs Ghostty | 0.67 s / 0.25 s | 0.22 s / 0.11 s | A / E | as above |
| launch to first write vs Ghostty | 1318 ms / 818 ms | 774 ms / 309 ms | A / E | cold PROTEUS daemon every run |

**Not established (measured, but within spread, inconsistent between campaigns, or not measured)**

| question | what was measured | why not established |
|---|---|---|
| throughput rank | A: PROTEUS 11.9, Ghostty 21.9, WT 6.8, WezTerm 5.9 MB/s; E: PROTEUS 90.7, Ghostty 91.3, WT 82.2 | ranges overlap; levels moved 4-10x between campaigns for an unknown reason |
| PROTEUS against its own ConPTY | A: 11.9 against ConPTY alone 21.2 MB/s; B: 21.3 | B, one minute before A, reaches the ceiling |
| daemon hop CPU | B: 0.42 vs 0.23 s (`--local`); E: 0.25 vs 0.28 s | reverses between 3 and 9 runs |
| CPU against WT | A: 0.67 vs 0.33 s; E: 0.25 vs 0.19 s | ranges overlap in E |
| launch to first write against WT | A: 1318 vs 1408 ms; E: 818 vs 744 ms | reverses between campaigns |
| idle CPU | F: PROTEUS 0.000 s over 20 s (1 kept run), WT Preview 0.000, WezTerm 0.000, WT 0.016, conhost 0.016, Ghostty 0.062 | 2 of 3 PROTEUS runs dropped (took foreground); CPU clock resolution 15.6 ms |
| SGR, Unicode, cargo, TUI, drip | one run each on the loaded machine (D and the subset check) | not run three times under one set of conditions |
| WT Preview, conhost throughput | D only | not in an interleaved campaign under the current harness |
| key-to-screen latency | method floor 18.2-31.8 ms on an instant window | floor spread exceeds one refresh; two terminals ignore posted keys; SendInput impossible while a person types |
| visible-window throughput | nothing | every run so far had covered windows or did not record it |

## Machine and versions

| | |
|---|---|
| CPU | Intel Core Ultra 9 285H, 16 logical processors (`[Environment]::ProcessorCount`, recorded in results.json) |
| GPU | Intel Arc 140T, driver 32.0.101.9175 |
| Display | 2880x1800 at 120 Hz |
| OS | Windows 11 Pro for Workstations Insider Preview, build 26340 |
| PROTEUS | 0.5.4 at commit 1ce14cd (`dist\proteus-versus-*.exe`), ConPTY/OpenConsole 1.24.2607 bundled |
| Windows Terminal | 1.25.3131.0 (OpenConsole inside the package) |
| Windows Terminal Preview | 1.26.3133.0 |
| WezTerm | 20261112-061903-ce8e6b14, bundles OpenConsole 1.22.2502 |
| Ghostty for Windows | 1.3.2-win.4 (ReleaseFast), bundles OpenConsole 1.25.2607 |
| conhost | inbox, build 26340 |

`versus.ps1` records these itself into `results.json`, together with the machine's CPU busy % and least free
RAM during every run.

## Method

**Workloads** (`forge/versus`, `versus emit`): a console program that the terminal runs as its program. It
prepares its whole payload before starting the clock, writes it to stdout with `WriteFile` in 64 KiB chunks
(UTF-8, code page 65001, VT processing on), and times its own writes with QueryPerformanceCounter. When a
terminal consumes slowly, its console host stops taking bytes and the writes block, so the time the writer
measures is the throughput the terminal imposes. No terminal is asked to report anything.

| workload | what it writes |
|---|---|
| ascii | 16 MiB of 91-byte plain lines |
| sgr | 16 MiB of lines with 16, 256 and 24-bit colour, bold, dim, underline, inverse badges |
| unicode | 16 MiB of lines with 2/3/4-byte UTF-8, wide CJK, emoji, box drawing, blocks |
| cargo | a real `cargo build --release --color always` log of this repository (`forge/samples/cargo-build.log`, 54 KB, progress bar redraws included, recorded with `CARGO_TERM_PROGRESS_WHEN=always`), replayed to 8 MiB |
| tui | 600 full-screen frames in the alternate screen: every row addressed with CUP, every cell rewritten with 256-colour fg/bg changes every 8 cells; reported as frames per second |
| drip | 500 lines, one every 10 ms on an absolute schedule; reports how long each write blocked (p50/p99/max) and lateness |

For floods it reports two rates: over the whole payload, and over the **second half** only. The second
figure is the sustained one. The first can be inflated by however much a terminal swallows into its own
buffers at the start. This was found the hard way: a 4 MB smoke run read 62 MB/s for Ghostty, while 16 MB
runs read 4.7 to 16 MB/s. Tables cite the second-half rate.

**Harness** (`versus run`): every terminal is started inside a fresh job object (CreateProcess suspended,
assigned, resumed; kill-on-close). The job's accounting gives CPU seconds of the whole process tree,
including processes that have already exited. The working set of every process in the job is summed every
20 ms and the peak is kept. At the end only the job is terminated, so nothing outside it is touched and
nothing is stopped by name. The workload checks whether it was born inside the job (`IsProcessInJob` on the
named job). If it was, its own CPU is subtracted, so the CPU column is the terminal's (window, renderer,
console host, daemon) and not the writer's. Windows Terminal starts its child outside the caller's job, so
its tree has nothing to subtract.

**Launch to first write**: QPC just before CreateProcess of the terminal, to the return of the workload's
first write. It covers the terminal's start-up, console host and child start-up. It is not first pixel.

**Idle**: the terminal with `cmd.exe` at its prompt; 10 s to settle, then job CPU and summed working set over
20 s.

**Fairness choices**
- Same grid for everyone: 123x30, the grid PROTEUS opens at by default. WT `--size`, WezTerm
  `initial_cols/rows`, Ghostty `--window-width/height`. conhost has no command-line size and runs 120x30.
- Default settings where the terminal allows: WezTerm `-n` (no config file), Ghostty
  `--config-default-files=false`, PROTEUS with a fresh `PROTEUS_DATA`. Windows Terminal has no switch to
  ignore its settings file. It runs with the owner's settings: acrylic, opacity 88, JetBrainsMono Nerd
  Font 12, Atlas engine.
- One warm-up run per terminal before measuring (discarded), so no terminal pays first-run unpacking. PROTEUS
  unpacks its runtime on first use: launch to first write was 14.1 s cold and 1.9 s warm in the smoke runs.
- Interleaved: run n of every terminal for a workload before run n+1, with the order rotated each round,
  so a change in machine load falls on all of them.
- PROTEUS gets a fresh daemon socket per run, so its daemon starts inside the job and is counted and stopped.
  So every PROTEUS launch here starts its daemon cold, which in normal use happens once per login.
  `PROTEUS --local` (session in the window process) and `PROTEUS --no-journal` are extra rows that put
  costs on a stage.
- **Payload CPU** is the job's CPU between the workload's first payload write (a marker file) and its
  report, less the workload's own: it covers no start-up, no shut-down and no harness grace period.
  **Launch-to-close CPU** includes all three; the loaded first campaign (D below) has only that.
- A run is dropped from the medians if a process in the job ever owned the foreground window (it took
  the keyboard), or, for Windows Terminal, if no `WindowsTerminal.exe` ever entered the job (the tab went to a
  WT window outside it), or if the session was locked at any look. The runner also marks every window of
  the job `WS_EX_NOACTIVATE` and records the least and greatest uncovered share of its largest window.
- Raw records for every section below are in untracked `forge/out/versus/<campaign>/` (results.json,
  table.md and one folder per run); `-Summarize -Out <folder>` rebuilds a table from them.
- An even number of runs reports the lower middle value as the median.

**Windows**: windows open on screen while the benchmark runs. They are started with `SW_SHOWNOACTIVATE`, and
PROTEUS with `PROTEUS_NO_FOCUS=1`, so none takes the keyboard. None is minimised, because a minimised
terminal may skip drawing, which would flatter it.

## Provisional results

### A. Back to back, 16 MiB ASCII (`forge/out/versus/ascii-stage`, 19:46-19:50, machine 72-74% busy, at least 3.2 GB free)

```powershell
pwsh forge\versus.ps1 -Terminals proteus,ghostty,wt,wezterm -Workloads ascii -SkipIdle -Stages
```

Median (min-max) of 3 runs.

| ASCII 16 MiB | PROTEUS | Ghostty | WT 1.25 | WezTerm |
|---|---|---|---|---|
| MB/s, second half | 11.88 (9.37-13.10) | 21.86 (5.36-42.85) | 6.84 (4.71-7.97) | 5.85 (4.81-12.06) |
| MB/s, whole | 13.01 (8.23-14.23) | 28.18 (5.75-67.72) | 8.27 (5.87-8.68) | 5.15 (4.97-13.72) |
| payload CPU s | 0.67 (0.48-0.91) | 0.22 (0.06-0.69) | 0.33 (0.22-0.39) | 1.20 (1.02-1.52) |
| launch-to-close CPU s | 1.28 (0.95-1.36) | 0.52 (0.44-0.97) | 1.05 (0.95-1.08) | 1.80 (1.56-2.00) |
| peak tree working set MB | 361 (360-362) | 228 (227-229) | 154 (154-154) | 294 (287-294) |
| launch to first write ms | 1318 (1225-1589) | 774 (625-1430) | 1408 (1162-1734) | 1095 (715-1507) |

The console host alone, measured right after with the same command (`-Stages`), MB/s over the second half:

| ConPTY with a null reader | ascii | sgr | unicode |
|---|---|---|---|
| PROTEUS's bundled 1.24.2607 | 21.17 (17.88-49.69) | 15.70 (13.20-29.01) | 33.20 (15.21-42.64) |
| WezTerm's bundled 1.22.2502 | 27.13 (15.98-36.86) | 16.37 (15.27-17.34) | 23.65 (11.72-30.64) |
| Ghostty's bundled 1.25.2607 | 16.24 (15.70-27.71) | 15.02 (9.99-15.07) | 8.82 (4.37-14.34) |
| inbox (kernel32) | 8.05 (7.92-8.18) | 6.44 (4.93-6.74) | 6.26 (3.16-8.63) |

### B. PROTEUS by stage, 16 MiB ASCII (`forge/out/versus/stage-proteus`, 19:45-19:46, machine 69-70% busy)

```powershell
pwsh forge\versus.ps1 -Terminals proteus,proteus-local,proteus-nojournal -Workloads ascii -SkipIdle
```

| | default (daemon) | --local (no daemon) | --no-journal (no daemon, no journal) |
|---|---|---|---|
| MB/s, second half | 21.30 (12.86-25.49) | 22.23 (9.14-52.42) | 26.67 (9.32-27.45) |
| payload CPU s | 0.42 (0.42-0.64) | 0.23 (0.17-0.38) | 0.34 (0.31-0.48) |
| peak tree working set MB | 362 (362-363) | 352 (350-358) | 348 (348-381) |
| launch to first write ms | 1186 (1097-1317) | 969 (731-1360) | 804 (794-1085) |

### C. Pipe read size (ConPTY 1.24 alone, 16 MiB ASCII, 3 runs each, interleaved, 4.1 GB free, 28% CPU)

`versus sink --conpty vendor\conpty --buf N ...` with a synchronous `ReadFile`: 64 KiB carried 19.5, 19.6
and 35.5 MB/s; 128 KiB carried 18.2, 19.9 and 18.8 MB/s. ConPTY hands over at most about 64 KB per read
(average 63,551 B with a 64 KiB buffer, 64,281 B with 128 KiB), so a larger buffer cannot help.

### D. The loaded first campaign (`full1`), kept for the record only

| ASCII 16 MiB | PROTEUS | WT 1.25 | WT 1.26 | WezTerm | Ghostty | conhost |
|---|---|---|---|---|---|---|
| MB/s, whole | 3.45 (1.66-3.81) | 3.41 (2.35-3.69) | 4.50 (3.95-5.14) | 3.88 (2.72-5.66) | 8.10 (4.65-16.18) | 1.33 (1.31-1.48) |
| launch-to-close CPU s | 1.44 (1.31-1.52) | 1.22 (1.11-1.25) | 1.06 (1.06-1.30) | 2.42 (1.81-2.78) | 0.55 (0.52-0.62) | 1.89 (1.86-2.02) |
| peak MB | 361 (360-361) | 154 | 157 (157-158) | 294 (294-296) | 228 | 43 |
| launch to first write ms | 2799 (2440-3808) | 2973 (1881-3185) | 1935 (1815-1938) | 1333 (1324-1840) | 874 (692-992) | 291 (202-374) |

`pwsh forge\versus.ps1 -Summarize -Out forge\out\versus\full1 -Terminals proteus,wt,wtp,wezterm,ghostty,conhost -Workloads ascii,sgr`
rebuilds this table from the run folders. SGR, cargo, TUI and drip have no comparable three-run figures
yet. The one-run subset check gave cargo replay PROTEUS 2.66 and Ghostty 3.15 MB/s, TUI PROTEUS 28.0
and Ghostty 30.9 fps, and drip write p99 PROTEUS 37.0 and Ghostty 36.0 ms, all on the loaded machine.

### E. Nine runs interleaved, 16 MiB ASCII (`forge/out/versus/hop9`, 20:22:54-20:24:22, machine 87-88% busy)

```powershell
pwsh forge\versus.ps1 -Runs 9 -Terminals proteus,proteus-local,wt,ghostty -Workloads ascii -SkipIdle
```

Every terminal read 4-10x higher than in A, 35 minutes earlier; see Status, point 2. No run was dropped.

| ASCII 16 MiB, 9 runs | PROTEUS | PROTEUS --local | WT 1.25 | Ghostty |
|---|---|---|---|---|
| MB/s, second half | 90.7 (34.9-171.6) | 108.3 (16.3-207.7) | 82.2 (21.1-217.3) | 91.3 (45.3-251.8) |
| payload CPU s | 0.25 (0.13-0.42) | 0.28 (0.16-0.61) | 0.19 (0.11-0.31) | 0.11 (0.03-0.20) |
| peak tree working set MB | 347 (334-371) | 353 (334-358) | 156 (154-158) | 226 (194-247) |
| launch to first write ms | 818 (615-1593) | 681 (496-873) | 744 (586-1941) | 309 (266-652) |

### F. Idle at a `cmd.exe` prompt, 20 s after 10 s to settle (`forge/out/versus/idle2`, 20:36, windows guarded)

```powershell
pwsh forge\versus.ps1 -SkipWorkloads -Terminals proteus,wt,wtp,wezterm,ghostty,conhost
```

Runs whose terminal took the foreground are dropped, which leaves 1 run for PROTEUS and conhost and 3 for
the others. The earlier `idle` campaign (5 s settle, no guard) read the same within the 15.6 ms CPU clock.
Its idle CPU over 20 s: PROTEUS 0.000, WT 0.000, WT Preview 0.000, WezTerm 0.078, Ghostty 0.078,
conhost 0.000 s. Its working set: 197, 146, 149, 240, 151 and 23 MB.

| idle | PROTEUS | WT 1.25 | WT 1.26 | WezTerm | Ghostty | conhost |
|---|---|---|---|---|---|---|
| job CPU s over 20 s | 0.000 (1 run) | 0.016 (0.000-0.047) | 0.000 (0.000-0.000) | 0.000 (0.000-0.031) | 0.062 (0.047-0.141) | 0.016 (1 run) |
| working set MB | 200 | 146 | 149 | 248 (241-255) | 152 (151-153) | 22 |
| runs dropped (took the foreground) | 2 of 3 | 0 | 0 | 0 | 0 | 2 of 3 |

## Where PROTEUS loses, provisionally, and why

Only a gap that holds within one interleaved campaign is listed as a loss.

1. **Peak memory, in every campaign.** During a 16 MiB flood: PROTEUS 347-361 MB, against WT 154-156,
   Ghostty 226-228 and WezTerm 294. At idle: PROTEUS 197-200 MB, against WT 146, Ghostty 151-152, WezTerm
   240-248 and conhost 22-23. The window grows from 199-212 MB after a 1 MB run to 312 MB after 16 MiB,
   with or without the daemon, so the flood growth follows output volume. PROTEUS keeps 50,000 lines of
   scrollback (`src/term.rs:169`), and 16 MiB of 91-byte lines is 184k lines, so the history fills. My
   estimate is 50,000 × 123 cells × an alacritty cell of about 24 bytes ≈ 148 MB, which matches the ~107 MB
   added; the cell size was not measured. That is a retention choice, not a leak. The other terminals'
   scrollback defaults were not read here, so this row compares different amounts of retained history.
   The ~50 MB idle gap to WT and Ghostty is not broken down.
2. **CPU per byte against Ghostty, in A and E.** Payload CPU for 16 MiB: PROTEUS 0.67 against Ghostty 0.22
   (A), and 0.25 against 0.11 (E). Against WT, PROTEUS is higher in both (0.67 vs 0.33; 0.25 vs 0.19), but the
   ranges overlap in E. **Where the cost sits is not shown.** In B (3 runs) the daemon path cost 0.42 s
   against 0.23 in-process. In E (9 runs) it cost 0.25 against 0.28, so the daemon hop is not separated from
   run-to-run spread. Copies on that path are listed in `forge/lanes/versus.json` as candidates to measure,
   not as a finding.
3. **Launch to first write, against Ghostty, in A and E.** PROTEUS 1318 against Ghostty 774 ms (A), and 818
   against 309 (E). Against WT, PROTEUS is ahead in A (1318 vs 1408) and behind in E (818 vs 744), so that is
   not a result. Every PROTEUS run here starts its daemon cold (fresh socket per run); in-process
   (`--local`) is 681-969 ms. Not broken into stages yet; proteus.log has one-second resolution.
4. **Throughput: no loss established.** In A, PROTEUS sustained 11.9 MB/s against Ghostty 21.9 and its own
   ConPTY alone 21.2. In B, one minute earlier, PROTEUS read 21.3; in E it read 90.7 against Ghostty 91.3.
   Every throughput range overlaps another terminal's. The pipe read size is not a cause (C).

## Latency: not published

`versus latency` measures key to compositor frame without a camera. It starts the terminal with
`versus echo` inside (raw console input; each key repaints a 60x3 block with the cursor hidden), captures the
window with Windows Graphics Capture, posts the key to the window, and times the first captured frame whose
pixels differ from the frame before the key, using the frame's SystemRelativeTime (QPC).

It is left out of the table for three measured reasons:
- **SendInput cannot be used.** It types into the foreground window, and a person is typing on this machine.
  Keys are posted instead (WM_KEYDOWN, then WM_KEYUP 40 ms later). **Ghostty and WT Preview did not respond
  to posted keys** (0 of 15), so two terminals cannot be measured at all.
- **The method's own floor is wider than a refresh.** `versus calib` measures a window of the tool's own
  that repaints inside its WM_KEYDOWN handler. It reads 18.2 to 31.8 ms (p50 23.6, 30 keys) where about one
  8.3 ms refresh is expected. The spread (13.6 ms) exceeds the roadmap's kill line of one refresh.
- **The terminal figures do not agree with PROTEUS's own counter.** One run of 15 keys per terminal read
  far above PROTEUS's own key-to-present counter (6 to 8 ms, ledger). The gap is not explained. Two
  untested suspects are PROTEUS's glyph fade-in crossing the pixel threshold late (try `--no-reveal`) and
  the load on the machine. The per-terminal figures are not reported for that reason.

Before this can be published, it needs a quiet machine, a calibration within one refresh, and posted-key
support or another input path for all six terminals.

## Caveats

- Self-timed throughput ends when the console host has accepted the last byte. A terminal may still be
  parsing or drawing what it holds. The second-half rate limits how much a large input buffer can flatter it,
  as long as that buffer is smaller than half the payload (8 MiB).
- Windows Terminal runs with the owner's settings (above); the others with defaults.
- The terminals bundle different ConPTY builds (1.22, 1.24, 1.25), and conhost is the inbox host. The stage
  table (`-Stages`) measures each one alone.
- Tree CPU includes the console host (OpenConsole/conhost) and, for PROTEUS, its daemon. WezTerm's tree also
  includes the `wsl.exe` probes WezTerm makes at start-up.
- conhost's grid is 120x30, not 123x30.
- Peak working set is sampled every 20 ms, so a shorter peak can be missed.
- One machine, one display, DX12 on an integrated GPU.
