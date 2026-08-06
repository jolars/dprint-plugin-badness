# dprint-plugin-badness

A [dprint](https://dprint.dev) Wasm plugin that wraps the
[badness](https://badness.dev) formatter for LaTeX (`.tex`, `.sty`, `.cls`,
`.dtx`, `.ins`) and BibTeX (`.bib`).

It is released independently of the main badness CLI. The plugin lives in its own
repository so that its `plugin.wasm` release asset does not interfere with
badness's own GitHub release stream, which the VS Code extension and the install
scripts resolve platform binaries from.

## Usage

Add the plugin with the dprint CLI:

```bash
dprint config add jolars/badness
```

This adds a versioned, checksummed entry under `plugins` in your `dprint.json`:

```jsonc
{
  "badness": {},
  "plugins": [
    "https://plugins.dprint.dev/jolars/badness-x.x.x.wasm@<checksum>"
  ]
}
```

Then format:

```bash
dprint fmt
```

## Configuration

Configure under the `badness` key in `dprint.json`. The keys are `badness.toml`'s,
camelCased; the *values* keep their `badness.toml` spelling, so the two configs
agree.

| Key                    | Values                                              | Default                   |
| ---------------------- | --------------------------------------------------- | ------------------------- |
| `lineWidth`            | integer, 1–1000                                     | dprint global, else `80`  |
| `indentWidth`          | integer, 1–1000                                     | dprint global, else `2`   |
| `wrap`                 | `reflow`, `stable`, `sentence`, `semantic`, `preserve` | per file kind (see below) |
| `mathWrap`             | `auto`, `preserve`, `single-line`, `break`          | `auto`                    |
| `lineEnding`           | `auto`, `lf`, `crlf`, `native`                      | from global `newLineKind` |
| `lang`                 | BCP-47 code (`en`, `de`, `pt-BR`, …)                | unset (English)           |
| `noBreakAbbreviations` | object: language code (or `default`) → array        | `{}`                      |

Leaving `wrap` unset is meaningful, not merely a default: each file kind then
uses its own — `.tex` reflows, while `.sty`, `.cls`, `.dtx`, `.ins`, and
`*.code.tex` are code and preserve authored line breaks. Setting `wrap` applies
one mode to every kind.

badness always indents with spaces, so dprint's global `useTabs` has no effect.
`lang` and `noBreakAbbreviations` are read only by the `sentence` and `semantic`
wrap modes. See [the badness docs](https://badness.dev/reference/configuration.html)
for what each option does.

### Differences from the `badness` CLI

Output is byte-for-byte identical to `badness format` for equivalent settings,
with one exception: the CLI folds in command signatures scanned from the
document's sibling `.sty`/`.cls` files, so a macro defined by a *local* package
with a non-default arity may be laid out differently. dprint Wasm plugins have
no filesystem access, so the plugin cannot read those files. Everything defined
inside the file being formatted, and everything in badness's built-in signature
database, is unaffected.

Like the CLI, the plugin refuses a file the parser flags, rather than reshaping
around a parse error.

## Building

The plugin is only usable when built for `wasm32-unknown-unknown`:

```bash
cargo build --release --target wasm32-unknown-unknown
```

The resulting `target/wasm32-unknown-unknown/release/dprint_plugin_badness.wasm`
is published as `plugin.wasm` on each GitHub release.

It also builds for the host target — `generate_plugin_code!` is gated to
`target_arch = "wasm32"` — so `cargo test` can run the config, formatting, and
schema tests natively. That native build is not a usable plugin.

## License

MIT
