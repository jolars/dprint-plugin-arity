//! A [dprint](https://dprint.dev) Wasm plugin wrapping the
//! [arity](https://arity.cc) formatter for the R language.
//!
//! The plugin holds no formatting logic of its own. It maps dprint
//! configuration onto an [`arity_formatter::FormatStyle`] plus an
//! [`arity_formatter::parser::ParseOptions`] and hands the file text over; layout
//! is entirely arity's business.

use arity_formatter::parser::ParseOptions;
use arity_formatter::rowan::{TextRange, TextSize};
use arity_formatter::{FormatStyle, LineEnding};
use dprint_core::configuration::{
    ConfigKeyMap, ConfigurationDiagnostic, GlobalConfiguration, NewLineKind,
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
/// Deliberately the same set `arity format` itself walks (see arity's
/// `src/file_discovery.rs`, which matches `.R` case-insensitively) rather than
/// a superset. Extensionless R files such as `.Rprofile` are left alone so the
/// plugin never formats something the CLI would skip.
const FILE_EXTENSIONS: &[&str] = &["R", "r"];

// The fallbacks used when neither the `arity` config block nor the matching
// dprint global sets a value. They exist as functions rather than plain
// `#[serde(default)]` so the published schema advertises the real numbers
// instead of `u32`/`String`'s zero values.
fn default_line_width() -> u32 {
    80
}

fn default_indent_width() -> u32 {
    2
}

fn default_line_ending_value() -> String {
    "auto".to_string()
}

/// dprint-facing configuration, serialized as camelCase.
///
/// `lineEnding` is stored as a `String` and parsed lazily, borrowing its JSON
/// schema from [`LineEnding`] so the published `schema.json` tracks arity's
/// accepted values instead of hand-listing them.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Configuration {
    /// Maximum line width the layout engine targets. Defaults to dprint's
    /// global `lineWidth`, or 80 if unset.
    #[serde(default = "default_line_width")]
    line_width: u32,
    /// Number of spaces per indentation level. Defaults to dprint's global
    /// `indentWidth`, or 2 if unset. Arity always indents with spaces, so
    /// dprint's global `useTabs` has no effect.
    #[serde(default = "default_indent_width")]
    indent_width: u32,
    /// Line-ending style for formatted output. Defaults to dprint's global
    /// `newLineKind`, or `auto` if unset.
    #[serde(default = "default_line_ending_value")]
    #[schemars(with = "LineEnding")]
    line_ending: String,
    /// Whether roxygen comments are markdown by default, i.e. whether the
    /// package sets `Roxygen: list(markdown = TRUE)`.
    ///
    /// The `arity` CLI discovers this by reading `DESCRIPTION` and
    /// `man/roxygen/meta.R`. dprint Wasm plugins run in a sandbox with no
    /// filesystem access, so here it has to be stated explicitly. A block's own
    /// `@md`/`@noMd` tag still wins over this default, exactly as in the CLI.
    #[serde(default)]
    roxygen_markdown: bool,
}

#[derive(Default)]
pub struct ArityHandler;

impl ArityHandler {
    #[must_use]
    pub const fn new() -> Self {
        ArityHandler
    }
}

