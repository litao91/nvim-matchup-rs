//! Translator from Vim magic-mode regular expressions (as found in
//! `b:match_words`) to fancy-regex compatible patterns.
//!
//! Vim and Rust regexes differ in escaping conventions (`\(` vs `(`,
//! `\+` vs `+`), word boundaries (`\<`/`\>`), lookarounds (`X\@<=`),
//! match extent markers (`\zs`/`\ze`) and conjunction (`\&`).
//! This module parses the Vim pattern into an AST and emits an
//! equivalent fancy-regex pattern.

use std::fmt;

#[derive(Debug)]
pub struct TranslateError(pub String);

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vim regex translation error: {}", self.0)
    }
}

impl std::error::Error for TranslateError {}

pub type Result<T> = std::result::Result<T, TranslateError>;

#[derive(Clone, Debug)]
pub struct Opts {
    /// Character class used for `\w`, `\k`, `\<`, `\>` (default `\w`).
    /// Built from `&iskeyword` when it deviates from the default.
    pub word: String,
    /// Value of `b:match_ignorecase`; overridable by `\c`/`\C` in the pattern.
    pub ignorecase: bool,
    /// When false, capture groups not needed by backrefs are demoted to
    /// non-capturing groups (port of `matchup#loader#remove_capture_groups`).
    pub captures: bool,
    /// Scan mode: emit a lookaround-free OVER-approximation suitable for
    /// the `regex` crate (DFA). Word boundaries, lookaheads and lookbehinds
    /// are dropped (positions they would reject are filtered later by the
    /// exact classify patterns); backrefs and `\zs`/`\ze` fail translation
    /// so such words keep using the fancy-regex scan union.
    pub scan: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            word: r"\w".to_string(),
            ignorecase: false,
            captures: true,
            scan: false,
        }
    }
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Quant {
    Star,
    Plus,
    Quest,
    Range(u32, Option<u32>),
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    /// Raw fancy-regex fragment, already valid output.
    Raw(String),
    /// Bracket expression, fully rendered including `[`/`]`.
    Class(String),
    Group {
        capture: bool,
        alt: Vec<Node>,
    },
    /// `\%[...]` optional group.
    Optional {
        alt: Vec<Node>,
    },
    Quantified {
        atom: Box<Node>,
        q: Quant,
        greedy: bool,
    },
    Lookahead {
        atom: Box<Node>,
        neg: bool,
    },
    Lookbehind {
        atom: Box<Node>,
        neg: bool,
    },
    Backref(u32),
    Concat(Vec<Node>),
    /// `\|` alternation; children are Concats.
    Alt(Vec<Node>),
    /// `A\&B` conjunction: all branches match at the same position,
    /// extent is that of the last branch.
    Conj(Vec<Node>),
    /// `^`
    AnchorStart,
    /// `$` (end of line)
    AnchorEnd,
    /// `\<`
    WordStart,
    /// `\>`
    WordEnd,
    /// `\zs`
    MatchStart,
    /// `\ze`
    MatchEnd,
}

// ---------------------------------------------------------------------------
// Translation result
// ---------------------------------------------------------------------------

/// A lookbehind assertion that fancy-regex cannot express natively
/// (variable width). The engine must verify that `pattern` matches the
/// text ending exactly at the match start position (or, for `neg`, that
/// it does NOT).
#[derive(Clone, Debug, PartialEq)]
pub struct PrefixCheck {
    pub pattern: String,
    pub neg: bool,
}

#[derive(Clone, Debug)]
pub struct Translated {
    /// fancy-regex pattern; match extent equals vim's match extent.
    pub pattern: String,
    /// Variable-width lookbehind obligations, all anchored at match start.
    pub prefix_checks: Vec<PrefixCheck>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// First-byte analysis (scan candidate dispatch)
// ---------------------------------------------------------------------------

/// Set of bytes a match of a pattern can start with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FirstBytes(pub [u64; 4]);

impl FirstBytes {
    pub fn empty() -> FirstBytes {
        FirstBytes([0; 4])
    }
    pub fn insert(&mut self, b: u8) {
        self.0[(b >> 6) as usize] |= 1u64 << (b & 63);
    }
    pub fn contains(&self, b: u8) -> bool {
        (self.0[(b >> 6) as usize] >> (b & 63)) & 1 == 1
    }
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&x| x == 0)
    }
    pub fn merge(&mut self, other: &FirstBytes) {
        for i in 0..4 {
            self.0[i] |= other.0[i];
        }
    }
}

/// Possible first bytes of non-empty matches of the vim regex `re`.
/// `None` means undetermined: the caller must treat the pattern as a
/// candidate at every position.
pub fn first_bytes(re: &str, ignorecase: bool) -> Option<FirstBytes> {
    let mut p = Parser {
        cs: re.chars().collect(),
        i: 0,
        case_override: None,
        warnings: Vec::new(),
    };
    let ast = p.parse_alt().ok()?;
    if p.i != p.cs.len() {
        return None;
    }
    let ic = p.case_override.unwrap_or(ignorecase);
    // a pattern that can match empty has no determined first byte
    if min_size(&ast) == 0 {
        return None;
    }
    fb_node(&ast, ic)
}

fn fb_insert_lit(fb: &mut FirstBytes, b: u8, ic: bool) {
    fb.insert(b);
    if ic && b.is_ascii_alphabetic() {
        fb.insert(if b.is_ascii_lowercase() {
            b - 32
        } else {
            b + 32
        });
    }
}

/// First bytes contributed by a single node. `Some(empty)` means the node
/// is zero-width (contributes nothing); `None` means undetermined.
fn fb_node(n: &Node, ic: bool) -> Option<FirstBytes> {
    match n {
        Node::Empty
        | Node::AnchorStart
        | Node::AnchorEnd
        | Node::WordStart
        | Node::WordEnd
        | Node::MatchStart
        | Node::MatchEnd
        | Node::Lookahead { .. }
        | Node::Lookbehind { .. } => Some(FirstBytes::empty()),
        Node::Raw(s) => {
            let b = s.as_bytes();
            if b.is_empty() {
                return Some(FirstBytes::empty());
            }
            let mut fb = FirstBytes::empty();
            if b[0] == b'\\' {
                match b.get(1) {
                    // escaped punctuation is that literal character
                    Some(&c) if c.is_ascii_punctuation() => fb.insert(c),
                    // \s \d \w ... : undetermined
                    _ => return None,
                }
            } else {
                fb_insert_lit(&mut fb, b[0], ic);
            }
            Some(fb)
        }
        // bracket classes are not analyzed; be conservative
        Node::Class(_) | Node::Backref(_) => None,
        Node::Group { alt, .. } | Node::Alt(alt) | Node::Optional { alt } => {
            let mut fb = FirstBytes::empty();
            for a in alt {
                match fb_node(a, ic) {
                    None => return None,
                    Some(s) => fb.merge(&s),
                }
            }
            Some(fb)
        }
        // conjunction extent is the last branch's
        Node::Conj(v) => v.last().map(|x| fb_node(x, ic)).unwrap_or(None),
        Node::Quantified { atom, .. } => fb_node(atom, ic),
        Node::Concat(v) => fb_concat(v, ic),
    }
}

fn fb_concat(nodes: &[Node], ic: bool) -> Option<FirstBytes> {
    // a leading `\zs` moves the match start: only bytes after the last
    // MatchStart can begin the match
    let start = nodes
        .iter()
        .rposition(|n| matches!(n, Node::MatchStart))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut acc = FirstBytes::empty();
    for n in &nodes[start..] {
        match fb_node(n, ic) {
            None => return None,
            Some(s) => acc.merge(&s),
        }
        if min_size(n) > 0 {
            break;
        }
    }
    if acc.is_empty() {
        None
    } else {
        Some(acc)
    }
}

/// Mandatory literal prefix of matches of the vim regex `re`, truncated to
/// `limit` bytes: every non-empty match starts with it (modulo case when
/// the returned flag is true). Used as a cheap filter before running the
/// anchored classify regexes. `None` = no usable literal prefix.
pub fn literal_prefix(re: &str, ignorecase: bool, limit: usize) -> Option<(String, bool)> {
    let mut p = Parser {
        cs: re.chars().collect(),
        i: 0,
        case_override: None,
        warnings: Vec::new(),
    };
    let ast = p.parse_alt().ok()?;
    if p.i != p.cs.len() {
        return None;
    }
    let ic = p.case_override.unwrap_or(ignorecase);
    if min_size(&ast) == 0 {
        return None;
    }
    let (out, _) = lp_node(&ast, limit);
    if out.is_empty() {
        None
    } else {
        Some((out, ic))
    }
}

