# Third-party notices

## SoundTouch

The `lavalink-server` binary statically links SoundTouch 2.3.2 through the
`soundtouch` 0.5.4 and `soundtouch-ffi` 0.4.1 Rust crates.

SoundTouch and those wrappers are licensed under the GNU Lesser General Public
License version 2.1. A copy is provided at `LICENSES/LGPL-2.1.txt`. The exact
dependency versions are pinned in `Cargo.lock`; their corresponding source is
available from:

- https://crates.io/crates/soundtouch/0.5.4
- https://crates.io/crates/soundtouch-ffi/0.4.1

This repository contains the complete source of the work using the library and
the Cargo build description needed to rebuild it against a modified compatible
SoundTouch. Binary distributors must accompany their distribution with the
notices, source or valid source offer, and relinking materials required by
LGPL-2.1 section 6.
