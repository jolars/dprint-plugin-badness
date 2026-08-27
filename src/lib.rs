//! A [dprint](https://dprint.dev) Wasm plugin wrapping the
//! [badness](https://badness.dev) formatter for LaTeX and BibTeX.
//!
//! The plugin holds no formatting logic of its own. It resolves the file's kind
//! from its path, maps dprint configuration onto a
//! [`badness_formatter::FormatStyle`], and hands the file text over; layout is
//! entirely badness's business.

use std::collections::BTreeMap;
use std::path::Path;

use badness_formatter::parser::{LatexFlavor, LexConfig, parse_with_flavor};
use badness_formatter::rowan::{TextRange, TextSize};
use badness_formatter::semantic::SignatureDb;
use badness_formatter::syntax::SyntaxNode;
use badness_formatter::{FormatStyle, ItemIndent, LineEnding, MathWrap, SentenceOptions, WrapMode};
use dprint_core::configuration::{
    ConfigKeyMap, ConfigurationDiagnostic, GlobalConfiguration, NewLineKind, get_nullable_value,
    get_unknown_property_diagnostics, get_value,
};
#[cfg(target_arch = "wasm32")]
use dprint_core::generate_plugin_code;
use dprint_core::plugins::{
    CheckConfigUpdatesMessage, ConfigChange, FileMatchingInfo, FormatError, FormatResult,
    PluginInfo, PluginResolveConfigurationResult, SyncFormatRequest, SyncHostFormatRequest,
    SyncPluginHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Extensions the plugin claims in dprint.
///
/// Deliberately the same set `badness format` itself walks (badness's
/// `src/file_discovery.rs`, `lint_file_kind`) rather than a superset, so the
/// plugin never formats something the CLI would skip. `*.code.tex` needs no
/// entry of its own: dprint matches on `tex`, and [`FileKind`] splits it back
/// out from the file *name*.
const FILE_EXTENSIONS: &[&str] = &["tex", "sty", "cls", "dtx", "ins", "bib"];

/// Bounds `badness.toml` enforces on both widths (its `MIN_WIDTH`/`MAX_WIDTH`).
const MIN_WIDTH: u32 = 1;
const MAX_WIDTH: u32 = 1000;

// The fallbacks used when neither the `badness` config block nor the matching
// dprint global sets a value. They exist as functions rather than plain
// `#[serde(default)]` so the published schema advertises the real numbers
// instead of `u32`'s zero value. They mirror `badness.toml`'s defaults.
fn default_line_width() -> u32 {
    80
}
fn default_indent_width() -> u32 {
    2
}
fn default_math_wrap() -> String {
    "auto".to_string()
}
fn default_item_indent() -> String {
    "hang".to_string()
}
fn default_line_ending_value() -> String {
    "auto".to_string()
}

/// dprint-facing configuration, serialized as camelCase.
///
/// The enum-valued options are stored as `String` and parsed lazily, borrowing
/// their JSON schema from the formatter's own enums (its `schema` feature) so
/// the published `schema.json` enumerates badness's accepted values instead of
/// hand-listing them here. Those wire values are `badness.toml`'s kebab-case
/// spellings (`single-line`), so the two config files agree on everything but
/// the key casing.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Configuration {
    /// Maximum line width the layout engine targets. Defaults to dprint's
    /// global `lineWidth`, or 80 if unset.
    #[serde(default = "default_line_width")]
    line_width: u32,
    /// Number of spaces per indentation level. Defaults to dprint's global
    /// `indentWidth`, or 2 if unset. badness always indents with spaces, so
    /// dprint's global `useTabs` has no effect.
    #[serde(default = "default_indent_width")]
    indent_width: u32,
    /// How continuation lines in list items are indented from the `\item`
    /// column.
    #[serde(default = "default_item_indent")]
    #[schemars(with = "ItemIndent")]
    item_indent: String,
    /// How to lay out line breaks inside a paragraph. When unset, each file kind
    /// uses its own default — `.tex` reflows; `.sty`, `.cls`, `.dtx`, `.ins`,
    /// and `*.code.tex` are code, so they preserve authored breaks.
    #[serde(default)]
    #[schemars(with = "Option<WrapMode>")]
    wrap: Option<String>,
    /// How to lay out line breaks inside display math. `auto` derives from the
    /// effective `wrap`.
    #[serde(default = "default_math_wrap")]
    #[schemars(with = "MathWrap")]
    math_wrap: String,
    /// How the formatted line breaks are spelled. Defaults to dprint's global
    /// `newLineKind`, or `auto` (keep what the file was written with) if unset.
    #[serde(default = "default_line_ending_value")]
    #[schemars(with = "LineEnding")]
    line_ending: String,
    /// Document language as a BCP-47-style code (`en`, `de`, `pt-BR`, …), used
    /// by the `sentence` and `semantic` wrap modes to pick the
    /// sentence-boundary abbreviation profile. Ignored by other wrap modes.
    #[serde(default)]
    lang: Option<String>,
    /// Extra no-break abbreviations for the `sentence` and `semantic` wrap
    /// modes, keyed by language code or the literal `default` bucket. An
    /// abbreviation listed here never ends a sentence.
    #[serde(default)]
    no_break_abbreviations: BTreeMap<String, Vec<String>>,
}