/// Collect the mandatory literal prefix of one node. The bool reports
/// whether the node is fully literal (so a following concat node can
/// extend the prefix).
fn lp_node(n: &Node, limit: usize) -> (String, bool) {
    let mut out = String::new();
    match n {
        Node::Raw(s) => {
            let mut cs = s.chars();
            while out.len() < limit {
                match cs.next() {
                    None => return (out, true),
                    Some('\\') => match cs.next() {
                        Some(e) if e.is_ascii_punctuation() => out.push(e),
                        // \s \d ... : not a literal
                        _ => return (out, false),
                    },
                    Some(c) => out.push(c),
                }
            }
            (out, false)
        }
        Node::Concat(v) => {
            let start = v
                .iter()
                .rposition(|x| matches!(x, Node::MatchStart))
                .map(|i| i + 1)
                .unwrap_or(0);
            for x in &v[start..] {
                if out.len() >= limit {
                    break;
                }
                match x {
                    Node::Empty
                    | Node::AnchorStart
                    | Node::AnchorEnd
                    | Node::WordStart
                    | Node::WordEnd
                    | Node::MatchEnd
                    | Node::Lookahead { .. }
                    | Node::Lookbehind { .. } => continue,
                    Node::MatchStart => {
                        out.clear();
                        continue;
                    }
                    _ => {}
                }
                if min_size(x) == 0 {
                    break;
                }
                let (p, full) = lp_node(x, limit - out.len());
                out.push_str(&p);
                if !full {
                    break;
                }
            }
            (out, false)
        }
        Node::Group { alt, .. } | Node::Alt(alt) => {
            // longest common prefix across branches
            let mut it = alt.iter();
            let first = match it.next() {
                Some(a) => lp_node(a, limit).0,
                None => return (out, false),
            };
            let mut common: Vec<char> = first.chars().collect();
            for a in it {
                let (p, _) = lp_node(a, limit);
                let pc: Vec<char> = p.chars().collect();
                let n = common
                    .iter()
                    .zip(pc.iter())
                    .take_while(|(x, y)| x == y)
                    .count();
                common.truncate(n);
            }
            (common.into_iter().collect::<String>(), false)
        }
        Node::Conj(v) => match v.last() {
            Some(x) => lp_node(x, limit),
            None => (out, false),
        },
        Node::Quantified { atom, q, .. } => match q {
            Quant::Plus | Quant::Range(1, _) => lp_node(atom, limit),
            _ => (out, false),
        },
        _ => (out, false),
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    cs: Vec<char>,
    i: usize,
    /// Set by `\c`/`\C`/`\Z` inside the pattern; overrides Opts::ignorecase.
    case_override: Option<bool>,
    warnings: Vec<String>,
}

/// Translate a Vim magic-mode regex to a fancy-regex pattern plus any
/// prefix-check obligations the engine must verify at the match start.
pub fn translate(re: &str, opts: &Opts) -> Result<Translated> {
    let mut p = Parser {
        cs: re.chars().collect(),
        i: 0,
        case_override: None,
        warnings: Vec::new(),
    };
    let ast = p.parse_alt()?;
    if p.i != p.cs.len() {
        // Unbalanced group: trailing `\)`.
        return Err(TranslateError(format!(
            "unexpected token at offset {} in {:?}",
            p.i, re
        )));
    }

    if opts.scan && contains_match_marker(&ast) {
        // \zs/\ze move match boundaries in ways the scan emitter cannot
        // express; such words keep using the fancy-regex scan union
        return Err(TranslateError(
            r"scan mode does not support \zs/\ze".to_string(),
        ));
    }

    // Collect backref numbers to decide which captures must be kept when
    // captures are being stripped.
    let mut backrefs = std::collections::HashSet::new();
    collect_backrefs(&ast, &mut backrefs);
    let max_backref = backrefs.iter().copied().max().unwrap_or(0);

    let insensitive = p.case_override.unwrap_or(opts.ignorecase);
    let mut ec = EmitCtx {
        opts,
        group: 0,
        max_keep_group: if opts.captures { u32::MAX } else { max_backref },
        top: true,
        leading: true,
        insensitive,
        prefix_checks: Vec::new(),
    };
    let mut out = String::new();
    if insensitive {
        out.push_str("(?i)");
    }
    emit(&ast, &mut out, &mut ec)?;
    Ok(Translated {
        pattern: out,
        prefix_checks: ec.prefix_checks,
        warnings: p.warnings,
    })
}

fn collect_backrefs(n: &Node, out: &mut std::collections::HashSet<u32>) {
    match n {
        Node::Backref(b) => {
            out.insert(*b);
        }
        Node::Group { alt, .. } | Node::Optional { alt } | Node::Alt(alt) | Node::Conj(alt) => {
            for a in alt {
                collect_backrefs(a, out);
            }
        }
        Node::Concat(v) => {
            for a in v {
                collect_backrefs(a, out);
            }
        }
        Node::Quantified { atom, .. }
        | Node::Lookahead { atom, .. }
        | Node::Lookbehind { atom, .. } => {
            collect_backrefs(atom, out);
        }
        _ => {}
    }
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.cs.get(self.i).copied()
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.cs.get(self.i + k).copied()
    }

    fn starts(&self, s: &str) -> bool {
        let mut j = self.i;
        for c in s.chars() {
            if self.cs.get(j) != Some(&c) {
                return false;
            }
            j += 1;
        }
        true
    }

    fn eat(&mut self, n: usize) {
        self.i += n;
    }

    fn parse_alt(&mut self) -> Result<Node> {
        let mut branches = vec![self.parse_concat()?];
        while self.starts(r"\|") && self.not_bslash_at(self.i) {
            self.eat(2);
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    /// not_bslash check for an explicit position (the token itself starts
    /// with a backslash, so count backslashes before it).
    fn not_bslash_at(&self, pos: usize) -> bool {
        let mut n = 0;
        let mut j = pos;
        while j > 0 && self.cs[j - 1] == '\\' {
            n += 1;
            j -= 1;
        }
        n % 2 == 0
    }

    fn parse_concat(&mut self) -> Result<Node> {
        let mut parts: Vec<Node> = Vec::new();
        let mut cur: Vec<Node> = Vec::new();
        loop {
            if self.i >= self.cs.len() {
                break;
            }
            // Branch terminators.
            if self.starts(r"\)") || self.starts(r"\|") {
                break;
            }
            if self.starts(r"\&") && self.not_bslash_at(self.i) {
                self.eat(2);
                parts.push(mk_concat(std::mem::take(&mut cur)));
                continue;
            }
            let atom = self.parse_atom()?;
            cur.push(atom);
        }
        parts.push(mk_concat(cur));
        if parts.len() == 1 {
            Ok(parts.pop().unwrap())
        } else {
            // A\&B -> all branches anchored at the same position, extent of
            // the last one. Trailing empty branches (vim's `re\&` overlap
            // hack) are dropped; the Rust engine controls overlap itself.
            while parts.len() > 1 {
                match parts.last() {
                    Some(Node::Empty) => {
                        parts.pop();
                    }
                    Some(Node::Concat(v)) if v.is_empty() => {
                        parts.pop();
                    }
                    _ => break,
                }
            }
            if parts.len() == 1 {
                Ok(parts.pop().unwrap())
            } else {
                Ok(Node::Conj(parts))
            }
        }
    }

    fn parse_atom(&mut self) -> Result<Node> {
        let mut atom = self.parse_atom_bare()?;
        // Quantifier.
        atom = self.parse_quantified(atom)?;
        // Lookaround suffix `\@=`, `\@!`, `\@<=`, `\@<!`, `\@N<=`, `\@>`.
        if self.starts(r"\@") && self.not_bslash_at(self.i) {
            self.eat(2);
            let (neg, behind) = if self.starts("=") {
                self.eat(1);
                (false, false)
            } else if self.starts("!") {
                self.eat(1);
                (true, false)
            } else if self.starts("<=") {
                self.eat(2);
                (false, true)
            } else if self.starts("<!") {
                self.eat(2);
                (true, true)
            } else {
                // `\@N<=` / `\@N<!` bounded lookbehind: consume digits.
                let mut k = 0;
                while matches!(self.peek_at(k), Some(c) if c.is_ascii_digit()) {
                    k += 1;
                }
                if k > 0 && self.starts_at(k, "<=") {
                    self.warnings
                        .push("bounded lookbehind width dropped".to_string());
                    self.eat(k + 2);
                    (false, true)
                } else if k > 0 && self.starts_at(k, "<!") {
                    self.warnings
                        .push("bounded lookbehind width dropped".to_string());
                    self.eat(k + 2);
                    (true, true)
                } else if self.starts(">") {
                    // Possessive `\@>`: no fancy-regex equivalent; approximate
                    // with the plain atom.
                    self.eat(1);
                    self.warnings
                        .push("possessive quantifier approximated".to_string());
                    return Ok(atom);
                } else {
                    return Err(TranslateError(format!(
                        "bad \\@ sequence at offset {}",
                        self.i
                    )));
                }
            };
            atom = if behind {
                Node::Lookbehind {
                    atom: Box::new(atom),
                    neg,
                }
            } else {
                Node::Lookahead {
                    atom: Box::new(atom),
                    neg,
                }
            };
        }
        Ok(atom)
    }

    fn starts_at(&self, off: usize, s: &str) -> bool {
        let mut j = self.i + off;
        for c in s.chars() {
            if self.cs.get(j) != Some(&c) {
                return false;
            }
            j += 1;
        }
        true
    }

    fn parse_quantified(&mut self, atom: Node) -> Result<Node> {
        let (q, greedy) = if self.peek() == Some('*') {
            self.eat(1);
            (Quant::Star, true)
        } else if self.starts(r"\+") && self.not_bslash_at(self.i) {
            self.eat(2);
            (Quant::Plus, true)
        } else if (self.starts(r"\?") || self.starts(r"\=")) && self.not_bslash_at(self.i) {
            self.eat(2);
            (Quant::Quest, true)
        } else if self.starts(r"\%=") && self.not_bslash_at(self.i) {
            self.eat(3);
            (Quant::Quest, true)
        } else if self.starts(r"\{") && self.not_bslash_at(self.i) {
            match self.parse_brace_quant() {
                Some(r) => r,
                None => return Ok(atom),
            }
        } else {
            return Ok(atom);
        };
        Ok(Node::Quantified {
            atom: Box::new(atom),
            q,
            greedy,
        })
    }

    /// Parse `\{n,m}`, `\{n,}`, `\{,m}`, `\{n}`, and lazy `\{-...}` forms.
    fn parse_brace_quant(&mut self) -> Option<(Quant, bool)> {
        let start = self.i;
        let mut j = self.i + 2;
        let greedy = if self.cs.get(j) == Some(&'-') {
            j += 1;
            false
        } else {
            true
        };
        let mut min: Option<u32> = None;
        let mut n: u32 = 0;
        let mut digits = false;
        while matches!(self.cs.get(j), Some(c) if c.is_ascii_digit()) {
            n = n.saturating_mul(10) + (self.cs[j] as u32 - '0' as u32);
            digits = true;
            j += 1;
        }
        if digits {
            min = Some(n);
        }
        let mut max = None;
        if self.cs.get(j) == Some(&',') {
            j += 1;
            let mut m: u32 = 0;
            let mut mdigits = false;
            while matches!(self.cs.get(j), Some(c) if c.is_ascii_digit()) {
                m = m.saturating_mul(10) + (self.cs[j] as u32 - '0' as u32);
                mdigits = true;
                j += 1;
            }
            if mdigits {
                max = Some(m);
            }
        } else if let Some(mn) = min {
            max = Some(mn);
        }
        if self.cs.get(j) != Some(&'}') {
            // Not a valid quantifier; `\{` is a literal `{`.
            self.i = start;
            return None;
        }
        self.i = j + 1;
        let q = match (min, max) {
            (None, None) => Quant::Star,
            (Some(_), _) => Quant::Range(min.unwrap(), max),
            (None, Some(_)) => Quant::Range(0, max),
        };
        Some((q, greedy))
    }

    fn parse_atom_bare(&mut self) -> Result<Node> {
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(Node::Empty),
        };
        if c == '\\' {
            return self.parse_escape();
        }
        if c == '[' {
            return self.parse_bracket();
        }
        self.eat(1);
        Ok(match c {
            '.' => Node::Raw(".".to_string()),
            '^' => Node::AnchorStart,
            '$' => {
                // `$` anchors only at the end of a branch in vim.
                if self.at_branch_end() {
                    Node::AnchorEnd
                } else {
                    Node::Raw(r"\$".to_string())
                }
            }
            // Literal in vim magic, special in Rust: escape on emit.
            _ => Node::Raw(lit(&c.to_string())),
        })
    }

    fn at_branch_end(&self) -> bool {
        matches!(self.peek(), None)
            || self.starts(r"\|")
            || self.starts(r"\)")
            || self.starts(r"\&")
            || self.starts(r"\]")
    }

    fn parse_escape(&mut self) -> Result<Node> {
        // self.cs[self.i] == '\\'
        let e = match self.peek_at(1) {
            Some(e) => e,
            None => {
                self.eat(1);
                return Ok(Node::Raw(lit(r"\")));
            }
        };
        match e {
            '(' => {
                self.eat(2);
                let alt = self.parse_alt()?;
                self.expect_close()?;
                Ok(Node::Group {
                    capture: true,
                    alt: vec![alt],
                })
            }
            ')' => Err(TranslateError("unbalanced \\)".to_string())),
            '%' => self.parse_percent(),
            '|' => {
                // Should have been consumed by parse_alt; defensive literal.
                self.eat(2);
                Ok(Node::Raw(lit("|")))
            }
            '&' => {
                self.eat(2);
                Ok(Node::Raw(lit("&")))
            }
            '*' => {
                self.eat(2);
                Ok(Node::Raw(lit("*")))
            }
            '+' | '=' | '?' | '{' => {
                // Quantifiers, handled in parse_quantified; literal here.
                self.eat(2);
                Ok(Node::Raw(lit(&e.to_string())))
            }
            '<' => {
                self.eat(2);
                Ok(Node::WordStart)
            }
            '>' => {
                self.eat(2);
                Ok(Node::WordEnd)
            }
            'z' => {
                if self.starts(r"\zs") {
                    self.eat(3);
                    Ok(Node::MatchStart)
                } else if self.starts(r"\ze") {
                    self.eat(3);
                    Ok(Node::MatchEnd)
                } else {
                    self.eat(2);
                    Ok(Node::Raw(lit("z")))
                }
            }
            'c' => {
                self.eat(2);
                self.case_override.get_or_insert(true);
                Ok(Node::Empty)
            }
            'C' => {
                self.eat(2);
                self.case_override.get_or_insert(false);
                Ok(Node::Empty)
            }
            'Z' => {
                self.eat(2);
                self.case_override.get_or_insert(true);
                Ok(Node::Empty)
            }
            'm' | 'M' | 'v' | 'V' => {
                self.eat(2);
                if e == 'v' || e == 'V' {
                    self.warnings
                        .push("magic-mode switch inside pattern ignored".to_string());
                }
                Ok(Node::Empty)
            }
            's' => cls(self, r"\s"),
            'S' => cls(self, r"\S"),
            'd' => cls(self, r"\d"),
            'D' => cls(self, r"\D"),
            'w' => cls(self, "@WORD@"),
            'W' => cls(self, "@NWORD@"),
            'k' => cls(self, "@WORD@"),
            'K' => cls(self, "@NWORD@"),
            'h' => cls(self, "[A-Za-z_]"),
            'H' => cls(self, "[^A-Za-z_]"),
            'a' => cls(self, "[A-Za-z]"),
            'A' => cls(self, "[^A-Za-z]"),
            'l' => cls(self, "[a-z]"),
            'L' => cls(self, "[^a-z]"),
            'u' => cls(self, "[A-Z]"),
            'U' => cls(self, "[^A-Z]"),
            'x' => cls(self, "[0-9A-Fa-f]"),
            'X' => cls(self, "[^0-9A-Fa-f]"),
            'o' => cls(self, "[0-7]"),
            'O' => cls(self, "[^0-7]"),
            'f' => cls(self, r"[\w.,+\-/#~$]"),
            'F' => cls(self, r"[^\w.,+\-/#~$]"),
            'p' => cls(self, "[^\\x00-\\x1F]"),
            'P' => cls(self, "[\\x00-\\x1F]"),
            't' => {
                self.eat(2);
                Ok(Node::Raw(r"\t".to_string()))
            }
            'e' => {
                self.eat(2);
                Ok(Node::Raw(r"\x1B".to_string()))
            }
            'r' => {
                self.eat(2);
                Ok(Node::Raw(r"\r".to_string()))
            }
            'b' => {
                self.eat(2);
                Ok(Node::Raw(r"\x08".to_string()))
            }
            'n' => {
                self.eat(2);
                // Never matches: the engine scans single lines.
                Ok(Node::Raw(r"\n".to_string()))
            }
            '_' => {
                self.eat(2);
                self.parse_underscore_class()
            }
            '1'..='9' => {
                self.eat(2);
                Ok(Node::Backref(e as u32 - '0' as u32))
            }
            '0' => {
                self.eat(2);
                self.warnings.push(r"\0 backref unsupported".to_string());
                Ok(Node::Empty)
            }
            _ => {
                // Unknown escape: literal character.
                self.eat(2);
                Ok(Node::Raw(lit(&e.to_string())))
            }
        }
    }

    fn parse_underscore_class(&mut self) -> Result<Node> {
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(Node::Raw("_".to_string())),
        };
        self.eat(1);
        Ok(match c {
            '.' => Node::Raw("(?s:.)".to_string()),
            's' => Node::Raw(r"\s".to_string()),
            'S' => Node::Raw(r"\S".to_string()),
            'd' => Node::Raw(r"[\d\n]".to_string()),
            'D' => Node::Raw(r"[\D\n]".to_string()),
            'w' | 'k' => Node::Raw("(?:@WORD@|\n)".to_string()),
            'W' | 'K' => Node::Raw("(?:@NWORD@)".to_string()),
            '[' => {
                // `\_x` where x is a bracket: include newline.
                self.i -= 1; // put back the '['
                let mut cls = self.parse_bracket_raw()?;
                if cls.starts_with("[^") {
                    self.warnings
                        .push(r"newline inclusion in \_[^...] dropped".to_string());
                } else {
                    cls.insert_str(1, r"\n");
                }
                Node::Class(cls)
            }
            _ => Node::Raw(lit(&format!("_{c}"))),
        })
    }

    fn parse_percent(&mut self) -> Result<Node> {
        // self.i points at '\', self.i+1 == '%'
        let c = match self.peek_at(2) {
            Some(c) => c,
            None => {
                self.eat(2);
                return Ok(Node::Raw(lit("%")));
            }
        };
        match c {
            '(' => {
                self.eat(3);
                let alt = self.parse_alt()?;
                self.expect_close()?;
                Ok(Node::Group {
                    capture: false,
                    alt: vec![alt],
                })
            }
            '[' => {
                self.eat(3);
                let alt = self.parse_alt_opt()?;
                Ok(Node::Optional { alt: vec![alt] })
            }
            '=' => {
                self.eat(3);
                Ok(Node::Empty) // handled as quantifier normally
            }
            '^' => {
                self.eat(3);
                Ok(Node::AnchorStart)
            }
            '$' => {
                self.eat(3);
                Ok(Node::AnchorEnd)
            }
            '#' => {
                // `\%#=N` regexp engine selection: strip.
                self.eat(3);
                if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.eat(1);
                }
                if self.peek() == Some('=') {
                    self.eat(1);
                    if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                        self.eat(1);
                    }
                }
                Ok(Node::Empty)
            }
            '>' | '<' | '0'..='9' => {
                // Column/line/virtual constraints: the Rust engine applies
                // position filtering itself; strip.
                self.eat(3);
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.eat(1);
                }
                if matches!(self.peek(), Some('c' | 'l' | 'v')) {
                    self.eat(1);
                }
                Ok(Node::Empty)
            }
            'd' | 'x' | 'o' | 'u' | 'U' | 'c' => {
                self.eat(3);
                let radix = match c {
                    'd' => 10,
                    'x' => 16,
                    'o' => 8,
                    'u' | 'U' => 16,
                    _ => 0, // \%cX: next char literally
                };
                if c == 'c' {
                    let ch = self.peek().unwrap_or(' ');
                    self.eat(1);
                    return Ok(Node::Raw(lit(&ch.to_string())));
                }
                let mut n: u32 = 0;
                let mut digits = 0;
                while let Some(ch) = self.peek() {
                    let d = ch.to_digit(radix);
                    match d {
                        Some(d) => {
                            n = n.saturating_mul(radix) + d;
                            digits += 1;
                            self.eat(1);
                        }
                        None => break,
                    }
                }
                if digits == 0 {
                    return Ok(Node::Raw(lit("%")));
                }
                let ch = char::from_u32(n).unwrap_or('\u{FFFD}');
                Ok(Node::Raw(lit(&ch.to_string())))
            }
            _ => {
                self.eat(2);
                Ok(Node::Raw(lit("%")))
            }
        }
    }

    /// Parse the body of `\%[...]` up to the matching bare `]`
    /// (vim closes `\%[` with an unescaped `]`; `\]` inside is a literal).
    fn parse_alt_opt(&mut self) -> Result<Node> {
        let mut branches = vec![self.parse_concat_opt()?];
        while self.starts(r"\|") && self.not_bslash_at(self.i) {
            self.eat(2);
            branches.push(self.parse_concat_opt()?);
        }
        if self.peek() == Some(']') && self.not_bslash_at(self.i) {
            self.eat(1);
        } else {
            return Err(TranslateError(r"unbalanced \%[".to_string()));
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat_opt(&mut self) -> Result<Node> {
        let mut cur: Vec<Node> = Vec::new();
        loop {
            if self.i >= self.cs.len() || self.starts(r"\|") {
                break;
            }
            if self.peek() == Some(']') && self.not_bslash_at(self.i) {
                break;
            }
            // Nested groups inside \%[ terminate on `\)` as usual; parse_atom
            // handles them. A nested `\&` is not expected; treat as literal.
            if self.starts(r"\&") && self.not_bslash_at(self.i) {
                self.eat(2);
                cur.push(Node::Raw(lit("&")));
                continue;
            }
            cur.push(self.parse_atom()?);
        }
        Ok(mk_concat(cur))
    }

    fn expect_close(&mut self) -> Result<()> {
        if self.starts(r"\)") && self.not_bslash_at(self.i) {
            self.eat(2);
            Ok(())
        } else {
            Err(TranslateError(r"unbalanced \(".to_string()))
        }
    }

    fn parse_bracket(&mut self) -> Result<Node> {
        let s = self.parse_bracket_raw()?;
        Ok(Node::Class(s))
    }

    /// Parse `[...]` starting at self.i == '['; returns rendered Rust class.
    fn parse_bracket_raw(&mut self) -> Result<String> {
        debug_assert_eq!(self.peek(), Some('['));
        self.eat(1);
        let mut out = String::from("[");
        let negated = self.peek() == Some('^');
        if negated {
            out.push('^');
            self.eat(1);
        }
        // A `]` immediately after `[` or `[^` is literal in vim.
        let mut first = true;
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err(TranslateError("unbalanced [".to_string())),
            };
            if c == ']' && !first {
                self.eat(1);
                break;
            }
            first = false;
            if c == '\\' {
                let e = self.peek_at(1).unwrap_or('\\');
                self.eat(2);
                match e {
                    ']' => out.push_str(r"\]"),
                    '\\' => out.push_str(r"\\"),
                    '^' => out.push_str(r"\^"),
                    '-' => out.push_str(r"\-"),
                    'n' => out.push_str(r"\n"),
                    'r' => out.push_str(r"\r"),
                    't' => out.push_str(r"\t"),
                    'e' => out.push_str(r"\x1B"),
                    'b' => out.push_str(r"\x08"),
                    // Vim does not recognize class escapes (\d, \s, ...)
                    // inside brackets; `\x` degrades to the literal `x`.
                    _ => push_escaped_char(&mut out, e),
                }
                continue;
            }
            if c == '[' {
                // POSIX class `[[:alpha:]]`, equivalence `[=x=]`, collation
                // `[.x.]` (inner bracket constructs).
                if self.starts("[:") {
                    if let Some(end) = self.find_str(":]") {
                        let inner: String = self.cs[self.i + 2..end].iter().collect();
                        out.push_str(&format!("[:{inner}:]"));
                        self.i = end + 2;
                        continue;
                    }
                } else if self.starts("[=") || self.starts("[.") {
                    let close = if self.starts("[=") { "=]" } else { ".]" };
                    if let Some(end) = self.find_str(close) {
                        let inner: String = self.cs[self.i + 2..end].iter().collect();
                        out.push_str(&lit(&inner));
                        self.i = end + 2;
                        continue;
                    }
                }
                out.push_str(r"\[");
                self.eat(1);
                continue;
            }
            self.eat(1);
            push_escaped_char(&mut out, c);
        }
        out.push(']');
        Ok(out)
    }

    fn find_str(&self, s: &str) -> Option<usize> {
        let pat: Vec<char> = s.chars().collect();
        let mut j = self.i;
        while j + pat.len() <= self.cs.len() {
            if self.cs[j..j + pat.len()] == pat[..] {
                return Some(j);
            }
            j += 1;
        }
        None
    }
}

