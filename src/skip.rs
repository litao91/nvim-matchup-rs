//! Evaluation of skip expressions (`b:match_skip`), port of
//! `s:init_delim_skip` (loader.vim:697) and `matchup#delim#skip`
//! (delim.vim:868-935).

use fancy_regex::Regex;
use nvim_oxi::api;
use once_cell::sync::Lazy;

use crate::vimregex::{translate, Opts};

static RE_COMMENT_STRING: Lazy<Regex> =
    Lazy::new(|| Regex::new("(?i)(?:String|Comment)").unwrap());

#[derive(Clone)]
pub enum SkipKind {
    /// No b:match_skip: skip when in a comment or string
    /// (matchup#util#in_comment_or_string).
    Default,
    /// `s:re` / `S:re`: syntax group name at position (un)matched.
    /// `empty` is the result for the empty syntax name, i.e. the answer
    /// whenever syntax highlighting is inactive (synID() == 0).
    Syn {
        re: Regex,
        invert: bool,
        empty: bool,
    },
    /// `r:re` / `R:re`: line prefix up to position (un)matched.
    Prefix { re: Regex, invert: bool },
    /// Raw vimscript expression, evaluated through the
    /// `matchup#rs#skip_eval` shim (handles effline/effcol rewriting).
    Raw { expr: String },
}

/// Compile b:match_skip into an evaluator.
/// Port of s:init_delim_skip (loader.vim:697-728).
pub fn compile_skip(match_skip: &str, word: &str) -> SkipKind {
    if match_skip.is_empty() {
        return SkipKind::Default;
    }
    let cs: Vec<char> = match_skip.chars().collect();
    if cs.len() >= 2 && matches!(cs[0], 's' | 'S' | 'r' | 'R') && cs[1] == ':' {
        let rest: String = cs[2..].iter().collect();
        let invert = cs[0] == 'S' || cs[0] == 'R';
        let syntax = cs[0] == 's' || cs[0] == 'S';
        // `=~?` : case-insensitive, unanchored
        if let Ok(t) = translate(
            &rest,
            &Opts {
                word: word.to_string(),
                ignorecase: true,
                captures: false,
                scan: false,
            },
        ) {
            if t.prefix_checks.is_empty() {
                if let Ok(re) = Regex::new(&t.pattern) {
                    return if syntax {
                        let empty = re.is_match("").unwrap_or(false) != invert;
                        SkipKind::Syn { re, invert, empty }
                    } else {
                        SkipKind::Prefix { re, invert }
                    };
                }
            }
        }
        // Fall back to evaluating the loader-built vimscript expression.
        let quoted = rest.replace('\'', "''");
        let expr = if syntax {
            format!(
                "synIDattr(synID(matchup#rs#effline('.'),matchup#rs#effcol('.'),1),'name') {}? '{}'",
                if invert { "!~" } else { "=~" },
                quoted
            )
        } else {
            format!(
                "strpart(matchup#rs#geteffline('.'),0,matchup#rs#effcol('.')) {}? '{}'",
                if invert { "!~" } else { "=~" },
                quoted
            )
        };
        return SkipKind::Raw { expr };
    }

    // Generic vimscript expression: rewrite cursor-relative functions to
    // their effective-position variants (loader.vim:719-725).
    SkipKind::Raw {
        expr: rewrite_eff(match_skip),
    }
}