#[derive(Default)]
pub struct BadnessHandler;

impl BadnessHandler {
    #[must_use]
    pub const fn new() -> Self {
        BadnessHandler
    }
}

/// What kind of file this is, mirroring badness's own `FileKind`
/// (`src/file_discovery.rs`). The kind decides three things the plugin cannot
/// get from the config: which pipeline runs (LaTeX or BibTeX), the lexer
/// flavor, and the default wrap mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Tex,
    CodeTex,
    Sty,
    Cls,
    Dtx,
    Ins,
    Bib,
}

impl FileKind {
    /// Resolve from a path by extension, case-insensitively, mirroring
    /// `file_kind_or_tex`: anything unrecognized is treated as `.tex`, since
    /// dprint has already decided the file is ours.
    fn from_path(path: &Path) -> Self {
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            return FileKind::Tex;
        };
        if ext.eq_ignore_ascii_case("tex") {
            if is_code_tex(path) {
                FileKind::CodeTex
            } else {
                FileKind::Tex
            }
        } else if ext.eq_ignore_ascii_case("sty") {
            FileKind::Sty
        } else if ext.eq_ignore_ascii_case("cls") {
            FileKind::Cls
        } else if ext.eq_ignore_ascii_case("dtx") {
            FileKind::Dtx
        } else if ext.eq_ignore_ascii_case("ins") {
            FileKind::Ins
        } else if ext.eq_ignore_ascii_case("bib") {
            FileKind::Bib
        } else {
            FileKind::Tex
        }
    }

    /// The [`LexConfig`] to parse this kind with: `.sty`/`.cls`/`*.code.tex` are
    /// loaded under an implicit `\makeatletter`, and `.dtx` runs the docstrip
    /// mode.
    fn lex_config(self) -> LexConfig {
        LexConfig {
            flavor: match self {
                FileKind::Sty | FileKind::Cls | FileKind::CodeTex => LatexFlavor::Package,
                _ => LatexFlavor::Document,
            },
            dtx: self == FileKind::Dtx,
        }
    }

    /// The default [`WrapMode`] when `wrap` is unset: a package/class/docstrip
    /// body is code, not prose, so it preserves authored breaks.
    fn default_wrap(self) -> WrapMode {
        match self {
            FileKind::Sty | FileKind::Cls | FileKind::Dtx | FileKind::Ins | FileKind::CodeTex => {
                WrapMode::Preserve
            }
            _ => WrapMode::Reflow,
        }
    }
}

/// Whether `path`'s file name ends with `.code.tex` (case-insensitive) — the
/// package-implementation convention (`tikz.code.tex`). Checked on the name,
/// not the extension, since `Path::extension` sees only `tex`.
fn is_code_tex(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".code.tex") && lower.len() > ".code.tex".len()
        })
}

/// Report an unknown value for `property_name`, listing what is accepted.
fn unknown_value(
    property_name: &str,
    value: &str,
    expected: &str,
    diagnostics: &mut Vec<ConfigurationDiagnostic>,
) {
    diagnostics.push(ConfigurationDiagnostic {
        property_name: property_name.to_string(),
        message: format!("Unknown value '{value}'. Expected one of: {expected}."),
    });
}