fn mk_concat(v: Vec<Node>) -> Node {
    if v.is_empty() {
        Node::Empty
    } else if v.len() == 1 {
        v.into_iter().next().unwrap()
    } else {
        Node::Concat(v)
    }
}

fn cls(p: &mut Parser, frag: &str) -> Result<Node> {
    p.eat(2);
    Ok(Node::Raw(frag.to_string()))
}

/// Escape a string for use as a literal in a fancy-regex pattern.
fn lit(s: &str) -> String {
    fancy_regex::escape(s).into_owned()
}

fn push_escaped_char(out: &mut String, c: char) {
    match c {
        // `-` stays bare: it is a range operator in vim and rust alike.
        ']' | '\\' | '^' | '[' => {
            out.push('\\');
            out.push(c);
        }
        _ => out.push(c),
    }
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

struct EmitCtx<'a> {
    opts: &'a Opts,
    group: u32,
    max_keep_group: u32,
    top: bool,
    /// True while the emit position coincides with the match start, so a
    /// variable-width lookbehind can be converted to a prefix check.
    leading: bool,
    insensitive: bool,
    prefix_checks: Vec<PrefixCheck>,
}

/// Minimum number of characters `n` can match.
fn min_size(n: &Node) -> usize {
    match n {
        Node::Empty
        | Node::AnchorStart
        | Node::AnchorEnd
        | Node::WordStart
        | Node::WordEnd
        | Node::MatchStart
        | Node::MatchEnd
        | Node::Lookahead { .. }
        | Node::Lookbehind { .. } => 0,
        Node::Raw(_) | Node::Class(_) | Node::Backref(_) => 1,
        Node::Group { alt, .. } => alt.iter().map(min_size).min().unwrap_or(0),
        Node::Optional { .. } => 0,
        Node::Quantified { atom, q, .. } => match q {
            Quant::Star | Quant::Quest => 0,
            Quant::Plus => min_size(atom),
            Quant::Range(a, _) => min_size(atom).saturating_mul(*a as usize),
        },
        Node::Concat(v) => v.iter().map(min_size).sum(),
        Node::Alt(v) => v.iter().map(min_size).min().unwrap_or(0),
        Node::Conj(v) => v.last().map(min_size).unwrap_or(0),
    }
}