/// Parses a `lineEnding` config value, reporting a diagnostic on an unknown one.
///
/// The wire values are [`LineEnding`]'s own serde spellings (lowercase), so
/// they stay in step with the schema borrowed from it.
fn parse_line_ending(value: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> LineEnding {
    match value.to_ascii_lowercase().as_str() {
        "auto" => LineEnding::Auto,
        "lf" => LineEnding::Lf,
        "crlf" => LineEnding::Crlf,
        "native" => LineEnding::Native,
        other => {
            diagnostics.push(ConfigurationDiagnostic {
                property_name: "lineEnding".to_string(),
                message: format!(
                    "Unknown line ending '{other}'. Expected one of: auto, lf, crlf, native."
                ),
            });
            LineEnding::Auto
        }
    }
}

/// Maps dprint's global `newLineKind` onto a `lineEnding` default.
///
/// dprint has no equivalent of arity's `native`, so an unset global falls back
/// to `auto`.
fn default_line_ending(global_config: &GlobalConfiguration) -> String {
    match global_config.new_line_kind {
        Some(NewLineKind::LineFeed) => "lf".to_string(),
        Some(NewLineKind::CarriageReturnLineFeed) => "crlf".to_string(),
        Some(NewLineKind::Auto) | None => default_line_ending_value(),
    }
}

fn build_style(cfg: &Configuration) -> FormatStyle {
    // Diagnostics were already reported at resolve time; discard them here.
    let mut throwaway = Vec::new();
    FormatStyle {
        line_width: cfg.line_width as usize,
        indent_width: cfg.indent_width as usize,
        line_ending: parse_line_ending(&cfg.line_ending, &mut throwaway),
    }
}

fn build_parse_options(cfg: &Configuration) -> ParseOptions {
    // `ParseOptions` is `#[non_exhaustive]`, so it has to be built by builder.
    ParseOptions::default().with_roxygen_markdown_default(cfg.roxygen_markdown)
}

/// Renders an arity format error as a dprint-facing message.
fn format_error(err: &arity_formatter::FormatError) -> FormatError {
    FormatError::new(err.to_string())
}

/// Reports a parse failure the same way [`arity_formatter::FormatError`] does,
/// so the range path and the whole-file path produce identical messages.
fn parse_error(count: usize) -> FormatError {
    format_error(&arity_formatter::FormatError::ParseErrors { count })
}

/// Formats only `range`, splicing the result back into `text`.
///
/// Two details matter here. `format_range` widens the requested range out to
/// node boundaries and reports what it actually covered, so the splice has to
/// use the returned range rather than the requested one. And it does not apply
/// line endings -- its text is always LF -- so the target ending is applied to
/// the replacement before splicing, or a CRLF file would grow LF-only islands.
fn format_text_range(
    text: &str,
    range: std::ops::Range<usize>,
    style: FormatStyle,
    parse_options: &ParseOptions,
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

    let parsed = arity_formatter::parser::parse_with_options(text, parse_options);
    if !parsed.diagnostics.is_empty() {
        return Err(parse_error(parsed.diagnostics.len()));
    }

    let Some(formatted) =
        arity_formatter::format_range(&parsed.cst, TextRange::new(start, end), style, text)
            .map_err(|e| format_error(&e))?
    else {
        return Ok(None);
    };

    let replaced_start = usize::from(formatted.range.start());
    let replaced_end = usize::from(formatted.range.end());
    let eol = style.line_ending.resolve(text);
    let replacement = if eol == "\n" {
        formatted.text
    } else {
        formatted.text.replace('\n', eol)
    };

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..replaced_start]);
    out.push_str(&replacement);
    out.push_str(&text[replaced_end..]);
    Ok(Some(out))
}

impl SyncPluginHandler<Configuration> for ArityHandler {
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
        let line_ending: String = get_value(
            &mut config,
            "lineEnding",
            default_line_ending(global_config),
            &mut diagnostics,
        );
        let roxygen_markdown: bool =
            get_value(&mut config, "roxygenMarkdown", false, &mut diagnostics);

        // Re-run the parse purely to surface a diagnostic for a bad value.
        let _ = parse_line_ending(&line_ending, &mut diagnostics);

        diagnostics.extend(get_unknown_property_diagnostics(config));