fn parse_wrap(value: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> WrapMode {
    match value.to_ascii_lowercase().as_str() {
        "reflow" => WrapMode::Reflow,
        "stable" => WrapMode::Stable,
        "sentence" => WrapMode::Sentence,
        "semantic" => WrapMode::Semantic,
        "preserve" => WrapMode::Preserve,
        other => {
            unknown_value(
                "wrap",
                other,
                "reflow, stable, sentence, semantic, preserve",
                diagnostics,
            );
            WrapMode::Reflow
        }
    }
}

fn parse_math_wrap(value: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> MathWrap {
    match value.to_ascii_lowercase().as_str() {
        "auto" => MathWrap::Auto,
        "preserve" => MathWrap::Preserve,
        "single-line" => MathWrap::SingleLine,
        "break" => MathWrap::Break,
        other => {
            unknown_value(
                "mathWrap",
                other,
                "auto, preserve, single-line, break",
                diagnostics,
            );
            MathWrap::Auto
        }
    }
}

fn parse_item_indent(value: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> ItemIndent {
    match value.to_ascii_lowercase().as_str() {
        "hang" => ItemIndent::Hang,
        "indent" => ItemIndent::Indent,
        "none" => ItemIndent::None,
        other => {
            unknown_value("itemIndent", other, "hang, indent, none", diagnostics);
            ItemIndent::Hang
        }
    }
}

fn parse_line_ending(value: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> LineEnding {
    match value.to_ascii_lowercase().as_str() {
        "auto" => LineEnding::Auto,
        "lf" => LineEnding::Lf,
        "crlf" => LineEnding::Crlf,
        "native" => LineEnding::Native,
        other => {
            unknown_value("lineEnding", other, "auto, lf, crlf, native", diagnostics);
            LineEnding::Auto
        }
    }
}

/// Maps dprint's global `newLineKind` onto a `lineEnding` default. dprint has
/// no equivalent of badness's `native`, and its `auto` means the same thing as
/// badness's, so an unset global falls back to `auto` either way.
fn default_line_ending(global_config: &GlobalConfiguration) -> String {
    match global_config.new_line_kind {
        Some(NewLineKind::LineFeed) => "lf".to_string(),
        Some(NewLineKind::CarriageReturnLineFeed) => "crlf".to_string(),
        Some(NewLineKind::Auto) | None => default_line_ending_value(),
    }
}

/// Report a width outside the range `badness.toml` accepts.
fn validate_width(property_name: &str, value: u32, diagnostics: &mut Vec<ConfigurationDiagnostic>) {
    if !(MIN_WIDTH..=MAX_WIDTH).contains(&value) {
        diagnostics.push(ConfigurationDiagnostic {
            property_name: property_name.to_string(),
            message: format!("Expected a value between {MIN_WIDTH} and {MAX_WIDTH}, got {value}."),
        });
    }
}

/// The style for a file of `kind`. `wrap` is resolved *per file* — the config
/// value if set, else the kind's own default — exactly as every call site in
/// the badness CLI does it.
fn build_style(cfg: &Configuration, kind: FileKind) -> FormatStyle {
    // Diagnostics were already reported at resolve time; discard them here.
    let mut throwaway = Vec::new();
    FormatStyle {
        line_width: cfg.line_width as usize,
        indent_width: cfg.indent_width as usize,
        item_indent: parse_item_indent(&cfg.item_indent, &mut throwaway),
        wrap: match &cfg.wrap {
            Some(value) => parse_wrap(value, &mut throwaway),
            None => kind.default_wrap(),
        },
        math_wrap: parse_math_wrap(&cfg.math_wrap, &mut throwaway),
        line_ending: parse_line_ending(&cfg.line_ending, &mut throwaway),
    }
}

/// Expand a selection to whole top-level-block boundaries: the cover of every
/// `ROOT` child *node* overlapping it. Range formatting's safe zone — the
/// formatter never lays out a fragment of a block, and
/// `format_node_range_with_signatures_sentence` assumes an already block-aligned
/// range. `None` when the selection touches no block, meaning there is nothing
/// to format. Ported from badness's LSP (`src/lsp.rs`,
/// `expand_to_top_level_blocks`).
fn expand_to_top_level_blocks(root: &SyntaxNode, sel: TextRange) -> Option<TextRange> {
    let mut acc: Option<TextRange> = None;
    for child in root.children() {
        let r = child.text_range();
        // A cursor (empty selection) hits the block whose range contains it
        // (touch-inclusive); a non-empty selection hits any block it overlaps.
        let hit = if sel.is_empty() {
            r.contains_inclusive(sel.start())
        } else {
            sel.start() < r.end() && r.start() < sel.end()
        };
        if hit {
            acc = Some(acc.map_or(r, |a| a.cover(r)));
        }
    }
    acc
}

/// Formats only the blocks `range` touches, splicing the result back into
/// `text`.
///
/// The splice covers the *expanded* block range, not the requested one: the
/// formatter lays out whole top-level blocks, so a partial selection always
/// pulls in the structural units it touches.
fn format_text_range(
    text: &str,
    range: std::ops::Range<usize>,
    style: FormatStyle,
    kind: FileKind,
    sentence: SentenceOptions<'_>,
) -> Result<Option<String>, FormatError> {
    let start = TextSize::try_from(range.start)
        .map_err(|_| FormatError::new("format range start does not fit in the file"))?;
    let end = TextSize::try_from(range.end)
        .map_err(|_| FormatError::new("format range end does not fit in the file"))?;
    if start > end {
        return Err(FormatError::new("format range start is after its end"));
    }
    if usize::from(end) > text.len() {
        return Err(FormatError::new(
            "format range extends past the end of file",
        ));
    }

    let parsed = parse_with_flavor(text, kind.lex_config());
    if !parsed.errors.is_empty() {
        return Err(FormatError::new(format!(
            "input contains {} parser diagnostic(s); the formatter only supports parseable input",
            parsed.errors.len()
        )));
    }
    let root = parsed.syntax();

    let Some(block_range) = expand_to_top_level_blocks(&root, TextRange::new(start, end)) else {
        return Ok(None);
    };

    let fragment = badness_formatter::formatter::format_node_range_with_signatures_sentence(
        &root,
        style,
        &SignatureDb::default(),
        block_range,
        sentence,
    )
    .map_err(|e| FormatError::new(e.to_string()))?;

    let replaced_start = usize::from(block_range.start());
    let replaced_end = usize::from(block_range.end());

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..replaced_start]);
    out.push_str(&fragment);
    out.push_str(&text[replaced_end..]);
    Ok(Some(out))
}

impl SyncPluginHandler<Configuration> for BadnessHandler {
    fn resolve_config(
        &mut self,
        config: ConfigKeyMap,
        global_config: &GlobalConfiguration,
    ) -> PluginResolveConfigurationResult<Configuration> {
        let mut config = config;
        let mut diagnostics = Vec::new();

        let line_width: u32 = get_value(
            &mut config,
            "lineWidth",
            global_config.line_width.unwrap_or_else(default_line_width),
            &mut diagnostics,
        );
        let indent_width: u32 = get_value(
            &mut config,
            "indentWidth",
            global_config
                .indent_width
                .map(u32::from)
                .unwrap_or_else(default_indent_width),
            &mut diagnostics,
        );
        let wrap: Option<String> = get_nullable_value(&mut config, "wrap", &mut diagnostics);
        let item_indent: String = get_value(
            &mut config,
            "itemIndent",
            default_item_indent(),
            &mut diagnostics,
        );
        let math_wrap: String = get_value(
            &mut config,
            "mathWrap",
            default_math_wrap(),
            &mut diagnostics,
        );
        let line_ending: String = get_value(
            &mut config,
            "lineEnding",
            default_line_ending(global_config),
            &mut diagnostics,
        );
        let lang: Option<String> = get_nullable_value(&mut config, "lang", &mut diagnostics);
        let no_break_abbreviations: BTreeMap<String, Vec<String>> =
            match config.shift_remove("noBreakAbbreviations") {
                None => BTreeMap::new(),
                Some(value) => match serde_json::to_value(&value)
                    .ok()
                    .and_then(|v| serde_json::from_value(v).ok())
                {
                    Some(map) => map,
                    None => {
                        diagnostics.push(ConfigurationDiagnostic {
                            property_name: "noBreakAbbreviations".to_string(),
                            message: "Expected an object mapping a language code (or \"default\") \
                                      to an array of abbreviations."
                                .to_string(),
                        });
                        BTreeMap::new()
                    }
                },
            };

        // Re-run the parses purely to surface diagnostics for bad values.
        validate_width("lineWidth", line_width, &mut diagnostics);
        validate_width("indentWidth", indent_width, &mut diagnostics);
        if let Some(value) = &wrap {
            let _ = parse_wrap(value, &mut diagnostics);
        }
        let _ = parse_item_indent(&item_indent, &mut diagnostics);
        let _ = parse_math_wrap(&math_wrap, &mut diagnostics);
        let _ = parse_line_ending(&line_ending, &mut diagnostics);

        diagnostics.extend(get_unknown_property_diagnostics(config));

        PluginResolveConfigurationResult {
            config: Configuration {
                line_width,
                indent_width,
                item_indent,
                wrap,
                math_wrap,
                line_ending,
                lang,
                no_break_abbreviations,
            },
            diagnostics,
            file_matching: FileMatchingInfo {
                file_extensions: FILE_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
                file_names: Vec::new(),
            },
        }
    }

    fn plugin_info(&mut self) -> PluginInfo {
        let version = env!("CARGO_PKG_VERSION").to_string();
        PluginInfo {
            name: env!("CARGO_PKG_NAME").to_string(),
            version: version.clone(),
            config_key: "badness".to_string(),
            help_url: "https://badness.dev".to_string(),
            config_schema_url: format!(
                "https://github.com/jolars/dprint-plugin-badness/releases/download/v{version}/schema.json"
            ),
            update_url: Some("https://plugins.dprint.dev/jolars/badness/latest.json".to_string()),
        }
    }

    fn license_text(&mut self) -> String {
        include_str!("../LICENSE").to_string()
    }

    fn check_config_updates(
        &self,
        _message: CheckConfigUpdatesMessage,
    ) -> Result<Vec<ConfigChange>, FormatError> {
        Ok(Vec::new())
    }

    fn format(
        &mut self,
        request: SyncFormatRequest<Configuration>,
        _format_with_host: impl FnMut(SyncHostFormatRequest) -> FormatResult,
    ) -> FormatResult {
        let text = String::from_utf8(request.file_bytes)
            .map_err(|e| FormatError::new(format!("input is not valid UTF-8: {e}")))?;

        let kind = FileKind::from_path(request.file_path);
        let style = build_style(request.config, kind);
        let cfg = request.config;

        // badness's API is `Result`-returning, so this is belt-and-braces: it
        // keeps an unexpected panic from tearing down the wasm instance.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // `scratch` owns the merged abbreviation entries for the call; the
            // resolved options borrow from it, so it must outlive them.
            let mut scratch = Vec::new();
            let sentence = SentenceOptions::resolve(
                cfg.lang.as_deref(),
                &cfg.no_break_abbreviations,
                &mut scratch,
            );
            match (kind, request.range) {
                // BibTeX has no range entry (badness's LSP does not offer one
                // either); format the whole file instead.
                (FileKind::Bib, _) => badness_formatter::bib::format_with_style(&text, style)
                    .map(Some)
                    .map_err(|e| FormatError::new(e.to_string())),
                (_, None) => badness_formatter::formatter::format_with_style_flavored_sentence(
                    &text,
                    style,
                    kind.lex_config(),
                    sentence,
                )
                .map(Some)
                .map_err(|e| FormatError::new(e.to_string())),
                (_, Some(range)) => format_text_range(&text, range, style, kind, sentence),
            }
        }));

        let formatted = match result {
            Ok(formatted) => formatted?,
            Err(payload) => {
                let message = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "badness panicked while formatting".to_string()
                };
                return Err(FormatError::new(format!("badness panicked: {message}")));
            }
        };

        match formatted {
            Some(formatted) if formatted != text => Ok(Some(formatted.into_bytes())),
            _ => Ok(None),
        }
    }
}

