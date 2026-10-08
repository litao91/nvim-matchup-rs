//! Per-buffer compiled state: delimiter sets, union scan regexes,
//! skip evaluator, caches, options snapshot and the timeout budget
//! (port of matchup#loader + matchup#perf).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Instant;

use fancy_regex::Regex;
use nvim_oxi::api::{self, Buffer};

use crate::skip::{compile_skip, SkipKind};
use crate::types::Delim;
use crate::vimregex::{translate, Opts};
use crate::words::*;

// ---------------------------------------------------------------------------
// Compiled patterns
// ---------------------------------------------------------------------------

pub type SharedRegex = Rc<Regex>;

/// A compiled delimiter word (one of open/mid/close of a set).
pub struct CompiledWord {
    /// The vim-regex source (regextwo word).
    pub vim: String,
    /// Translated main pattern (no captures), for union scans.
    pub main: String,
    /// Compiled main pattern; None when translation/compilation failed
    /// (the word is then disabled).
    pub scan: Option<SharedRegex>,
    /// Compiled pattern with capture groups, for classification.
    pub classify: Option<SharedRegex>,
    /// Prefix-check obligations: (compiled `(?:X)\z`, negated).
    pub checks: Vec<(SharedRegex, bool)>,
    /// Classification variant with hlend processing applied
    /// (only when the word has the hlend flag).
    pub hlend_classify: Option<SharedRegex>,
    pub hlend_checks: Vec<(SharedRegex, bool)>,
    pub has_hlend: bool,
}

pub struct CompiledSet {
    pub open: CompiledWord,
    pub mids: Vec<CompiledWord>,
    pub close: CompiledWord,
}

impl CompiledSet {
    pub fn word(&self, side: Side, mid_id: usize) -> Option<&CompiledWord> {
        match side {
            Side::Open => Some(&self.open),
            Side::Close => Some(&self.close),
            Side::Mid => self.mids.get(mid_id - 1),
        }
    }
}

pub struct Union {
    pub re: Option<SharedRegex>,
    /// (set index, side, mid_id) alternatives in union order, for
    /// diagnostics; classification re-tries patterns anyway.
    pub empty: bool,
}

pub struct BufCompiled {
    pub lists: DelimLists,
    pub sets: Vec<CompiledSet>,
    pub unions: HashMap<SideQuery, Union>,
    /// Word character class derived from &iskeyword.
    pub word: String,
    pub ignorecase: bool,
    pub skip: SkipKind,
    pub src_hash: u64,
}

// ---------------------------------------------------------------------------
// Options / perf
// ---------------------------------------------------------------------------

/// Snapshot of the g:matchup_* options used by the engine.
#[derive(Clone, Debug)]
pub struct GOpts {
    pub delim_noskips: i64,
    pub delim_nomids: bool,
    pub delim_stopline: usize,
    pub delim_count_fail: bool,
    pub delim_count_max: usize,
    pub matchparen_enabled: bool,
    pub matchparen_stopline: usize,
    pub matchparen_timeout: f64,
    pub matchparen_insert_timeout: f64,
    pub matchparen_singleton: bool,
    pub matchparen_offscreen_method: String,
    pub matchparen_start_sign: String,
    pub matchparen_end_sign: String,
    pub matchparen_pumvisible: i64,
    pub matchparen_nomode: String,
    pub matchparen_deferred: bool,
    pub matchparen_deferred_show_delay: i64,
    pub matchparen_deferred_hide_delay: i64,
    pub matchparen_deferred_fade_time: i64,
    pub matchparen_hi_background: bool,
    pub motion_cursor_end: bool,
    pub motion_override_npercent: i64,
    pub motion_keepjumps: bool,
    pub text_obj_linewise_operators: Vec<String>,
}

fn gvar_i64(name: &str, default: i64) -> i64 {
    api::get_var::<i64>(name).unwrap_or(default)
}

fn gvar_f64(name: &str, default: f64) -> f64 {
    if let Ok(v) = api::get_var::<f64>(name) {
        v
    } else {
        gvar_i64(name, default as i64) as f64
    }
}

fn gvar_string(name: &str, default: &str) -> String {
    api::get_var::<String>(name).unwrap_or_else(|_| default.to_string())
}

