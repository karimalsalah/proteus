# Roadmap

Ranked by value over effort, dependencies respected. Every item says how it will be judged: the number that proves it worked, and the reading that means drop it. Compiled 2026-11-16 from four research sweeps (agent pain points, Windows capabilities, terminal engines, experience design) against primary sources. Tags: [S] checked in the repository or a source opened by the synthesiser; [R] verified by a researcher in a source they opened; [U] unverified.

**In one line.** Build the command as an object first (injected shell marks through one pre-parser OSC scanner), put OSC 7501 on the same scanner, then route agent commands into PROTEUS sessions: every other gain for agents and for the driver depends on those three, and their one shared untested leg is whether the vendored ConPTY 1.24 delivers marks in place and replies in order. Nothing already in this push takes a slot; it appears only as 'in-flight' dependencies and in the corrections as what it should become. Tags used throughout: [S] checked by the synthesiser today in the repository or in a source opened today; [R] verified by a researcher in a source they opened, not re-opened by me; [U] unverified or from memory. Nothing was edited, built or run in the repository.

## Items

### 1. Command boundaries as journal records: one pre-parser OSC scanner plus shell marks injected by PROTEUS

*Effort and evidence: M, strong*

**Why.** There is no command object today: src has no OSC 133/633 handling and src/journal.rs marks K_MARK as dead code [S]; vte 0.15.0 discards OSC 7, 9 and 133 [R]. All four researchers independently named this the missing primitive and the ledger already says sound.tune 'needs command boundaries'. Microsoft's own PowerShell snippet never emits 133;C or the command text and cmd.exe cannot report an exit code [R], so marks must be injected and checked, not assumed. Receipts, per-command cost, outcome sound, estimates, the unseen digest, lean-read ranges and undo-by-command all depend on it.

**Build.** (a) Day one, a probe: a child prints OSC 133 A/B/C/D, 633;E, 9;9 and 7501 at known offsets through the bundled ConPTY 1.24 and through 1.25, and the journal bytes are compared. (b) A streaming OSC scanner in the pty-read thread of src/session.rs, ahead of the parser, tolerant of sequences split across reads. It writes K_MARK records (kind, journal byte offset, time, exit status, cwd, command text) and a sidecar index so 'output of command N' is one seek. (c) Integration injected at spawn without touching the user's profile: wrap the existing pwsh prompt function (the stash-and-wrap pattern Microsoft documents for oh-my-posh) to emit D;status, A, 9;9;cwd and B, hook PSReadLine for C and the command line, and pass a per-session nonce by environment variable so printed fake marks are ignored. cmd gets the PROMPT form, labelled 'no exit status'. (d) Command text comes from the shell's own report, never from keystrokes (keeps the no-keys-journaled rule). (e) The scanner answers queries on the live path only, once per session, in the daemon.

**Success metric.** Scripted run of 1,000 commands in pwsh: >= 99% carry command text, exit status and an output byte range; byte-offset error against ground truth 0; added time per prompt < 1 ms; forged marks accepted 0 of 10,000; flood MB/s within 1% with the scanner on; idle still 0.00% of a core.

**Kill criterion.** If the probe shows marks displaced or dropped on BOTH ConPTY 1.24 and 1.25 so that fewer than 95% of the 1,000 commands get an exact output range, stop treating in-band marks as the source of truth: carry marks out of band (a pipe from the injected prompt function to the daemon, matched to the journal by time) and downgrade every dependent to 'approximate range'. If the wrapped prompt adds more than 5 ms at p95 with a prompt framework installed, injection becomes opt-in.

### 2. Agent state with zero setup: OSC 7501 (plus 9;4, 9/99/777 and mode 2031) on the same scanner, feeding one attention queue

*Effort and evidence: M (parser, reply and table are S; the queue surface makes it M), strong*

**Why.** Claude Code v2.1.295 and later reports working, waiting and done with 'nothing to turn on', but only to a terminal that answers the query [S: docs page opened today]. The spec is a draft, revision 0.4 dated 2026-10-10; the reply to 'OSC 7501 ; ? ST' must start with '?' [S: spec opened today]. PROTEUS's parser drops the sequence, so the query goes unanswered and no report is ever sent [R]. Three researchers found this independently. A third-party tracker lists 7 of 24 terminals done and none on Windows displaying the records (absence not proven) [R]. It replaces most of ledger agents.detect and agents.channel for state.

**Build.** In the daemon-side scanner: answer the query once per live session ahead of the parser's DA1 reply (never on replay, never from two windows); keep the record table by the spec (256 records, 4096 bytes per sequence, refuse msg/title with control characters, evict least recently updated); drop working/blocked on real process exit (in-flight job object) and at OSC 133 A; keep done/error until visited; journal each record as its own kind so state is seekable. Surface: 'proteus sessions' shows state, kind, message and age; one key jumps to the oldest blocked session; msg and title are shown as untrusted text with their session named. Same scanner: OSC 9;4 progress to the taskbar and field, OSC 9/99/777 as a rate-limited fallback, CSI ? 996 n and mode 2031 theme reports. Second channel only for detail 7501 lacks (tool names): a loopback endpoint for Claude Code 'http' hooks, with hook configuration written only by an explicit user action that shows exactly what is written.

**Success metric.** Count of OSC 7501 reports in the journal of a Claude Code >= 2.1.295 session above 0 (must be 0 today); transitions shown / transitions emitted 100%; state visible within one frame (8.3 ms at 120 Hz) of its journal timestamp; stale 'blocked' records after program exit 0; frames drawn while state is unchanged 0; with four agents, median blocked-to-attended latency under 30 s.

**Kill criterion.** If the reply is confirmed written but Claude Code still sends 0 reports on both ConPTY 1.24 and 1.25 (reply swallowed or arriving after DA1 on the input side), drop in-band 7501 as the state source and use Notification hooks (permission_prompt, idle_prompt, agent_needs_input, agent_completed) instead. If the draft changes incompatibly before 1.0, re-read the spec before fixing code.

**Depends on.** 1: Command boundaries (shared scanner, OSC 133 A lifetime rule); in-flight: process senses (exact process exit)

### 3. Instruments before claims: staged flood bench with a native emitter, time blocked in acquire, key-to-vblank, image pass-through probe

*Effort and evidence: S, strong*

**Why.** The flood figure is produced by typing 'cmd /c type <file>' (src/app.rs line 704 [S]), so it measures cmd and the console API as much as ConPTY, and there is no passthrough flag to investigate [R]. redraw locks the terminal and builds the grid (line 771) before get_current_texture (line 787) [S], while wgpu-hal's DX12 backend waits on the frame-latency waitable inside acquire by default [R]; the size of that wait is unknown. The typing figure ends at the present call, not the display. Three researchers converge: measure the stages before spending effort on throughput, pacing, HDR or images.

**Build.** An emitter mode in the same exe (ASCII, SGR-heavy, Unicode; 64 KB WriteFile and small WriteConsoleW) that times its own writes. Report MB/s for emitter to pipe, emitter through ConPTY to a null sink, and the full path to a rendered frame; in-process and through the daemon; ConPTY 1.24 against 1.25. Per-frame ms blocked in get_current_texture (p50, p95, max) in stats.json. key_to_vblank from the swap chain's frame statistics, with the monitor count recorded because the API is documented as unreliable with several monitors. Typing p99.9 while another session floods. One sixel and one kitty APC image through the bundled host, bytes in against bytes out. Batch the journal write and socket flush per read to recover the daemon hop.

**Success metric.** bench.json names the limiting stage with a number (at least four figures in place of one); acquire wait p50/p95/max recorded; key_to_vblank p50/p95 recorded; daemon path within 5% of in-process (about 25% slower today); key-to-present p99.9 under 20 ms while another session floods; image probe recorded as byte equality for sixel and for APC.

**Kill criterion.** Drop any instrument whose spread between two back-to-back runs is larger than the effect it is meant to detect (flood spread above the 25% daemon gap; vblank spread above one refresh), or that moves idle off 0.00% or typing p50 by more than 0.5 ms; publish nothing rather than noise. Follow-on work dies on these readings: p95 blocked in acquire under 1 ms means no pacing change; native emitter through ConPTY at 25 MB/s or more means close conpty.throughput as not ConPTY-bound; echo frames landing on the first vblank in >= 95% of cases means no present-path work.

### 4. Real-program correctness gate: environment-scrub allowlist and recorded Claude Code sessions now, differential VT oracle later

*Effort and evidence: S for stage 1, M for stage 2, strong*

**Why.** SCRUB_PREFIXES strips every variable starting with CLAUDE_CODE_ (src/session.rs lines 66-69 [S]), which removes documented user switches such as CLAUDE_CODE_FORCE_SYNC_OUTPUT, CLAUDE_CODE_NO_FLICKER and CLAUDE_CODE_BS_AS_CTRL_BACKSPACE [S: docs page]. The ledger's 'renders correctly' rests on an idle prompt with nothing submitted; Claude Code's docs say ConPTY-backed hosts leave stale fragments under its fullscreen renderer, the default for newer users [R]; vt.sync, scroll.partial and vim are untested by the ledger's own account [S]. The rank is earned by stage 1 only.

