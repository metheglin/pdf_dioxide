# pdf_dioxide

Ruby bindings for the [pdf_oxide](https://crates.io/crates/pdf_oxide) PDF engine,
built as a native extension with [Magnus](https://github.com/matsadler/magnus).

> **Naming note**: `pdf_dioxide` is a provisional name while this gem is
> private and may change before any public release. "PDFOxide" /
> "pdf_oxide" are trademarks of the upstream project (see its TRADEMARKS.md) —
> clear naming/namespace with upstream before publishing.

This is the Ruby counterpart to pdf_oxide's PyO3 bindings for Python: instead of
talking to a C ABI over FFI, the Rust types are wrapped as real Ruby objects.

| Role | Python side | Ruby side (here) |
|---|---|---|
| Binding crate | PyO3 | Magnus (rb-sys underneath) |
| Build tool | maturin | rb_sys + rake-compiler |
| Entry point | `#[pymodule]` | `#[magnus::init]` |
| Class export | `#[pyclass]` | `#[magnus::wrap]` |
| Dev build | `maturin develop` | `rake compile` |

## Requirements

- Ruby >= 3.2
- A Rust toolchain (stable; install via [rustup](https://rustup.rs))
- A C compiler for the extension link step

## Development

The extension currently depends on the sibling `pdf_oxide` working tree via a
`path` dependency in `ext/pdf_dioxide/Cargo.toml`, so the checkout is
expected to sit next to this one:

```
pdf/
├── pdf_oxide/            # the Rust crate
└── pdf_dioxide/          # this gem
```

Switch that to `pdf_oxide = "0.3"` (crates.io) before publishing.

```bash
bundle install
bundle exec rake compile   # builds ext/ into lib/pdf_dioxide/ (≈ maturin develop)
bundle exec rake test
```

`rake` with no arguments runs `compile` then `test`.

## Installation: precompiled gems only

This gem is distributed **only** as platform (native) gems with the compiled
extension bundled, so installing it never requires a Rust toolchain:

```bash
gem install pkg/pdf_dioxide-<version>-aarch64-linux.gem   # or x86_64-linux / arm64-darwin-24
```

Supported targets: `aarch64-linux` and `x86_64-linux` (each carrying binaries
for Ruby 3.4 and 4.0) plus the build host's macOS platform. The loader picks
`lib/pdf_dioxide/<major.minor>/` per Ruby version.

Building a release:

```bash
tools/build_release.sh                                            # every target
tools/verify_native_gem.sh pkg/pdf_dioxide-*-aarch64-linux.gem    # check in a Rust-less container
```

Linux targets cross-compile in Docker via [rb-sys-dock](https://github.com/oxidize-rb/rb-sys)
(`tools/cross_build.sh`); see that script's header for the mount and
Ruby-version caveats. No source gem is published — `rake build` / `rake install`
remain for local development only, and those do compile Rust.

## Usage

```ruby
require "pdf_dioxide"

doc = PdfDioxide::PdfDocument.open("report.pdf")
doc.page_count             #=> Integer
puts doc.extract_text(0)   # page index is 0-based
```

## License

MIT. pdf_oxide itself is dual-licensed MIT / Apache-2.0; keep its license notice
in any distributed build.

### Third-party data

Exception builds of this gem (versions ending in `.fixNNNN`, see CHANGELOG)
may bundle tables derived from Adobe's CMap resources
(<https://github.com/adobe-type-tools/cmap-resources>,
<https://github.com/adobe-type-tools/mapping-resources-pdf>), which carry this
notice:

> Copyright 1990-2023 Adobe. All rights reserved.
>
> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are met:
>
> Redistributions of source code must retain the above copyright notice, this
> list of conditions and the following disclaimer.
>
> Redistributions in binary form must reproduce the above copyright notice,
> this list of conditions and the following disclaimer in the documentation
> and/or other materials provided with the distribution.
>
> Neither the name of Adobe nor the names of its contributors may be used to
> endorse or promote products derived from this software without specific
> prior written permission.
>
> THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
> AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
> IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
> ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
> LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
> CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
> SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
> INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
> CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
> ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
> POSSIBILITY OF SUCH DAMAGE.
