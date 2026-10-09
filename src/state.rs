//! Per-buffer compiled state: delimiter sets, union scan regexes,
//! skip evaluator, caches, options snapshot and the timeout budget
//! (port of matchup#loader + matchup#perf).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Instant;

use fancy_regex::Regex;
use nvim_oxi::api::Buffer;
use nvim_oxi::conversion::FromObject;
use nvim_oxi::{Array, Dictionary, Object};

use crate::skip::{compile_skip, SkipKind};
use crate::types::Delim;
use crate::vimregex::{self, translate, Opts};
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
    /// Bytes a match of this word can start with; None = undetermined
    /// (the word is a classify candidate at every position).
    pub first: Option<vimregex::FirstBytes>,
    /// Lookaround-free over-approximation for the DFA scan union
    /// (`regex` crate); None when the word cannot be scan-translated.
    pub fast_main: Option<String>,
    /// Mandatory literal prefix of matches (string, case-insensitive):
    /// cheap filter before the anchored classify regex.
    pub lit_prefix: Option<(String, bool)>,
    /// Literals that must occur in the line prefix before a match for the
    /// positive prefix-check obligations to hold; a cheap pre-filter that
    /// skips the anchored capture regex entirely.
    pub req_lits: Vec<String>,
    pub hlend_req_lits: Vec<String>,
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
    /// Full fancy-regex union (exact extents): used for Current scans.
    pub re: Option<SharedRegex>,
    /// Lookaround-free DFA union (over-approximate positions; classify
    /// filters exactly): used for Next/Prev multi-line scans.
    pub fast: Option<regex::Regex>,
    /// Fancy union of the words that could not be scan-translated.
    pub exotic: Option<SharedRegex>,
    /// (set index, side, mid_id) alternatives in union order, for
    /// diagnostics; classification re-tries patterns anyway.
    pub empty: bool,
}

/// A (set, side, word) slot, for classify candidate dispatch.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WordRef {
    pub set: usize,
    pub side: Side,
    pub mid_id: usize,
}

/// Classify candidates indexed by the first byte at the hit position.
pub struct Dispatch {
    pub by_byte: Vec<Vec<WordRef>>,
    /// All words in classify order, for positions with no byte (EOL).
    pub all: Vec<WordRef>,
}

pub struct BufCompiled {
    pub lists: DelimLists,
    pub sets: Vec<CompiledSet>,
    pub unions: HashMap<SideQuery, Union>,
    pub dispatch: HashMap<SideQuery, Dispatch>,
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
    pub ts_enabled: bool,
    pub ts_disabled: Vec<String>,
    pub ts_stopline: usize,
    pub ts_enable_quotes: bool,
    pub ts_include_match_words: bool,
    pub ts_disable_virtual_text: bool,
    // activation gates (previously g:matchup_{mappings,motion,text_obj}_enabled)
    pub mappings_enabled: bool,
    pub motion_enabled: bool,
    pub text_obj_enabled: bool,
    pub matchparen_offscreen_scrolloff: i64,
    /// `g:matchup_matchpref` equivalent: filetype -> pref id -> bool.
    pub matchpref: HashMap<String, HashMap<String, bool>>,
}

impl Default for GOpts {
    fn default() -> GOpts {
        GOpts {
            delim_noskips: 0,
            delim_nomids: false,
            delim_stopline: 1500,
            delim_count_fail: false,
            delim_count_max: 8,
            matchparen_enabled: true,
            matchparen_stopline: 400,
            matchparen_timeout: 300.0,
            matchparen_insert_timeout: 60.0,
            matchparen_singleton: false,
            matchparen_offscreen_method: "status".to_string(),
            matchparen_offscreen_scrolloff: 0,
            matchparen_start_sign: "\u{25B6}".to_string(),
            matchparen_end_sign: "\u{25C0}".to_string(),
            matchparen_pumvisible: 1,
            matchparen_nomode: String::new(),
            matchparen_deferred: false,
            matchparen_deferred_show_delay: 50,
            matchparen_deferred_hide_delay: 700,
            matchparen_deferred_fade_time: 0,
            matchparen_hi_background: false,
            motion_cursor_end: true,
            motion_override_npercent: 6,
            motion_keepjumps: false,
            text_obj_linewise_operators: vec!["d".to_string(), "y".to_string()],
            ts_enabled: false,
            ts_disabled: Vec::new(),
            ts_stopline: 400,
            ts_enable_quotes: true,
            ts_include_match_words: false,
            ts_disable_virtual_text: false,
            mappings_enabled: true,
            motion_enabled: true,
            text_obj_enabled: true,
            matchpref: HashMap::new(),
        }
    }
}