impl GOpts {
    pub fn read() -> GOpts {
        let linewise = api::get_var::<Vec<String>>("matchup_text_obj_linewise_operators")
            .unwrap_or_else(|_| vec!["d".to_string(), "y".to_string()]);
        GOpts {
            delim_noskips: gvar_i64("matchup_delim_noskips", 0),
            delim_nomids: gvar_i64("matchup_delim_nomids", 0) != 0,
            delim_stopline: gvar_i64("matchup_delim_stopline", 1500).max(0) as usize,
            delim_count_fail: gvar_i64("matchup_delim_count_fail", 0) != 0,
            delim_count_max: gvar_i64("matchup_delim_count_max", 8).max(0) as usize,
            matchparen_enabled: gvar_i64("matchup_matchparen_enabled", 1) != 0,
            matchparen_stopline: gvar_i64("matchup_matchparen_stopline", 400).max(0) as usize,
            matchparen_timeout: gvar_f64("matchup_matchparen_timeout", 300.0),
            matchparen_insert_timeout: gvar_f64("matchup_matchparen_insert_timeout", 60.0),
            matchparen_singleton: gvar_i64("matchup_matchparen_singleton", 0) != 0,
            matchparen_offscreen_method: {
                // g:matchup_matchparen_offscreen is a dict; method key
                use nvim_oxi::conversion::FromObject;
                api::get_var::<nvim_oxi::Dictionary>("matchup_matchparen_offscreen")
                    .ok()
                    .and_then(|d| {
                        d.get("method")
                            .and_then(|v| String::from_object(v.clone()).ok())
                    })
                    .unwrap_or_else(|| "status".to_string())
            },
            matchparen_start_sign: gvar_string("matchup_matchparen_start_sign", "\u{25B6}"),
            matchparen_end_sign: gvar_string("matchup_matchparen_end_sign", "\u{25C0}"),
            matchparen_pumvisible: gvar_i64("matchup_matchparen_pumvisible", 1),
            matchparen_nomode: gvar_string("matchup_matchparen_nomode", ""),
            matchparen_deferred: gvar_i64("matchup_matchparen_deferred", 0) != 0,
            matchparen_deferred_show_delay: gvar_i64("matchup_matchparen_deferred_show_delay", 50),
            matchparen_deferred_hide_delay: gvar_i64("matchup_matchparen_deferred_hide_delay", 700),
            matchparen_deferred_fade_time: gvar_i64("matchup_matchparen_deferred_fade_time", 0),
            matchparen_hi_background: gvar_i64("matchup_matchparen_hi_background", 0) != 0,
            motion_cursor_end: gvar_i64("matchup_motion_cursor_end", 1) != 0,
            motion_override_npercent: gvar_i64("matchup_motion_override_Npercent", 6),
            motion_keepjumps: gvar_i64("matchup_motion_keepjumps", 0) != 0,
            text_obj_linewise_operators: linewise,
        }
    }
}

/// Port of matchup#perf: EMA timing and the cooperative timeout budget.
pub struct Perf {
    starts: RefCell<HashMap<String, Instant>>,
    pub times: RefCell<HashMap<String, TimeEntry>>,
    timeout: Cell<f64>,
    timeout_enabled: Cell<bool>,
    timeout_pulse: Cell<Instant>,
}

#[derive(Clone, Debug)]
pub struct TimeEntry {
    pub maximum: f64,
    pub emavg: f64,
    pub last: f64,
}

const ALPHA: f64 = 2.0 / (10.0 + 1.0);

impl Perf {
    pub fn new() -> Perf {
        Perf {
            starts: RefCell::new(HashMap::new()),
            times: RefCell::new(HashMap::new()),
            timeout: Cell::new(0.0),
            timeout_enabled: Cell::new(false),
            timeout_pulse: Cell::new(Instant::now()),
        }
    }

    pub fn tic(&self, context: &str) {
        self.starts
            .borrow_mut()
            .insert(context.to_string(), Instant::now());
    }

