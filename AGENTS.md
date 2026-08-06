# Agent instructions

This file provides guidance to coding agents when working with code in this
repository.

## What this is

A thin [dprint](https://dprint.dev) Wasm plugin that wraps the
[`badness-formatter`](https://crates.io/crates/badness-formatter) crate so the
badness LaTeX/BibTeX formatter can run inside dprint. The plugin holds no
formatting logic of its own; it resolves the file's kind from its path, maps
dprint configuration onto a `badness_formatter::FormatStyle`, and forwards the
file text.

`badness-formatter` is the only badness dependency: it re-exports
`badness-parser`'s `parser`, `syntax`, `semantic`, and `ast` modules plus the
`rowan` it is built against, so everything the plugin needs is reachable through
it without a second version-locked dependency.

This crate is released independently of the main badness CLI (which lives in the
`jolars/badness` repo). The separate repo exists so the `plugin.wasm` release
asset does not pollute badness's `v*` GitHub release stream, which the VS Code
extension and install scripts resolve platform binaries from.

## Build, lint, test

Only the `wasm32-unknown-unknown` target produces a usable plugin (the target is
pinned in `rust-toolchain.toml`). The crate *also* builds for the host target —
`generate_plugin_code!` is cfg-gated to `target_arch = "wasm32"` — so a native
`cargo build`/`cargo test` compiles the library without the plugin entrypoints.
That native build exists to run the tests; it is not a usable plugin artifact.

```bash
cargo build --release --target wasm32-unknown-unknown   # target/wasm32-unknown-unknown/release/dprint_plugin_badness.wasm
cargo test                                              # native; config, formatting, and schema tests
cargo fmt                                               # rustfmt is a git hook
cargo clippy --all-targets -- -D warnings
```

`mod schema_tests` generates the config schema with
`schemars::schema_for!(Configuration)` and asserts the committed `schema.json` is
in sync (regenerate with `UPDATE_SCHEMA=1 cargo test`) and that it advertises the
real defaults.

Beyond the unit tests, correctness is enforced in CI
(`.github/workflows/ci.yml`) by a **parity + idempotence smoke test**: it builds
the wasm plugin, downloads the latest badness CLI release, formats the same
samples through both, and `diff`s the outputs (they must be byte-identical), then
re-runs `dprint fmt` to confirm stability. When changing config mapping, mirror
this locally. **The plugin must stay byte-for-byte identical to the CLI for
equivalent settings** — that is the only invariant that matters here, subject to
the one documented exception below.

The smoke-test samples must not depend on a local `.sty`: the CLI folds in
signatures scanned from sibling `.sty`/`.cls` files (`disk_scope_signatures`),
which a sandboxed plugin cannot read, so such a sample would diverge legitimately
and turn the parity check into noise. That exception is documented in the README;
do not try to "fix" it here.

## Architecture

Everything lives in `src/lib.rs`:

- `FileKind` — mirrors badness's own `FileKind` (`src/file_discovery.rs`),
  resolved from `request.file_path`. Unlike a single-extension plugin this is
  load-bearing: it decides which pipeline runs (LaTeX vs BibTeX), the
  `LexConfig` (`.sty`/`.cls`/`*.code.tex` lex under an implicit `\makeatletter`,
  `.dtx` runs the docstrip mode), and the default wrap mode. `*.code.tex` is
  matched on the file *name*, since `Path::extension` sees only `tex`. Unknown
  extensions fall back to `Tex`, mirroring `file_kind_or_tex` — dprint has
  already decided the file is ours.
- `Configuration` — the dprint-facing config struct (camelCase,
  `deny_unknown_fields`). Enum-valued options are stored as `String` and parsed
  lazily. Their wire values keep `badness.toml`'s kebab-case spelling
  (`single-line`) so the two config files agree; `badness-formatter` carries no
  serde, so the accepted values are listed here rather than borrowed from it —
  the same "mirror type in the consuming crate" split badness's own `config.rs`
  and `cli.rs` use. When the formatter grows an option, add the mirror here.
- `parse_wrap` / `parse_math_wrap` / `parse_line_ending` — map a string onto the
  formatter enum, pushing a `ConfigurationDiagnostic` on an unknown value. Each
  runs twice: once in `resolve_config` purely to collect diagnostics, and again
  in `build_style` to produce the real value.
- `validate_width` — mirrors `badness.toml`'s `1..=1000` bound on both widths.
- `default_line_ending` — seeds `lineEnding` from dprint's global `newLineKind`.
  dprint has no equivalent of badness's `native`, and its `auto` means what
  badness's does, so an unset global falls back to `auto` either way.
- `build_style` — the whole config mapping. **`wrap` is resolved per file**: the
  config value if set, else `kind.default_wrap()`. That per-file resolution is
  what every call site in the badness CLI does; a plugin that hard-defaulted
  `wrap` would reflow package sources.
- `expand_to_top_level_blocks` + `format_text_range` — the range-format path,
  ported from badness's LSP (`src/lsp.rs`).
  `format_node_range_with_signatures_sentence` **assumes a block-aligned range**,
  so the selection is first widened to the cover of every `ROOT` child node it
  overlaps, and the splice covers that *expanded* range. A selection touching no
  block is a no-op. BibTeX has no range entry (badness's LSP does not offer one
  either), so `.bib` falls back to a whole-file format.
- `SyncPluginHandler` impl — `resolve_config` reads the dprint globals and
  validates; `format` decodes UTF-8, resolves the kind, and dispatches to
  `bib::format_with_style`, `format_with_style_flavored_sentence`, or
  `format_text_range`, returning `Ok(None)` when the output equals the input. The
  whole thing is wrapped in `catch_unwind` so an unexpected panic becomes a
  `FormatError` rather than tearing down the wasm instance.
- `generate_plugin_code!` — the wasm entrypoints, cfg-gated to
  `target_arch = "wasm32"`.

**Parse errors are surfaced as format errors, deliberately.** badness's formatter
only operates on a clean parse (`FormatError::ParseErrors`), and `badness format`
refuses such a file too. Do not add a fallback that passes unparseable input
through — that would hide exactly the divergence the parity test exists to catch.

`FILE_EXTENSIONS` is the set the plugin claims in dprint. Keep it aligned with
what `badness format` itself walks (badness's `src/file_discovery.rs`,
`lint_file_kind`) — **not** a superset — so the plugin never formats something
the CLI would skip.

## The sandbox constraint

dprint Wasm plugins get exactly these host imports: `fd_write`,
`host_has_cancelled`, `host_write_buffer`, `host_format`,
`host_get_formatted_text`, `host_get_error_text`. There is **no** filesystem
access. Everything the plugin needs must arrive through the config or the file
text; do not try to add file reading here — it is not a missing feature, it is a
hard platform limit. (A dprint *process* plugin would have OS access, but that is
a different, unsandboxed, per-platform-binary product.) This is also why
`badness-formatter` must stay `wasm32-unknown-unknown`-clean, an invariant
badness's own CI enforces, and why the local-package signature scope is
unavailable here.

## Bootstrapping (remove once the first release is out)

Two things are deliberately unfinished until `badness-formatter` ships the
`line_ending` release:

1. `Cargo.toml` depends on the sibling checkout by path, not on crates.io. The
   published 0.1.0 has no `FormatStyle::line_ending`, which this plugin sets.
   Switch to `badness-formatter = "0.2"` once it is published.
2. The CI parity check downloads the *latest released* badness CLI, which still
   normalizes CRLF to LF. The `crlf.tex` sample therefore diverges until a CLI
   release carrying `line-ending` exists. That is the check doing its job; do
   not weaken it — cut the badness release instead.

## Releasing

Versioning is managed by [versionary](https://github.com/jolars/versionary)
(`versionary.jsonc`, `release-type: rust`). Pushing a `v*` tag triggers
`publish-dprint-wasm.yml`, which builds the wasm, names it `plugin.wasm`, writes
a `plugin.wasm.sha256`, copies the generated `schema.json`, and uploads all three
to the matching GitHub release. The asset **must** be named `plugin.wasm`: that is
the name the `plugins.dprint.dev` service resolves
`plugins.dprint.dev/jolars/badness-<tag>.wasm` to. The version the plugin
reports, its `update_url`, and its `config_schema_url` all come from
`CARGO_PKG_VERSION`, so the crate version must match the release tag.

`bump-badness-formatter.yml` watches crates.io daily and opens a releasable
`feat:`/`fix:` PR when a new `badness-formatter` lands (dependabot deliberately
ignores that crate). When bumping, expect `build_style` and the `parse_*` helpers
to need updates if the upstream config API changed; the CI build and parity steps
exist specifically to catch that drift.
