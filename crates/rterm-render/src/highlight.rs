//! WindTerm-style client-side syntax highlighting.
//!
//! A set of regex rules is run over each visible row's text; matched
//! character ranges get a foreground-colour (and optional bold)
//! override. The renderer applies these overrides in `build_spans`
//! ONLY to cells that carry the terminal's DEFAULT foreground — so the
//! feature enhances plain command output / logs without ever fighting
//! output a program already coloured (`ls --color`, `bat`, `git`,
//! TUIs). See [`HighlightEngine::overlay`].
//!
//! The active engine lives in a process-global `OnceLock<Mutex<Arc<…>>>`
//! (mirroring [`crate::palette`]) so it can be swapped on config
//! hot-reload without threading it through every render call.
//!
//! Precedence is FIRST-match-wins per column: rules are stored
//! custom-first then built-in (specific → generic), so a user rule
//! beats a built-in and a specific built-in (IPv4) beats a generic one
//! (bare number) on the same text.

use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;

/// Foreground override a matched column receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HStyle {
    pub fg: [u8; 3],
    pub bold: bool,
}

/// One raw rule as handed in from config (rterm-app parses the colour
/// string into RGB before constructing this so the highlight module
/// stays independent of the config crate).
#[derive(Debug, Clone)]
pub struct HighlightRuleInput {
    pub pattern: String,
    pub fg: [u8; 3],
    pub bold: bool,
}

struct CompiledRule {
    re: Regex,
    fg: [u8; 3],
    bold: bool,
}

/// The compiled, ready-to-run rule set.
pub struct HighlightEngine {
    enabled: bool,
    rules: Vec<CompiledRule>,
}

impl HighlightEngine {
    fn empty() -> Self {
        Self { enabled: false, rules: Vec::new() }
    }

    /// True when highlighting should run at all (feature on AND at
    /// least one rule compiled). Lets `build_spans` skip the per-row
    /// text/column bookkeeping entirely in the common no-rules case.
    pub(crate) fn is_active(&self) -> bool {
        self.enabled && !self.rules.is_empty()
    }

