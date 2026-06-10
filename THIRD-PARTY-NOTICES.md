# Third-party notices

## scrcpy server

This project bundles `assets/scrcpy-server` (the scrcpy device-side server,
release v4.0) from:

- Genymobile/scrcpy — https://github.com/Genymobile/scrcpy

scrcpy is licensed under the **Apache License, Version 2.0**. The full license
text is available at:

- https://www.apache.org/licenses/LICENSE-2.0
- https://github.com/Genymobile/scrcpy/blob/master/LICENSE

The server jar is redistributed unmodified and is pushed to the device at
runtime to capture the screen. All credit for the scrcpy server (and for the
Meta Quest black-screen fix it contains) goes to the scrcpy authors.

## Rust crates

Built on the Rust crate ecosystem, including `eframe`/`egui`, `wgpu`,
`windows`, `cpal`, `clap`, `image`, `anyhow`, and `crossbeam-channel`, each
under their respective MIT/Apache-2.0 licenses.