    pub fn toc(&self, context: &str, state: &str) {
        let start = match self.starts.borrow().get(context) {
            Some(t) => *t,
            None => return,
        };
        let elapsed = start.elapsed().as_secs_f64();
        let key = format!("{context}#{state}");
        let mut times = self.times.borrow_mut();
        match times.get_mut(&key) {
            Some(e) => {
                if elapsed > e.maximum {
                    e.maximum = elapsed;
                }
                e.last = elapsed;
                e.emavg = ALPHA * elapsed + (1.0 - ALPHA) * e.emavg;
            }
            None => {
                times.insert(
                    key,
                    TimeEntry {
                        maximum: elapsed,
                        emavg: elapsed,
                        last: elapsed,
                    },
                );
            }
        }
    }

    pub fn timeout_start(&self, ms: f64) {
        self.timeout.set(ms);
        self.timeout_enabled.set(ms != 0.0);
        self.timeout_pulse.set(Instant::now());
    }

    /// Returns true when the budget is exhausted (perf.vim:91).
    pub fn timeout_check(&self) -> bool {
        if !self.timeout_enabled.get() {
            return false;
        }
        let pulse = self.timeout_pulse.get();
        let elapsed = 1000.0 * pulse.elapsed().as_secs_f64();
        self.timeout_pulse.set(Instant::now());
        let t = self.timeout.get() - elapsed;
        self.timeout.set(t);
        t <= 0.0
    }

    pub fn clear_times(&self) {
        self.times.borrow_mut().clear();
    }
}

impl Default for Perf {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

pub struct State {
    /// True while re-feeding keys for an operator-pending motion
    /// (prevents recursion in the op() dance).
    pub in_op: Cell<bool>,
    /// Stashed v:operator during operator-pending motions.
    pub op_operator: RefCell<String>,
    pub bufs: RefCell<HashMap<i32, BufCompiled>>,
    pub regex_cache: RefCell<HashMap<String, SharedRegex>>,
    /// Cache for expression-valued b:match_words (loader.vim:123).
    pub expr_cache: RefCell<HashMap<String, String>>,
    /// get_surrounding memo: buf -> (changedtick, memo map).
    pub surround_memo: RefCell<HashMap<i32, (u32, HashMap<MemoKey, Option<Delim>>)>>,
    pub matchparen: RefCell<crate::matchparen::MatchParenState>,
    pub perf: Perf,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct MemoKey {
    pub lnum: usize,
    pub cnum: usize,
    pub mode: char,
}

impl State {
    pub fn new() -> State {
        State {
            in_op: Cell::new(false),
            op_operator: RefCell::new(String::new()),
            bufs: RefCell::new(HashMap::new()),
            regex_cache: RefCell::new(HashMap::new()),
            expr_cache: RefCell::new(HashMap::new()),
            surround_memo: RefCell::new(HashMap::new()),
            matchparen: RefCell::new(Default::default()),
            perf: Perf::new(),
        }
    }

    pub fn reload(&self) {
        self.bufs.borrow_mut().clear();
        self.expr_cache.borrow_mut().clear();
        self.surround_memo.borrow_mut().clear();
    }

    /// Translate + compile with the shared cache.
    pub fn compile(&self, pattern: &str, opts: &Opts) -> Option<SharedRegex> {
        // Translate (cheap, deterministic) then look up the compiled regex
        // by its translated pattern string.
        let t = translate(pattern, opts).ok()?;
        if !t.prefix_checks.is_empty() {
            // Callers that cannot honor prefix checks must not use this
            // path; they use compile_word/compile_checked instead.
            return None;
        }
        let mut cache = self.regex_cache.borrow_mut();
        if let Some(re) = cache.get(&t.pattern) {
            return Some(Rc::clone(re));
        }
        let re = Rc::new(Regex::new(&t.pattern).ok()?);
        cache.insert(t.pattern.clone(), Rc::clone(&re));
        Some(re)
    }

