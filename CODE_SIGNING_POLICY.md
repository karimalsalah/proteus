# Code signing policy

Windows releases of PROTEUS (`PROTEUS.exe`) are built by GitHub Actions from this repository ([release-build.yml](.github/workflows/release-build.yml)) and, once the project is accepted, signed with a certificate issued to SignPath Foundation.

Free code signing provided by [SignPath.io](https://about.signpath.io/), certificate by [SignPath Foundation](https://signpath.org/). *(Applies to releases built after the project is accepted; earlier releases are not Windows-signed.)*

## Team roles

- **Committers and reviewers:** [karimalsalah](https://github.com/karimalsalah)
- **Approvers** (who approve each signing request): [karimalsalah](https://github.com/karimalsalah)

Contributions from others reach a release only through a pull request reviewed by a committer.

## Two signatures

1. **Windows code signature (SignPath):** tells Windows and SmartScreen who published the exe.
2. **Update manifest signature (Ed25519):** every release carries `PROTEUS.manifest` (build, size, SHA-256 of the exe) signed with a key held only on the release machine; the exe's built-in updater refuses anything that does not verify. The public key is in [`src/update.rs`](src/update.rs).

## Privacy

PROTEUS does not send anything about you or your work anywhere. Two kinds of network access exist, both to GitHub and both for updates:

- At most once a day when a window starts, it asks `api.github.com` for the newest release of this repository and, if newer, downloads the release files from `github.com`. Set `"update": "off"` in `%LOCALAPPDATA%\proteus\config.json` to stop it.
- `proteus update` does the same on request.

Session journals, process records and tree history stay on your machine in `%LOCALAPPDATA%\proteus`.
