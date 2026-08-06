# Agent instructions

This file provides guidance to coding agents when working with code in this
repository.

## What this is

A thin [dprint](https://dprint.dev) Wasm plugin that wraps the
[`arity-formatter`](https://crates.io/crates/arity-formatter) crate so the arity
R formatter can run inside dprint. The plugin holds no formatting logic of its
own; it maps dprint configuration onto an `arity_formatter::FormatStyle` plus an
`arity_parser::parser::ParseOptions` and forwards the file text.

This crate is released independently of the main arity CLI (which lives in the
`jolars/arity` repo). The separate repo exists so the `plugin.wasm` release
asset does not pollute arity's `v*` GitHub release stream, which the VS Code
extension and install scripts resolve platform binaries from.

## Build, lint, test

Only the `wasm32-unknown-unknown` target produces a usable plugin (the target is
pinned in `rust-toolchain.toml`). The crate *also* builds for the host target —
`generate_plugin_code!` is cfg-gated to `target_arch = "wasm32"` — so a native
`cargo build`/`cargo test` compiles the library without the plugin entrypoints.
That native build exists to run the tests; it is not a usable plugin artifact.

```bash
cargo build --release --target wasm32-unknown-unknown   # target/wasm32-unknown-unknown/release/dprint_plugin_arity.wasm
cargo test                                              # native; config, formatting, and schema tests
cargo fmt                                               # rustfmt is a git hook
cargo clippy --all-targets -- -D warnings
```

`mod schema_tests` generates the config schema with
`schemars::schema_for!(Configuration)` and asserts the committed `schema.json`
is in sync (regenerate with `UPDATE_SCHEMA=1 cargo test`) and that the
`lineEnding` wire values stay lowercase.

Beyond the unit tests, correctness is enforced in CI
(`.github/workflows/ci.yml`) by a **parity + idempotence smoke test**: it builds
the wasm plugin, downloads the latest arity CLI release, formats the same sample
through both, and `diff`s the outputs (they must be byte-identical), then
re-runs `dprint fmt` to confirm stability. When changing config mapping, mirror
this locally. **The plugin must stay byte-for-byte identical to the CLI for
equivalent settings** — that is the only invariant that matters here.

## Architecture

Everything lives in `src/lib.rs`:

- `Configuration` — the dprint-facing config struct (camelCase,
  `deny_unknown_fields`). `lineEnding` is stored as a `String` and parsed
  lazily; it borrows its JSON schema from `arity_formatter::LineEnding` via
  `#[schemars(with = ...)]` (gated behind arity-formatter's `schema` feature) so
  the published `schema.json` tracks arity's accepted values instead of
  hand-listing them.
- `parse_line_ending` — maps the string onto `LineEnding`, pushing a
  `ConfigurationDiagnostic` on an unknown value. It runs twice: once in
  `resolve_config` purely to collect diagnostics, and again in `build_style` to
  produce the real value.
- `default_line_ending` — seeds the `lineEnding` default from dprint's global
  `newLineKind`. dprint has no equivalent of arity's `native`, so an unset
  global falls back to `auto`.
- `build_style` / `build_parse_options` — the whole config mapping.
  `ParseOptions` is `#[non_exhaustive]`, so it must be built via its builder.
- `format_text_range` — the range-format path. Two things are easy to get wrong
  here and are covered by tests: `arity_formatter::format_range` **widens** the
  requested range to node boundaries and reports what it actually covered (so
  splice over the *returned* range, not the requested one), and it does **not**
  apply line endings (its text is always LF, so the target ending must be
  applied to the replacement before splicing or a CRLF file grows LF islands).
- `SyncPluginHandler` impl — `resolve_config` reads the dprint globals and
  validates; `format` decodes UTF-8, dispatches to
  `arity_formatter::format_with_options` (whole file) or `format_text_range`
  (range), and returns `Ok(None)` when the output equals the input. The whole
  thing is wrapped in `catch_unwind` so an unexpected panic becomes a
  `FormatError` rather than tearing down the wasm instance.
- `generate_plugin_code!` — the wasm entrypoints, cfg-gated to
  `target_arch = "wasm32"`.

`FILE_EXTENSIONS` is the set the plugin claims in dprint. Keep it aligned with
what `arity format` itself walks (arity's `src/file_discovery.rs`: `.R`,
case-insensitive) — **not** a superset. Extensionless R files such as
`.Rprofile` are deliberately excluded so the plugin never formats something the
CLI would skip.

## The sandbox constraint

dprint Wasm plugins get exactly these host imports: `fd_write`,
`host_has_cancelled`, `host_write_buffer`, `host_format`,
`host_get_formatted_text`, `host_get_error_text`. There is **no** filesystem
access. That is why `roxygenMarkdown` is a config key rather than being
discovered from `DESCRIPTION`/`man/roxygen/meta.R` the way the CLI does it. Do
not try to add file reading here; it is not a missing feature, it is a hard
platform limit. (A dprint *process* plugin would have OS access, but that is a
different, unsandboxed, per-platform-binary product.)

## Releasing

Versioning is managed by [versionary](https://github.com/jolars/versionary)
(`versionary.jsonc`, `release-type: rust`). Pushing a `v*` tag triggers
`publish-dprint-wasm.yml`, which builds the wasm, names it `plugin.wasm`, writes
a `plugin.wasm.sha256`, copies the generated `schema.json`, and uploads all
three to the matching GitHub release. The asset **must** be named `plugin.wasm`:
that is the name the `plugins.dprint.dev` service resolves
`plugins.dprint.dev/jolars/arity-<tag>.wasm` to. The version the plugin reports,
its `update_url`, and its `config_schema_url` all come from `CARGO_PKG_VERSION`,
so the crate version must match the release tag.

When bumping `arity-formatter`, expect `build_style` and the `parse_*` helpers
to need updates if the upstream config API changed; the CI build and parity
steps exist specifically to catch that drift.