    /// Compile a pattern that may carry prefix-check obligations.
    /// Returns (compiled main, checks, main pattern string).
    pub fn compile_checked(
        &self,
        pattern: &str,
        opts: &Opts,
    ) -> Option<(SharedRegex, Vec<(SharedRegex, bool)>, String)> {
        let t = translate(pattern, opts).ok()?;
        let main = {
            let mut cache = self.regex_cache.borrow_mut();
            match cache.get(&t.pattern) {
                Some(re) => Rc::clone(re),
                None => {
                    let re = Rc::new(Regex::new(&t.pattern).ok()?);
                    cache.insert(t.pattern.clone(), Rc::clone(&re));
                    re
                }
            }
        };
        let mut checks = Vec::new();
        for c in &t.prefix_checks {
            let anchored = format!("(?:{})\\z", c.pattern);
            let re = {
                let mut cache = self.regex_cache.borrow_mut();
                match cache.get(&anchored) {
                    Some(re) => Rc::clone(re),
                    None => {
                        let re = Rc::new(Regex::new(&anchored).ok()?);
                        cache.insert(anchored.clone(), Rc::clone(&re));
                        re
                    }
                }
            };
            checks.push((re, c.neg));
        }
        Some((main, checks, t.pattern))
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Buffer compilation
// ---------------------------------------------------------------------------

fn buf_var_string(buf: &Buffer, name: &str) -> String {
    buf.get_var::<String>(name).unwrap_or_default()
}

fn buf_option(buf: &Buffer, name: &str) -> String {
    // NOTE: nvim_call_function's C signature changed in nvim 0.13-dev
    // (leading channel_id), which nvim-oxi 0.6 does not know about;
    // use eval (verified working) with integer interpolation only.
    api::eval::<String>(&format!("getbufvar({}, '&{}')", buf.handle(), name))
        .unwrap_or_default()
}

/// Build the word character class from &iskeyword.
pub fn word_class(isk: &str) -> String {
    if isk.is_empty() {
        return r"\w".to_string();
    }
    let mut include: Vec<(u32, u32)> = Vec::new();
    let mut exclude: Vec<(u32, u32)> = Vec::new();
    for item in isk.split(',') {
        if item.is_empty() {
            continue;
        }
        let (neg, item) = match item.strip_prefix('^') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, item),
        };
        let mut add = |a: u32, b: u32| {
            if neg {
                exclude.push((a, b));
            } else {
                include.push((a, b));
            }
        };
        if item == "@" {
            add('A' as u32, 'Z' as u32);
            add('a' as u32, 'z' as u32);
            continue;
        }
        if let Some((a, b)) = item.split_once('-') {
            let pa = parse_code(a);
            let pb = parse_code(b);
            if let (Some(pa), Some(pb)) = (pa, pb) {
                if pa <= pb {
                    add(pa, pb);
                    continue;
                }
            }
        }
        if let Some(c) = parse_code(item) {
            add(c, c);
        }
    }
    if include.is_empty() {
        return r"\w".to_string();
    }
    // subtract exclusions
    let mut ranges = merge_ranges(include);
    for (a, b) in merge_ranges(exclude) {
        let mut out: Vec<(u32, u32)> = Vec::new();
        for (ra, rb) in ranges {
            if rb < a || ra > b {
                out.push((ra, rb));
                continue;
            }
            if ra < a {
                out.push((ra, a - 1));
            }
            if rb > b {
                out.push((b + 1, rb));
            }
        }
        ranges = out;
    }
    if ranges.is_empty() {
        return r"\w".to_string();
    }
    let mut out = String::from("[");
    for (a, b) in ranges {
        if a == b {
            push_class_char(&mut out, a);
        } else {
            push_class_char(&mut out, a);
            out.push('-');
            push_class_char(&mut out, b);
        }
    }
    out.push(']');
    out
}

fn parse_code(s: &str) -> Option<u32> {
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c as u32),
        _ => None,
    }
}

fn merge_ranges(mut v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    if v.is_empty() {
        return v;
    }
    v.sort();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (a, b) in v {
        match out.last_mut() {
            Some((_, lb)) if a <= *lb + 1 => {
                if b > *lb {
                    *lb = b;
                }
            }
            _ => out.push((a, b)),
        }
    }
    out
}

fn push_class_char(out: &mut String, c: u32) {
    match c {
        0..=0x7F => {
            let ch = c as u8 as char;
            match ch {
                ']' | '\\' | '^' | '-' | '[' => {
                    out.push('\\');
                    out.push(ch);
                }
                ' ' => out.push_str(r"\ "),
                _ => out.push(ch),
            }
        }
        _ => out.push_str(&format!("\\u{{{c:X}}}")),
    }
}