/// True when `n` always matches exactly the same number of characters
/// (fancy-regex requires this for native lookbehind). Alternation branches
/// may have different fixed sizes.
fn fixed_size(n: &Node) -> bool {
    match n {
        Node::Empty
        | Node::AnchorStart
        | Node::AnchorEnd
        | Node::WordStart
        | Node::WordEnd
        | Node::MatchStart
        | Node::MatchEnd
        | Node::Raw(_)
        | Node::Class(_)
        | Node::Lookahead { .. }
        | Node::Lookbehind { .. } => true,
        Node::Backref(_) => false,
        Node::Group { alt, .. } => alt.iter().all(fixed_size),
        Node::Optional { alt } => alt.iter().all(|a| min_size(a) == 0 && fixed_size(a)),
        Node::Quantified { atom, q, .. } => match q {
            Quant::Range(a, Some(b)) if a == b => fixed_size(atom),
            _ => min_size(atom) == 0 && fixed_size(atom),
        },
        Node::Concat(v) => v.iter().all(fixed_size),
        Node::Alt(v) => v.iter().all(fixed_size),
        Node::Conj(_) => false,
    }
}

fn contains_capture(n: &Node) -> bool {
    match n {
        Node::Group { capture, alt } => *capture || alt.iter().any(contains_capture),
        Node::Optional { alt } | Node::Alt(alt) | Node::Conj(alt) => {
            alt.iter().any(contains_capture)
        }
        Node::Concat(v) => v.iter().any(contains_capture),
        Node::Quantified { atom, .. }
        | Node::Lookahead { atom, .. }
        | Node::Lookbehind { atom, .. } => contains_capture(atom),
        _ => false,
    }
}