        PluginResolveConfigurationResult {
            config: Configuration {
                line_width,
                indent_width,
                line_ending,
                roxygen_markdown,
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
            config_key: "arity".to_string(),
            help_url: "https://arity.cc".to_string(),
            config_schema_url: format!(
                "https://github.com/jolars/dprint-plugin-arity/releases/download/v{version}/schema.json"
            ),
            update_url: Some("https://plugins.dprint.dev/jolars/arity/latest.json".to_string()),
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

        let style = build_style(request.config);
        let parse_options = build_parse_options(request.config);

        // arity's API is `Result`-returning, so this is belt-and-braces: it
        // keeps an unexpected panic from tearing down the wasm instance.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match request.range {
                None => arity_formatter::format_with_options(&text, style, &parse_options)
                    .map(Some)
                    .map_err(|e| format_error(&e)),
                Some(range) => format_text_range(&text, range, style, &parse_options),
            }));

        let formatted = match result {
            Ok(formatted) => formatted?,
            Err(payload) => {
                let message = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "arity panicked while formatting".to_string()
                };
                return Err(FormatError::new(format!("arity panicked: {message}")));
            }
        };

        match formatted {
            Some(formatted) if formatted != text => Ok(Some(formatted.into_bytes())),
            _ => Ok(None),
        }
    }
}

#[cfg(target_arch = "wasm32")]
generate_plugin_code!(ArityHandler, ArityHandler::new());

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Configuration {
        Configuration {
            line_width: 80,
            indent_width: 2,
            line_ending: "auto".to_string(),
            roxygen_markdown: false,
        }
    }

    fn format_all(cfg: &Configuration, text: &str) -> String {
        arity_formatter::format_with_options(text, build_style(cfg), &build_parse_options(cfg))
            .expect("format should succeed")
    }

    /// Two statements, so the braced body survives rather than collapsing to
    /// the single-expression one-liner arity prefers.
    const BLOCK_SOURCE: &str = "f<-function(x){y<-x+1\ny*2}\n";

    #[test]
    fn formats_whole_file() {
        let cfg = config();
        assert_eq!(
            format_all(&cfg, BLOCK_SOURCE),
            "f <- function(x) {\n  y <- x + 1\n  y * 2\n}\n"
        );
    }

    #[test]
    fn honors_indent_width() {
        let mut cfg = config();
        cfg.indent_width = 4;
        assert_eq!(
            format_all(&cfg, BLOCK_SOURCE),
            "f <- function(x) {\n    y <- x + 1\n    y * 2\n}\n"
        );
    }

    #[test]
    fn honors_line_width() {
        let mut cfg = config();
        cfg.line_width = 20;
        let narrow = format_all(&cfg, "result <- some_function(alpha, beta, gamma, delta)\n");
        assert!(
            narrow.lines().count() > 1,
            "a 20-column budget should force a break: {narrow:?}"
        );
    }

    #[test]
    fn crlf_input_round_trips_under_auto() {
        let cfg = config();
        let out = format_all(&cfg, "f<-function(x){x+1}\r\n");
        assert!(out.contains("\r\n"), "auto should preserve CRLF: {out:?}");
        assert!(
            !out.contains("\r\r"),
            "line endings must not double: {out:?}"
        );
    }

    #[test]
    fn explicit_line_ending_overrides_source() {
        let mut cfg = config();
        cfg.line_ending = "crlf".to_string();
        let out = format_all(&cfg, "f<-function(x){x+1}\n");
        assert!(out.contains("\r\n"), "crlf should be applied: {out:?}");
    }

    #[test]
    fn parse_failure_is_reported() {
        let cfg = config();
        let err = arity_formatter::format_with_options(
            "f <- function(x) {",
            build_style(&cfg),
            &build_parse_options(&cfg),
        )
        .expect_err("unbalanced brace should not format");
        assert!(matches!(
            err,
            arity_formatter::FormatError::ParseErrors { .. }
        ));
    }

    #[test]
    fn roxygen_markdown_default_is_threaded_through() {
        let source = "#' Title\n#'\n#' Some `code` and _emphasis_.\nf <- function() NULL\n";
        let mut on_cfg = config();
        on_cfg.roxygen_markdown = true;

        let off = format_all(&config(), source);
        let on = format_all(&on_cfg, source);

        // Each mode must be a fixed point of itself; that is what proves the
        // flag actually reaches the parser rather than being dropped.
        assert_eq!(format_all(&config(), &off), off);
        assert_eq!(format_all(&on_cfg, &on), on);
    }

    #[test]
    fn range_format_only_touches_its_range() {
        let cfg = config();
        let text = "a<-1\nb<-2\n";
        let start = text
            .find("b<-2")
            .expect("fixture should contain the statement");
        let out = format_text_range(
            text,
            start..start + 4,
            build_style(&cfg),
            &build_parse_options(&cfg),
        )
        .expect("range format should succeed")
        .expect("range should cover a statement");
        assert_eq!(out, "a<-1\nb <- 2\n");
    }

    #[test]
    fn range_format_applies_line_endings_to_the_splice() {
        let mut cfg = config();
        cfg.line_ending = "crlf".to_string();
        let text = "a<-1\r\nif(x){y}\r\n";
        let start = text
            .find("if(x){y}")
            .expect("fixture should contain the if");
        let out = format_text_range(
            text,
            start..start + "if(x){y}".len(),
            build_style(&cfg),
            &build_parse_options(&cfg),
        )
        .expect("range format should succeed")
        .expect("range should cover a statement");
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "spliced text left a bare LF behind: {out:?}"
        );
    }

    #[test]
    fn range_outside_the_file_is_an_error() {
        let cfg = config();
        assert!(
            format_text_range(
                "a<-1\n",
                0..999,
                build_style(&cfg),
                &build_parse_options(&cfg)
            )
            .is_err()
        );
    }

    #[test]
    fn default_line_ending_follows_the_dprint_global() {
        let global = |kind| GlobalConfiguration {
            line_width: None,
            use_tabs: None,
            indent_width: None,
            new_line_kind: kind,
        };
        assert_eq!(default_line_ending(&global(None)), "auto");
        assert_eq!(
            default_line_ending(&global(Some(NewLineKind::Auto))),
            "auto"
        );
        assert_eq!(
            default_line_ending(&global(Some(NewLineKind::LineFeed))),
            "lf"
        );
        assert_eq!(
            default_line_ending(&global(Some(NewLineKind::CarriageReturnLineFeed))),
            "crlf"
        );
    }

    #[test]
    fn unknown_line_ending_reports_a_diagnostic() {
        let mut diagnostics = Vec::new();
        assert_eq!(
            parse_line_ending("bogus", &mut diagnostics),
            LineEnding::Auto
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].property_name, "lineEnding");
    }
}