fn read_midmap(buf: &Buffer) -> Option<Vec<(String, String)>> {
    use nvim_oxi::conversion::FromObject;
    let arr = buf.get_var::<nvim_oxi::Array>("match_midmap").ok()?;
    let mut out = Vec::new();
    for item in arr {
        if let Ok(pair) = nvim_oxi::Array::from_object(item.clone()) {
            let mut it = pair.into_iter();
            if let (Some(a), Some(b)) = (it.next(), it.next()) {
                if let (Ok(s), Ok(w)) = (String::from_object(a), String::from_object(b)) {
                    out.push((s, w));
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn hash_inputs(parts: &[&str]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
        0u8.hash(&mut h);
    }
    h.finish()
}

/// Ensure the buffer's compiled state is current; returns the buffer
/// handle. Port of matchup#loader#init_buffer/refresh_match_words.
pub fn ensure_buf(state: &State, buf: &Buffer) -> i32 {
    let h = buf.handle();

    let match_words_raw = buf_var_string(buf, "match_words");
    let match_words = if !match_words_raw.is_empty() && !match_words_raw.contains(':') {
        // expression-valued: evaluate and use the global cache.
        // SECURITY NOTE: this mirrors vim-matchup's own behavior
        // (`execute 'let l:match_words =' b:match_words`, loader.vim:97).
        // b:match_words is buffer-local config set by ftplugins; anyone
        // able to set it can already execute arbitrary vimscript.
        if let Some(cached) = state.expr_cache.borrow().get(&match_words_raw) {
            cached.clone()
        } else {
            let v: String = api::eval(&match_words_raw).unwrap_or_default();
            state
                .expr_cache
                .borrow_mut()
                .insert(match_words_raw.clone(), v.clone());
            v
        }
    } else {
        match_words_raw
    };

    let matchpairs = buf_option(buf, "matchpairs");
    let iskeyword = buf_option(buf, "iskeyword");
    let nomps = buf
        .get_var::<i64>("matchup_delim_nomatchpairs")
        .unwrap_or(0)
        != 0;
    let ignorecase = buf.get_var::<i64>("match_ignorecase").unwrap_or(0) != 0;
    let match_skip = buf_var_string(buf, "match_skip");
    let midmap = read_midmap(buf);

    let midmap_key: String = midmap
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(a, b)| format!("{a}\u{1}{b}"))
                .collect::<Vec<_>>()
                .join("\u{2}")
        })
        .unwrap_or_default();
    let hash = hash_inputs(&[
        &match_words,
        &matchpairs,
        &iskeyword,
        &match_skip,
        &midmap_key,
        if nomps { "1" } else { "0" },
        if ignorecase { "1" } else { "0" },
    ]);

    {
        let bufs = state.bufs.borrow();
        if let Some(bc) = bufs.get(&h) {
            if bc.src_hash == hash {
                return h;
            }
        }
    }

    let word = word_class(&iskeyword);
    let lists = init_delim_lists(&ParseInput {
        match_words: &match_words,
        matchpairs: &matchpairs,
        nomatchpairs: nomps,
        midmap: midmap.clone(),
    });

    let opts_scan = Opts {
        word: word.clone(),
        ignorecase,
        captures: false,
    };
    let opts_class = Opts {
        word: word.clone(),
        ignorecase,
        captures: true,
    };

    let mut sets: Vec<CompiledSet> = Vec::with_capacity(lists.sets.len());
    for set in &lists.sets {
        let n_extra = set.regextwo.extra_list.len();
        let mk = |vim: &String, extra_idx: usize| -> CompiledWord {
            compile_word(state, vim, &opts_scan, &opts_class, extra_idx, &set.regextwo, n_extra)
        };
        let open = mk(&set.regextwo.open, 0);
        let mids: Vec<CompiledWord> = set
            .regextwo
            .mid_list
            .iter()
            .enumerate()
            .map(|(k, w)| mk(w, k + 1))
            .collect();
        let close = mk(&set.regextwo.close, n_extra.saturating_sub(1));
        sets.push(CompiledSet { open, mids, close });
    }

    // union scan regexes per side query
    let mut unions = HashMap::new();
    for q in SideQuery::ALL {
        let mut parts: Vec<&str> = Vec::new();
        for (si, set) in lists.sets.iter().enumerate() {
            for side in q.sides() {
                match side {
                    Side::Open => {
                        if sets[si].open.scan.is_some() {
                            parts.push(&sets[si].open.main);
                        }
                    }
                    Side::Mid => {
                        for m in &sets[si].mids {
                            if m.scan.is_some() {
                                parts.push(&m.main);
                            }
                        }
                    }
                    Side::Close => {
                        if sets[si].close.scan.is_some() {
                            parts.push(&sets[si].close.main);
                        }
                    }
                }
            }
            let _ = set;
        }
        let union = if parts.is_empty() {
            Union {
                re: None,
                empty: true,
            }
        } else {
            let combined = parts
                .iter()
                .map(|p| format!("(?:{p})"))
                .collect::<Vec<_>>()
                .join("|");
            let re = {
                let mut cache = state.regex_cache.borrow_mut();
                match cache.get(&combined) {
                    Some(re) => Some(Rc::clone(re)),
                    None => Regex::new(&combined).ok().map(|re| {
                        let re = Rc::new(re);
                        cache.insert(combined.clone(), Rc::clone(&re));
                        re
                    }),
                }
            };
            let empty = re.is_none();
            Union { re, empty }
        };
        unions.insert(q, union);
    }

    let skip = compile_skip(&match_skip, &word);

    state.bufs.borrow_mut().insert(
        h,
        BufCompiled {
            lists,
            sets,
            unions,
            word,
            ignorecase,
            skip,
            src_hash: hash,
        },
    );
    h
}

fn compile_word(
    state: &State,
    vim: &str,
    opts_scan: &Opts,
    opts_class: &Opts,
    extra_idx: usize,
    two: &RegexTwo,
    _n_extra: usize,
) -> CompiledWord {
    if vim.is_empty() {
        return CompiledWord {
            vim: String::new(),
            main: String::new(),
            scan: None,
            classify: None,
            checks: vec![],
            hlend_classify: None,
            hlend_checks: vec![],
            has_hlend: false,
        };
    }
    let scan_t = translate(vim, opts_scan).ok();
    let main = scan_t.as_ref().map(|t| t.pattern.clone()).unwrap_or_default();
    let scan = scan_t.as_ref().and_then(|t| {
        let mut cache = state.regex_cache.borrow_mut();
        if let Some(re) = cache.get(&t.pattern) {
            return Some(Rc::clone(re));
        }
        let re = Rc::new(Regex::new(&t.pattern).ok()?);
        cache.insert(t.pattern.clone(), Rc::clone(&re));
        Some(re)
    });
    let class_c = state.compile_checked(vim, opts_class);
    let (classify, checks) = match class_c {
        Some((re, checks, _)) => (Some(re), checks),
        None => (None, vec![]),
    };

    let has_hlend = two
        .extra_list
        .get(extra_idx)
        .map(|e| e.contains_key("hlend"))
        .unwrap_or(false);
    let (hlend_classify, hlend_checks) = if has_hlend {
        let p = process_hlend(vim, -1);
        match state.compile_checked(&p, opts_class) {
            Some((re, checks, _)) => (Some(re), checks),
            None => (None, vec![]),
        }
    } else {
        (None, vec![])
    };

    CompiledWord {
        vim: vim.to_string(),
        main,
        scan,
        classify,
        checks,
        hlend_classify,
        hlend_checks,
        has_hlend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_class_default() {
        let c = word_class("@,48-57,_,192-255");
        assert_eq!(c, r"[0-9A-Z_a-z\u{C0}-\u{FF}]");
    }

    #[test]
    fn word_class_vim_ft() {
        let c = word_class("@,48-57,_,192-255,#,:");
        assert!(c.contains('#'));
        assert!(c.contains(':'));
    }

    #[test]
    fn word_class_exclusion() {
        // keyword chars except digits
        let c = word_class("@,^48-57");
        assert_eq!(c, "[A-Za-z]");
    }

    #[test]
    fn word_class_compiles_in_fancy() {
        for isk in [
            "@,48-57,_,192-255",
            "@,48-57,_,192-255,#,:",
            "33,35-39,42-43,45-48,60-62,64-90,94,95,97-122,124,126",
            "",
        ] {
            let c = word_class(isk);
            let pat = format!("(?<!{c})(?={c})foo(?<={c})(?!{c})");
            assert!(
                Regex::new(&pat).is_ok(),
                "class {c:?} from {isk:?} failed to compile"
            );
        }
    }
}
