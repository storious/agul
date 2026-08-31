# Third-party notices

Agul release binaries statically link Rust libraries. This notice records the
direct runtime dependency versions resolved by the checked-in
[Cargo.lock](Cargo.lock) for this source revision. Cargo selects a
target-specific subset of the transitive lock graph for each Windows, Linux,
or macOS artifact, so not every package in the lock file appears in every
binary.

Development-only dependencies are deliberately excluded. Model weights,
provider SDKs, Python Plugins, Agulater, and AgentKube are not bundled into an
Agul release archive.

| Crate | Locked version | License expression | Upstream |
| --- | --- | --- | --- |
| `atomic-write-file` | `0.3.1` | `BSD-3-Clause` | [source](https://github.com/andreacorbellini/rust-atomic-write-file) |
| `clap` | `4.6.1` | `MIT OR Apache-2.0` | [source](https://github.com/clap-rs/clap) |
| `crossterm` | `0.29.0` | `MIT` | [source](https://github.com/crossterm-rs/crossterm) |
| `ctrlc` | `3.5.2` | `MIT OR Apache-2.0` | [source](https://github.com/Detegr/rust-ctrlc) |
| `pulldown-cmark` | `0.13.4` | `MIT` | [source](https://github.com/raphlinus/pulldown-cmark) |
| `ratatui` | `0.30.2` | `MIT` | [source](https://github.com/ratatui/ratatui) |
| `ratatui-textarea` | `0.9.2` | `MIT` | [source](https://github.com/ratatui/ratatui-textarea) |
| `reqwest` | `0.12.28` | `MIT OR Apache-2.0` | [source](https://github.com/seanmonstar/reqwest) |
| `serde` | `1.0.228` | `MIT OR Apache-2.0` | [source](https://github.com/serde-rs/serde) |
| `serde_json` | `1.0.150` | `MIT OR Apache-2.0` | [source](https://github.com/serde-rs/json) |
| `syntect` | `5.3.0` | `MIT` | [source](https://github.com/trishume/syntect) |
| `textwrap` | `0.16.2` | `MIT` | [source](https://github.com/mgeisler/textwrap) |
| `tokio` | `1.53.1` | `MIT` | [source](https://github.com/tokio-rs/tokio) |
| `unicode-segmentation` | `1.13.3` | `MIT OR Apache-2.0` | [source](https://github.com/unicode-rs/unicode-segmentation) |
| `unicode-width` | `0.2.2` | `MIT OR Apache-2.0` | [source](https://github.com/unicode-rs/unicode-width) |
| `windows-sys` | `0.61.2` | `MIT OR Apache-2.0` | [source](https://github.com/microsoft/windows-rs) |

The target-specific transitive graph is predominantly MIT and Apache-2.0. The
lock graph also contains packages offered under permissive expressions that
include 0BSD, BSD-3-Clause, BSL-1.0, CDLA-Permissive-2.0, ISC, Unicode-3.0,
Unicode-DFS-2016, Unlicense, WTFPL, and Zlib terms. Some packages offer LGPL or
MPL as an alternative to MIT or Apache-2.0; Agul uses the available permissive
option. Exact package names, versions, checksums, and source registries remain
in `Cargo.lock`; each upstream repository above and each locked crate's source
distribution carries its controlling license text and copyright notices.

This summary is provided for practical release inspection and does not replace
the controlling upstream license terms.