fn contains_match_marker(n: &Node) -> bool {
    match n {
        Node::MatchStart | Node::MatchEnd => true,
        Node::Group { alt, .. } | Node::Optional { alt } | Node::Alt(alt) | Node::Conj(alt) => {
            alt.iter().any(contains_match_marker)
        }
        Node::Concat(v) => v.iter().any(contains_match_marker),
        Node::Quantified { atom, .. }
        | Node::Lookahead { atom, .. }
        | Node::Lookbehind { atom, .. } => contains_match_marker(atom),
        _ => false,
    }
}

/// Emit a `\%[...]` branch as progressively-optional atoms:
/// `[a, b, c]` becomes `(?:a(?:b(?:c)?)?)?`.
fn emit_seq_opt(a: &Node, out: &mut String, ec: &mut EmitCtx) -> Result<()> {
    let atoms: Vec<&Node> = match a {
        Node::Concat(v) => v.iter().collect(),
        Node::Empty => return Ok(()),
        other => vec![other],
    };
    for atom in &atoms {
        out.push_str("(?:");
        emit(atom, out, ec)?;
    }
    for _ in &atoms {
        out.push_str(")?");
    }
    Ok(())
}

fn emit(n: &Node, out: &mut String, ec: &mut EmitCtx) -> Result<()> {
    match n {
        Node::Empty => {}
        Node::Raw(s) => {
            let s = s
                .replace("@NWORD@", &neg_word(ec.opts))
                .replace("@WORD@", &ec.opts.word);
            out.push_str(&s);
        }
        Node::Class(s) => {
            let s = s
                .replace("@NWORD@", &neg_word_inner(ec.opts))
                .replace("@WORD@", &word_inner(ec.opts));
            out.push_str(&s);
        }
        Node::Group { capture, alt } => {
            ec.group += 1;
            let g = ec.group;
            let keep = *capture && (ec.opts.captures || g <= ec.max_keep_group);
            if keep {
                out.push('(');
            } else {
                out.push_str("(?:");
            }
            let was_top = std::mem::replace(&mut ec.top, false);
            for a in alt {
                emit(a, out, ec)?;
            }
            ec.top = was_top;
            out.push(')');
        }
        Node::Optional { alt } => {
            // vim `\%[abc]` matches '', 'a', 'ab' or 'abc': each atom is
            // progressively optional, so emit nested optionals rather than
            // one all-or-nothing group
            let was_top = std::mem::replace(&mut ec.top, false);
            let multi = alt.len() != 1;
            if multi {
                out.push_str("(?:");
            }
            for (bi, a) in alt.iter().enumerate() {
                if bi > 0 {
                    out.push('|');
                }
                emit_seq_opt(a, out, ec)?;
            }
            if multi {
                out.push(')');
            }
            ec.top = was_top;
        }
        Node::Quantified { atom, q, greedy } => {
            let needs_group = !matches!(
                atom.as_ref(),
                Node::Raw(_) | Node::Class(_) | Node::Group { .. } | Node::Optional { .. }
            );
            if needs_group {
                out.push_str("(?:");
            }
            let was_top = std::mem::replace(&mut ec.top, false);
            emit(atom, out, ec)?;
            ec.top = was_top;
            if needs_group {
                out.push(')');
            }
            match q {
                Quant::Star => out.push('*'),
                Quant::Plus => out.push('+'),
                Quant::Quest => out.push('?'),
                Quant::Range(a, b) => match b {
                    Some(b) if a == b => out.push_str(&format!("{{{a}}}")),
                    Some(b) => out.push_str(&format!("{{{a},{b}}}")),
                    None => out.push_str(&format!("{{{a},}}")),
                },
            }
            if !greedy {
                out.push('?');
            }
        }
        Node::Lookahead { atom, neg } => {
            if ec.opts.scan {
                // over-approximate: dropping the assertion only admits more
                // candidate positions, which classify filters exactly
                return Ok(());
            }
            out.push_str(if *neg { "(?!" } else { "(?=" });
            let was_top = std::mem::replace(&mut ec.top, false);
            let was_leading = std::mem::replace(&mut ec.leading, false);
            emit(atom, out, ec)?;
            ec.leading = was_leading;
            ec.top = was_top;
            out.push(')');
        }
        Node::Lookbehind { atom, neg } => {
            if ec.opts.scan {
                return Ok(());
            }
            if fixed_size(atom) {
                out.push_str(if *neg { "(?<!" } else { "(?<=" });
                let was_top = std::mem::replace(&mut ec.top, false);
                let was_leading = std::mem::replace(&mut ec.leading, false);
                emit(atom, out, ec)?;
                ec.leading = was_leading;
                ec.top = was_top;
                out.push(')');
            } else if ec.leading && !contains_capture(atom) {
                // fancy-regex only supports constant-size lookbehind.
                // A variable-width lookbehind at the match start becomes a
                // prefix-check obligation verified by the engine.
                let check = emit_standalone(std::slice::from_ref(atom.as_ref()), ec)?;
                ec.prefix_checks.push(PrefixCheck {
                    pattern: check,
                    neg: *neg,
                });
            } else {
                return Err(TranslateError(
                    "variable-width lookbehind with captures or in non-leading position"
                        .to_string(),
                ));
            }
        }
        Node::Backref(b) => {
            if ec.opts.scan {
                return Err(TranslateError(
                    "backref not supported in scan mode".to_string(),
                ));
            }
            out.push_str(&format!("\\{b}"));
        }
        Node::Concat(v) => {
            if ec.top {
                emit_top_concat(v, out, ec)?;
            } else {
                for x in v {
                    emit(x, out, ec)?;
                }
            }
        }
        Node::Alt(v) => {
            let was_top = std::mem::replace(&mut ec.top, false);
            let was_leading = ec.leading;
            for (k, x) in v.iter().enumerate() {
                if k > 0 {
                    out.push('|');
                }
                ec.leading = was_leading;
                emit(x, out, ec)?;
            }
            ec.leading = was_leading && v.iter().all(|x| min_size(x) == 0);
            ec.top = was_top;
        }
        Node::Conj(v) => {
            // (?=A)(?=B)C — extent of the last branch.
            let was_top = std::mem::replace(&mut ec.top, false);
            if ec.opts.scan {
                // over-approximate: keep only the extent-determining branch
                emit(v.last().unwrap(), out, ec)?;
                ec.top = was_top;
                return Ok(());
            }
            for x in &v[..v.len() - 1] {
                out.push_str("(?=");
                let was_leading = std::mem::replace(&mut ec.leading, false);
                emit(x, out, ec)?;
                ec.leading = was_leading;
                out.push(')');
            }
            emit(v.last().unwrap(), out, ec)?;
            ec.top = was_top;
        }
        Node::AnchorStart => out.push('^'),
        Node::AnchorEnd => out.push('$'),
        Node::WordStart => {
            if !ec.opts.scan {
                out.push_str(&format!("(?<!{w})(?={w})", w = ec.opts.word));
            }
        }
        Node::WordEnd => {
            if !ec.opts.scan {
                out.push_str(&format!("(?<={})(?!{})", ec.opts.word, ec.opts.word));
            }
        }
        Node::MatchStart | Node::MatchEnd => {
            // Only meaningful at top level; dropped elsewhere.
        }
    }
    if min_size(n) > 0 {
        ec.leading = false;
    }
    Ok(())
}