#[cfg(target_arch = "wasm32")]
generate_plugin_code!(BadnessHandler, BadnessHandler::new());

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Configuration {
        Configuration {
            line_width: default_line_width(),
            indent_width: default_indent_width(),
            item_indent: default_item_indent(),
            wrap: None,
            math_wrap: default_math_wrap(),
            line_ending: default_line_ending_value(),
            lang: None,
            no_break_abbreviations: BTreeMap::new(),
        }
    }

    fn format_all(cfg: &Configuration, path: &str, text: &str) -> String {
        let kind = FileKind::from_path(Path::new(path));
        let style = build_style(cfg, kind);
        let mut scratch = Vec::new();
        let sentence = SentenceOptions::resolve(
            cfg.lang.as_deref(),
            &cfg.no_break_abbreviations,
            &mut scratch,
        );
        match kind {
            FileKind::Bib => badness_formatter::bib::format_with_style(text, style)
                .expect("bib format should succeed"),
            _ => badness_formatter::formatter::format_with_style_flavored_sentence(
                text,
                style,
                kind.lex_config(),
                sentence,
            )
            .expect("format should succeed"),
        }
    }

    #[test]
    fn file_kinds_follow_the_cli() {
        for (path, expected) in [
            ("doc.tex", FileKind::Tex),
            ("DOC.TEX", FileKind::Tex),
            ("tikz.code.tex", FileKind::CodeTex),
            ("TIKZ.CODE.TEX", FileKind::CodeTex),
            // `.code.tex` on its own is a plain document, not an implementation
            // file — the name must be *longer* than the suffix.
            ("code.tex", FileKind::Tex),
            ("pkg.sty", FileKind::Sty),
            ("cls.cls", FileKind::Cls),
            ("src.dtx", FileKind::Dtx),
            ("src.ins", FileKind::Ins),
            ("refs.bib", FileKind::Bib),
            // dprint only hands us files it matched, so anything else is a
            // document (mirrors `file_kind_or_tex`).
            ("Makefile", FileKind::Tex),
        ] {
            assert_eq!(FileKind::from_path(Path::new(path)), expected, "{path}");
        }
    }

    #[test]
    fn wrap_defaults_per_file_kind() {
        let cfg = config();
        // Prose reflows in a document...
        let tex = format_all(&cfg, "doc.tex", "one\ntwo\nthree\n");
        assert_eq!(tex, "one two three\n");
        // ...but a package body is code, so authored breaks survive.
        for path in ["pkg.sty", "cls.cls", "src.ins", "tikz.code.tex"] {
            let out = format_all(&cfg, path, "one\ntwo\nthree\n");
            assert_eq!(out, "one\ntwo\nthree\n", "for {path}");
        }
    }

    #[test]
    fn explicit_wrap_overrides_the_file_kind_default() {
        let mut cfg = config();
        cfg.wrap = Some("reflow".to_string());
        assert_eq!(format_all(&cfg, "pkg.sty", "one\ntwo\n"), "one two\n");

        cfg.wrap = Some("preserve".to_string());
        assert_eq!(format_all(&cfg, "doc.tex", "one\ntwo\n"), "one\ntwo\n");
    }

    #[test]
    fn package_flavor_lexes_at_as_a_letter() {
        let cfg = config();
        // `\my@macro` is one control word under `\makeatletter`; the formatter
        // refuses nothing here, but the `.sty` flavor must reach the parser.
        let out = format_all(&cfg, "pkg.sty", "\\def\\my@macro{x}\n");
        assert_eq!(out, "\\def\\my@macro{x}\n");
    }

    #[test]
    fn package_preserve_mode_keeps_command_body_breaks() {
        let cfg = config();
        let input = "\\ProvidesPackage{pkg}\n\\newcommand{\\my@helper}[1]{%\n  #1}\n";
        assert_eq!(format_all(&cfg, "pkg.sty", input), input);
    }

    #[test]
    fn bib_files_route_to_the_bib_formatter() {
        let cfg = config();
        let out = format_all(&cfg, "refs.bib", "@misc{k, t = {x}}\n");
        assert_eq!(out, "@misc{k,\n  t = {x}\n}\n");
    }

    #[test]
    fn honors_indent_width() {
        let mut cfg = config();
        cfg.indent_width = 4;
        let out = format_all(&cfg, "refs.bib", "@misc{k, t = {x}}\n");
        assert_eq!(out, "@misc{k,\n    t = {x}\n}\n");
    }

    #[test]
    fn item_indent_is_honored() {
        let mut cfg = config();
        cfg.wrap = Some("sentence".to_string());
        cfg.item_indent = "none".to_string();
        let out = format_all(
            &cfg,
            "doc.tex",
            "\\begin{itemize}\n\\item First sentence. Second sentence.\n\\end{itemize}\n",
        );
        assert_eq!(
            out,
            "\\begin{itemize}\n  \\item First sentence.\n  Second sentence.\n\\end{itemize}\n"
        );
    }

    #[test]
    fn honors_line_width() {
        let mut cfg = config();
        cfg.line_width = 20;
        let out = format_all(&cfg, "doc.tex", "alpha beta gamma delta epsilon zeta\n");
        assert!(
            out.lines().all(|l| l.len() <= 20),
            "expected a wrap at 20 columns, got {out:?}"
        );
        assert!(out.contains('\n'));
    }

    #[test]
    fn math_wrap_is_honored() {
        let mut cfg = config();
        cfg.math_wrap = "preserve".to_string();
        let out = format_all(&cfg, "doc.tex", "\\[\n  a\n  + b\n\\]\n");
        assert!(out.contains("a\n"), "authored math breaks kept: {out:?}");
    }

    #[test]
    fn crlf_input_round_trips_under_auto() {
        let cfg = config();
        let out = format_all(&cfg, "doc.tex", "one   two\r\n\r\nthree\r\n");
        assert!(out.contains("\r\n"));
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "no bare LF: {out:?}"
        );
    }

    #[test]
    fn explicit_line_ending_overrides_the_source() {
        let mut cfg = config();
        cfg.line_ending = "lf".to_string();
        let out = format_all(&cfg, "doc.tex", "one\r\n\r\ntwo\r\n");
        assert_eq!(out, "one\n\ntwo\n");

        cfg.line_ending = "crlf".to_string();
        let out = format_all(&cfg, "doc.tex", "one\n\ntwo\n");
        assert_eq!(out, "one\r\n\r\ntwo\r\n");
    }

    #[test]
    fn default_line_ending_follows_the_dprint_global() {
        for (kind, expected) in [
            (Some(NewLineKind::LineFeed), "lf"),
            (Some(NewLineKind::CarriageReturnLineFeed), "crlf"),
            (Some(NewLineKind::Auto), "auto"),
            (None, "auto"),
        ] {
            let global = GlobalConfiguration {
                line_width: None,
                use_tabs: None,
                indent_width: None,
                new_line_kind: kind,
            };
            assert_eq!(default_line_ending(&global), expected, "for {kind:?}");
        }
    }

    #[test]
    fn sentence_wrap_honors_lang_and_user_abbreviations() {
        let mut cfg = config();
        cfg.wrap = Some("sentence".to_string());
        let out = format_all(&cfg, "doc.tex", "One. Two.\n");
        assert_eq!(out, "One.\nTwo.\n");

        // An abbreviation never ends a sentence, so no break follows it.
        cfg.no_break_abbreviations
            .insert("default".to_string(), vec!["approx.".to_string()]);
        let out = format_all(&cfg, "doc.tex", "See approx. five. Next.\n");
        assert_eq!(out, "See approx. five.\nNext.\n");
    }

    #[test]
    fn range_format_only_touches_the_blocks_it_hits() {
        let cfg = config();
        let kind = FileKind::Tex;
        let style = build_style(&cfg, kind);
        let text = "first    paragraph.\n\nsecond    paragraph.\n";
        let second = text.find("second").expect("offset");

        let out = format_text_range(
            text,
            second..text.len(),
            style,
            kind,
            SentenceOptions::default(),
        )
        .expect("formats")
        .expect("a block was hit");

        assert!(out.starts_with("first    paragraph."), "{out:?}");
        assert!(out.contains("second paragraph."), "{out:?}");
    }

    #[test]
    fn range_between_blocks_is_a_no_op() {
        let cfg = config();
        let kind = FileKind::Tex;
        let style = build_style(&cfg, kind);
        // The gap between the two blocks is trivia, owned by no block.
        let text = "first.\n\n\n\nsecond.\n";
        let gap = text.find("\n\n\n\n").expect("offset") + 2;

        let out = format_text_range(text, gap..gap, style, kind, SentenceOptions::default())
            .expect("formats");
        assert!(out.is_none(), "expected no edit, got {out:?}");
    }

    #[test]
    fn range_outside_the_file_is_an_error() {
        let cfg = config();
        let kind = FileKind::Tex;
        let style = build_style(&cfg, kind);
        let text = "short.\n";
        assert!(format_text_range(text, 0..999, style, kind, SentenceOptions::default()).is_err());
        // Built from bindings so clippy does not read it as a literal empty
        // range; a reversed range is exactly what this asserts on.
        let (start, end) = (5, 1);
        assert!(
            format_text_range(text, start..end, style, kind, SentenceOptions::default()).is_err()
        );
    }

    #[test]
    fn unparseable_input_is_an_error() {
        // badness refuses input the parser flagged, exactly as the CLI does;
        // the plugin surfaces that rather than passing the file through.
        let text = "\\begin{itemize}\n\\item a\n";
        let kind = FileKind::Tex;
        assert!(
            badness_formatter::formatter::format_with_style_flavored_sentence(
                text,
                build_style(&config(), kind),
                kind.lex_config(),
                SentenceOptions::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_values_report_a_diagnostic() {
        let mut diagnostics = Vec::new();
        let _ = parse_wrap("smart", &mut diagnostics);
        let _ = parse_item_indent("deep", &mut diagnostics);
        let _ = parse_math_wrap("never", &mut diagnostics);
        let _ = parse_line_ending("cr", &mut diagnostics);
        let names: Vec<_> = diagnostics
            .iter()
            .map(|d| d.property_name.as_str())
            .collect();
        assert_eq!(names, ["wrap", "itemIndent", "mathWrap", "lineEnding"]);
    }

    #[test]
    fn out_of_range_widths_report_a_diagnostic() {
        let mut diagnostics = Vec::new();
        validate_width("lineWidth", 0, &mut diagnostics);
        validate_width("indentWidth", 1001, &mut diagnostics);
        validate_width("lineWidth", 80, &mut diagnostics);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].property_name, "lineWidth");
        assert_eq!(diagnostics[1].property_name, "indentWidth");
    }

    #[test]
    fn resolve_config_reads_globals_and_reports_unknown_keys() {
        let mut handler = BadnessHandler::new();
        let global = GlobalConfiguration {
            line_width: Some(100),
            use_tabs: Some(true),
            indent_width: Some(4),
            new_line_kind: Some(NewLineKind::LineFeed),
        };
        let mut map = ConfigKeyMap::new();
        map.insert("nope".to_string(), 1.into());

        let result = handler.resolve_config(map, &global);
        assert_eq!(result.config.line_width, 100);
        assert_eq!(result.config.indent_width, 4);
        assert_eq!(result.config.line_ending, "lf");
        assert_eq!(result.config.wrap, None);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].property_name, "nope");
        assert_eq!(
            result.file_matching.file_extensions,
            FILE_EXTENSIONS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod schema_tests {
    use super::Configuration;
    use dprint_core::configuration::ConfigurationDiagnostic;

    const SCHEMA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/schema.json");

    fn generated_schema() -> String {
        let schema = schemars::schema_for!(Configuration);
        let mut out = serde_json::to_string_pretty(&schema).expect("schema should serialize");
        out.push('\n');
        out
    }

    #[test]
    fn committed_schema_is_in_sync() {
        let generated = generated_schema();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(SCHEMA_PATH, &generated).expect("schema should be writable");
            return;
        }
        let committed = std::fs::read_to_string(SCHEMA_PATH)
            .expect("schema.json should exist; run `UPDATE_SCHEMA=1 cargo test` to create it");
        assert_eq!(
            committed, generated,
            "schema.json is stale; regenerate with `UPDATE_SCHEMA=1 cargo test`"
        );
    }

    /// The `const` values of a borrowed enum schema, in declaration order.
    fn advertised_values(schema: &serde_json::Value, name: &str) -> Vec<String> {
        schema["$defs"][name]["oneOf"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} should be a borrowed enum schema"))
            .iter()
            .map(|variant| variant["const"].as_str().expect("a const").to_string())
            .collect()
    }

    /// The whole point of borrowing the formatter's schemas: every value the
    /// schema advertises has to be one the plugin can actually parse. When
    /// badness grows a wrap mode, the regenerated schema gains a value the
    /// matching `parse_*` does not know yet, and this fails — the schema and
    /// the mapping cannot drift apart silently.
    #[test]
    fn every_advertised_value_parses() {
        let schema: serde_json::Value =
            serde_json::from_str(&generated_schema()).expect("valid JSON");

        for name in ["WrapMode", "ItemIndent", "MathWrap", "LineEnding"] {
            let values = advertised_values(&schema, name);
            assert!(!values.is_empty(), "{name} advertises no values");
            for value in values {
                let mut diagnostics: Vec<ConfigurationDiagnostic> = Vec::new();
                match name {
                    "WrapMode" => {
                        super::parse_wrap(&value, &mut diagnostics);
                    }
                    "ItemIndent" => {
                        super::parse_item_indent(&value, &mut diagnostics);
                    }
                    "MathWrap" => {
                        super::parse_math_wrap(&value, &mut diagnostics);
                    }
                    _ => {
                        super::parse_line_ending(&value, &mut diagnostics);
                    }
                }
                assert!(
                    diagnostics.is_empty(),
                    "{name} advertises '{value}', which the plugin rejects: {}",
                    diagnostics[0].message
                );
            }
        }
    }

    #[test]
    fn schema_advertises_the_real_defaults() {
        let schema: serde_json::Value =
            serde_json::from_str(&generated_schema()).expect("valid JSON");
        let props = &schema["properties"];
        assert_eq!(props["lineWidth"]["default"], 80);
        assert_eq!(props["indentWidth"]["default"], 2);
        assert_eq!(props["itemIndent"]["default"], "hang");
        assert_eq!(props["mathWrap"]["default"], "auto");
        assert_eq!(props["lineEnding"]["default"], "auto");
        assert_eq!(schema["additionalProperties"], false);
    }
}