**Build.** Stage 1 (S, do now): replace the prefix scrub with the exact names that identify a parent session plus a pass-through allowlist of user-facing variables. Record journals of real Claude Code sessions in both renderers during normal work (no extra usage), add them to forge/samples, and have the gate replay them: stale cells against the final transcript, count of ESC[?2026h, no partial frame, frames presented during a scroll storm at or below the refresh rate. Add a five-point zero-configuration checklist (Shift+Enter newline, no half-drawn frame, finish notification, OSC 7501 status, theme auto-detected). Stage 2 (M, waits behind every S and M item below it): feed every journal and sample through alacritty_terminal and libghostty-vt 1.0.1 with no pty and no GPU, diff the grids cell by cell, time both parsers; use the prebuilt x86_64 DLL embedded through src/runtime.rs with a hand-bound C header and the release signature checked at vendoring; this is the 'measured comparison' the ledger's decision asks for, not a core swap.

**Success metric.** Stale cells after replay 0 across at least three recorded sessions of 100 turns or more; ESC[?2026h count above 0 per session; partial frames 0; checklist 5 of 5 with no user configuration. Stage 2: cells differing per million and parser MB/s for both cores written to the ledger, with the exe size delta against 16.9 MB.

**Kill criterion.** Drop the recorded-session step if replaying the same journal twice gives different final grids (a non-deterministic test proves nothing) or if it adds more than 60 s to the gate (move it to a long gate). If fragments persist on ConPTY 1.25 and with CLAUDE_CODE_ALT_SCREEN_FULL_REPAINT=1, stop renderer work, set the variable for sessions and report upstream with the journal. Stage 2: if the oracle finds 0 differing cells per million over the corpus including a vim sample, or would add more than 2 MB to the exe, keep it as an offline test and do not ship it.

### 5. Routing: agent commands run inside PROTEUS sessions through one consented hook, and the command id becomes the unit of the API

*Effort and evidence: M, moderate*

**Why.** A terminal does not see an agent's commands: Claude Code runs each tool command in a separate process and streams its output to a working file, so a pane running it journals only the interface it draws [R]. Every pty-based feature in this push (wait, lean reads, undo by command) and item 16 apply to agent commands only when those commands are routed. Routing also fixes two documented harness gaps: environment does not persist between tool calls, and the Bash tool has no pty (issue #9881, open, 72 thumbs-up) [R]. The premise is well sourced; the hook's interaction with permission rules is untested and comes from one researcher.

**Build.** (a) One named long-lived session per agent or task; the in-flight 'proteus run --session S -- <command>' types into that shell, cuts output by the marks from item 1 and exits with the real status, so the harness sees an ordinary command. Its receipt line always states shell kind and version, working directory, environment difference since the session began and the command id. (b) A PreToolUse hook that rewrites a Bash or PowerShell tool input into that call; installed only with explicit consent and a display of exactly what is written. (c) Every verb addresses a command id, and one call returns exit status, output-only text, duration, cwd, child pids and files changed. (d) Counters: routed share, and the Windows baseline of not-found and shell-syntax failures per 100 agent commands (this decides the deferred preflight lint). (e) Say on screen that a routed command runs outside whatever sandbox the harness has.

**Success metric.** Routed share of an agent session's shell commands >= 95% with the hook installed; 0 exit-status regressions against unrouted runs on a 200-command suite, run again with de-elevation (item 8) on; added latency per routed command p95 under 30 ms; context tokens added by the CLI route 0; six of six facts retrievable for a past command in one call; repeated setup prefixes (cd, activate, export) per 100 agent commands 0.

**Kill criterion.** Drop the hook route (keep CLI verbs and MCP tools the agent calls by choice) if a rewritten command changes Claude Code's permission outcome for any command in the suite (auto-allowed becomes prompted or the reverse), if routed share stays under 80%, or if added latency p95 exceeds 100 ms.

**Depends on.** 1: Command boundaries; in-flight: proteus run; in-flight: MCP server (sessions, send, wait, lean reads)

### 6. Coverage oracle in the gate for the process and file senses

*Effort and evidence: S, strong*

**Why.** Two researchers verified in Microsoft's documentation that job completion-port messages are 'not guaranteed' to be delivered and that ReadDirectoryChangesW discards the whole buffer on overflow while still returning success [R]. The in-flight process senses and tree history are described in the ledger as 'every process ... with exit codes' and 'undo any command'. CLAUDE.md allows measured claims only, and the case for item 7 (job at creation) is a coverage number that only this oracle can produce.

**Build.** Workload scripts that log their own spawns, exits and file operations as ground truth, no admin: a multi-process dev server started and killed 100 times, a cargo build, an npm install, 100,000 creates, modifies and deletes in 10 s, and a 500-command mix of edits, rm -r, moves, git checkout, formatters and code generation. The gate prints: process starts seen, exits reconciled, share with command line, share with exit code; tree hash rebuilt from the history against the real tree; overflow count and rescan time; undo byte-identical or the misses listed. Optional and never in the product: when the gate is run elevated, a kernel ETW process and file trace as a second oracle.

**Success metric.** Starts seen >= 99.9%; exits reconciled 100%; share with command line and with exit code reported, not assumed; orphans after 100 start-and-kill cycles 0; tree-hash equality in 20 of 20 torture runs; rescan under 200 ms for a 50,000-file tree; watcher under 2% of a core during the build and 0.00% idle.

**Kill criterion.** Demote it from the default gate to a long gate if it adds more than 60 s, and to a weekly run if it reports identical perfect figures on 10 consecutive gate runs (zero information). Drop the ETW half if it needs a consent prompt on every run. If the ground-truth scripts disagree with themselves across identical runs, remove the step until fixed rather than trust it.

**Depends on.** in-flight: process senses (job objects); in-flight: content-addressed tree history and undo

### 7. Own the pseudo-console creation call: job at creation, flags that exist, read to end-of-pipe, ConPTY 1.25 by A/B

*Effort and evidence: M, strong*

**Why.** portable-pty 0.9.0 passes flag bits that current ConPTY no longer defines (RESIZE_QUIRK, WIN32_INPUT_MODE) and cannot add a job list, so a job assigned after spawn races with early children [R, checked against the main and release-1.24 headers]. src/session.rs line 221 sleeps 60 ms after the child exits as a guess at 'trailing output drained' [S] and never releases the pseudo-console. The embedded host is 1.24.260710001 [S]; stable 1.25 exists and changes start-up behaviour that matters to a host terminal [R]. This is also the only place a different token can be chosen (item 8).

**Build.** A private CreateProcessW carrying PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE and PROC_THREAD_ATTRIBUTE_JOB_LIST (documented for Windows 10 and later, so no launcher mode is needed), with the token choice left open for item 8. Pass only flags that exist; choose the glyph-width mode that matches the grid's width function and prove it. Call ConptyReleasePseudoConsole after spawn, read until the pipe breaks, delete the sleep. Keep answering the start-up cursor request promptly. One completion port in the daemon for all sessions' job messages. The gate compares the embedded ConPTY version with upstream. A/B 1.24 against 1.25 with item 3's bench, the gate images and item 4's recordings before embedding 1.25.

**Success metric.** Cursor-column mismatches between console host and grid on an emoji, CJK and combining-mark file 0; sessions whose last bytes are missing over 1,000 short commands 0 with the sleep removed; process starts seen in item 6's oracle >= 99.9% with no window outside the job; spawn to first shell byte in ms for 1.24 and 1.25.

**Kill criterion.** Keep portable-pty and take only the token work if the existing path already scores 0 width mismatches and 0 truncated tails with the sleep removed AND item 6 shows the in-flight assign-after-spawn job missing under 0.1% of starts. Do not embed 1.25 if any gate image or Claude Code recording regresses.

**Depends on.** 3: Instruments; 4: Real-program gate (stage 1 recordings); 6: Coverage oracle; in-flight: process senses

### 8. De-elevate by default: sessions start at Medium integrity even when PROTEUS is launched elevated

*Effort and evidence: S once item 7 exists, moderate*

**Why.** On the development machine, the shell agents ran in held a High-integrity token with SeDebugPrivilege enabled [S]. Anything PROTEUS starts from such a context, every agent included, inherits full administrator rights. Dropping rights never needs admin. The parent-process attribute is documented [R]; the restricted-token recipe is from memory [U]; combining either with the pseudo-console and job-list attributes is untested, and only one researcher covered this.

**Build.** Read the token's elevation type at daemon start and show integrity level as a real state on every session. Default spawn at Medium integrity: either parent the child to the desktop shell process (PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, passing the job list explicitly because the parent's handles and job are inherited too) or a restricted token with a Medium label. Opening an elevated session is an explicit, visible, journaled act. The environment still comes from the window, as env.fresh does now.

**Success metric.** Inside a default session 'whoami /groups' shows Medium Mandatory Level and no enabled SeDebugPrivilege in 100 of 100 launches, whether PROTEUS was started elevated or not; canary writes to C:/Windows and to HKLM fail; a fixed tool matrix (git, cargo build, npm install, python venv, pwsh) passes at the same rate as with the inherited token.

**Kill criterion.** Ship only the integrity badge and a confirmation before any elevated session if neither mechanism gives a working interactive session in 100 of 100 launches when combined with the pseudo-console and job-list attributes, or if the tool matrix pass rate drops. Also stop if the owner says he relies on elevated sessions as his normal mode (see open questions).

**Depends on.** 7: Own the pseudo-console creation call

### 9. Awaiting-input verdict: what the in-flight wait should return besides 'exited'

*Effort and evidence: M, moderate*

**Why.** An agent cannot tell finished from silently running from waiting for input; Claude Code's foreground default is 120 s before a command is backgrounded [R], Windows tool children may have no console at all and 'can hang the Bash tool indefinitely' (issue #84836) [R], and harness authors describe the question as close to the halting problem [R]. Only the terminal holds the signals. Two researchers proposed it independently; the classifier itself is unproven, and whether ConPTY exposes 'blocked on console read' is unknown [U].

**Build.** A classifier in the daemon that returns a typed verdict with its evidence and a confidence: exited(status), at_prompt, awaiting_input(reason), running(cpu in the last second, child count, age of last output), full_screen_app. Signals: OSC 133 marks, alternate-screen entry, cursor parked after an unterminated line that reads as a question, CPU-time deltas across the session's job (a silent build burns CPU, a prompt does not), and OSC 7501 blocked. It is returned by the in-flight wait and shown to the person as a real state; because the session is a visible window, the person can type the answer (never journaled) and the agent resumes. Optional per agent-launched session: a non-interactive environment (pager off).

**Success metric.** Scripted corpus of 60 commands (10 each: quick exit, long silent build, yes/no prompt, pager, interpreter, watch mode): verdict correct in >= 95% within 1 s of the true state; time to report a blocking prompt under 500 ms against a 120,000 ms default timeout; pass rate on 20 interactive tasks (git rebase -i, npm init, ssh host-key prompt, python REPL, pager, editor) against the same tasks through a plain Bash tool; agent seconds lost to timeouts on the corpus 0.

**Kill criterion.** Return only the raw signals (cpu, age of last output, alternate screen, cursor line) and no verdict if accuracy stays under 90% on the corpus after tuning, or if 'awaiting_input' is reported for more than 2% of silent builds (an agent would type into a compile).

**Depends on.** 1: Command boundaries; 2: OSC 7501; in-flight: process senses; in-flight: event-driven wait

### 10. Feedback by outcome: three effect classes with budgets, an alert tier, tick and grain opt-in, and sound that follows the output device

*Effort and evidence: S, moderate*

**Why.** PROTEUS's most frequent sounds are its least informative (a tick per key, a grain per arrival) and it has none for a failed command or a blocked agent; config defaults are sound on, reveal on, light 1.0 [S]. The largest juiciness study (N=3018, abstract only) found extreme feedback as harmful as none; a pre-registered CHI 2024 study (n=1,699) found success-dependent feedback helped and amplification hurt; the one documented key-and-click sonification was found annoying; process-industry norms sit near one alarm per ten minutes [R]. The literature is from games and control rooms, one researcher, and no person has yet listened to PROTEUS, so this is moderate. Device follow is a plain defect: cpal 0.16.0 never registers the endpoint notification [R] and src/sound.rs opens the default device once [S].

**Build.** Classes: acknowledgement (echo within a frame, no added animation), arrival (reveal and light, capped, making no claim about meaning), outcome (exit status from item 1, agent state from item 2); only outcome may be distinctive. Outcome earcons under 300 ms, told apart by rhythm and contour. Attention tier only for 'blocked on you' and 'error': once per episode, never for the focused session, at most 6 per hour across all sessions, an OS toast only when the window is not in the foreground and counted in the same budget, no speech. Key tick and arrival grain become opt-in; the red and green notes retire once exit-status sounds exist. One intensity setting (0, 0.5, 1). Every effect settles within 400 ms of its event; the single continuous animation allowed is a slow anchored needs-you pulse near 1 Hz, driven by a real ongoing state. Counters in stats.json: alerts per hour and the fraction acted on. Device: register IMMNotificationClient, set an atomic flag in the callback and nothing else, reopen the new default paused on the sound thread, coalesce the per-role callbacks.

**Success metric.** Attention-tier sounds average 6 or fewer per hour over a working day and never exceed 10 in any 10 minutes; fraction followed by input to that session within 60 s >= 0.8; error cues on commands that exit 0: none in a 500-command session that includes diffs; sounds per hour of ordinary work down by more than 90% against today's build while every failed command and blocked agent still gives exactly one; 'sound' still true in config.json after 14 days; 20 plug and unplug cycles each put the next sound on the new device with callback-to-ready under 250 ms; idle with sound 0.00%; pulse at or under 0.5% of a core.

**Kill criterion.** If after 14 days the owner has switched outcome sounds off or switched tick and grain back on, make his settings the defaults and stop tuning from the literature. If the acted-on fraction is under 0.5 over a week, remove the attention sound and keep the visual mark. If the endpoint registration moves idle off 0.00%, go back to reopening on focus.

**Depends on.** 1: Command boundaries; 2: OSC 7501; in-flight: sound from real events

### 11. What a command cost, and guardrails on a session, from job counters

*Effort and evidence: M, moderate*

**Why.** Job objects keep accounting for every process ever associated, terminated ones included, so short-lived children are not missed [R]; no Windows terminal shows CPU-seconds, bytes and peak memory per command without an elevated profiler. Windows does not kill children when a parent dies: issue #77593 reports 12 orphaned processes holding 4.04 GB and a dwm crash, and Claude Code has no memory-pressure handling on Windows [R]. Primary documentation, one researcher for the counters and two for tree kill, nothing tested here.

**Build.** At each start and end mark read the session job's counters and store the difference in the command record: cpu_user, cpu_kernel, read and write bytes, peak commit, process count. For commands that overlap, put the command's first process in a nested job when its start message arrives; the nested job reaching zero active processes is the OS-level 'this command has fully finished'. Guardrails per session in config: soft thresholds as notification limits (the only job messages with guaranteed delivery) that raise a real visual and sound event; hard caps on job memory and active process count; a lower CPU weight for agent sessions than for the session being typed in; kill means TerminateJobObject. 'proteus ps' and 'proteus kill --tree'; survivors are shown when a session or agent ends; keeping a dev server alive is an explicit tracked act. State what is missing: GPU time and network bytes are not in job accounting.

**Success metric.** Synthetic workload burning a known 10.0 CPU-seconds across 50 short children and writing exactly 1 GiB: CPU within 1%, bytes within 0.1%; 100% of commands carry the five numbers; with a 16-thread burner and a 64 GB allocator in a capped session, typing p95 in another session stays at or under 15.4 ms; survivors after kill 0 in 100 of 100 runs; ms to kill a tree reported.

**Kill criterion.** Report per-session totals only if accounting error exceeds 5% on the synthetic workload or the nested-job assignment misses grandchildren in more than 1% of commands. Drop CPU weights (keep kill and caps) if they do not hold typing p95 at 15.4 ms under the burner. Make limits opt-in if any tool in the matrix (cargo, npm, a tool that sets its own UI-restricted job) fails inside the session job.

**Depends on.** 1: Command boundaries; 7: Own the pseudo-console creation call; in-flight: process senses

### 12. Attention accounting for the driver: 'since you last looked' digest, blocked-to-attended latency, honest progress, conflict notice

*Effort and evidence: M, moderate*

**Why.** With several agents the scarce resource is the operator: fan-out is activity time over interaction time (CHI 2004); a probe with 16 developers found a central status view raised completed tickets from 4.19 to 5.69 of 6 and progress awareness from 3.44 to 6.06 (arXiv, small N, one company); after an alert people took on average 9 min 33 s to return to a suspended application; operations over about 10 s need a percent-done indicator [R]. PROTEUS keeps no trace of what happened off-focus and shows no estimate although its journal holds how long the same command took before. This repository's own agents are coordinated by a hand-kept locks file (CLAUDE.md [S]). All targets below are proposals, not sourced.

**Build.** The daemon computes per session: activity time, interaction time (focus-in to focus-out, using input timestamps only, never content), blocked-to-attended latency, predicted against achieved fan-out; shown in 'proteus sessions' and stats.json. A 'since you last looked' record per session (commands ok and failed, state changes, lines, files changed, time blocked) drawn as a persistent badge on the session's fixed slot and as one digest line on focus, cleared at the first key; a divider at the last-seen scrollback position; a 'since <cursor>' read for agents. Progress: elapsed time for every running command; median and range when the journal holds three or more earlier runs of the same command line in the same directory; OSC 9;4 and 7501 progress when present; expected_duration for agents; 'overdue' beyond the 95th percentile is a real state. Conflict notice: a path that changed while two sessions had commands in flight is reported to both, labelled as inferred by time window.

**Success metric.** With four concurrent agents: blocked-to-attended median under 30 s and p95 under 3 minutes; share of agent wall-clock spent blocked on the person under 10%; failed or blocked events not visited within 2 minutes under 5%; focus to first keystroke median under 5 s; estimate median absolute error under 25% for commands with three or more earlier runs; agent timeouts firing on commands that later succeed 0; seeded test of 50 overlapping edits by two sessions detected within 1 s in >= 95%.

**Kill criterion.** Remove the digest line (keep the metrics) if blocked-to-attended median does not improve against a baseline week measured before it ships. Show elapsed time only if estimate error exceeds 50%. Drop the conflict notice if it fires even once per hour of single-agent work.

**Depends on.** 1: Command boundaries; 2: OSC 7501; in-flight: tree history (files changed); in-flight: world view (fixed slots)

### 13. Accessibility and flash-safety gate

*Effort and evidence: M (S without the UI Automation stage), moderate*

**Why.** The engine animates every arriving glyph, emits light in the glyph's own colour (red for red text), plays sound by default and draws everything on the GPU, which assistive technology sees as an empty window; src contains no reduced-motion, flash, contrast or UI Automation handling (search today for SPI_ and UIAutomation found nothing [S]). WCAG 2.3.1 limits flashes to three per second and counts saturated-red transitions; Windows exposes the 'animations off' setting; Windows Terminal has a UIA provider and text notifications; Narrator does not set SPI_GETSCREENREADER [R]. Red error text arriving in bursts through the light field is the pattern the red-flash rule describes. One researcher, primary specifications.

**Build.** (1) A gate step that renders stress samples (red flood, alternating full-screen updates) through 'proteus shot' at 60 steps per second, computes relative luminance and saturated-red transitions per region, and fails on more than three flashes in any second at light=3.0, the config maximum. (2) Read SPI_GETCLIENTAREAANIMATION at start and on WM_SETTINGCHANGE: reveal becomes instant, light 0, the pulse a static badge. (3) A parity table in the repository: every sound has a persistent visual twin and every colour signal a glyph twin; the gate fails on an unmatched row. (4) UI Automation in stages: first rate-limited notifications carrying newly written text, a full text provider later. (5) done and error marks linger at least SPI_GETMESSAGEDURATION.

**Success metric.** Zero one-second windows above threshold across all samples at light=3.0; with OS animations off, frames drawn after a burst equal the number of content changes; parity table with zero unmatched rows; a state identifiable from a greyscale shot in 100% of cases; at stage 1 NVDA speaks a command's output within 1 s and does not hang at a 1 MB/s flood.

**Kill criterion.** Remove the flash step until fixed, rather than trust it, if the analyser flags the plain basic.vt sample or fails to flag a synthetic 4 Hz full-screen red and black alternation. Withdraw the UI Automation notifications until a full text provider exists if NVDA lags more than 1 s or hangs at 1 MB/s with rate limiting. Reduced-motion handling has no drop condition: it is a floor before the exe goes to anyone else.

**Depends on.** 10: Feedback by outcome (the parity table lists its sounds and marks)

### 14. Links: draw and open OSC 8, then resolve plain path:line from the working directory

*Effort and evidence: S, strong*

**Why.** The parser handles OSC 8 and the cell model stores the hyperlink [R], but nothing in src reads it (search today found no use [S]), so links are invisible and cannot be followed. The value is modest; it ranks here because the gap is confirmed and the work is small. The spec says not to open on a plain click and to restrict schemes [R].

**Build.** Ctrl+click opens; hover shows the target in an engine-drawn strip; underline grouped by id; allow http, https, mailto and local file only, with the local-host check for file URIs. Then, with the working directory from item 1 and the file tree, resolve plain 'path:line' text in output to a real file without the program's help, and return link targets through the in-flight lean reads so an agent gets targets, not underlined text.

**Success metric.** Gate sample with three link shapes: hit-testing returns the right URI for 100% of link cells; mouse movement outside link cells draws 0 frames; path:line resolution names the right file in >= 99% of cases on a recorded compiler and test-output corpus.

**Kill criterion.** Ship Ctrl+click without hover if hover handling makes mouse movement draw frames and cannot be fixed inside the idle budget. Drop path:line resolution and keep OSC 8 only if its precision is under 95% on the corpus.

**Depends on.** 1: Command boundaries (cwd; the OSC 8 half needs nothing)

### 15. Agent-facing safety: redaction, 'this is program output' labelling, journal protection

*Effort and evidence: M, moderate*

**Why.** Journals are plaintext and hold everything printed (ledger known issue [S]); the in-flight search makes secrets easier to find; anything a program prints becomes model input once an API returns it, and can forge marks. The ledger's replay.silent entry records a real leak found in practice [S]. Claude Code masks only credentials with a recognisable prefix [R]. Per-user encryption through DPAPI is from memory [U]; tamper-evidence against the same user account is limited and must be said so.

**Build.** A redaction pass on lean reads and recall results (prefixed tokens; report the miss rate for unprefixed ones). Every agent-facing read carries its command id and a statement that it is data from a program, not instruction. Receipts bind only to marks that carried the nonce. A per-session 'do not journal' switch. Journals encrypted at rest with a per-user key and restricted by ACL. Keep the rule that replayed or agent-read history can never answer a terminal query.

**Success metric.** Forged marks accepted in a fuzz of 10,000: 0; seeded prefixed secrets appearing in lean reads: 0, with the miss rate for unprefixed ones reported; journal bytes readable from a second user account: 0; encryption costs under 5% of flood MB/s and of attach time.

**Kill criterion.** Drop encryption and rely on ACLs plus the do-not-journal switch (and say so in the ledger) if it costs more than 5% of flood MB/s or adds more than 50 ms to attach. Make redaction opt-in per session if it alters more than 1% of non-secret reads in the corpus.

**Depends on.** 1: Command boundaries (nonce); in-flight: lean reads; in-flight: keyframes and search

### 16. Receipts and a claim check: a witness for 'tests pass' that is not the agent, and history beside every permission prompt

*Effort and evidence: L, moderate*

**Why.** Agents report passing tests on failed runs: a NeurIPS 2026 paper names it 'apparent-success reporting' and a Microsoft paper shows a hand-off citing '222 passed' over a single-file implementation (abstracts only, no rates) [R]. Hooks cannot witness success because PostToolUse carries no exit code [R]. People accept 93% of permission prompts [R]. The mechanism is untested and one researcher covered it, so moderate; it is ledger item 'receipts', sharpened.

**Build.** For each routed command a hash-chained record: command as executed, cwd, start, end, the shell-reported status (133;D) and, where one process maps to the command, the OS exit code, each labelled; SHA-256 of the journal range; tree hash before and after from the in-flight history; cost from item 11; policy hash if boxed. A claim check for a Stop or TaskCompleted hook: a claim of passing tests must cite a receipt whose status is 0 AND whose tree-hash-after equals the tree hash now, which catches tests that passed before the last edit. Counts parsed for common runners, including 'zero tests ran'. A per-command badge driven by status, never by colour. Beside a permission prompt (OSC 7501 blocked with kind permission, or a PermissionRequest hook): how often this exact command ran, its exit statuses, the paths it wrote, how long it took. State what a receipt does not prove: that the tests mean anything, or tamper-resistance against the same user.

**Success metric.** Seeded corpus of agent final messages with known ground truth: >= 95% of false 'tests pass' claims flagged with a false-flag rate under 2%; share of claims that cite a receipt; share of cited receipts whose tree hash still matches at claim time; median seconds from permission prompt to answer with history shown.

**Kill criterion.** Keep receipts as a passive record and drop the claim check if the flag rate is under 80% or the false-flag rate over 5% on the corpus. Keep only the badge if the routed share from item 5 is under 50% in real sessions. Remove tree hashes from receipts if item 6 cannot reach tree-hash equality in 20 of 20 runs.

**Depends on.** 1: Command boundaries; 5: Routing; 6: Coverage oracle; 11: Command cost; 15: Agent-facing safety; in-flight: tree history and undo

### 17. Boxed sessions through Microsoft Execution Containers: a one-day spike first, the full build only if it passes

*Effort and evidence: S for the spike, L for the full build, weak*

**Why.** Kept on exceptional upside, not on evidence: containment as a property of the session, for any agent, is the one thing prompts and observation cannot give, and on native Windows Claude Code runs commands unsandboxed [R]. Evidence that it works here is weak. The SDK reached 1.0.0 on 2026-10-07 and 1.1.0 on 2026-11-15 with a Rust crate [R]; this machine's build 26340.9882 is not a row in the support table [R]; the README says nothing about administrator rights and says its telemetry needs 'no user consent or per-request opt-in', while the Rust crate 'builds the MXC SDK, engine, and selected backends into the consuming application' [S: README opened today]. The spike depends on nothing above and can run at any time; the rank is for the full build.

**Build.** Spike (S, from a dist copy with an isolated data folder): start pwsh in a process container through mxc-sdk with read-write on the project root, read-only on toolchains, denied on SSH keys and credential stores, network denied; record whether it needs elevation, whether any file must sit beside the exe, whether telemetry is off in a from-source build, and whether its PTY can feed PROTEUS's journal. Full build (L): a 'boxed' session type spawned through the SDK and still inside PROTEUS's job; denials as journal events on the timeline; a learning run that proposes the minimal policy; the policy hash in receipts; the boxed state drawn on the session. No home-made sandbox; if the spike fails, a write-restricted token may be offered only under the label 'write guard, not a sandbox'.

**Success metric.** Canary suite of 20 operations (write outside the workspace, read a planted key file, connect to a host off the allow-list, kill an outside process, write a Run key): 0 successes; a fixed tool matrix (git, cargo build, npm install, python venv, pwsh) with the pass rate reported; spawn overhead p50 in ms and cargo build overhead in percent against unboxed; permission prompts per hour before and after.

**Kill criterion.** Stop after the spike, stay with observation plus de-elevation and say plainly that PROTEUS does not contain, if any one of these holds: it needs administrator rights; it needs a file beside the exe that cannot be embedded and unpacked; telemetry cannot be switched off in a source build; the tool matrix passes under 80%; the interactive session cannot be journaled.

**Depends on.** 6: Coverage oracle (full build only); 7: Own the pseudo-console creation call (full build only); 8: De-elevate (full build only)

### 18. Text shaping and colour emoji: glyph-id atlas, harfrust for shaping, skrifa for outlines and colour

*Effort and evidence: L, moderate*

**Why.** The ledger lists monochrome emoji, no ligatures and approximated combining marks [S]. The atlas is keyed by character in one 8-bit texture, so it cannot address a ligature or a joined emoji sequence or hold colour, and ab_glyph has no COLR support [R]. Segoe UI Emoji on this machine is COLR version 1 but also carries 3,487 version-0 base glyphs with 55,497 flat layers, so flat colour emoji need no gradient painter [R]. One researcher, facts read from local sources that I did not re-read; assumed not covered by this push's 'drawing fixes' (see open questions).

**Build.** In order: (1) key the atlas by glyph id plus font; (2) add an RGBA8 atlas page and paint COLR v0 layers first, v1 gradients later through skrifa's ColorPainter; (3) shape per run of same-style cells with harfrust and cache by run hash, with an ASCII fast path that stays a table lookup; (4) fold a cell's zero-width characters into its cluster. Cell width always comes from the grid, never from the shaper. Not cosmic-text: a grid needs run shaping, not paragraph layout.

**Success metric.** A 'proteus shot' test sheet (emoji including joined sequences, flags and skin tones; combining marks; a ligature list) against a golden image: percent of glyphs correct (colour is 0% today); grid_build_us average stays inside 100-220 us on the ASCII dense sample; run-cache hit rate above 99% on a recorded agent journal; atlas bytes reported.

**Kill criterion.** Ship colour emoji from v0 layers and combining marks only, with ligatures left off, if grid_build_us on the ASCII dense sample leaves the 100-220 us band with the fast path on, or if the run-cache hit rate is under 95% on the recorded journal.

**Depends on.** in-flight: drawing fixes (shared atlas code)

### 19. Images: kitty graphics through an APC tap, stored once in the journal; sixel after

*Effort and evidence: L, moderate*

**Why.** Both image protocols vanish in the parser before the grid sees them: vte 0.15.0 routes APC to a discard state and its DCS hooks are stubs [R]. Since the 2024 rewrite ConPTY forwards application VT unmodified [R], but whether APC survives the bundled 1.24 host byte for byte is untested [U]. The kitty protocol is the most widely implemented image path among current terminals [R]. No terminal keeps images in a replayable journal.

**Build.** Only after item 3's probe shows byte equality: extend the scanner to APC G; direct transmission (RGBA and PNG; the png crate is already a dependency); placements tied to cells that scroll with the text; delete; the query reply; Unicode placeholders so placements live in the existing grid. Payloads are content-addressed blobs referenced from the journal, so time travel and 'proteus shot' replay them and repeated frames cost nothing. Sixel afterwards on the same placement path, advertised in DA1. No file or shared-memory transmission at first: those make the terminal read files on a program's behalf.

**Success metric.** Golden-image match in the gate for a kitty-graphics sample and a sixel sample; a 1 MB PNG on screen within one frame of its final chunk; journal growth equal to the blob size once, however often the image is redrawn; idle 0.00% with an image on screen.

**Kill criterion.** Stop if the probe shows APC bytes in differ from bytes out through the bundled host on both 1.24 and 1.25 (not fixable inside PROTEUS). Leave it unbuilt if the scanner counts zero image sequences in two weeks of the owner's real journals.

**Depends on.** 1: Command boundaries (scanner); 3: Instruments (pass-through probe); in-flight: keyframes

### 20. HDR light field, carried by the wgpu 27 to 30 migration

*Effort and evidence: L, moderate*

**Why.** The light field is computed in 16-bit float and then clamped into an 8-bit SDR surface. wgpu 27 has no way to set the swap chain's colour space; wgpu 30.0.0 added it together with a query for display headroom [R, read from both source trees in the local registry]. One researcher found the built-in panel has HDR enabled; the other did not check [R]. This is the only concrete reason to migrate: the DX12 present path is the same in 27 and 30 and the migration gives no latency change [R]. Last because it is large, changes nothing an agent sees, and must not outrun the flash gate.

**Build.** Migrate the few touch points (instance descriptor in src/gpu.rs, the acquire result and present through the queue in app.rs, shot.rs). Query display_hdr_info; when HDR is on, configure Rgba16Float with ExtendedSrgbLinear, keep text and chrome at SDR reference white tracking the system setting, and let only event-driven arrival light exceed 1.0, capped by tone_map_headroom(); blend in linear light; fall back to today's path when HDR is off. 'proteus shot' stays SDR so golden images do not change. Optionally record the share of frames in independent flip with PresentMon in an elevated bench mode.

**Success metric.** stats.json records max_nits, sdr_white_nits and headroom; peak written for an arrival flash above 1.0 and at or below the reported headroom; steady text luminance unchanged within 2%; idle still 0.00% CPU and GPU; shared GPU memory delta in MB (about 41 MB per buffer expected against about 21 today); typing p50 within noise; flash gate passes at HDR peak.

**Kill criterion.** Revert to the SDR path if typing p50 or p95 worsens beyond noise in an interleaved A/B, if the window drops out of independent flip, or if the owner cannot tell HDR from SDR arrival light in a blind A/B on this panel. Without HDR the migration is not kept for its own sake.

**Depends on.** 3: Instruments (present path measured first); 13: Accessibility and flash-safety gate

## Avoid (tempting, and the evidence says no)

- A built-in agent or model. Warp's full terminal control serves only its own agent; a terminal's leverage is serving every harness. Keep a counter of model calls made by the exe and keep it at 0.
- A home-made restricted-token or ACL sandbox presented as containment. The vendor that built one says the no-admin variant has weaker network isolation and no denied read paths. Use Microsoft Execution Containers if the spike passes; otherwise say PROTEUS observes and does not contain. A write-restricted token is acceptable only labelled 'write guard, not a sandbox'.
- Model-written summaries of output inside the terminal. The measured gains in agent interfaces came from deterministic shaping (100-line windows, summarised search, a linter guard); a model in the loop adds cost and cannot be checked under 'measured claims only'.
- Claude Code channels as the main event path. They are a research preview behind --dangerously-load-development-channels, ignored in non-interactive mode, and events are dropped silently when unregistered. Use the blocking wait and hooks. Revisit when channels leave preview.
- A tmux-compatible shim. Control mode carries no shell command boundaries or exit codes, and nobody has captured which tmux commands a harness sends. Revisit when a captured command list from a real team session exists.
- Claiming exact per-process attribution of file writes. Directory notifications carry no writer, and exact attribution needs kernel tracing or a filter driver, which means elevation. Report attribution at session level and label it inferred.
- The NTFS change journal as a change source. Microsoft's documentation requires administrator rights, and the unprivileged control code has no reference page (the expected Learn URL returned 404 today). Revisit only if an unprivileged read is demonstrated on this machine.
- Investigating or waiting for a ConPTY passthrough mode. The flag does not exist in the current or the 1.24 header; application VT has been forwarded unmodified since the 2024 rewrite. Measure the stages instead (item 3).
- Treating flood throughput as experience work, or ranking PROTEUS against published terminal numbers. Nobody reads at megabytes per second; latency tails are what people feel. Published figures come from other systems, methods and years, and PROTEUS's 6-8 ms stops at the present call. Not now: a same-machine cross-terminal table with the self-timing emitter, only if the owner wants one.
- Distance-field (MSDF) glyphs or Vello as the grid text renderer. Grid build is already 100-220 us; distance fields lose hinting at 12-16 px and Vello's own authors rank its GPU path below its CPU path.
- Slug outline rendering now. Not now, revisit when a feature needs continuously scaled text (zoom animation, large headings) AND the patent disclaimer is confirmed in the USPTO record; the page a researcher opened still showed the patent as active.
- Swapping the VT core to libghostty-vt now. Build the differential oracle first (item 4, stage 2); swap only on a measured difference.
- Migrating to wgpu 30 for its own sake. It changes nothing in the DX12 present path and gives no occlusion signal on Windows; damage-rectangle presentation and the acquire-stall fix are unreleased. Migrate only with HDR (item 20).
- Tearing present by default, compositor-clock pacing and OSC 3008. Text scrolling tears visibly and the ledger measured no typing gain; the swap chain's own waitable handle gives the same wake-up; OSC 3008's emitters are Linux system tools.
- Changing frame pacing (acquire before building the grid, or DontWait with a manual wait) before the wait has been measured. Not now, revisit when item 3 shows p95 blocked in acquire at 1 ms or more.
- Escalating juice: screen shake, particles, travelling motion, blinking, idle shimmer, bigger effects for bigger output. Extreme feedback measured as bad as none, blinking and travelling motion are the most irritating cues, large luminance swings are a seizure risk, and they break the project's own rules (nothing decorative, zero frames when idle).
- Colour as meaning (red is an error, green is success) in light or sound. The ledger already records that a diff's removed lines are red; colour is the weakest channel in peripheral vision (24% missed) and red pulses are the red-flash pattern. Drive outcome from exit status and agent state, always paired with a glyph.
- Per-key ticks and per-arrival grains as the default sound. They are the lowest-information events; keep the code, make them opt-in.
- A notification, toast or spoken line for every event. One attention queue; only 'blocked' and 'error' interrupt, once per episode, never for the focused session; 'done' goes into the digest; no speech.
- A quiet looping sound bed on by default. It bends 'nothing loops'; if tried at all, off by default and judged by the owner.
- Inferring agent state by scraping the agent's screen, recolouring its output or wrapping it in a drawn HUD (ledger agents.detect). The layout changes across point releases; OSC 7501 and hooks give typed state. Draw PROTEUS's information outside the grid and leave the program's cells alone.
- Impersonating another terminal's name to get onto an agent's allowlist. Fix the query replies instead.
- Writing hook or agent configuration silently. Only on an explicit user action that shows exactly what will be written.
- Journaling keystrokes to learn command text. The text comes from the shell's own report; the no-keys rule stays.
- KILL_ON_JOB_CLOSE on ordinary sessions. If the daemon died it would kill every session, which contradicts 'sessions outlive their window'; use it only for boxed sessions.
- ETW or any elevated component in the default product. It breaks the no-admin rule and is an attack surface; use it only as an optional oracle when the gate is run elevated. Not now: a consented 'full fidelity' helper, revisit only if item 6 shows a coverage gap that matters.
- Volume Shadow Copy, Projected File System, Windows Sandbox, Host Compute Service or Hyper-V containers, bind links, process snapshots as checkpoints, and hard-link forks. Each needs administrator rights or an optional Windows feature, or does not do what it appears to (Windows Sandbox's command line has no process I/O; hard links share in-place writes; a snapshot cannot be resumed).
- Instant tree forks on ReFS or a Dev Drive now. This machine has only NTFS volumes, a Dev Drive needs a one-time administrator action and at least 50 GB of the 82.3 GB free, and a real copy on NTFS costs minutes. Not now, revisit when the owner agrees to create a Dev Drive.
- Freeze and thaw of a whole process tree as a headline feature now. The true stop relies on an undocumented ntdll export; the documented near-freeze is a CPU hard cap with a floor above zero. Not now, revisit when undo or forks show torn snapshots in item 6's oracle.
- A command-not-found and shell-syntax preflight now. The 24.1% figure is a share of failures in a Linux-container benchmark, and PowerShell functions and aliases are invisible to a PATH lookup. Not now, revisit when item 5's counter shows the Windows baseline is worth fixing.
- Advisory leases across agents now. Detection comes first and nearly free (item 12's conflict notice); enforcement works only for hooked agents and write attribution is by time window. Not now, revisit when the conflict notice shows real overlaps with zero false reports.
- File and shared-memory transmission for images at first. They make the terminal read files on a program's behalf.
- cosmic-text for the grid. A grid needs run shaping, not paragraph layout.
## Corrections to earlier assumptions

- LEDGER known_issues 'Journals are never pruned or compressed; disk has ~92 GB free': pruning exists. src/daemon.rs line 297 calls journal::prune with journal_days (14) and journal_max_mb (2048), and the ledger's own done item 'housekeeping' says so [S]. Only 'never compressed' holds. Free space is 82.3 GB by a researcher's Get-Volume reading [R], which matters because a Dev Drive needs at least 50 GB.
- LEDGER done 'pty.own' is stale twice: the console host is embedded and unpacked at run time, not shipped 'beside the exe' (CLAUDE.md and 'onefile') [S]; and it is version 1.24.260710001 [S] while stable 1.25 (NuGet 1.25.261109001, Terminal v1.25.3131.0, 2026-11-11) and preview 1.26 exist [R]. 1.25 no longer waits for a DA1 reply at start, no longer retries its cursor-position request, and carries kitty keyboard fixes [R].
- LEDGER next 'conpty.throughput: ... investigate passthrough': there is no passthrough mode. The flag is absent from the main and release-1.24 headers; application VT has been forwarded unmodified since microsoft/terminal PR 17510 (merged 2024-08-01), which closed the passthrough request (issue 1173; issue 1985 was closed earlier as a duplicate, which reconciles two researchers) [R]. The 2.3-3.0 MB/s figure was measured with 'cmd /c type' (app.rs line 704 [S]), so it is not shown to be ConPTY-bound. The same PR does not settle mark placement: VS Code's docs still say sequences 'may be misplaced' under ConPTY and Warp's fork story names no version, so treat unknown OSCs as 'should arrive; placement unmeasured' [R].
- LEDGER next 'input.win32' implies a mode to switch on in ConPTY. There is none: the WIN32_INPUT_MODE (0x4) and RESIZE_QUIRK (0x2) bits portable-pty 0.9.0 passes are undefined in current ConPTY and ignored, and the bit it names PASSTHROUGH_MODE (0x8) now means grapheme width [R]. The in-flight keyboard work should become: answer the host's CSI ? 9001 h request and encode win32-input-mode toward console programs, turn on alacritty_terminal's kitty keyboard mode (off by default; src does not set it [S]) and encode by the active flags, and prove both legs with a key-echo probe child on ConPTY 1.24 and 1.25. Shift+Enter is not the reason: PROTEUS already sends it as ESC CR [R].
- LEDGER next 'senses.proc: See every process a session spawns, with exit codes': stronger than the platform allows. Completion-port messages are 'not guaranteed' to be delivered, the pid may already be recycled, an exit code exists only if a handle was opened before the process ended, and work started through WMI, Task Scheduler or services escapes the job by design [R]. The in-flight work should become: job list at process creation (PROC_THREAD_ATTRIBUTE_JOB_LIST, documented for Windows 10 and later, which removes the need for one researcher's launcher mode), a handle opened on every start message, reconciliation against the job's process list at command boundaries and on a timer, and coverage reported as a number (item 6).
- LEDGER next 'chronicle ... driven by file-change events': directory change notifications are not a reliable stream. On overflow 'the entire contents of the buffer are discarded' and the call still succeeds [R]. The in-flight tree history should become: an index of file id, size, write time and hash; overflow as a first-class journal event followed by a bounded rescan; a baseline at session start plus post-images per command, because a watcher only reports after the write; attribution by time window, said plainly. On this machine the baseline is a real copy (NTFS only [R]), so ignore rules and size caps are required. Undo must mark remote, database, registry and network effects as not undoable.
- LEDGER 'receipts', 'organs.mcp: lean reads' and 'agents.channel' assume the terminal sees an agent's commands. It does not: Claude Code runs each tool command in a separate process and streams the output to a working file, so a pane running it journals only its interface [R]. Output-based features reach agent commands only through explicit routing (item 5).
- LEDGER 'agents.channel: Structured channel from Claude Code hooks' cannot supply exit codes on success: the PostToolUse Bash response has stdout, stderr, interrupted and isImage, and a code appears only as 'Exit code N' text on failure [R]. Status must come from shell marks or the OS.
- LEDGER 'verify.sources' OSC 7501: real, and still a draft. Program Status Protocol, revision 0.4 dated 2026-10-10; the answer to 'OSC 7501 ; ? ST' must start with '?' and may list states and kinds; known implementations Rex and libghostty-vt [S: spec opened today]. Claude Code reports through it from v2.1.295 with nothing to turn on, checks tmux instead of the terminal when inside tmux, and does not report from background sessions [S: docs opened today]. Merged into libghostty-vt on 2026-10-06 while the Ghostty app ignores the reports; a third-party tracker lists iTerm2 3.7.5 as shipped and tmux as declined [R]. Do not call it a standard.
- LEDGER 'verify.sources' libghostty-vt 1.0: true. Released 2026-11-06, latest 1.0.1, C ABI declared stable, Windows x86_64 built and tested (a GNU-ABI DLL; the static library does not link with MSVC link.exe; building needs Zig 0.17). The Ghostty repository README's 'API signatures are still in flux' is stale. The Rust crate of the same name (0.2.2) predates 1.0 and is community-owned [R].
- LEDGER 'verify.sources' Slug: per its author, US 10,373,352 was dedicated to the public domain on 2026-03-17 and reference shaders are MIT; the Google Patents page a researcher opened still showed 'Active' with no disclaimer event, so the register record is unconfirmed [R].
- LEDGER next 'agents.detect: Detect agents, write their theme config, recolour output, HUD wrap': largely unnecessary and fragile. State comes from OSC 7501 first and hooks second; the agent is identified by process name from the job, not by screen text; the program's cells are never altered [R].
- LEDGER next 'images: Kitty graphics / sixel through the bundled ConPTY' puts the problem in the wrong place: the parser (vte 0.15.0) discards APC and stubs DCS, and DA1 answers without a sixel attribute [R]. Whether APC survives the bundled host byte for byte is untested [U].
- LEDGER decision 'swash/skrifa when shaping lands': skrifa does not shape (its docs point to HarfRust) and swash's COLRv1 support is unconfirmed. The pairing is harfrust for shaping and skrifa for outlines, hinting and colour [R].
- LEDGER known issue 'wgpu is 27.0.1 ... no measured need yet': there is one concrete need, HDR. wgpu-hal 27.0.4 never sets a swap chain colour space; 30.0.0 added surface colour-space selection [R]. One researcher found the built-in panel has HDR enabled; the other left it unchecked [R]. The upgrade changes nothing in the DX12 present path and gives no occlusion signal on Windows; present_with_damage and the one-second acquire-stall fix are unreleased, not in 30.0.2 [R].
- CODE COMMENT in src/app.rs (line 258 [S]) that Mailbox never blocks waiting for the display: with wgpu's DX12 defaults get_current_texture waits on the swap chain's frame-latency waitable before acquiring [R], and redraw builds the grid (line 771) before that call (line 787) [S]. The duration is unmeasured and may be small. gpu.rs builds the instance with ..Default::default() [S], so wgpu's environment variable for this cannot be used to test it.
- LEDGER 'claude_code_inside: renders correctly' reads broader than its evidence (an idle prompt, nothing submitted). Claude Code's docs say ConPTY-backed hosts 'coalesce these positioned writes incorrectly and leave fragments' under its fullscreen renderer, the starting renderer for users who first used it on or after 2026-05-06 [R]. Treat correctness as untested until a recorded session passes the gate.
- LEDGER 'vt.sync' and 'env.fresh': mode 2026 already answers a DECRQM query as 'reset but supported' [R], but how Claude Code decides to use it is not documented; the docs only offer CLAUDE_CODE_FORCE_SYNC_OUTPUT=1 for a terminal that 'isn't auto-detected' [S]. That switch cannot reach a PROTEUS session today because SCRUB_PREFIXES removes every CLAUDE_CODE_ variable (session.rs lines 66-69 [S]).
- LEDGER known issue on sound following the default device only on focus after a minute: not a platform limit. IMMNotificationClient reports the change at once without privileges; cpal 0.16.0 does not register it [R]; sound.rs opens the default device once [S].
- LEDGER 'sound.v1' and 'sound.colour' (design correction for the in-flight sound work): a tick per key and a grain per arrival are the lowest-information events, and the one documented deployment of key and click sonification was found annoying [R]. It should become outcome-driven (exit status, agent state), budgeted, with tick and grain opt-in and the red and green notes retired.
- LEDGER 'light' and next 'field.more': effects currently grow 'the more arrives'. The evidence favours dependence on outcomes, not amplification: extreme feedback measured as bad as none (N=3018, abstract) and amplification reduced motivation in a pre-registered study (n=1,699) [R].
- IN-FLIGHT world view should become lanes on one time axis with a fixed slot per session, not a scene: position means identity, length means duration, colour means state class and is always paired with a glyph, motion is reserved for 'needs you', repeated commands merge, and the same data is available as text for agents [R]. The fixed-slot rule is the researcher's inference, not a tested result.
- IN-FLIGHT event-driven wait should return a compact receipt that says plainly no polling is needed, and typed verdicts (item 9). 'quiet' and 'match' work on raw bytes today; 'exit' and 'prompt' need item 1. The cost of polling is documented only as one reporter's estimate (about 2,640 model requests and about 500 dollars in six hours, issue 92062) [R].
- IN-FLIGHT lean reads should return the final rendered text of a command's range (redraws collapsed), with ANSI removed, identical lines folded with a count, context windows around failure evidence, about 100 lines by default (SWE-agent: 100 lines 18.0% resolved against 12.7% for the full file), one capped summarised search list, and always a handle to page or grep the journal. Deterministic, no model, and never red text as the error signal [R].
- IN-FLIGHT keyframes and search should index rows as they leave the screen on the alternate screen too, because Claude Code's fullscreen renderer keeps the conversation out of scrollback, and should ship redaction and a per-session do-not-journal switch before search makes secrets easier to find [R].
- IN-FLIGHT 'proteus run' should type into a persistent named session and state shell kind and version, working directory and environment difference in every receipt. Researchers disagreed on the exit-code source: the shell-reported status (OSC 133 D) and an OS process exit code are different facts, and a pipeline or a built-in has no single process. Record both where both exist and label each [synthesis of two researchers].
- LEDGER known issue 'Two windows on one session would both answer terminal queries': answering in the daemon-side scanner, once per live session, removes this for the sequences the scanner owns [R, proposal].
- LEDGER measured typing figure (6.1-7.8 ms) must not be set beside published terminal latencies: it starts after the OS input path and stops at the present call, while the one Windows camera study found (33-87 ms) is key to photon [R].
- LEDGER (implied): the parser drops more than the ledger suggests. vte 0.15.0 handles only OSC 0/2, 4, 8, 10-12, 22, 50, 52, 104 and 110-112; OSC 7, 9, 9;4, 99, 133, 777 and 7501 are discarded with a debug log [R]. OSC 8 is parsed and stored per cell but nothing in src reads it [S].
- COMMON BELIEF 'Claude Code's rewind does nothing about Bash changes' is too strong: once Claude has edited a file with its edit tools, rewind also undoes later Bash changes to that file. Untracked are files only Bash changed, sub-agent edits to other files, edits from concurrent sessions, and symlinked or hard-linked paths [R].
- COMMON BELIEF 'shell state persists between an agent's commands' is wrong for Claude Code: each command runs in a separate process, exports are lost, the working directory carries over only inside the project directory and never for sub-agents; the PowerShell tool follows the same rules [R].
- COMMON BELIEF 'Microsoft's PowerShell shell-integration snippet gives full command boundaries' is wrong: it emits 133;D, 133;A, OSC 9;9 and 133;B, never 133;C and never the command text; cmd.exe cannot report an exit code in its prompt [R].
- COMMON BELIEF 'tmux control mode gives agents structured command results' is wrong: %begin, %end and %error frame tmux's own commands; pane output is raw bytes with no shell boundaries or exit codes [R].
- COMMON BELIEFS about Windows sandboxing, both out of date. 'There is no OS sandbox for agents on Windows': Microsoft Execution Containers were announced 2026-06-02, SDK 1.0.0 shipped 2026-10-07 (the date one researcher called general availability), 1.1.0 on 2026-11-15; minimum builds 26100.9278, 26200.9278 and 26300.9550; this machine's 26340.9882 is not a row in the table [R]. 'Claude Code's 84% prompt reduction applies everywhere': on native Windows it runs commands unsandboxed [R]. The MXC README says nothing about administrator rights, says its telemetry needs 'no user consent or per-request opt-in', and says the Rust crate builds the engine into the consuming application [S: opened today].
- COMMON BELIEF 'Windows 11 copies are copy-on-write now': block cloning is ReFS-only and same-volume, so the automatic cloning in copy operations does nothing on this machine, whose volumes are NTFS and FAT32 [R]. A Dev Drive needs a one-time administrator action and at least 50 GB [U].
- COMMON BELIEF 'a normal user can read the USN change journal': Microsoft's documentation requires administrator rights for all change-journal operations [R]. FSCTL_READ_UNPRIVILEGED_USN_JOURNAL exists in the SDK headers, but its Learn reference URL returned HTTP 404 when I fetched it today [S]; do not design around it.
- COMMON BELIEF 'NtQueryInformationProcess gives a documented way to read another process's command line': class 60 is an SDK constant, absent from the Learn page's table, and the function carries a 'may be altered or unavailable' warning [R].
- COMMON BELIEF 'the Windows Sandbox command line is a scriptable agent sandbox': it has 'no support for process I/O', so a command's output cannot be retrieved [R].
- COMMON BELIEF 'AppContainer is the natural Windows sandbox for coding agents': Microsoft calls it 'complicated' for unpackaged programs [R]; OpenAI's stated reasons for not using it are known only from search excerpts [U].
- ASSUMPTION 'no admin' against this machine: the shell agents run in holds a High-integrity token with SeDebugPrivilege enabled (re-run today [S]). PROTEUS must not assume it is unelevated; whatever it spawns inherits that token unless it drops rights on purpose.
- COMMON BELIEF 'Windows Terminal lacks the kitty keyboard protocol' is out of date: it ships in 1.25 Preview (PR 19817) [R].
- COMMON BELIEF 'kitty publishes its input latency in milliseconds': its performance page gives throughput in MB/s and names methods, no latency figures [R].
- COMMON BELIEF 'colour emoji on Windows 11 need a COLRv1 gradient renderer': the Segoe UI Emoji file on this machine also carries 3,487 v0 base-glyph records with 55,497 flat layers, enough for flat colour emoji [R].
- COMMON BELIEF 'an agent's conversation lives in terminal scrollback': under Claude Code's fullscreen renderer it lives in the alternate screen, so terminal search and copy do not see it [R].
- COMMON BELIEF 'more juice gives better feel': the largest study found extreme juiciness as harmful as none; a later pre-registered study found amplification reduced motivation while success-dependent feedback raised it [R].
- COMMON BELIEF 'terminal speed means throughput': throughput benchmarks are close to useless for people; latency tails are what is perceived [R].
- COMMON BELIEF about the fan-out formula: the CHI 2004 paper states FO = AT / IT with activity time, and reports that its neglect-time measure 'consistently overestimated' [R].
- COMMON ASSUMPTION 'SPI_GETSCREENREADER detects a screen reader': Narrator does not set that flag, so it cannot be the only check [R].
- RESEARCHER CONFLICTS settled without changing the ranking: nested jobs are documented since Windows 8 (one researcher had it from memory, another verified it); the OSC 7501 query reply is a '?'-prefixed answer that may list states and kinds (two researchers described it differently; spec opened today [S]); ConPTY 'latest' was given under two naming schemes for the same 1.25 release [R].
- STILL UNVERIFIED and carried as such [U]: creating and assigning a job needs no administrator rights (standard practice, not opened); the restricted-token recipe for a Medium-integrity child; per-user encryption through DPAPI; PresentMon needing elevation; the ntdll suspend export; CreateBindLink needing administrator rights; Codex CLI truncation limits and its persistent exec tool; OpenHands' 30-second default; reports of agents hanging on pagers; that Claude Code emits OSC 8; that other GPU terminals expose nothing to accessibility APIs; that MXC process isolation needs no administrator setup.
## Open questions

- Does the vendored ConPTY 1.24 deliver OSC 133, 633 and 7501 byte-exact and in place, and does a reply to the 7501 query reach the program through ConPTY's input side ahead of the DA1 reply? Three sources leave it open and no researcher tested it. It is the first step of item 1 and decides items 1 and 2.
- Does the owner launch elevated on purpose? The High-integrity shell measured today may be a deliberate habit. Ask before item 8 changes the default to Medium integrity.
- What does 'drawing fixes' in this push cover? I treated it as double-line boxes and undercurl, and treated shaping, ligatures and colour emoji (item 18) as not covered. If shaping is already in flight, item 18 leaves the list and 19 and 20 move up.
- How does a PreToolUse rewrite of a tool's input interact with Claude Code's permission rules, auto mode and its announced MXC support? Routing takes a command out of whatever sandbox the harness has. Untested; it is item 5's kill criterion.
- How does Claude Code decide a terminal supports synchronized output: by query or by terminal name? The docs say only 'isn't auto-detected'. Test: count ESC[?2026h in the journal of a real session.
- Does an MXC process container start without administrator rights on build 26340.9882, with nothing beside the exe and with telemetry off in a source build? The README is silent on elevation. The one-day spike in item 17 answers all three.
- Can PROC_THREAD_ATTRIBUTE_PARENT_PROCESS or a restricted token be combined with the pseudo-console and job-list attributes in one CreateProcess call? Untested here.
- Which exit status should a receipt trust when the shell-reported status and an OS process exit code disagree (pipelines, built-ins, wrappers)?
- How long does get_current_texture block per frame on this machine? Unmeasured; it may be near zero, in which case no pacing work is justified.
- Is the remaining flood limit the producing program, the console API path, the pipe or PROTEUS's own journal and socket hop? Hypotheses only until the staged bench runs.
- Is the in-flight tree history affordable on NTFS for a large tree with build output (the baseline is a real copy)? No measurement exists; megabytes per 1,000 commands and ms per boundary are the numbers.
- Do the kitty keyboard query and CSI u keys survive ConPTY's input side on 1.24 and on 1.25? Not established from any source a researcher could read in full.
- If an agent runs inside a multiplexer nested in a PROTEUS session, Claude Code asks the multiplexer, not the terminal, about OSC 7501. Does the owner's setup nest one, and does it pass the query through?
- What is the Windows baseline for command-not-found and shell-syntax failures per 100 agent commands? The only published figure is from Linux containers.
- No person has yet judged the light field in motion or listened to the sound (ledger). Items 10, 13 and 20 have success and kill criteria that need the owner's eyes and ears and 14 days of real use.
- Is the Slug patent disclaimer recorded at the USPTO, and what is the origin and licence of the embedded font (which also decides which ligatures exist)? Both must be settled before the exe goes to anyone else.
- Does a full prompt-and-answer round trip with Claude Code work inside PROTEUS? The ledger has never run one, and one unreproduced report says a prompt sent about 20 s after launch did not appear.
- The targets for attention metrics (30 s median blocked-to-attended, 10% blocked share, 6 alerts per hour) are proposals by a researcher, not sourced; a baseline week must be measured first. The METR 2026 update that supersedes the cited 2025 result was not read.

## Next wave (harvested 2026-11-16 from Warp, Windows Terminal and Intelligent Terminal, VS Code, herdr, WezTerm, Zellij and the Agent Client Protocol)

Each item names where it was seen, why it matters, how it ports to PROTEUS's architecture, and its effort. Full sources: the research record in the release notes of this push.

### 21. Ship the agent-state display together with the OSC 7501 reply

*Seen in: Claude Code docs, OSC 7501 support from v2.1.295 (opened today). Zellij 0.46 inbox (2026-11-11). Intelligent Terminal v0.3 sidebar (2026-11-04). Ghostty 1.4 only parses it (2026-10-27).. Effort: S for the caption and `sessions` field; M with the attention queue (roadmap item 2).*

**Why.** Claude Code drops its title status glyph in any terminal that answers the 7501 query. PROTEUS answers it (ledger status.reply) but draws nothing (ledger known_issues), so today a Claude Code session in PROTEUS probably shows less state than in a terminal that ignores 7501. This is predicted from the docs, the ledger and src/app.rs:1302; it has not been observed. It is the cheapest high-value fix in the list.

**Port.** 1. In src/marks.rs and src/daemon.rs, keep the per-session record table that roadmap item 2 specifies. 2. Send state changes to windows as a new message in src/proto.rs, received in src/remote.rs. 3. In src/app.rs, put one state glyph in the caption beside the program's own title, and add state to `proteus sessions`. 4. Treat msg and title as untrusted text. 5. Gate: replay a recorded Claude Code journal and require the caption to show working, then blocked, then done, with zero frames drawn while the state is unchanged. Do not remove the reply: that would zero roadmap item 2's metric.

### 22. Order every agent by need: an inbox of blocked first, then error, working and done

*Seen in: Warp orchestration child pills sorted 'attention-needed items first' (2026-05-27). Zellij 0.46 inbox with a count of programs needing attention (2026-11-11). Intelligent Terminal v0.3 'Agents only' sidebar filter (2026-11-04). herdr 0.9.2 Go To list with status per agent (2026-09-29).. Effort: M.*

**Why.** With several agents, the operator's attention is the scarce resource. Four of the five competitors converged on the same answer within six months.

**Port.** 1. `proteus sessions` and the MCP `sessions` tool sort by state (blocked > error > working > idle > done), then by age. 2. One key, for example Ctrl+Shift+J, jumps to the oldest blocked session (already in roadmap item 2). 3. The world view (src/world.rs) gives each session a fixed slot, with an engine-drawn count of sessions needing you that is redrawn only on a state change. 4. Measure blocked-to-attended latency (roadmap item 12).

### 23. Clicking a notification lands on the exact session

*Seen in: Warp: 'A focus URL lets external notification tools return focus to the originating pane' (2026-05-27). herdr 0.9.4: 'Windows notifications stay in Notification Center. Clicking one jumps to its pane and brings the terminal window forward' (2026-10-13).. Effort: M.*

**Why.** An alert is only useful if acting on it takes one click. Roadmap item 10 already allows an OS toast for the attention tier when the window is in the background.

**Port.** 1. Register a per-user protocol, proteus://session/<id>, under HKCU/Software/Classes. This needs no admin, is written once, and is shown to the user. 2. Raise toasts with WinRT ToastNotification through the windows crate, using an AppUserModelID set on a Start-menu shortcut [unverified: the AUMID requirement for unpackaged apps]. 3. Activation starts PROTEUS.exe, which asks the daemon (src/remote.rs) to focus or attach that session. 4. Count toasts in roadmap item 10's budget of 6 per hour.

### 24. Agents report their own resume command and release; PROTEUS restores them after a restart

*Seen in: herdr 0.9.2: report-agent with -- <resume argv>, release-agent, and pane variables HERDR_BIN_PATH and HERDR_PANE_ID (2026-09-29). Intelligent Terminal v0.3: recent agent sessions with a resume flow (2026-11-04).. Effort: M.*

**Why.** Sessions already outlive windows (ledger daemon) but not reboots. Resuming the agent conversation is what users want after a restart. herdr has published a contract that agent authors are adopting.

**Port.** 1. src/session.rs exports PROTEUS_SESSION_ID and PROTEUS_EXE to every session. Avoid PROTEUS_SOCKET and PROTEUS_DATA: they are test-isolation switches. 2. `proteus report --state working|idle|blocked --seq N -- <argv>` writes a journal record (src/journal.rs). Accept herdr-compatible semantics: a rising --seq, and argv[0] must be a plain command on PATH. 3. On daemon start after a restart, offer to resume recorded argv in their cwd, 100 ms apart. 4. A release, or a return to a prompt (OSC 133 A), clears the record.

### 25. Survive Windows shutdown and restart cleanly

*Seen in: herdr 0.9.4: 'Herdr saves before the system starts closing apps, so agents come back where they were', and saves are written before replacing the old file (2026-10-13).. Effort: S.*

**Why.** The daemon holds every session's journal. A shutdown that kills it mid-write loses the tail and leaves no record of why sessions ended.

**Port.** 1. The daemon in src/daemon.rs creates a hidden message-only window. 2. On WM_QUERYENDSESSION and WM_ENDSESSION, flush and sync journals and write a 'system shutdown' record per session. 3. Optionally call RegisterApplicationRestart [unverified for a windowless daemon]. 4. At next launch, offer the resume list from the previous item. 5. Gate with a simulated end-session message against an isolated PROTEUS_DATA, requiring zero lost journal bytes. Zero idle cost: the window only waits for messages.

### 26. Submit after the text lands: a delayed Enter for agent UIs

*Seen in: Warp: Antigravity CLI 'gets structured notifications and delayed-enter input' (2026-10-22).. Effort: S.*

**Why.** It addresses a ledger known issue: 'A prompt sent ~20 s after launching Claude Code did not appear'. Typing text and Enter in one write races the agent UI's input handling. MCP `send` is how agents drive agents in PROTEUS.

**Port.** 1. The MCP `send` tool (src/mcp.rs) gains a submit mode: write the text as a bracketed paste. 2. Wait for the grid change in src/term.rs that shows the text, or an OSC 7501 idle state, with a bounded timeout. 3. Then send CR on its own. 4. Return 'not echoed within N ms' as an error rather than pressing Enter blind. 5. Gate: 100 prompts sent at random delays after starting a recorded TUI fixture must all land.

### 27. Mark agent-run sessions, and stop at secret prompts

*Seen in: VS Code 1.121: a 'VSCODE_AGENT environment variable for agent-initiated terminal commands', and 'When a chat terminal command reaches a sensitive prompt, VS Code now intercepts it' (2026-05-20).. Effort: S to M.*

**Why.** Agents hang on password, passphrase and PIN prompts, and must never type secrets. An environment marker lets tools choose non-interactive behaviour. This sharpens roadmap items 9 (awaiting input) and 15 (agent-facing safety).

**Port.** 1. Sessions started through MCP spawn or `proteus run` get PROTEUS_AGENT=1. Document it; change no other variables silently. 2. The classifier in roadmap item 9 adds awaiting_input(reason=secret), triggered when the cursor is parked after a line matching password, passphrase, PIN or verification code, or by an OSC 7501 blocked report of kind auth. 3. While that verdict holds, MCP `send` refuses to type into the session and tells the person to type in the window. Keys are never journaled. 4. Gate: a corpus of git, ssh-keygen and Read-Host -AsSecureString prompts gives the right verdict in at least 95% of cases.

### 28. Unattended runs end as 'blocked', not as a hang

*Seen in: Warp: 'Unattended CLI runs end as blocked instead of hanging' (2026-10-07). Intelligent Terminal v0.3 fixed false 'Waiting for input' transitions (2026-11-04).. Effort: M.*

**Why.** An agent-spawned session with nobody watching is where a stuck prompt costs the most: model calls and wall-clock time.

**Port.** 1. In roadmap item 9's wait verdict, when no window is attached and there has been no input for T seconds, return blocked with its evidence: the cursor line, the CPU-time change across the session's job (src/procs_win.rs) and the age of the last output. 2. Never kill anything automatically. 3. Gate: the 60-command corpus in roadmap item 9.

### 29. Agent hooks run the exe directly, never through a shell

*Seen in: Intelligent Terminal v0.3 (#1158): 'Hooks invoke the bridge executable directly rather than starting PowerShell or Bash first, reducing per-event overhead' (2026-11-04).. Effort: S.*

**Why.** Roadmap item 5's kill criterion is routed-command latency p95 over 100 ms. Starting pwsh per hook would spend much of that budget [unverified: pwsh start-up on this machine].

**Port.** 1. The consented PreToolUse hook writes `"command": "<absolute path to PROTEUS.exe> hook pretooluse"`, with no `pwsh -c` wrapper. 2. A `hook` subcommand in src/main.rs reads the hook JSON from stdin. 3. Record hook-to-exit time p50 and p95 in stats.json. 4. Use the same rule for Notification and Stop hooks.

### 30. Name each session by where it is: cwd, git branch, dirty files, listening ports

*Seen in: Intelligent Terminal v0.3: up to two rich metadata fields per tab (agent status, working directory, repository, branch, changes) (2026-11-04). cmux sidebar: branch, PR status, cwd, listening ports and latest notification [SN].. Effort: S to M.*

**Why.** Ten agents named 'pwsh' cannot be told apart. These facts are what make many agents readable at a glance.

**Port.** 1. Take cwd from OSC 7 and 9;9, already parsed (src/marks.rs). 2. Read the branch from .git/HEAD on a cwd change, without starting git. 3. Read ports with GetExtendedTcpTable filtered to the session job's pids (src/procs_win.rs) [unverified: needs no admin], refreshed only at command boundaries. 4. Draw them outside the grid, and serve them to agents in the MCP `sessions` tool. 5. Gate: idle stays at 0.00%.

### 31. Command-level navigation, selection and copy

*Seen in: Zellij 0.45: OSC 133 jump between prompts, 'Select a command with its whole output', 'Copy the last command's output' (2026-08-20).. Effort: S once roadmap item 1 lands.*

**Why.** Once roadmap item 1 makes commands first-class records, these are the first everyday payoffs for people. Agents get the same unit.

**Port.** 1. After marks are journaled, add keys in src/input.rs for previous and next prompt, select a command's output, and copy the last output. 2. Read the range from the sidecar index (roadmap item 1b). 3. Add an MCP `read` by command id that returns the lean text of that range (src/lean.rs).

### 32. Peek at another session and send one line without switching

*Seen in: Zellij 0.46: previews of other sessions and panes, and 'enter a quick command without switching to them' (2026-11-11).. Effort: M.*

**Why.** Answering a blocked agent without losing your place is the moment that decides blocked-to-attended latency.

**Port.** 1. In the world view, hovering or selecting a session slot renders its live grid into an inset through the existing shot pipeline (src/shot.rs). 2. A one-line input sends through `proteus send` (ledger organ.send). 3. Draw only on change.

### 33. Program notifications (OSC 9, 777, 99), with ConEmu subcommands told apart

*Seen in: Windows Terminal 1.26 Preview: OSC 777 notifications, off by default (2026-10-02). Zellij 0.45.1 fixed OSC 9 overloading the OSC 99 notification spec (2026-08-28). Ghostty 1.4 parses but does not implement kitty notifications (2026-10-27).. Effort: S.*

**Why.** These are the fallback attention signal for agents without OSC 7501. Getting OSC 9;<digit> wrong turns progress pings into toast spam.

**Port.** 1. In src/marks.rs, treat OSC 9;<digit>; as ConEmu subcommands: 9;4 is progress and 9;9 is cwd. 2. Only a plain OSC 9 body, OSC 777;notify or OSC 99 becomes a notification record. 3. Rate-limit through roadmap item 10's budget.

### 34. An ACP-shaped terminal API: create, output, wait_for_exit, kill, release

*Seen in: Agent Client Protocol v1.18.0 (2026-11-14) [gh]. Intelligent Terminal routes agents over ACP (2026-06-02 onward). Warp sends resource links to ACP agents (2026-10-22).. Effort: M.*

**Why.** ACP is becoming the harness-to-host contract on Windows (Microsoft, Zed, Warp). Its terminal methods match what PROTEUS already does, so matching their semantics makes PROTEUS a drop-in terminal provider. It is not a built-in agent, so it does not hit the roadmap's 'Avoid' list.

**Port.** 1. Add MCP tools in src/mcp.rs that mirror ACP: terminal_create(command, args, env, cwd, outputByteLimit) returns an id immediately; terminal_output returns output, truncated and exitStatus{exitCode, signal}; then wait_for_exit, kill (the terminal stays readable) and release. 2. Truncate from the beginning, at a character boundary. 3. Later, implement ACP's client-side terminal capability for hosts that embed PROTEUS.

### 35. Agent-called command resolver: what would this name run, here?

*Seen in: Intelligent Terminal WTA: `wta resolve-command which --cwd . --json`, which resolves from cwd, PATH and shell-specific sources (tools/wta/OVERVIEW.md, opened today; the repository is from 2026-05 onward).. Effort: S.*

**Why.** On Windows, PowerShell functions, aliases and .cmd or .ps1 shims are invisible to a PATH lookup, so agents guess wrong. This is an on-demand tool, unlike the preflight lint the roadmap defers.

**Port.** 1. MCP `resolve(session, name)` runs `Get-Command <name> | Select CommandType, Source, Version` inside that live pwsh session through the routed run path (roadmap item 5). 2. Return JSON. 3. Count use, which feeds item 5's not-found baseline.

### 36. Agent sessions clean themselves up when done

*Seen in: VS Code 1.121: 'automatically disposes background terminals created by the chat agent when their command completes' (2026-05-20).. Effort: S.*

**Why.** Agent-spawned sessions pile up and hold processes (roadmap item 11 cites 12 orphaned processes holding 4.04 GB).

**Port.** 1. MCP spawn gains on_exit: keep or dispose, defaulting to dispose for agent-started sessions. 2. On exit, the daemon seals the journal and frees the session after the job reports zero active processes (src/procs_win.rs). 3. Survivors are listed, never silently killed.

### 37. A daemon started elevated refuses lower-integrity clients by default

*Seen in: herdr 0.9.4: 'An elevated Windows server now only accepts administrator clients by default' (2026-10-13).. Effort: S.*

**Why.** The ledger records that the shell agents ran in held a High-integrity token with SeDebugPrivilege. A Medium process that reaches an elevated PROTEUS daemon socket could drive administrator shells.

**Port.** 1. On accept in src/daemon.rs, get the client pid. For AF_UNIX on Windows, SIO_AF_UNIX_GETPEERPID [unverified]. 2. Read its token integrity. 3. Refuse a Medium client to a High daemon unless config allows it. 4. Show integrity on every session (roadmap item 8). 5. Gate: a Medium test client is refused by an elevated isolated daemon (PROTEUS_DATA and PROTEUS_SOCKET).

### 38. Queue a command for the next prompt

*Seen in: Warp: 'Terminal commands can now be queued' (2026-06-10). Intelligent Terminal v0.3: queued follow-up messages (2026-11-04).. Effort: S once roadmap item 1 lands.*

**Why.** An agent or person can line up the next step without polling for the prompt.

**Port.** 1. `proteus run --session S --queue -- <cmd>` appends to a per-session queue in the daemon. 2. The queued command is typed at the next trusted OSC 133;A (nonce-checked). 3. The receipt arrives by the existing wait.

### 39. Kitty graphics as the single image path

*Seen in: herdr 0.9.2 removed its own pane graphics API: 'Apps show images by writing standard Kitty graphics' (2026-09-29). Zellij 0.45 implemented kitty graphics (2026-08-20).. Effort: L.*

**Why.** It confirms the order of roadmap item 19 (kitty first, sixel after) against the multiplexers agents now run in.

**Port.** As roadmap item 19: an APC tap in src/marks.rs after the item 3 pass-through probe, content-addressed blobs in the journal, and placements tied to cells.

## In flight now (one agent per lane, each inside a PROTEUS session)

- marks: commands as objects (OSC 133), agent state shown (OSC 7501), outcome sound
- speed: fast attach from keyframes, stage-by-stage throughput bench, daemon hop
- reach: OSC 8 links, sound follows the output device, reduced motion and flash safety
- onboard: proteus doctor, agent detection and setup, update at every start
- deps: alacritty_terminal 0.26, sha2 0.11, cpal 0.18, pollster 1.0
- shaping: colour emoji, combining marks, run shaping
- wgpu30: wgpu 30 and HDR light
- site: the public website
- fleet: every session live in one window; spawns grow out of their parent
- look: CRT presets from real optics, themes, 3D views with meaning, synthesised sound presets, adaptive quality
- versus: reproducible head-to-head against Windows Terminal, WT Preview, WezTerm, Ghostty and conhost, looping on every row PROTEUS loses

## Competitive map

- **Warp.** Agent features: Built-in Warp Agent with local and cloud runs. Multi-agent orchestration arrived 2026-05-18. Runs hand off from local to cloud and back ('Continue locally', 2026-06-24) [O changelog]. Detects third-party CLI agents automatically: Claude Code, Codex, Gemini, OpenCode, goose and Mistral Vibe (2026-05-06), Kiro (2026-10-07), Antigravity agy, Grok Build and oh-my-pi. Each gets a rich input editor, with status shown on the pane and in vertical tabs [O changelog; SN docs.warp.dev/agents/cli-agents]. Weak spots: Its business is its own agent and paid usage: AI is tied to a Warp account (onboarding change 2026-07-03), and the server is closed [O changelog; O pricing; SN]. Third-party agent sessions do not appear in Warp's conversation panel [SN docs]. Claude Code notifications need Warp's plugin or a hook [O Claude Code docs].
- **Windows Terminal 1.25 and Microsoft Intelligent Terminal 0.3.** Agent features: WT: no agent features in the 1.24 to 1.26 release notes [O]. 'Terminal Chat' is being deprecated in Canary [O devblog 2026-06-02]. IT agent pane (Ctrl+Shift+.) talks ACP over stdio to Copilot, Claude, Codex, Gemini, OpenCode and Antigravity. GitHub Copilot CLI is the default and is installed through WinGet if no agent is found. It 'always asks before running commands' [O README, devblog]. Weak spots: WT shell integration needs manual profile and settings edits, and cmd cannot report an exit code [O tutorial]. WT notifications (OSC 777 and bell) are off by default. OSC 7 support was announced in 1.25 and then removed [O 1.26 notes].
- **VS Code integrated terminal (with Copilot agent terminal tools).** Agent features: The send_to_terminal tool lets the agent keep typing into background terminals. The experimental setting chat.tools.terminal.backgroundNotifications tells the agent when a background command 'finishes or requires user input' (1.115, 2026-04-08: supporting evidence, before the six-month window) [O]. 1.121 (2026-05-20): a VSCODE_AGENT environment variable on agent-initiated commands. 'When a chat terminal command reaches a sensitive prompt, VS Code now intercepts it.' Background terminals are disposed when their command completes, and more kinds of verbose output are compressed before reaching the model [O]. Weak spots: The terminal tools serve VS Code's own chat agent; external CLI agents get a plain terminal [O 1.115 and 1.121 notes]. Per Claude Code's docs: no desktop notification in the VS Code terminal, Shift+Enter needs setup, GPU acceleration is turned off for correctness, and large pastes can drop characters [O].
- **herdr.** Agent features: Every pane is marked working, blocked or idle. Agents are recognised by process and screen output (Claude Code, Codex, Cursor, OpenCode, Grok and others) [O README; SN reviews]. Agents can report on themselves through $HERDR_BIN_PATH report-agent: idle, working or blocked, with a --seq number that must increase. A resume command goes after --, and release-agent clears the pane on exit. Panes get HERDR_ENV, HERDR_PANE_ID, HERDR_BIN_PATH and HERDR_SOCKET_PATH (0.9.2, 2026-09-29) [O herdr.dev/docs/add-herdr-support]. Weak spots: It is not a renderer. It draws inside another terminal, so latency, fonts and images depend on the host, and 'Windows Terminal does not expose the Kitty graphics path Herdr uses' [O Windows doc]. Some state still comes from screen detection: 0.9.5 says Codex status 'comes from screen detection again' [O].
- **WezTerm.** Agent features: No built-in agent features on the features page [O]. wezterm cli drives a running GUI or mux instance with spawn, send-text, get-text, list, split-pane, activate-pane and more; agents can use it [O wezterm.org/cli/cli]. Weak spots: No tagged release since 2024-02-03 [gh]. No agent state, notification routing or command journal in any source opened today. Kitty keyboard and OSC 133 are not on its features page (unknown).

