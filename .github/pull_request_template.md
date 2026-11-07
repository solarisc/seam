## What and why

<!-- What this changes and why. Link the issue it fixes, e.g. "Fixes #123". -->

## How it was tested

<!-- New or changed tests, and what was checked by hand on which computers (Windows and/or macOS). -->

## Checklist

- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets --features gui -- -D warnings` and `cargo test --all-targets --features gui` pass (`scripts\check.ps1` on Windows)
- [ ] Nothing new runs per input event on the input path: no timers, allocation or logging
- [ ] Anything that can hold a key or button down on another computer is released when the connection ends
- [ ] User-visible changes are in `CHANGELOG.md` and the README