// --- setup(opts) parsing helpers -------------------------------------------
//
// Configuration is supplied only through `require('matchup_rs').setup{...}`
// (a nested Lua table). Each field is optional and layered over the defaults
// above; there is deliberately no `g:matchup_*` fallback.

fn sub(d: &Dictionary, key: &str) -> Option<Dictionary> {
    d.get(key)
        .and_then(|v| Dictionary::from_object(v.clone()).ok())
}

fn obj_bool(o: &Object) -> Option<bool> {
    if let Ok(i) = i64::try_from(o.clone()) {
        return Some(i != 0);
    }
    nvim_oxi::Boolean::from_object(o.clone()).ok()
}

fn obj_i64(o: &Object) -> Option<i64> {
    i64::try_from(o.clone()).ok()
}

fn obj_f64(o: &Object) -> Option<f64> {
    if let Ok(f) = f64::from_object(o.clone()) {
        return Some(f);
    }
    i64::try_from(o.clone()).ok().map(|i| i as f64)
}

fn obj_str(o: &Object) -> Option<String> {
    String::from_object(o.clone()).ok()
}

fn obj_strlist(o: &Object) -> Option<Vec<String>> {
    Vec::<String>::from_object(o.clone()).ok()
}

/// True when nvim is new enough for the treesitter default (upstream gates on
/// 0.11.2). Evaluated once per setup() call.
fn ts_default_enabled() -> bool {
    crate::nvimrs::call_fn_as::<i64>(
        "has",
        &Array::from_iter([Object::from("nvim-0.11.2")]),
    )
    .unwrap_or(0)
        != 0
}