/// Single-pass rewrite of `col(`, `line(`, `getline(` to their
/// effective-position shim variants, respecting word boundaries
/// (port of the substitute loop in loader.vim:719-725).
fn rewrite_eff(expr: &str) -> String {
    let b: Vec<char> = expr.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let pats: [(&str, &str); 3] = [
        ("getline(", "matchup#rs#geteffline("),
        ("line(", "matchup#rs#effline("),
        ("col(", "matchup#rs#effcol("),
    ];
    while i < b.len() {
        let mut matched = false;
        for (pat, repl) in pats {
            let pc: Vec<char> = pat.chars().collect();
            if b[i..].starts_with(&pc) {
                let prev_ok = i == 0 || {
                    let c = b[i - 1];
                    !(c.is_alphanumeric() || c == '_' || c == '#')
                };
                if prev_ok {
                    out.push_str(repl);
                    i += pc.len();
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Syntax group name at position. `translate_id` corresponds to vim's
/// synID(lnum, cnum, 1) vs synID(lnum, cnum, 0).
pub fn syn_name(lnum: usize, cnum: usize, translate_id: bool) -> String {
    let expr = format!(
        "vim.fn.synIDattr(vim.fn.synID({},{},{}),'name')",
        lnum,
        cnum,
        if translate_id { 1 } else { 0 }
    );
    api::eval::<String>(&expr).unwrap_or_default()
}

/// Evaluate the skip expression at (lnum, cnum); `line` is the text of
/// lnum. Returns the raw expression value; the caller applies
/// invert_skip (XOR), mirroring matchup#delim#skip (delim.vim:868).
pub fn skip_at(
    kind: &SkipKind,
    line: &str,
    lnum: usize,
    cnum: usize,
    syntax_on: bool,
) -> bool {
    match kind {
        SkipKind::Default => {
            if !syntax_on {
                return RE_COMMENT_STRING.is_match("").unwrap_or(false);
            }
            let name = syn_name(lnum, cnum, true);
            RE_COMMENT_STRING.is_match(&name).unwrap_or(false)
        }
        SkipKind::Syn { re, invert, empty } => {
            if !syntax_on {
                return *empty;
            }
            let name = syn_name(lnum, cnum, true);
            re.is_match(&name).unwrap_or(false) != *invert
        }
        SkipKind::Prefix { re, invert } => {
            let mut end = cnum.min(line.len());
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            re.is_match(&line[..end]).unwrap_or(false) != *invert
        }
        SkipKind::Raw { expr } => {
            // nvim_call_function is ABI-broken on nvim 0.13-dev; inline the
            // expression as a single-quoted vimscript literal (fully literal,
            // quotes doubled) in one eval round-trip.
            let q = crate::motion::vim_quote(expr);
            let r: i64 =
                api::eval(&format!("matchup#rs#skip_eval({q}, {lnum}, {cnum})"))
                    .unwrap_or(0);
            r != 0
        }
    }
}

/// Advanced mid disambiguation (b:match_midmap), port of
/// matchup#delim#skip1/skip2 (delim.vim:900-914).
#[derive(Clone)]
pub enum MidSkip {
    /// skip1: [syntax pattern, word pattern]
    Skip1 { syn_re: Regex, word_re: Regex },
    /// skip2: strike pattern
    Skip2 { strike_re: Regex },
}

impl MidSkip {
    pub fn compile_skip1(syn: &str, word: &str, word_class: &str) -> Option<MidSkip> {
        // `=~#`: case-sensitive; word pattern is anchored at the start of
        // the text from the cursor position.
        let syn_t = translate(
            syn,
            &Opts {
                word: word_class.to_string(),
                ignorecase: false,
                captures: false,
                scan: false,
            },
        )
        .ok()?;
        let word_t = translate(
            word,
            &Opts {
                word: word_class.to_string(),
                ignorecase: false,
                captures: false,
                scan: false,
            },
        )
        .ok()?;
        if !syn_t.prefix_checks.is_empty() || !word_t.prefix_checks.is_empty() {
            return None;
        }
        Some(MidSkip::Skip1 {
            syn_re: Regex::new(&syn_t.pattern).ok()?,
            word_re: Regex::new(&format!("^(?:{})", word_t.pattern)).ok()?,
        })
    }

    pub fn compile_skip2(strike: &str, word_class: &str) -> Option<MidSkip> {
        let t = translate(
            strike,
            &Opts {
                word: word_class.to_string(),
                ignorecase: false,
                captures: false,
                scan: false,
            },
        )
        .ok()?;
        if !t.prefix_checks.is_empty() {
            return None;
        }
        Some(MidSkip::Skip2 {
            strike_re: Regex::new(&format!("^(?:{})", t.pattern)).ok()?,
        })
    }

    /// Evaluate; `base` is the underlying skip closure.
    pub fn eval<F: Fn() -> bool>(&self, line: &str, lnum: usize, cnum: usize, base: F) -> bool {
        let mut start = (cnum - 1).min(line.len());
        while start > 0 && !line.is_char_boundary(start) {
            start -= 1;
        }
        let suffix = &line[start..];
        match self {
            MidSkip::Skip1 { syn_re, word_re } => {
                if word_re.is_match(suffix).unwrap_or(false) {
                    return base();
                }
                let s = syn_name(lnum, cnum, false);
                !syn_re.is_match(&s).unwrap_or(false) || base()
            }
            MidSkip::Skip2 { strike_re } => {
                strike_re.is_match(suffix).unwrap_or(false) || base()
            }
        }
    }
}

/// matchup#util#in_synstack (util.vim:49): true when any syntax stack
/// entry name matches `^pat$`.
pub fn in_synstack(pat: &str, lnum: usize, cnum: usize, word: &str) -> bool {
    let (pat, invert) = if let Some(rest) = pat.strip_prefix('!') {
        (rest, true)
    } else {
        (pat, false)
    };
    let re = translate(
        &format!(r"^(?:{})$", pat),
        &Opts {
            word: word.to_string(),
            ignorecase: false,
            captures: false,
            scan: false,
        },
    )
    .ok()
    .and_then(|t| {
        if t.prefix_checks.is_empty() {
            Regex::new(&t.pattern).ok()
        } else {
            None
        }
    });
    let names: Vec<String> = api::eval::<Vec<String>>(&format!(
        "vim.tbl_map(function(i) return vim.fn.synIDattr(i,'name') end, vim.fn.synstack({lnum},{cnum}))"
    ))
    .unwrap_or_default();
    let found = match &re {
        Some(re) => names.iter().any(|n| re.is_match(n).unwrap_or(false)),
        None => false,
    };
    if invert {
        !found
    } else {
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_skip_matches_line_prefix() {
        let kind = match compile_skip(r"r:\\\@<!\%(\\\\\)*%", "\\w") {
            SkipKind::Prefix { re, invert } => {
                assert!(!invert);
                re
            }
            _ => panic!("expected Prefix"),
        };
        // unescaped % (even number of preceding backslashes) matches
        assert!(kind.is_match(r"\\%").unwrap());
        assert!(kind.is_match("%").unwrap());
        // escaped % (odd number) does not
        assert!(!kind.is_match(r"\%").unwrap());
    }

    #[test]
    fn rewrite_eff_functions() {
        let e = rewrite_eff(r"getline('.') =~ '^#' && col('.') > 2");
        assert_eq!(
            e,
            r"matchup#rs#geteffline('.') =~ '^#' && matchup#rs#effcol('.') > 2"
        );
        let e2 = rewrite_eff(r"line('.') + Myline(x)");
        assert_eq!(e2, r"matchup#rs#effline('.') + Myline(x)");
    }
}
