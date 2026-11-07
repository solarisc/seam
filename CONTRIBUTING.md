# Contributing

Thanks for helping. Bug reports with `agent.log` from both computers (see the
README for its location) and the output of `seam-agent status` are the
most useful.

By contributing you agree that your work is licensed under the project's
terms: **MIT OR Apache-2.0**, at the user's option (see
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)).

## Development

Every change must pass the checks in `scripts/check.ps1`:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --features gui -- -D warnings
cargo test --all-targets --features gui
```

On Windows, `powershell -ExecutionPolicy Bypass -File scripts\check.ps1`
runs them all and also lints the macOS build once
`rustup target add aarch64-apple-darwin` is installed. Two clipboard tests
need an elevated PowerShell for an isolated clipboard and are skipped
otherwise. On a Mac, run the three commands above.

The `gui` feature builds the settings window; without it only the agent is
built.

Run a development build without installing it:

```sh
cargo run --features gui --bin seam
```

The window starts `seam-agent` from the same directory when no agent is
running. On macOS the development agent needs its own Accessibility
permission (System Settings → Privacy & Security → Accessibility); stop any
installed copy first (`launchctl bootout gui/$(id -u)/dev.seam.agent`).

## Releases

Releases are built locally on Windows; the repository has no build service.
You need Inno Setup 6, the GitHub CLI (`gh auth login`) and the macOS lint
target (`rustup target add aarch64-apple-darwin`).

1. Set `version` in `Cargo.toml`, add a `## <version>` section to
   `CHANGELOG.md`, and commit.
2. From an elevated PowerShell, run
   `powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Publish`.
   It runs every check, builds the setup program, tags the commit
   `v<version>` and attaches the installer and `SHA256SUMS.txt` to a draft
   release with that section as its notes.
3. Install the draft's setup program on two computers, then publish the
   draft on GitHub.

The Mac app is not part of releases yet: `scripts/build-macos.sh --dmg`
builds a disk image on a Mac.

## Guidelines

- Keep work off the input path: no timers, allocation or logging per event.
- Anything that can hold a key or button down on another computer must have a
  release path when the connection ends.
- Breaking wire-protocol changes bump `PROTOCOL_VERSION` in `protocol.rs`.
- Configuration changes must keep reading older `config.json` files.