#[cfg(test)]
mod schema_tests {
    use super::Configuration;

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

    /// The schema is what editors show users, so the advertised defaults have
    /// to be the real fallbacks rather than `u32`/`String`'s zero values.
    #[test]
    fn schema_advertises_the_real_defaults() {
        let schema: serde_json::Value =
            serde_json::from_str(&generated_schema()).expect("schema should parse");
        let props = &schema["properties"];
        assert_eq!(props["lineWidth"]["default"], serde_json::json!(80));
        assert_eq!(props["indentWidth"]["default"], serde_json::json!(2));
        assert_eq!(props["lineEnding"]["default"], serde_json::json!("auto"));
        assert_eq!(
            props["roxygenMarkdown"]["default"],
            serde_json::json!(false)
        );
    }

    /// Guards against an upstream serde-rename change leaking PascalCase
    /// variants into the published schema.
    #[test]
    fn line_ending_values_stay_lowercase() {
        let schema = generated_schema();
        for expected in ["\"auto\"", "\"lf\"", "\"crlf\"", "\"native\""] {
            assert!(schema.contains(expected), "schema is missing {expected}");
        }
        for unexpected in ["\"Auto\"", "\"Lf\"", "\"Crlf\"", "\"Native\""] {
            assert!(
                !schema.contains(unexpected),
                "schema leaked a PascalCase variant: {unexpected}"
            );
        }
    }
}