/// Emit a node sequence as a self-contained pattern (used for prefix-check
/// obligations). Nested obligations are not supported.
fn emit_standalone(nodes: &[Node], ec: &EmitCtx) -> Result<String> {
    let mut out = String::new();
    if ec.insensitive {
        out.push_str("(?i)");
    }
    let mut sub = EmitCtx {
        opts: ec.opts,
        group: 0,
        max_keep_group: u32::MAX,
        top: false,
        leading: false,
        insensitive: ec.insensitive,
        prefix_checks: Vec::new(),
    };
    for n in nodes {
        emit(n, &mut out, &mut sub)?;
    }
    if !sub.prefix_checks.is_empty() {
        return Err(TranslateError(
            "nested variable-width lookbehind".to_string(),
        ));
    }
    Ok(out)
}

/// Top-level concat: apply `\zs`/`\ze` semantics by moving the excluded
/// parts into lookbehind/lookahead assertions.
fn emit_top_concat(v: &[Node], out: &mut String, ec: &mut EmitCtx) -> Result<()> {
    // Last \zs wins for the match start; first \ze (after that) for the end.
    let zs = v.iter().rposition(|n| matches!(n, Node::MatchStart));
    let (pre, rest) = match zs {
        Some(k) => (&v[..k], &v[k + 1..]),
        None => (&v[..0], v),
    };
    let ze = rest.iter().position(|n| matches!(n, Node::MatchEnd));
    let (mid, post) = match ze {
        Some(k) => (&rest[..k], &rest[k + 1..]),
        None => (rest, &rest[..0]),
    };

    ec.top = false;
    if !pre.is_empty() {
        if pre.iter().all(|x| min_size(x) == 0 && fixed_size(x)) {
            // Zero-width prefix (`^`, `\<`, lookarounds): keep inline; it
            // asserts at the match start.
            for x in pre {
                emit(x, out, ec)?;
            }
            ec.leading = true;
        } else if pre.iter().all(fixed_size) {
            out.push_str("(?<=");
            let was_leading = std::mem::replace(&mut ec.leading, false);
            for x in pre {
                emit(x, out, ec)?;
            }
            ec.leading = was_leading;
            out.push(')');
        } else if pre.iter().any(contains_capture) {
            return Err(TranslateError(
                r"variable-width \zs prefix with capture groups".to_string(),
            ));
        } else {
            let check = emit_standalone(pre, ec)?;
            ec.prefix_checks.push(PrefixCheck {
                pattern: check,
                neg: false,
            });
        }
    }
    // The matched extent starts here.
    ec.leading = true;
    for x in mid {
        emit(x, out, ec)?;
    }
    if !post.is_empty() {
        out.push_str("(?=");
        let was_leading = std::mem::replace(&mut ec.leading, false);
        for x in post {
            emit(x, out, ec)?;
        }
        ec.leading = was_leading;
        out.push(')');
    }
    ec.top = true;
    Ok(())
}

fn word_inner(opts: &Opts) -> String {
    // For embedding inside a bracket expression.
    let w = &opts.word;
    if w == r"\w" {
        r"\w".to_string()
    } else if w.starts_with('[') && w.ends_with(']') {
        w[1..w.len() - 1].to_string()
    } else {
        w.clone()
    }
}

fn neg_word(opts: &Opts) -> String {
    if opts.word == r"\w" {
        r"\W".to_string()
    } else {
        format!("[^{}]", word_inner(opts))
    }
}

fn neg_word_inner(opts: &Opts) -> String {
    if opts.word == r"\w" {
        r"\W".to_string()
    } else {
        format!("^{}", word_inner(opts))
    }
}

// ---------------------------------------------------------------------------
// Capture group extraction (port of matchup#loader#get_capture_groups)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CaptureGroup {
    /// Full source of the group including `\(` and `\)`.
    pub str: String,
    /// Nesting depth among capture groups (1 = outermost).
    pub depth: usize,
    /// Enclosing capture group number, 0 if none.
    pub parent: usize,
    /// Byte offsets [start, end) of the group in the source pattern.
    pub pos: (usize, usize),
}

