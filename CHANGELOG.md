# Changelog

Numbers and how each was measured: `forge/ledger.json`. Signed builds: [Releases](https://github.com/karimalsalah/proteus/releases).

## 0.5.5

- Security (from the adversarial audit): a program may write the clipboard (OSC 52) only while its window has focus, at most 8 KB, and each write is logged; a paste into a program that does not mark pastes never presses Enter by itself.
- Durability (audit): a journal still open in a running session is never pruned, however old.
- Links: Ctrl+click opens OSC 8 links and plain path:line text; hover shows the target.
- Sound follows the default output device the moment it changes.
- Reduced motion honoured from Windows; a flash-safety analyser and gate step (WCAG 2.3.1).
- A key held while the window gains focus is no longer typed.
- `proteus conhost flood|echo|idle`: console-host bench; finding: line-at-a-time output is bounded by the console host's per-call cost.
- The export's privacy search now covers file names, UTF-16BE, URL and base64 encodings, compressed streams and the release key in every form.

## 0.5.4

- `proteus send --raw` types text exactly; Windows paths are no longer turned into control keys (#20).
- `proteus shot --journal` keeps the recorded grid and shrinks the font to fit, instead of rewrapping history into a smaller grid (#22).
- Built on GitHub Actions from the public source (release-build.yml), ready for SignPath Foundation signing; code signing policy published.
- Claude Code plugin marketplace in the repository: `claude plugin marketplace add karimalsalah/proteus`.
- A test no longer depends on the machine's System32 listing; CI runs tests serially while #24 is open.

## 0.5.3

- Claude Desktop bundle (`proteus-mcp.mcpb`) in every release; listed in the MCP Registry as `io.github.karimalsalah/proteus`.
- Scoop manifest (`packaging/scoop/proteus.json`).
- Copies installed by Scoop, winget or Claude Desktop leave updating to their manager.
- Sample journals ship in the repository again (the world-view tests use them).

## 0.5.2

- The updater asks GitHub's API for the newest release; the "latest" download link can serve a stale one.

## 0.5.1

- First update delivered through the signed channel.

## 0.5.0 (first public build)

- Signed self-update: Ed25519 manifest with build, size and SHA-256; no downgrades; daily check, applied at next launch.
- Process senses: every process a session starts, from a Windows job object, with command line and exit code. `proteus procs`.
- World view (Ctrl+Shift+W): the session's processes as a tree and a timeline.
- Time travel: keyframes, seek, `proteus recall find`, scrubbing (Ctrl+Shift+←/→).
- Tree history and undo: `proteus run`, `proteus chronicle`.
- MCP server: `proteus mcp` with sessions, spawn, send, wait, lean read, screen, shot.
- Kitty keyboard protocol; engine-drawn double-line boxes, undercurl, dotted and dashed underlines.
- Answers the OSC 7501 program-status query; scanner for shell marks; PowerShell integration (built, not yet on by default).
- Sound computed from real events; the audio device sleeps between sounds.
- Embedded font: unmodified Maple Mono NF. `proteus licenses` prints every bundled notice.
