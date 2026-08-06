# dprint-plugin-arity

A [dprint](https://dprint.dev) Wasm plugin that wraps the
[arity](https://arity.cc) formatter for the R language (`.R`).

It is released independently of the main arity CLI. The plugin lives in its own
repository so that its `plugin.wasm` release asset does not interfere with
arity's own GitHub release stream, which the VS Code extension and the install
scripts resolve platform binaries from.

## Usage

Add the plugin with the dprint CLI:

```bash
dprint config add jolars/arity
```

This adds a versioned, checksummed entry under `plugins` in your `dprint.json`:

```jsonc
{
  "arity": {},
  "plugins": [
    "https://plugins.dprint.dev/jolars/arity-x.x.x.wasm@<checksum>"
  ]
}
```

Then format:

```bash
dprint fmt
```

## Configuration

Configure under the `arity` key in `dprint.json`. Supported keys:

| Key               | Values                        | Default                       |
| ----------------- | ----------------------------- | ----------------------------- |
| `lineWidth`       | integer                       | dprint global, else `80`      |
| `indentWidth`     | integer                       | dprint global, else `2`       |
| `lineEnding`      | `auto`, `lf`, `crlf`, `native`| from global `newLineKind`     |
| `roxygenMarkdown` | boolean                       | `false`                       |

Arity always indents with spaces, so dprint's global `useTabs` has no effect.

Formatting is deterministic and rule-based: the input's existing line breaks
never influence the result. See
[the arity docs](https://arity.cc) for the formatting rules themselves.

### `roxygenMarkdown`

Set this to `true` when the package enables roxygen markdown package-wide, i.e.
when `DESCRIPTION` has:

```
Roxygen: list(markdown = TRUE)
```

The `arity` CLI discovers this on its own by reading `DESCRIPTION` and
`man/roxygen/meta.R`. A dprint Wasm plugin cannot: dprint hands plugins the file
*path* but the Wasm sandbox exposes no filesystem imports at all, so the setting
has to be stated explicitly here. With it set, the plugin's output is
byte-identical to `arity format`.

Per-block `@md` and `@noMd` tags still override this default, exactly as in the
CLI, so a package that opts in per block needs no configuration.

## Building

The plugin is only usable when built for `wasm32-unknown-unknown`:

```bash
cargo build --release --target wasm32-unknown-unknown
```

The resulting `target/wasm32-unknown-unknown/release/dprint_plugin_arity.wasm`
is published as `plugin.wasm` on each GitHub release.

It also builds for the host target — `generate_plugin_code!` is gated to
`target_arch = "wasm32"` — so `cargo test` can run the config, formatting, and
schema tests natively. That native build is not a usable plugin.

## License

MIT