    /// Mark the grid columns covered by any rule match in `text`.
    /// `char_cols[i]` is the grid column of the i-th char of `text`
    /// (the caller skips WIDE_SPACER cells when building both). Writes
    /// into `overrides`, indexed by column. FIRST-match-wins: an
    /// already-set column is never overwritten, so earlier (more
    /// specific / user) rules take precedence.
    pub(crate) fn overlay(
        &self,
        text: &str,
        char_cols: &[u16],
        overrides: &mut [Option<HStyle>],
    ) {
        if !self.enabled || text.is_empty() {
            return;
        }
        for rule in &self.rules {
            for m in rule.re.find_iter(text) {
                if m.start() == m.end() {
                    continue; // zero-width match — nothing to colour
                }
                // Regex yields byte offsets; map to char indices, then
                // to grid columns via `char_cols`.
                let start_char = text[..m.start()].chars().count();
                let end_char = start_char + text[m.start()..m.end()].chars().count();
                let style = HStyle { fg: rule.fg, bold: rule.bold };
                for ci in start_char..end_char {
                    if let Some(&col) = char_cols.get(ci) {
                        if let Some(slot) = overrides.get_mut(col as usize) {
                            if slot.is_none() {
                                *slot = Some(style);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn engine_slot() -> &'static Mutex<Arc<HighlightEngine>> {
    static SLOT: OnceLock<Mutex<Arc<HighlightEngine>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Arc::new(HighlightEngine::empty())))
}

/// The active engine — cloned Arc so `build_spans` reads it without
/// holding the lock across the row loop.
pub(crate) fn active() -> Arc<HighlightEngine> {
    engine_slot()
        .lock()
        .map(|g| Arc::clone(&g))
        .unwrap_or_else(|e| Arc::clone(&e.into_inner()))
}

/// Install a fresh rule set (startup + config hot-reload). Rules are
/// compiled here; a rule whose pattern fails to compile is logged and
/// skipped rather than aborting the whole set. Order of evaluation is
/// `custom` first, then the built-ins (when `use_builtins`), so a user
/// rule wins on overlap and specific built-ins beat generic ones.
pub fn set_rules(enabled: bool, use_builtins: bool, custom: Vec<HighlightRuleInput>) {
    let mut inputs = custom;
    if use_builtins {
        inputs.extend(builtin_rules());
    }
    let mut rules = Vec::with_capacity(inputs.len());
    for r in inputs {
        match Regex::new(&r.pattern) {
            Ok(re) => rules.push(CompiledRule { re, fg: r.fg, bold: r.bold }),
            Err(e) => tracing::warn!(
                pattern = %r.pattern,
                "highlight rule skipped — invalid regex: {e}"
            ),
        }
    }
    let engine = Arc::new(HighlightEngine { enabled, rules });
    if let Ok(mut g) = engine_slot().lock() {
        *g = engine;
    }
}

/// Parse a colour string into RGB. Accepts `#RRGGBB`, `#RGB`, the same
/// without `#`, and a set of common colour names (One Dark-ish so they
/// read well on a dark theme). Returns `None` for anything else so the
/// caller can warn about a typo instead of getting silent garbage.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let t = s.trim();
    // Named colours first (case-insensitive).
    let named: Option<[u8; 3]> = match t.to_ascii_lowercase().as_str() {
        "red" => Some([224, 108, 117]),
        "green" => Some([152, 195, 121]),
        "yellow" => Some([229, 192, 123]),
        "orange" => Some([209, 154, 102]),
        "blue" => Some([97, 175, 239]),
        "magenta" | "purple" => Some([198, 120, 221]),
        "cyan" => Some([86, 182, 194]),
        "white" => Some([220, 223, 228]),
        "gray" | "grey" => Some([92, 99, 112]),
        "black" => Some([40, 44, 52]),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let hex = t.strip_prefix('#').unwrap_or(t);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => Some([
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ]),
        3 => {
            // CSS short form: #FA0 → #FFAA00.
            let expand = |b: &str| u8::from_str_radix(&b.repeat(2), 16).ok();
            Some([
                expand(&hex[0..1])?,
                expand(&hex[1..2])?,
                expand(&hex[2..3])?,
            ])
        }
        _ => None,
    }
}

/// The default, always-available rule set (WindTerm-style). Ordered
/// specific → generic so first-match-wins keeps IPv4 / hex / URLs from
/// being clobbered by the bare-number rule. Colours are One Dark-ish.
fn builtin_rules() -> Vec<HighlightRuleInput> {
    const RED: [u8; 3] = [224, 108, 117];
    const YELLOW: [u8; 3] = [229, 192, 123];
    const GREEN: [u8; 3] = [152, 195, 121];
    const BLUE: [u8; 3] = [97, 175, 239];
    const CYAN: [u8; 3] = [86, 182, 194];
    const MAGENTA: [u8; 3] = [198, 120, 221];
    let r = |pattern: &str, fg: [u8; 3], bold: bool| HighlightRuleInput {
        pattern: pattern.to_string(),
        fg,
        bold,
    };
    vec![
        // Specific tokens first (URLs, IPs, UUID, hex) so their columns
        // are claimed before the generic number rule runs.
        r(r"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s]+", BLUE, false),
        r(r"\b\d{1,3}(?:\.\d{1,3}){3}\b", CYAN, false),
        r(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
            MAGENTA,
            false,
        ),
        r(r"\b0[xX][0-9a-fA-F]+\b", CYAN, false),
        // Log-level keywords (word-bounded, case-insensitive).
        r(r"(?i)\b(?:error|fatal|panic|critical|failed|failure)\b", RED, true),
        r(r"(?i)\b(?:warn|warning)\b", YELLOW, false),
        r(r"(?i)\b(?:info|notice|success|passed|ok)\b", GREEN, false),
        // Quoted strings (double then single). Raw strings use `#`
        // delimiters so the quote characters sit inside cleanly.
        r(r#""[^"\n]*""#, GREEN, false),
        r(r"'[^'\n]*'", GREEN, false),
        // Bare numbers LAST — IPv4 / hex / version octets are already
        // claimed above under first-match-wins.
        r(r"\b\d+(?:\.\d+)?\b", CYAN, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(inputs: Vec<HighlightRuleInput>) -> HighlightEngine {
        let rules = inputs
            .into_iter()
            .map(|r| CompiledRule {
                re: Regex::new(&r.pattern).unwrap(),
                fg: r.fg,
                bold: r.bold,
            })
            .collect();
        HighlightEngine { enabled: true, rules }
    }

    /// Run the engine over `text` (each char one column) and return the
    /// per-column override list.
    fn run(engine: &HighlightEngine, text: &str) -> Vec<Option<HStyle>> {
        let char_cols: Vec<u16> = (0..text.chars().count() as u16).collect();
        let mut out = vec![None; text.chars().count()];
        engine.overlay(text, &char_cols, &mut out);
        out
    }

    #[test]
    fn parse_color_accepts_hex_and_names() {
        assert_eq!(parse_color("#ff0000"), Some([255, 0, 0]));
        assert_eq!(parse_color("00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_color("#f00"), Some([255, 0, 0]));
        assert_eq!(parse_color("RED"), Some([224, 108, 117]));
        assert_eq!(parse_color("  cyan "), Some([86, 182, 194]));
        assert_eq!(parse_color("nonsense"), None);
        assert_eq!(parse_color("#gg0000"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn overlay_colours_only_matched_columns() {
        let eng = compile(vec![HighlightRuleInput {
            pattern: r"\bERROR\b".to_string(),
            fg: [1, 2, 3],
            bold: true,
        }]);
        // "x ERROR y" — cols 2..7 are ERROR.
        let out = run(&eng, "x ERROR y");
        assert_eq!(out[0], None);
        assert_eq!(out[1], None);
        let want = Some(HStyle { fg: [1, 2, 3], bold: true });
        assert!(out[2..7].iter().all(|s| *s == want), "ERROR cols coloured");
        assert_eq!(out[7], None);
        assert_eq!(out[8], None);
    }

    #[test]
    fn first_match_wins_keeps_specific_over_generic() {
        // A specific rule (IPv4) listed BEFORE a generic one (bare
        // number) must keep its columns — the number rule can't
        // overwrite them.
        let eng = compile(vec![
            HighlightRuleInput { pattern: r"\b\d{1,3}(?:\.\d{1,3}){3}\b".to_string(), fg: [10, 10, 10], bold: false },
            HighlightRuleInput { pattern: r"\b\d+\b".to_string(), fg: [99, 99, 99], bold: false },
        ]);
        let out = run(&eng, "ip 10.0.0.1 end");
        // The IP columns (3..11) carry the specific colour, not the
        // generic number colour.
        assert!(
            out[3..11].iter().all(|s| s.map(|x| x.fg) == Some([10, 10, 10])),
            "IP columns keep the specific colour"
        );
    }

    #[test]
    fn builtin_rules_all_compile_and_flag_common_tokens() {
        let eng = compile(builtin_rules());
        // Log level.
        assert!(run(&eng, "ERROR: boom").iter().take(5).all(|s| s.is_some()));
        // IPv4 stays cyan even though a number rule exists.
        let ipline = run(&eng, "192.168.0.1");
        assert!(ipline.iter().all(|s| s.is_some()));
        // A URL.
        assert!(run(&eng, "see https://example.com now")[4].is_some());
    }

    #[test]
    fn disabled_or_empty_engine_does_nothing() {
        let mut eng = compile(vec![HighlightRuleInput {
            pattern: "x".to_string(),
            fg: [1, 1, 1],
            bold: false,
        }]);
        eng.enabled = false;
        assert!(run(&eng, "xxxx").iter().all(|s| s.is_none()));
        assert!(!eng.is_active());
        let empty = compile(vec![]);
        assert!(!empty.is_active());
    }

    #[test]
    fn overlay_maps_multibyte_chars_to_correct_columns() {
        // Cyrillic before the match: byte offsets != char offsets, so
        // the byte→char→column mapping must not mis-place the colour.
        let eng = compile(vec![HighlightRuleInput {
            pattern: r"\d+".to_string(),
            fg: [7, 7, 7],
            bold: false,
        }]);
        let out = run(&eng, "код 42");
        // "код " = 4 chars (cols 0..4), "42" = cols 4,5.
        assert_eq!(out[3], None);
        assert_eq!(out[4].map(|s| s.fg), Some([7, 7, 7]));
        assert_eq!(out[5].map(|s| s.fg), Some([7, 7, 7]));
    }
}