impl GOpts {
    /// Layer the `setup(opts)` table over `base` (normally `GOpts::default()`).
    pub fn from_opts(base: &GOpts, opts: Option<&Dictionary>) -> GOpts {
        let mut g = base.clone();
        let opts = match opts {
            Some(o) => o,
            None => return g,
        };

        if let Some(v) = opts.get("mappings").and_then(obj_bool) {
            g.mappings_enabled = v;
        }

        if let Some(mp) = sub(opts, "matchpref") {
            for (ft, v) in mp.iter() {
                if let Ok(inner) = Dictionary::from_object(v.clone()) {
                    let m = g.matchpref.entry(ft.to_string()).or_default();
                    for (id, iv) in inner.iter() {
                        if let Some(b) = obj_bool(iv) {
                            m.insert(id.to_string(), b);
                        }
                    }
                }
            }
        }

        if let Some(d) = sub(opts, "delim") {
            if let Some(v) = d.get("noskips").and_then(obj_i64) {
                g.delim_noskips = v;
            }
            if let Some(v) = d.get("nomids").and_then(obj_bool) {
                g.delim_nomids = v;
            }
            if let Some(v) = d.get("stopline").and_then(obj_i64) {
                g.delim_stopline = v.max(0) as usize;
            }
            if let Some(v) = d.get("count_fail").and_then(obj_bool) {
                g.delim_count_fail = v;
            }
            if let Some(v) = d.get("count_max").and_then(obj_i64) {
                g.delim_count_max = v.max(0) as usize;
            }
        }

        if let Some(d) = sub(opts, "matchparen") {
            if let Some(v) = d.get("enable").and_then(obj_bool) {
                g.matchparen_enabled = v;
            }
            if let Some(v) = d.get("stopline").and_then(obj_i64) {
                g.matchparen_stopline = v.max(0) as usize;
            }
            if let Some(v) = d.get("timeout").and_then(obj_f64) {
                g.matchparen_timeout = v;
            }
            if let Some(v) = d.get("insert_timeout").and_then(obj_f64) {
                g.matchparen_insert_timeout = v;
            }
            if let Some(v) = d.get("singleton").and_then(obj_bool) {
                g.matchparen_singleton = v;
            }
            if let Some(v) = d.get("pumvisible").and_then(obj_bool) {
                g.matchparen_pumvisible = v as i64;
            }
            if let Some(v) = d.get("nomode").and_then(obj_str) {
                g.matchparen_nomode = v;
            }
            if let Some(v) = d.get("hi_background").and_then(obj_bool) {
                g.matchparen_hi_background = v;
            }
            if let Some(v) = d.get("start_sign").and_then(obj_str) {
                g.matchparen_start_sign = v;
            }
            if let Some(v) = d.get("end_sign").and_then(obj_str) {
                g.matchparen_end_sign = v;
            }
            if let Some(v) = d.get("deferred").and_then(obj_bool) {
                g.matchparen_deferred = v;
            }
            if let Some(v) = d.get("deferred_show_delay").and_then(obj_i64) {
                g.matchparen_deferred_show_delay = v;
            }
            if let Some(v) = d.get("deferred_hide_delay").and_then(obj_i64) {
                g.matchparen_deferred_hide_delay = v;
            }
            if let Some(v) = d.get("deferred_fade_time").and_then(obj_i64) {
                g.matchparen_deferred_fade_time = v;
            }
            // offscreen: `false` disables; a table sets method/scrolloff.
            if let Some(ov) = d.get("offscreen") {
                if obj_bool(ov) == Some(false) {
                    g.matchparen_offscreen_method = String::new();
                    g.matchparen_offscreen_scrolloff = 0;
                } else if let Ok(od) = Dictionary::from_object(ov.clone()) {
                    // A provided table replaces the default entirely; an
                    // absent `method` disables offscreen (as in vim-matchup).
                    g.matchparen_offscreen_method =
                        od.get("method").and_then(obj_str).unwrap_or_default();
                    if let Some(s) = od.get("scrolloff").and_then(obj_i64) {
                        g.matchparen_offscreen_scrolloff = s;
                    }
                }
            }
        }

        if let Some(d) = sub(opts, "motion") {
            if let Some(v) = d.get("enable").and_then(obj_bool) {
                g.motion_enabled = v;
            }
            if let Some(v) = d.get("cursor_end").and_then(obj_bool) {
                g.motion_cursor_end = v;
            }
            if let Some(v) = d.get("override_Npercent").and_then(obj_i64) {
                g.motion_override_npercent = v;
            }
            if let Some(v) = d.get("keepjumps").and_then(obj_bool) {
                g.motion_keepjumps = v;
            }
        }

        if let Some(d) = sub(opts, "text_obj") {
            if let Some(v) = d.get("enable").and_then(obj_bool) {
                g.text_obj_enabled = v;
            }
            if let Some(v) = d.get("linewise_operators").and_then(obj_strlist) {
                g.text_obj_linewise_operators = v;
            }
        }

        // treesitter: default `enable` follows has('nvim-0.11.2') even when the
        // group is omitted, so an explicit setup{} matches upstream behavior.
        let ts_dflt = ts_default_enabled();
        if let Some(d) = sub(opts, "treesitter") {
            g.ts_enabled = d.get("enable").and_then(obj_bool).unwrap_or(ts_dflt);
            if let Some(v) = d.get("disabled").and_then(obj_strlist) {
                g.ts_disabled = v;
            }
            if let Some(v) = d.get("stopline").and_then(obj_i64) {
                g.ts_stopline = v.max(0) as usize;
            }
            if let Some(v) = d.get("enable_quotes").and_then(obj_bool) {
                g.ts_enable_quotes = v;
            }
            if let Some(v) = d.get("include_match_words").and_then(obj_bool) {
                g.ts_include_match_words = v;
            }
            if let Some(v) = d.get("disable_virtual_text").and_then(obj_bool) {
                g.ts_disable_virtual_text = v;
            }
        } else {
            g.ts_enabled = ts_dflt;
        }

        g
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
    /// Cache for lookaround-free `regex`-crate scan patterns.
    pub fast_cache: RefCell<HashMap<String, regex::Regex>>,
    /// Translation cache: (pattern, word, ignorecase, captures, scan).
    pub trans_cache: RefCell<HashMap<TransKey, vimregex::Translated>>,
    /// Cache for expression-valued b:match_words (loader.vim:123).
    pub expr_cache: RefCell<HashMap<String, String>>,
    /// get_surrounding memo: buf -> (changedtick, memo map).
    pub surround_memo: RefCell<HashMap<i32, (u32, HashMap<MemoKey, Option<Delim>>)>>,
    pub matchparen: RefCell<crate::matchparen::MatchParenState>,
    pub perf: Perf,
    pub ts: RefCell<crate::treesitter::TsState>,
    /// Configuration supplied via `require('matchup_rs').setup{...}`.
    pub gopts: RefCell<GOpts>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TransKey {
    pub pattern: String,
    pub word: String,
    pub ignorecase: bool,
    pub captures: bool,
    pub scan: bool,
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
            fast_cache: RefCell::new(HashMap::new()),
            trans_cache: RefCell::new(HashMap::new()),
            expr_cache: RefCell::new(HashMap::new()),
            surround_memo: RefCell::new(HashMap::new()),
            matchparen: RefCell::new(Default::default()),
            perf: Perf::new(),
            ts: RefCell::new(Default::default()),
            gopts: RefCell::new(GOpts::default()),
        }
    }