/// Extract `\(...\)` capture groups from a vim regex, keyed by group number.
/// Port of `matchup#loader#get_capture_groups` (loader.vim:635).
pub fn get_capture_groups(s: &str) -> Vec<(usize, CaptureGroup)> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut out: Vec<(usize, CaptureGroup)> = Vec::new();
    // Stack of (group_number_or_0, start_byte_offset); 0 marks `\%(`.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut counter = 0usize;

    while i < b.len() {
        if b[i] != b'\\' || !not_bslash_at(b, i) {
            i += 1;
            continue;
        }
        if i + 1 < b.len() && b[i + 1] == b'(' {
            counter += 1;
            stack.push((counter, i));
            i += 2;
        } else if i + 2 < b.len() && b[i + 1] == b'%' && b[i + 2] == b'(' {
            stack.push((0, i));
            i += 3;
        } else if i + 1 < b.len() && b[i + 1] == b')' {
            let (n, start) = match stack.pop() {
                Some(x) => x,
                None => break,
            };
            i += 2;
            if n < 1 {
                continue;
            }
            let end = i;
            let gstr = s[start..end].to_string();
            let cgstack: Vec<usize> = stack.iter().map(|x| x.0).filter(|x| *x > 0).collect();
            let depth = cgstack.len() + 1;
            let parent = if cgstack.len() >= 1 {
                *cgstack.last().unwrap()
            } else {
                0
            };
            out.push((
                n,
                CaptureGroup {
                    str: gstr,
                    depth,
                    parent,
                    pos: (start, end),
                },
            ));
        } else {
            i += 1;
        }
    }
    out.sort_by_key(|x| x.0);
    out
}

fn not_bslash_at(b: &[u8], pos: usize) -> bool {
    let mut n = 0;
    let mut j = pos;
    while j > 0 && b[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 0
}

/// Split `s` on occurrences of `delim` not preceded by a backslash
/// (vim's `g:matchup#re#not_bslash` splitting).
pub fn split_not_bslash(s: &str, delim: char) -> Vec<String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == delim {
            let mut n = 0;
            let mut j = i;
            while j > 0 && b[j - 1] == '\\' {
                n += 1;
                j -= 1;
            }
            if n % 2 == 0 {
                out.push(b[start..i].iter().collect());
                start = i + 1;
            }
        }
        i += 1;
    }
    out.push(b[start..].iter().collect());
    out
}

