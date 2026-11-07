# Third-party material

Seam itself is licensed under MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE)). The repository and the Seam programs also
contain the following material from other projects, under their own licenses.

## Fonts

| What | Where | Source | License |
|---|---|---|---|
| Inter (Regular and SemiBold) | `assets/fonts/Inter-Regular.ttf`, `assets/fonts/Inter-SemiBold.ttf`, embedded in `seam` (the settings window) | [rsms/inter](https://github.com/rsms/inter), © 2016 The Inter Project Authors | SIL Open Font License 1.1: full text in [`assets/fonts/OFL.txt`](assets/fonts/OFL.txt), installed on Windows as `Inter-LICENSE.txt` |
| egui's default fonts: Hack, Noto Emoji, Ubuntu Light and an emoji icon font | Embedded in `seam` through the `epaint_default_fonts` crate (eframe's `default_fonts` feature); not committed to this repository | [emilk/egui](https://github.com/emilk/egui) | Hack: MIT. Noto Emoji: SIL Open Font License 1.1. Ubuntu Light: Ubuntu Font Licence 1.0. Emoji icon font: MIT, © 2014 John Slegers. The full texts ship with the crate. |

## Artwork

| What | Where | License |
|---|---|---|
| The Seam icon (two screens with the pointer crossing between them) | `assets/icon/seam.svg`, `assets/icon/seam-small.svg` | Original artwork made for Seam, distributed under the project's MIT OR Apache-2.0 terms |
| App icons | `assets/icon-256.png`, `assets/seam.ico`, `assets/seam.icns` | Generated from the SVGs by `tools/icons`; same terms as the icon |

## Rust dependencies

Crates from crates.io are compiled into `seam` and `seam-agent` under their own
licenses: MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 (the ICU crates' Unicode
data) and similar permissive terms, plus BSL-1.0 for the Windows clipboard
crates (`clipboard-win`, `error-code`, `windows-win`) and MPL-2.0 for
`option-ext`, used unmodified through `dirs`. `cargo metadata --format-version 1`
reports the license of every crate in the tree.