    pub fn reload(&self) {
        self.bufs.borrow_mut().clear();
        self.expr_cache.borrow_mut().clear();
        self.surround_memo.borrow_mut().clear();
        crate::treesitter::invalidate(self, None);
    }

    /// Drop all per-buffer caches (BufDelete/BufWipeout).
    pub fn drop_buf(&self, bufnr: i32) {
        self.bufs.borrow_mut().remove(&bufnr);
        self.surround_memo.borrow_mut().remove(&bufnr);
        crate::treesitter::invalidate(self, Some(bufnr));
    }

    /// Snapshot of the setup(opts) configuration.
    pub fn gopts(&self) -> GOpts {
        self.gopts.borrow().clone()
    }

    pub fn set_gopts(&self, g: GOpts) {
        *self.gopts.borrow_mut() = g;
    }

    /// `:NoMatchParen` / `:DoMatchParen` runtime toggle.
    pub fn set_matchparen_enabled(&self, on: bool) {
        self.gopts.borrow_mut().matchparen_enabled = on;
    }

    /// Translate + compile with the shared cache.
    pub fn compile(&self, pattern: &str, opts: &Opts) -> Option<SharedRegex> {
        // Translate (cheap, deterministic) then look up the compiled regex
        // by its translated pattern string.
        let t = self.translate_cached(pattern, opts)?;
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

    /// Translate with a cache keyed by pattern and options.
    pub fn translate_cached(&self, pattern: &str, opts: &Opts) -> Option<vimregex::Translated> {
        let key = TransKey {
            pattern: pattern.to_string(),
            word: opts.word.clone(),
            ignorecase: opts.ignorecase,
            captures: opts.captures,
            scan: opts.scan,
        };
        if let Some(t) = self.trans_cache.borrow().get(&key) {
            return Some(t.clone());
        }
        let t = translate(pattern, opts).ok()?;
        self.trans_cache.borrow_mut().insert(key, t.clone());
        Some(t)
    }

    /// Compile (and cache) a fancy-regex pattern by its translated form.
    pub fn compile_fancy(&self, pattern: &str) -> Option<SharedRegex> {
        {
            let cache = self.regex_cache.borrow();
            if let Some(r) = cache.get(pattern) {
                return Some(Rc::clone(r));
            }
        }
        let re = Rc::new(Regex::new(pattern).ok()?);
        self.regex_cache
            .borrow_mut()
            .insert(pattern.to_string(), Rc::clone(&re));
        Some(re)
    }

    /// Compile (and cache) a lookaround-free scan pattern for the DFA.
    pub fn compile_fast(&self, pattern: &str) -> Option<regex::Regex> {
        {
            let cache = self.fast_cache.borrow();
            if let Some(r) = cache.get(pattern) {
                return Some(r.clone());
            }
        }
        let r = regex::Regex::new(pattern).ok()?;
        self.fast_cache
            .borrow_mut()
            .insert(pattern.to_string(), r.clone());
        Some(r)
    }

    /// Compile a pattern that may carry prefix-check obligations.
    /// Returns (compiled main, checks, main pattern string).
    pub fn compile_checked(
        &self,
        pattern: &str,
        opts: &Opts,
    ) -> Option<(SharedRegex, Vec<(SharedRegex, bool)>, String)> {
        let t = self.translate_cached(pattern, opts)?;
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
    crate::nvimrs::get_option_as::<String>(name, buf.handle(), 0).unwrap_or_default()
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
/// How the treesitter engine modifies the classic match_words load
/// (loader.vim:24-33): None = TS inactive, NoWords = drop match_words,
/// Filter = keep only punctuation-only sets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TsWords {
    None,
    NoWords,
    Filter,
}

pub fn ensure_buf(state: &State, buf: &Buffer, ts_words: TsWords) -> i32 {
    let h = buf.handle();

    let match_words_raw = buf_var_string(buf, "match_words");
    let mut match_words = if !match_words_raw.is_empty() && !match_words_raw.contains(':') {
        // expression-valued: evaluate and use the global cache.
        // SECURITY NOTE: this mirrors vim-matchup's own behavior
        // (`execute 'let l:match_words =' b:match_words`, loader.vim:97).
        // b:match_words is buffer-local config set by ftplugins; anyone
        // able to set it can already execute arbitrary vimscript.
        if let Some(cached) = state.expr_cache.borrow().get(&match_words_raw) {
            cached.clone()
        } else {
            let v: String = crate::nvimrs::eval_as(&match_words_raw).unwrap_or_default();
            state
                .expr_cache
                .borrow_mut()
                .insert(match_words_raw.clone(), v.clone());
            v
        }
    } else {
        match_words_raw
    };
    match ts_words {
        TsWords::NoWords => match_words = String::new(),
        TsWords::Filter => {
            let sets = crate::vimregex::split_not_bslash(&match_words, ',');
            let kept: Vec<String> = sets
                .into_iter()
                .filter(|s| {
                    (3..=18).contains(&s.chars().count())
                        && s.bytes().all(|c| !c.is_ascii_alphabetic())
                })
                .collect();
            match_words = kept.join(",");
        }
        TsWords::None => {}
    }

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
    let ts_mode = match ts_words {
        TsWords::None => "0",
        TsWords::NoWords => "1",
        TsWords::Filter => "2",
    };
    let hash = hash_inputs(&[
        &match_words,
        &matchpairs,
        &iskeyword,
        &match_skip,
        &midmap_key,
        if nomps { "1" } else { "0" },
        if ignorecase { "1" } else { "0" },
        ts_mode,
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
        scan: false,
    };
    let opts_class = Opts {
        word: word.clone(),
        ignorecase,
        captures: true,
        scan: false,
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
        let mut fast_parts: Vec<&str> = Vec::new();
        let mut exotic_parts: Vec<&str> = Vec::new();
        for si in 0..sets.len() {
            let cset = &sets[si];
            for &side in q.sides() {
                let n_words = if side == Side::Mid {
                    cset.mids.len()
                } else {
                    1
                };
                for mid_id in 1..=n_words {
                    let cw = match cset.word(side, mid_id) {
                        Some(w) => w,
                        None => continue,
                    };
                    if cw.scan.is_none() || cw.main.is_empty() {
                        continue;
                    }
                    parts.push(&cw.main);
                    match cw.fast_main.as_deref() {
                        Some(fm) => fast_parts.push(fm),
                        None => exotic_parts.push(&cw.main),
                    }
                }
            }
        }
        let join = |ps: &[&str]| {
            ps.iter()
                .map(|p| format!("(?:{p})"))
                .collect::<Vec<_>>()
                .join("|")
        };
        let fancy_union = |combined: String| -> Option<SharedRegex> {
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
        let re = if parts.is_empty() {
            None
        } else {
            fancy_union(join(&parts))
        };
        let fast = if fast_parts.is_empty() {
            None
        } else {
            regex::Regex::new(&join(&fast_parts)).ok()
        };
        let exotic = if exotic_parts.is_empty() {
            None
        } else {
            fancy_union(join(&exotic_parts))
        };
        let empty = re.is_none() && fast.is_none() && exotic.is_none();
        unions.insert(
            q,
            Union {
                re,
                fast,
                exotic,
                empty,
            },
        );
    }

    let skip = compile_skip(&match_skip, &word);

    // classify candidate dispatch: words that can match at a position,
    // indexed by the position's first byte, in classify order
    let mut dispatch = HashMap::new();
    for q in SideQuery::ALL {
        let mut by_byte: Vec<Vec<WordRef>> = vec![Vec::new(); 256];
        let mut all: Vec<WordRef> = Vec::new();
        for si in 0..sets.len() {
            let cset = &sets[si];
            for &side in q.sides() {
                let n_words = if side == Side::Mid {
                    cset.mids.len()
                } else {
                    1
                };
                for mid_id in 1..=n_words {
                    let cw = match cset.word(side, mid_id) {
                        Some(w) => w,
                        None => continue,
                    };
                    if cw.classify.is_none() {
                        continue;
                    }
                    let wref = WordRef {
                        set: si,
                        side,
                        mid_id,
                    };
                    all.push(wref);
                    match cw.first {
                        None => {
                            for v in by_byte.iter_mut() {
                                v.push(wref);
                            }
                        }
                        Some(fb) => {
                            for b in 0..=255usize {
                                if fb.contains(b as u8) {
                                    by_byte[b].push(wref);
                                }
                            }
                        }
                    }
                }
            }
        }
        dispatch.insert(q, Dispatch { by_byte, all });
    }

    state.bufs.borrow_mut().insert(
        h,
        BufCompiled {
            lists,
            sets,
            unions,
            dispatch,
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
            first: None,
            fast_main: None,
            lit_prefix: None,
            req_lits: vec![],
            hlend_req_lits: vec![],
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
    let first = vimregex::first_bytes(vim, opts_scan.ignorecase);
    let lit_prefix = vimregex::literal_prefix(vim, opts_class.ignorecase, 16);
    let req_lits = positive_check_lits(vim, opts_class);
    let fast_main = {
        let mut o = opts_scan.clone();
        o.scan = true;
        translate(vim, &o)
            .ok()
            .map(|t| t.pattern)
            .filter(|p| regex::Regex::new(p).is_ok())
    };

    let has_hlend = two
        .extra_list
        .get(extra_idx)
        .map(|e| e.contains_key("hlend"))
        .unwrap_or(false);
    let (hlend_classify, hlend_checks, hlend_req_lits) = if has_hlend {
        let p = process_hlend(vim, -1);
        match state.compile_checked(&p, opts_class) {
            Some((re, checks, _)) => (Some(re), checks, positive_check_lits(&p, opts_class)),
            None => (None, vec![], vec![]),
        }
    } else {
        (None, vec![], vec![])
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
        first,
        fast_main,
        lit_prefix,
        req_lits,
        hlend_req_lits,
    }
}

/// Leading literal of a regex fragment, if any (stops at the first
/// metacharacter; escaped punctuation counts as that literal character).
fn leading_literal(pat: &str) -> Option<String> {
    let mut lit = String::new();
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(e) = it.next() {
                    if e.is_ascii_punctuation() {
                        lit.push(e);
                    }
                }
                break;
            }
            '(' | ')' | '[' | ']' | '{' | '}' | '?' | '*' | '+' | '|' | '^'
            | '$' | '.' => break,
            _ => lit.push(c),
        }
    }
    if lit.is_empty() {
        None
    } else {
        Some(lit)
    }
}

/// Mandatory prefix literals implied by a pattern's positive prefix-check
/// obligations: none of them can match a line prefix lacking the literal.
fn positive_check_lits(vim: &str, opts: &Opts) -> Vec<String> {
    let t = match translate(vim, opts) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let mut lits = Vec::new();
    for c in &t.prefix_checks {
        if c.neg {
            continue;
        }
        if let Some(l) = leading_literal(&c.pattern) {
            lits.push(l);
        }
    }
    lits
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
