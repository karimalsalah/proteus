# Contributing

Thank you for looking. PROTEUS is early and opinionated; these rules are what keep it honest.

## The rules

1. **Measured claims only.** A performance or coverage statement comes from a command anyone can run: the gate, `--bench`, a test, a counter. Write the command next to the number. "Fast" is not a number.
2. **Nothing mock.** Tests use real processes, real files in temp directories and real journals through the real console host. Generated input is allowed when it says so.
3. **Nothing decorative.** Every visual and every sound is driven by a real event. No idle animation.
4. **Zero frames when idle.** If you touch the frame loop, the gate's idle step must still read 0.00%.
5. **One file.** The product is a single exe. Anything it needs is embedded and unpacked at run time.
6. **Say what you did not test.** A truthful "not done" is worth more than a claim. Put it in the pull request and in `forge/ledger.json`.

## Before you open a pull request

```powershell
cargo test --release
pwsh forge\gate.ps1 -SkipBench     # headless: opens no window
pwsh forge\gate.ps1                # full: opens test windows that do not take the keyboard
```

- Update `forge/ledger.json` in the same change: what is done with its evidence, what is known to be wrong.
- If you changed how anything is drawn, render it with `proteus shot` and look at the PNG. Attach it.
- If you changed sound, run `proteus ears` and attach the numbers.
- Keep new code in new modules where you can; shared files (`app.rs`, `renderer.rs`, `session.rs`, `daemon.rs`) are where merges hurt.

## Working safely on a machine you also use

- Test from a copy of the exe with `PROTEUS_DATA` and `PROTEUS_SOCKET` pointing at a temp folder, so your own sessions are untouched.
- Never kill PROTEUS processes by name; you may be killing your own terminal.
- `proteus chronicle restore` and `undo` delete and overwrite files. Their tests run only in temp directories. Keep it that way.

## Where to start

Issues labelled `good first issue` are small and self-contained. The ranked roadmap in `forge/roadmap.md` says what matters most and how each item will be judged; the first three items (command boundaries switched on, agent state shown, agent commands routed into sessions) are where most other work depends.

## Reporting security problems

See [SECURITY.md](SECURITY.md). Please do not open a public issue for a vulnerability.
