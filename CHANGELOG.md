# Changelog

Numbers and how each was measured: `forge/ledger.json`. Signed builds: [Releases](https://github.com/karimalsalah/proteus/releases).

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