/// Replace `\1`-`\9` backrefs in a vim regex using captured group texts.
/// Missing groups are replaced with the empty string, mirroring
/// `matchup#delim#fill_backrefs` + `s:get_backref` (delim.vim:939-955).
/// The returned string is still vim-regex syntax (literals are escaped
/// with `\V..\m` semantics approximated by backslash-escaping).
pub fn fill_backrefs_vim(re: &str, groups: &std::collections::HashMap<u32, String>) -> String {
    let cs: Vec<char> = re.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\' && not_bslash_char(&cs, i) {
            if i + 1 < cs.len() && cs[i + 1].is_ascii_digit() && cs[i + 1] != '0' {
                let n = cs[i + 1] as u32 - '0' as u32;
                match groups.get(&n) {
                    Some(text) => {
                        // Escape the characters that are special in vim-magic
                        // syntax so the captured text matches literally
                        // (equivalent of vim's `\V<escaped>\m`).
                        for c in text.chars() {
                            if "\\^$.*[]".contains(c) {
                                out.push('\\');
                            }
                            out.push(c);
                        }
                    }
                    None => {}
                }
                i += 2;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

fn not_bslash_char(cs: &[char], pos: usize) -> bool {
    let mut n = 0;
    let mut j = pos;
    while j > 0 && cs[j - 1] == '\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tt(re: &str) -> Translated {
        translate(re, &Opts::default()).unwrap()
    }

    fn t(re: &str) -> String {
        tt(re).pattern
    }

    #[test]
    fn basic_keywords() {
        assert_eq!(t(r"\<if\>"), r"(?<!\w)(?=\w)if(?<=\w)(?!\w)");
        assert_eq!(
            t(r"\<el\%[seif]\>"),
            r"(?<!\w)(?=\w)el(?:s(?:e(?:i(?:f)?)?)?)?(?<=\w)(?!\w)"
        );
    }

    #[test]
    fn groups_and_alts() {
        assert_eq!(t(r"\%(foo\|bar\)"), "(?:foo|bar)");
        assert_eq!(t(r"\(foo\)"), "(foo)");
        assert_eq!(t(r"\%(wh\%[ile]\|for\)"), "(?:wh(?:i(?:l(?:e)?)?)?|for)");
    }

    #[test]
    fn quantifiers() {
        assert_eq!(t(r"a\+"), "a+");
        assert_eq!(t(r"a\="), "a?");
        assert_eq!(t(r"a\*"), r"a\*");
        assert_eq!(t(r"a*"), "a*");
        assert_eq!(t(r"\s\+"), r"\s+");
        assert_eq!(t(r"x\{2,3}"), "x{2,3}");
        assert_eq!(t(r"x\{-}"), "x*?");
    }

    #[test]
    fn literals_escaped() {
        assert_eq!(t(r"("), r"\(");
        assert_eq!(t(r"+"), r"\+");
        assert_eq!(t(r"|"), r"\|");
        assert_eq!(t(r"{"), r"\{");
    }

    #[test]
    fn lookarounds() {
        // Variable-width lookbehind becomes a prefix-check obligation.
        let tr = tt(r"\%(\%(^\||\)\s*\)\@<=\<retu\%[rn]\>");
        assert_eq!(tr.pattern, r"(?<!\w)(?=\w)retu(?:r(?:n)?)?(?<=\w)(?!\w)");
        assert_eq!(
            tr.prefix_checks,
            vec![PrefixCheck {
                pattern: r"(?:(?:^|\|)\s*)".to_string(),
                neg: false
            }]
        );
        assert_eq!(t(r"\%(END\>\)\@!\S"), r"(?!(?:END(?<=\w)(?!\w)))\S");
        // Fixed-width lookbehind stays native.
        assert_eq!(t(r"\%(foo\)\@<=bar"), "(?<=(?:foo))bar");
    }

    #[test]
    fn backrefs() {
        assert_eq!(t(r"--\[\(=*\)\["), r"--\[(=*)\[");
        assert_eq!(t(r"\%(--\)\=]\1]"), r"(?:--)?\]\1\]");
    }

    #[test]
    fn zs_ze() {
        assert_eq!(t(r"\<if\>\ze\s"), r"(?<!\w)(?=\w)if(?<=\w)(?!\w)(?=\s)");
        assert_eq!(t(r"foo\zsbar"), "(?<=foo)bar");
        assert_eq!(t(r"a\zsB\zeC"), "(?<=a)B(?=C)");
        // Variable-width \zs prefix becomes an obligation.
        let tr = tt(r".*\zsfoo");
        assert_eq!(tr.pattern, "foo");
        assert_eq!(
            tr.prefix_checks,
            vec![PrefixCheck {
                pattern: ".*".to_string(),
                neg: false
            }]
        );
    }

    #[test]
    fn anchors() {
        assert_eq!(t(r"^\s*#\s*if\>"), r"^\s*\#\s*if(?<=\w)(?!\w)");
        assert_eq!(t(r"foo$"), "foo$");
        assert_eq!(t(r"a$b"), r"a\$b");
    }

    #[test]
    fn case_flags() {
        assert_eq!(t(r"\cfoo"), "(?i)foo");
        let o = Opts {
            ignorecase: true,
            ..Default::default()
        };
        assert_eq!(translate(r"foo", &o).unwrap().pattern, "(?i)foo");
        assert_eq!(translate(r"\Cfoo", &o).unwrap().pattern, "foo");
    }

    #[test]
    fn brackets() {
        assert_eq!(t(r"[[:punct:]]"), "[[:punct:]]");
        assert_eq!(t(r"[]]"), r"[\]]");
        assert_eq!(t(r"[a-z]"), "[a-z]");
        // vim does not recognize \s inside brackets: it is a literal 's'.
        assert_eq!(t(r"[^\s]"), "[^s]");
        assert_eq!(t(r"[\t]"), r"[\t]");
    }

    #[test]
    fn conjunction() {
        assert_eq!(t(r"foo\&f.."), "(?=foo)f..");
        // Trailing `\&` (vim's overlap hack) is dropped.
        assert_eq!(t(r"foo\&"), "foo");
    }

    #[test]
    fn capture_stripping() {
        let o = Opts {
            captures: false,
            ..Default::default()
        };
        assert_eq!(translate(r"\(foo\)\1", &o).unwrap().pattern, "(foo)\\1");
        assert_eq!(translate(r"\(foo\)", &o).unwrap().pattern, "(?:foo)");
    }

    #[test]
    fn capture_groups_extraction() {
        let cg = get_capture_groups(r"\(\(foo\)\(bar\)\)");
        assert_eq!(cg.len(), 3);
        assert_eq!(cg[0].0, 1);
        assert_eq!(cg[0].1.str, r"\(\(foo\)\(bar\)\)");
        assert_eq!(cg[0].1.depth, 1);
        assert_eq!(cg[0].1.parent, 0);
        assert_eq!(cg[1].1.str, r"\(foo\)");
        assert_eq!(cg[1].1.depth, 2);
        assert_eq!(cg[1].1.parent, 1);
        // \%() does not count
        let cg2 = get_capture_groups(r"\%(a\)\(b\)");
        assert_eq!(cg2.len(), 1);
        assert_eq!(cg2[0].1.str, r"\(b\)");
    }

    #[test]
    fn split_not_bslash_works() {
        assert_eq!(split_not_bslash(r"a:b\:c:d", ':'), vec!["a", r"b\:c", "d"]);
        assert_eq!(split_not_bslash(r"a\\:b", ':'), vec![r"a\\", "b"]);
    }

    #[test]
    fn optional_group_sequence_behavior() {
        // vim's \%[seif] matches any prefix of the atom sequence:
        // '', 's', 'se', 'sei', 'seif' - so both "else" and "elseif"
        // match \<el\%[seif]\>, while "elif" does not
        let re = fancy_regex::Regex::new(&t(r"\<el\%[seif]\>")).unwrap();
        let m = |s: &str| {
            re.find(s)
                .unwrap()
                .map(|m| (m.start(), m.as_str().to_string()))
        };
        assert_eq!(m("else"), Some((0, "else".to_string())));
        assert_eq!(m("elseif 2"), Some((0, "elseif".to_string())));
        assert_eq!(m("x = els"), Some((4, "els".to_string())));
        assert_eq!(m("elif"), None);
        assert_eq!(m("element"), None); // \> blocks the partial match
    }

    #[test]
    fn literal_prefix_works() {
        let lp = |re: &str| literal_prefix(re, false, 16).map(|(s, ic)| (s, ic));
        assert_eq!(lp(r"\<endif\>"), Some(("endif".to_string(), false)));
        assert_eq!(lp(r"\<el\%[seif]\>"), Some(("el".to_string(), false)));
        assert_eq!(
            lp(r"\%(\%(^\||\)\s*\)\@<=\<retu\%[rn]\>"),
            Some(("retu".to_string(), false))
        );
        // alternation: common prefix only
        assert_eq!(lp(r"\%(endif\|endfor\)"), Some(("end".to_string(), false)));
        assert_eq!(lp(r"\%(fu\%[nction]\|def\)"), None);
        // class start: no literal
        assert_eq!(lp(r"\S\+"), None);
        // case flag propagates
        assert_eq!(
            literal_prefix(r"\cfoo", false, 16),
            Some(("foo".to_string(), true))
        );
    }

    #[test]
    fn prefix_check_behavior() {
        // The engine's contract: main pattern matches at position p, and
        // each prefix check must match the text ending exactly at p.
        let tr = tt(r"\%(\%(^\||\)\s*\)\@<=\<retu\%[rn]\>");
        let main = fancy_regex::Regex::new(&tr.pattern).unwrap();
        let check =
            fancy_regex::Regex::new(&format!(r"(?:{})\z", tr.prefix_checks[0].pattern)).unwrap();

        // "  return x": match at col 2, prefix "  " satisfies ^\s*
        let line = "  return x";
        let m = main.find_from_pos(line, 0).unwrap().unwrap();
        assert_eq!(m.start(), 2);
        assert_eq!(m.as_str(), "return");
        assert!(check.is_match(&line[..m.start()]).unwrap());

        // "foo return": prefix "foo " does NOT satisfy (^\||)\s*
        let line = "foo return";
        let m = main.find_from_pos(line, 0).unwrap().unwrap();
        assert!(!check.is_match(&line[..m.start()]).unwrap());

        // "| return": pipe continuation prefix satisfies
        let line = "| return";
        let m = main.find_from_pos(line, 0).unwrap().unwrap();
        assert_eq!(m.as_str(), "return");
        assert!(check.is_match(&line[..m.start()]).unwrap());
    }

    #[test]
    fn fill_backrefs_works() {
        let mut groups = std::collections::HashMap::new();
        groups.insert(1, "==".to_string());
        groups.insert(2, "a.b".to_string());
        assert_eq!(
            fill_backrefs_vim(r"\%(--\)\=]\1]", &groups),
            r"\%(--\)\=]==]"
        );
        // Missing groups are dropped, special chars escaped.
        assert_eq!(fill_backrefs_vim(r"x\2y\3", &groups), r"xa\.by");
    }

    #[test]
    fn real_world_patterns_compile() {
        // Patterns taken from nvim runtime ftplugins and vim-matchup.
        let pats = [
            r"^\s*#\s*if\%(\|def\|ndef\)\>",
            r"^\s*#\s*elif\%(\|def\|ndef\)\>",
            r"\<\%(fu\%[nction]\|def\)!\=\s\+\S\+\s*(",
            r"\%(\%(^\||\)\s*\)\@<=\<retu\%[rn]\>",
            r"\%(\%(^\||\)\s*\)\@<=\<\%(endf\%[unction]\|enddef\)\>",
            r"\<\%(wh\%[ile]\|for\)\>",
            r"\<aug\%[roup]\s\+\%(END\>\)\@!\S",
            r"\<aug\%[roup]\ze\s\+\%(END\>\)\@!\S",
            r"--\[\(=*\)\[",
            r"\%(--\)\=]\(=*\)]",
            r"\<repeat\>",
            r"<!--",
            r"\<begin\>\|\<case\>\|\<do\>",
            r"\v^\s*\%(public\s+\|abstract\s+\|final\s+\)*%(class\|interface\|enum)\s",
            r"/\*",
            r"\*/",
            r"\<module\>\|\<\%(::\s*\)\@<=\S\+\s*\%(<\)\@=",
            r"\%(^\|\s\)\@<=\%(END\)\>",
            r"``",
            r"``\g{syn;!JanetString}",
            r"\\begin{\(\a\+\*\=\)}",
            r"\\end{\(\a\+\*\=\)}",
            r"\c<\zsq\=[^ \t()>]*\>",
            r"<\zs/\=[^ \t<>]>]\+\>",
        ];
        for p in pats {
            // Strip matchup's inline \g{...} flags the way the loader does.
            let cleaned = strip_gspec(p);
            let r = translate(&cleaned, &Opts::default());
            assert!(r.is_ok(), "failed to translate {p:?}: {:?}", r.err());
            let tr = r.unwrap();
            // The result must compile as a fancy-regex.
            assert!(
                fancy_regex::Regex::new(&tr.pattern).is_ok(),
                "fancy-regex rejected {:?} (from {p:?})",
                tr.pattern
            );
            for c in &tr.prefix_checks {
                assert!(
                    fancy_regex::Regex::new(&c.pattern).is_ok(),
                    "fancy-regex rejected prefix check {:?} (from {p:?})",
                    c.pattern
                );
            }
        }
    }

    fn strip_gspec(s: &str) -> String {
        let mut out = String::new();
        let cs: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            if cs[i] == '\\'
                && i + 1 < cs.len()
                && cs[i + 1] == 'g'
                && i + 2 < cs.len()
                && cs[i + 2] == '{'
            {
                let mut j = i + 3;
                while j < cs.len() && cs[j] != '}' {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            out.push(cs[i]);
            i += 1;
        }
        out
    }
}
