//! Pure-Rust treesitter matching engine.
//!
//! Port of vim-matchup's `lua/treesitter-matchup/internal.lua` with NO
//! dependency on the `vim.treesitter` Lua API: grammar parsers are loaded
//! from the runtimepath `parser/` directories via `libloading`, and the
//! `matchup.scm` queries (copied from vim-matchup, MIT) are compiled with
//! the `tree-sitter` crate, including `; inherits:` concatenation and the
//! `#eq?` / `#not-has-parent?` / `#lua-match?` predicates.
//!
//! Trees are cached per buffer by changedtick and re-parsed incrementally:
//! the previous text is diffed at line granularity to build a
//! `tree_sitter::InputEdit`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use libloading::Library;
use nvim_oxi::api::{self, Buffer};
use tree_sitter::{
    InputEdit, Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};
use tree_sitter_language::LanguageFn;

use crate::engine::{Direction, GetDelimOpts};
use crate::state::{GOpts, State};
use crate::types::Delim;
use crate::words::Side;

/// Sentinel `Delim::set` value marking treesitter-engine delims.
pub const TS_SET: usize = usize::MAX - 1;

/// Cap for the delim info cache (internal.lua: `lru.new(150)`).
const DELIM_CACHE_CAP: usize = 150;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchType {
    Scope,
    Open,
    Mid,
    Close,
    Skip,
}

/// Port of `matchup.treesitter.Match`. Range components are i64 because
/// the end_col == 0 normalization produces -1 columns (internal.lua:96).
#[derive(Clone, Debug)]
pub struct TsMatch {
    pub identifier: String,
    pub mtype: MatchType,
    /// (start_row, start_col, end_row, end_col), 0-based, normalized.
    pub range: (i64, i64, i64, i64),
    pub length: i64,
    /// Byte range of the last captured node, for scope parent-walks.
    pub last_node: (usize, usize),
    pub text: String,
}

/// Cached per-delim info (internal.lua `cache:set(result._id, ...)`,
/// consumed by get_matching).
#[derive(Clone)]
pub struct CachedDelim {
    pub bufnr: i32,
    pub tick: u32,
    /// Range id of the seed match (identity for "not the seed" checks).
    pub info_id: String,
    pub row: i64,
    pub col: i64,
    pub key: String,
    /// Range id of the containing scope node (raw node range).
    pub scope_id: String,
    /// Raw scope node range rows: (start_row, end_row).
    pub search_rows: (i64, i64),
    /// Raw scope end (row, col) for the empty-match sentinel.
    pub scope_end: (i64, i64),
}

struct BufTree {
    tick: u32,
    lang: String,
    tree: Tree,
    text: String,
    /// Byte offset of each line start, plus a final sentinel == text.len().
    line_starts: Vec<usize>,
}

#[derive(Default)]
pub struct TsState {
    libs: HashMap<String, (Library, Language)>,
    lang_failed: HashSet<String>,
    queries: HashMap<String, Rc<Query>>,
    query_failed: HashSet<String>,
    trees: HashMap<i32, BufTree>,
    delim_cache: HashMap<u64, CachedDelim>,
    cache_order: Vec<u64>,
    next_id: u64,
    /// (bufnr) -> (filetype, verdict); invalidated when the filetype changes
    verdicts: HashMap<i32, (String, Option<String>)>,
}

impl TsState {
    fn put_cached(&mut self, id: u64, info: CachedDelim) {
        if self.delim_cache.len() >= DELIM_CACHE_CAP {
            if let Some(old) = self.cache_order.first().copied() {
                self.cache_order.remove(0);
                self.delim_cache.remove(&old);
            }
        }
        self.delim_cache.insert(id, info);
        self.cache_order.push(id);
    }
}

// ---------------------------------------------------------------------------
// Language / parser loading
// ---------------------------------------------------------------------------

fn rtp_dirs() -> Vec<PathBuf> {
    let rtp: String = api::eval("&rtp").unwrap_or_default();
    rtp.split(',')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// `lang` may contain dashes (markdown-inline); parser symbols use
/// underscores.
fn sym_lang(lang: &str) -> String {
    lang.replace('-', "_")
}

fn find_parser(lang: &str) -> Option<PathBuf> {
    let name = format!("{}.so", sym_lang(lang));
    for dir in rtp_dirs() {
        let p = dir.join("parser").join(&name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Load (and cache) the treesitter Language for `lang` by dlopen'ing the
/// runtimepath parser and resolving its `tree_sitter_<lang>` symbol.
pub fn language(state: &State, lang: &str) -> Option<Language> {
    {
        let ts = state.ts.borrow();
        if let Some((_, l)) = ts.libs.get(lang) {
            return Some(l.clone());
        }
        if ts.lang_failed.contains(lang) {
            return None;
        }
    }
    let loaded = (|| -> Option<Language> {
        let path = find_parser(lang)?;
        unsafe {
            let lib = Library::new(&path).ok()?;
            let sym = format!("tree_sitter_{}\0", sym_lang(lang));
            let func: libloading::Symbol<
                unsafe extern "C" fn() -> *const (),
            > = lib.get(sym.as_bytes()).ok()?;
            let language = Language::new(LanguageFn::from_raw(*func));
            // keep the library alive alongside the language
            let mut ts = state.ts.borrow_mut();
            ts.libs.insert(lang.to_string(), (lib, language.clone()));
            Some(language)
        }
    })();
    if loaded.is_none() {
        state.ts.borrow_mut().lang_failed.insert(lang.to_string());
    }
    loaded
}

// ---------------------------------------------------------------------------
// Query loading (`; inherits:` aware)
// ---------------------------------------------------------------------------

fn runtime_query_files(lang: &str) -> Vec<PathBuf> {
    if !lang
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for pat in [
        format!("queries/{lang}/matchup.scm"),
        format!("after/queries/{lang}/matchup.scm"),
    ] {
        let q = crate::motion::vim_quote(&pat);
        if let Ok(arr) = api::eval::<Vec<String>>(&format!(
            "nvim_get_runtime_file({q}, v:true)"
        )) {
            for s in arr {
                out.push(PathBuf::from(s));
            }
        }
    }
    out
}

fn parse_inherits(text: &str) -> Vec<String> {
    let mut langs = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(';') {
            let rest = rest.trim();
            if let Some(list) = rest
                .strip_prefix("inherits:")
                .or_else(|| rest.strip_prefix("inherits :"))
            {
                for l in list.split(',') {
                    let l = l.trim();
                    if !l.is_empty() {
                        langs.push(l.to_string());
                    }
                }
            }
            continue;
        }
        // only leading comment lines carry the directive
        break;
    }
    langs
}

fn query_source(state: &State, lang: &str, seen: &mut HashSet<String>, out: &mut String) {
    if !seen.insert(lang.to_string()) {
        return;
    }
    // parents first (nvim prepends inherited queries)
    let files = runtime_query_files(lang);
    let texts: Vec<String> = files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    for t in &texts {
        for parent in parse_inherits(t) {
            query_source(state, &parent, seen, out);
        }
    }
    for t in &texts {
        out.push_str(t);
        out.push('\n');
    }
}

/// Compile (and cache) the matchup query for `lang`, resolving `; inherits:`.
pub fn query(state: &State, lang: &str) -> Option<Rc<Query>> {
    {
        let ts = state.ts.borrow();
        if let Some(q) = ts.queries.get(lang) {
            return Some(Rc::clone(q));
        }
        if ts.query_failed.contains(lang) {
            return None;
        }
    }
    let language = language(state, lang)?;
    let mut src = String::new();
    let mut seen = HashSet::new();
    query_source(state, lang, &mut seen, &mut src);
    if src.trim().is_empty() {
        state.ts.borrow_mut().query_failed.insert(lang.to_string());
        return None;
    }
    match Query::new(&language, &src) {
        Ok(q) => {
            let q = Rc::new(q);
            state
                .ts
                .borrow_mut()
                .queries
                .insert(lang.to_string(), Rc::clone(&q));
            Some(q)
        }
        Err(e) => {
            crate::matchparen::trace(&format!("TS query compile failed for {lang}: {e}"));
            state.ts.borrow_mut().query_failed.insert(lang.to_string());
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tree cache with incremental parsing
// ---------------------------------------------------------------------------

fn fetch_text(buf: &Buffer) -> Option<(String, Vec<usize>, u32)> {
    let tick: u32 = buf.get_changedtick().unwrap_or(0);
    let n = buf.line_count().ok()? as usize;
    let lines: Vec<String> = buf
        .get_lines(0..n, false)
        .ok()?
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let mut text = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    let mut line_starts = Vec::with_capacity(lines.len() + 1);
    for l in &lines {
        line_starts.push(text.len());
        text.push_str(l);
        text.push('\n');
    }
    // drop the trailing newline's phantom empty last line: nvim buffers of
    // n lines joined with \n have a trailing \n; tree-sitter sees it as an
    // extra empty line, harmless for queries but keep line_starts aligned
    line_starts.push(text.len());
    Some((text, line_starts, tick))
}

/// Line-granularity diff of old vs new text producing a single InputEdit
/// covering the changed region (sufficient for incremental re-parse).
fn diff_edit(old: &str, new: &str) -> Option<InputEdit> {
    let ob: &[u8] = old.as_bytes();
    let nb: &[u8] = new.as_bytes();
    let mut start = 0usize;
    while start < ob.len() && start < nb.len() && ob[start] == nb[start] {
        start += 1;
    }
    if start == ob.len() && start == nb.len() {
        return None;
    }
    let mut eold = ob.len();
    let mut enew = nb.len();
    while eold > start && enew > start && ob[eold - 1] == nb[enew - 1] {
        eold -= 1;
        enew -= 1;
    }
    // expand to line boundaries so points stay consistent
    let ls = old[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let start_position = point_of_lines(old, ls);
    let old_end_position = point_of_lines(old, eold);
    let new_end_position = point_of_lines(new, enew);
    Some(InputEdit {
        start_byte: ls,
        old_end_byte: eold.max(ls),
        new_end_byte: enew.max(ls),
        start_position,
        old_end_position,
        new_end_position,
    })
}

fn point_of_lines(text: &str, byte: usize) -> Point {
    let b = byte.min(text.len());
    let row = text[..b].matches('\n').count();
    let col = b - text[..b].rfind('\n').map(|i| i + 1).unwrap_or(0);
    Point::new(row, col)
}

/// Fetch the (possibly incrementally re-parsed) tree for the buffer.
/// Returns cloned handles so callers never hold the state borrow.
fn ensure_tree(
    state: &State,
    bufnr: i32,
    buf: &Buffer,
) -> Option<(Tree, String, Vec<usize>, u32)> {
    let tick: u32 = buf.get_changedtick().unwrap_or(0);
    {
        let ts = state.ts.borrow();
        if let Some(bt) = ts.trees.get(&bufnr) {
            if bt.tick == tick {
                return Some((bt.tree.clone(), bt.text.clone(), bt.line_starts.clone(), tick));
            }
        }
    }
    let (text, line_starts, tick) = fetch_text(buf)?;
    let lang = {
        let ts = state.ts.borrow();
        ts.trees.get(&bufnr).map(|_| ())
    };
    let _ = lang;
    // language for this buffer
    let ft: String = api::eval(&format!("getbufvar({bufnr}, '&filetype', '')")).unwrap_or_default();
    let language = language(state, &ft)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    // bound worst-case parse time; a timed-out parse yields None and the
    // engine falls back to classic matching
    #[allow(deprecated)]
    parser.set_timeout_micros(250_000);

    let old = state.ts.borrow().trees.get(&bufnr).map(|bt| {
        (bt.tree.clone(), bt.text.clone(), bt.lang.clone())
    });
    let tree = match old {
        Some((mut old_tree, ref old_text, ref old_lang)) if *old_lang == ft => {
            if let Some(edit) = diff_edit(old_text, &text) {
                old_tree.edit(&edit);
            }
            parser.parse(text.as_bytes(), Some(&old_tree))?
        }
        _ => parser.parse(text.as_bytes(), None)?,
    };
    state.ts.borrow_mut().trees.insert(
        bufnr,
        BufTree {
            tick,
            lang: ft.clone(),
            tree: tree.clone(),
            text: text.clone(),
            line_starts: line_starts.clone(),
        },
    );
    Some((tree, text, line_starts, tick))
}

// ---------------------------------------------------------------------------
// Match collection (port of get_lang_matches)
// ---------------------------------------------------------------------------

fn range_id(r: (i64, i64, i64, i64)) -> String {
    format!("range_{}_{}_{}_{}", r.0, r.1, r.2, r.3)
}

/// Byte offset of a (row, 0-based byte col) point, clamped to the text.
fn byte_at(line_starts: &[usize], text_len: usize, row: i64, col: i64) -> usize {
    if row < 0 {
        return 0;
    }
    let r = (row as usize).min(line_starts.len().saturating_sub(1));
    let start = line_starts[r];
    let end = if r + 1 < line_starts.len() {
        line_starts[r + 1].saturating_sub(1).max(start)
    } else {
        text_len
    };
    if col < 0 {
        return start;
    }
    (start + col as usize).min(end)
}

fn line_text(text: &str, line_starts: &[usize], row: usize, sc: i64, ec_row_same: bool, ec: i64) -> String {
    if row + 1 >= line_starts.len() {
        return String::new();
    }
    let s = line_starts[row];
    let e = line_starts[row + 1].saturating_sub(1).max(s); // strip \n
    let from = if sc < 0 { 0 } else { (s + sc as usize).min(e) };
    let to = if ec_row_same {
        if ec < 0 {
            e
        } else {
            (s + ec as usize).min(e).max(from)
        }
    } else {
        e
    };
    text[from..to].to_string()
}

/// Minimal Lua-pattern matcher for `#lua-match?` (only the constructs used
/// by the shipped queries: literal text, `^` anchor, `%x` classes).
fn lua_match(text: &str, pat: &str) -> bool {
    let pb: Vec<char> = pat.chars().collect();
    let anchored = pb.first() == Some(&'^');
    let p = if anchored { &pb[1..] } else { &pb[..] };
    let tb: Vec<char> = text.chars().collect();
    let starts = if anchored {
        0..=0.min(tb.len())
    } else {
        0..=tb.len()
    };
    for start in starts {
        if match_at(&tb, start, p) {
            return true;
        }
    }
    false
}

fn match_at(tb: &[char], mut ti: usize, p: &[char]) -> bool {
    let mut pi = 0;
    while pi < p.len() {
        match p[pi] {
            '$' if pi + 1 == p.len() => return ti == tb.len(),
            '%' if pi + 1 < p.len() => {
                let cls = p[pi + 1];
                let c = match tb.get(ti) {
                    Some(c) => *c,
                    None => return false,
                };
                let ok = match cls {
                    'a' => c.is_alphabetic(),
                    'd' => c.is_ascii_digit(),
                    's' => c.is_whitespace(),
                    'w' => c.is_alphanumeric(),
                    'A' => !c.is_alphabetic(),
                    'D' => !c.is_ascii_digit(),
                    'S' => !c.is_whitespace(),
                    'W' => !c.is_alphanumeric(),
                    other => c == other,
                };
                if !ok {
                    return false;
                }
                ti += 1;
                pi += 2;
            }
            '.' => {
                if ti >= tb.len() {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
            c => {
                if tb.get(ti) != Some(&c) {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
        }
    }
    true
}

fn eval_general_predicates(
    q: &Query,
    pattern_index: usize,
    captures: &[tree_sitter::QueryCapture],
    text: &[u8],
) -> bool {
    for pred in q.general_predicates(pattern_index) {
        let op = pred.operator.as_ref();
        match op {
            "not-has-parent?" => {
                if let Some(tree_sitter::QueryPredicateArg::Capture(idx)) = pred.args.first() {
                    let node = captures.iter().find(|c| c.index == *idx).map(|c| c.node);
                    let kind = match pred.args.get(1) {
                        Some(tree_sitter::QueryPredicateArg::String(s)) => s.as_ref(),
                        _ => continue,
                    };
                    if let Some(n) = node {
                        if let Some(p) = n.parent() {
                            if p.kind() == kind {
                                return false;
                            }
                        }
                    }
                }
            }
            "lua-match?" => {
                if let Some(tree_sitter::QueryPredicateArg::Capture(idx)) = pred.args.first() {
                    let node = captures.iter().find(|c| c.index == *idx).map(|c| c.node);
                    let pat = match pred.args.get(1) {
                        Some(tree_sitter::QueryPredicateArg::String(s)) => s.to_string(),
                        _ => continue,
                    };
                    if let Some(n) = node {
                        let t = n.utf8_text(text).unwrap_or("");
                        if !lua_match(t, &pat) {
                            return false;
                        }
                    }
                }
            }
            // unknown predicates: mirror nvim's leniency for query files we
            // did not write; they neither add nor remove structure here
            _ => {}
        }
    }
    true
}

/// Port of get_lang_matches: run the query over the window and collect
/// normalized matches.
fn collect_matches(
    tree: &Tree,
    text: &str,
    line_starts: &[usize],
    q: &Query,
    srow: usize,
    erow: usize,
) -> Vec<TsMatch> {
    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = QueryCursor::new();
    cursor.set_point_range(Point::new(srow, 0)..Point::new(erow, 0));
    let bytes = text.as_bytes();
    let mut matches = cursor.matches(q, root, bytes);
    let (mut b1, mut b2) = (Vec::new(), Vec::new());
    let mut provider: &[u8] = bytes;
    while let Some(m) = matches.next() {
        if !m.satisfies_text_predicates(q, &mut b1, &mut b2, &mut provider) {
            continue;
        }
        if !eval_general_predicates(q, m.pattern_index, m.captures, bytes) {
            continue;
        }
        // group captures by capture index, preserving order
        let mut order: Vec<u32> = Vec::new();
        let mut groups: HashMap<u32, Vec<Node>> = HashMap::new();
        for c in m.captures {
            let e = groups.entry(c.index).or_default();
            if e.is_empty() {
                order.push(c.index);
            }
            e.push(c.node);
        }
        // nvim `#offset!` directives adjust capture ranges (rows/cols)
        let mut offsets: HashMap<u32, (i64, i64, i64, i64)> = HashMap::new();
        for pred in q.general_predicates(m.pattern_index) {
            if pred.operator.as_ref() != "offset!" || pred.args.len() < 5 {
                continue;
            }
            let idx = match pred.args[0] {
                tree_sitter::QueryPredicateArg::Capture(i) => i,
                _ => continue,
            };
            let nums: Vec<i64> = pred.args[1..5]
                .iter()
                .map(|a| match a {
                    tree_sitter::QueryPredicateArg::String(sv) => {
                        sv.parse::<i64>().unwrap_or(0)
                    }
                    _ => 0,
                })
                .collect();
            let e = offsets.entry(idx).or_insert((0, 0, 0, 0));
            e.0 += nums[0];
            e.1 += nums[1];
            e.2 += nums[2];
            e.3 += nums[3];
        }
        for idx in order {
            let nodes = match groups.get(&idx) {
                Some(n) => n,
                None => continue,
            };
            let first = nodes[0];
            let last = *nodes.last().unwrap();
            let sp = first.start_position();
            let ep = last.end_position();
            // internal.lua takes the start from first:range() (raw) and
            // only the end from ts.get_range(last, metadata), so #offset!
            // effectively applies to the end point alone
            let (_dr1, _dc1, dr2, dc2) = offsets.get(&idx).copied().unwrap_or((0, 0, 0, 0));
            let (mut sr, mut sc) = (sp.row as i64, sp.column as i64);
            let (mut er, mut ec) = (ep.row as i64 + dr2, ep.column as i64 + dc2);
            let length = byte_at(line_starts, text.len(), er, ec) as i64
                - first.start_byte() as i64;
            if ec == 0 {
                if sr == er {
                    sc = -1;
                    sr -= 1;
                }
                ec = -1;
                er -= 1;
            }
            let name = q.capture_names()[idx as usize];
            let (mtype, identifier) = match name.split_once('.') {
                Some((t, rest)) => {
                    let ident = rest.split('.').next().unwrap_or(rest);
                    let mt = match t {
                        "scope" => MatchType::Scope,
                        "open" => MatchType::Open,
                        "mid" => MatchType::Mid,
                        "close" => MatchType::Close,
                        "skip" => MatchType::Skip,
                        _ => continue,
                    };
                    (mt, ident.to_string())
                }
                None => continue,
            };
            let same_row = sr == er && sr >= 0;
            let text_str = if sr >= 0 {
                line_text(
                    text,
                    line_starts,
                    sr as usize,
                    sc,
                    same_row,
                    ec,
                )
            } else {
                String::new()
            };
            out.push(TsMatch {
                identifier,
                mtype,
                range: (sr, sc, er, ec),
                length,
                last_node: (last.start_byte(), last.end_byte()),
                text: text_str,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Active matches / scopes (ports of get_active_matches, get_scopes)
// ---------------------------------------------------------------------------

struct Active {
    open: Vec<usize>,
    mid: Vec<usize>,
    close: Vec<usize>,
    symbols: HashMap<String, String>,
}

fn get_active(matches: &[TsMatch], enable_quotes: bool) -> Active {
    let mut info = Active {
        open: Vec::new(),
        mid: Vec::new(),
        close: Vec::new(),
        symbols: HashMap::new(),
    };
    for (i, m) in matches.iter().enumerate() {
        let id = range_id(m.range);
        match m.mtype {
            MatchType::Open | MatchType::Close => {
                let reject = !enable_quotes && m.identifier.contains("quote");
                if !reject && !info.symbols.contains_key(&id) {
                    info.symbols.insert(id, m.identifier.clone());
                    if m.mtype == MatchType::Open {
                        info.open.push(i);
                    } else {
                        info.close.push(i);
                    }
                }
            }
            MatchType::Mid => {
                if !info.symbols.contains_key(&id) {
                    info.symbols.insert(id, m.identifier.clone());
                    info.mid.push(i);
                }
            }
            _ => {}
        }
    }
    info
}

fn get_scopes(matches: &[TsMatch]) -> HashMap<String, HashSet<String>> {
    let mut scopes: HashMap<String, HashSet<String>> = HashMap::new();
    for m in matches {
        if m.mtype == MatchType::Scope {
            scopes
                .entry(m.identifier.clone())
                .or_default()
                .insert(range_id(m.range));
        }
    }
    scopes
}

/// Walk parents from the match's last node until a node whose raw range id
/// is a registered scope for `key` (port of containing_scope). Returns
/// (scope_id, start_row, end_row, end_row, end_col-raw).
fn containing_scope<'t>(
    root: Node<'t>,
    scopes: &HashMap<String, HashSet<String>>,
    last: (usize, usize),
    key: &str,
) -> Option<(String, i64, i64, i64)> {
    let set = scopes.get(key)?;
    let mut node = root.descendant_for_byte_range(last.0, last.1)?;
    loop {
        let sp = node.start_position();
        let ep = node.end_position();
        let id = range_id((sp.row as i64, sp.column as i64, ep.row as i64, ep.column as i64));
        if set.contains(&id) {
            return Some((id, sp.row as i64, ep.row as i64, ep.column as i64));
        }
        match node.parent() {
            Some(p) => node = p,
            None => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// Engine entry points
// ---------------------------------------------------------------------------

/// Port of M.is_enabled + language resolution. `lang` = filetype for now
/// (nvim's get_lang only diverges for explicit registrations).
pub fn active_lang(state: &State, gopts: &GOpts, bufnr: i32) -> Option<String> {
    if !gopts.ts_enabled {
        return None;
    }
    let ft: String =
        api::eval(&format!("getbufvar({bufnr}, '&filetype', '')")).unwrap_or_default();
    if ft.is_empty() {
        return None;
    }
    {
        let ts = state.ts.borrow();
        if let Some((cached_ft, verdict)) = ts.verdicts.get(&bufnr) {
            if *cached_ft == ft {
                return verdict.clone();
            }
        }
    }
    let lang = ft.clone();
    let verdict = (|| -> Option<String> {
        let buf_ok: i64 = api::eval(&format!(
            "+getbufvar({bufnr}, 'matchup_treesitter_enabled', 1)"
        ))
        .unwrap_or(1);
        if buf_ok == 0 {
            return None;
        }
        if gopts.ts_disabled.iter().any(|d| *d == lang) {
            return None;
        }
        language(state, &lang)?;
        query(state, &lang)?;
        Some(lang)
    })();
    state
        .ts
        .borrow_mut()
        .verdicts
        .insert(bufnr, (ft, verdict.clone()));
    verdict
}

fn is_in_range(m: &TsMatch, line: i64, col: i64) -> bool {
    let (rs, cs, re, ce) = m.range;
    let (ps, pe_row, pe_col) = (line, line, col + 1);
    if ps < rs {
        return false;
    }
    if ps == rs && col < cs {
        return false;
    }
    if pe_row > re {
        return false;
    }
    if pe_row == re && pe_col > ce {
        return false;
    }
    true
}

fn sides_for(side: &crate::words::SideQuery) -> &'static [MatchType] {
    use crate::words::SideQuery::*;
    match side {
        Open => &[MatchType::Open],
        Mid => &[MatchType::Mid],
        Close => &[MatchType::Close],
        Both => &[MatchType::Close, MatchType::Open],
        BothAll => &[MatchType::Close, MatchType::Mid, MatchType::Open],
        OpenMid => &[MatchType::Mid, MatchType::Open],
    }
}

fn side_of(mt: MatchType) -> Side {
    match mt {
        MatchType::Open => Side::Open,
        MatchType::Mid => Side::Mid,
        _ => Side::Close,
    }
}

/// Port of M.get_delim: find the treesitter delim for the position given
/// by `opts.at` (or the window cursor).
pub fn get_delim(
    state: &State,
    gopts: &GOpts,
    buf: &Buffer,
    bufnr: i32,
    lang: &str,
    opts: &GetDelimOpts,
) -> Option<Delim> {
    let (tree, text, line_starts, tick) = ensure_tree(state, bufnr, buf)?;
    let q = query(state, lang)?;
    let nlines = line_starts.len().saturating_sub(1).max(1);

    let cur = match opts.at {
        Some(p) => p,
        None => {
            let w = api::get_current_win();
            let (r, c) = w.get_cursor().ok()?;
            crate::types::Pos::new(r, c + 1)
        }
    };
    let cur_row0 = cur.lnum.saturating_sub(1) as i64;

    let stopline = gopts.ts_stopline;
    let srow = (cur_row0 - stopline as i64).max(0) as usize;
    let erow = ((cur_row0 + stopline as i64).max(0) as usize).min(nlines);

    let matches = collect_matches(&tree, &text, &line_starts, &q, srow, erow);
    if matches.is_empty() {
        return None;
    }
    let active = get_active(&matches, gopts.ts_enable_quotes);
    let scopes = get_scopes(&matches);
    let root = tree.root_node();

    let mut sel: Option<(usize, MatchType)> = None;
    if opts.direction == Direction::Current {
        let cur_col0 = cur.cnum.saturating_sub(1) as i64;
        let mut smallest: i64 = i64::MAX;
        for mt in sides_for(&opts.side) {
            if *mt == MatchType::Mid && gopts.delim_nomids {
                continue;
            }
            let list = match mt {
                MatchType::Open => &active.open,
                MatchType::Mid => &active.mid,
                _ => &active.close,
            };
            for &i in list {
                let m = &matches[i];
                if is_in_range(m, cur_row0, cur_col0) && m.length < smallest {
                    smallest = m.length;
                    sel = Some((i, *mt));
                }
            }
        }
    } else {
        let max_col: i64 = 100_000;
        let cur_pos = max_col * cur_row0 + (cur.cnum.saturating_sub(1) as i64);
        let mut closest_dist = i64::MAX;
        for mt in sides_for(&opts.side) {
            let list = match mt {
                MatchType::Open => &active.open,
                MatchType::Mid => &active.mid,
                _ => &active.close,
            };
            for &i in list {
                let m = &matches[i];
                let pos = max_col * m.range.0 + m.range.1;
                let ok = if opts.direction == Direction::Next {
                    pos >= cur_pos
                } else {
                    pos <= cur_pos
                };
                if ok {
                    let dist = (pos - cur_pos).abs();
                    if dist < closest_dist {
                        closest_dist = dist;
                        sel = Some((i, *mt));
                    }
                }
            }
        }
    }

    let (idx, mt) = sel?;
    let info = &matches[idx];
    let key = info.identifier.clone();
    let scope = containing_scope(root, &scopes, info.last_node, &key)?;

    let id = {
        let mut ts = state.ts.borrow_mut();
        ts.next_id += 1;
        ts.next_id
    };
    let cached = CachedDelim {
        bufnr,
        tick,
        info_id: range_id(info.range),
        row: info.range.0,
        col: info.range.1,
        key: key.clone(),
        scope_id: scope.0.clone(),
        search_rows: (scope.1, scope.2),
        scope_end: (scope.2, scope.3),
    };
    state.ts.borrow_mut().put_cached(id, cached);

    Some(Delim {
        lnum: (info.range.0 + 1).max(0) as usize,
        cnum: (info.range.1 + 1).max(0) as usize,
        match_: info.text.clone(),
        side: side_of(mt),
        set: TS_SET,
        word_id: 0,
        skip: false,
        groups: HashMap::new(),
        augment_str: String::new(),
        augment_unresolved: Default::default(),
        highlighting: opts.highlighting,
        match_index: 0,
        ts_id: id,
    })
}

/// Port of M.get_matching: sibling delimiters of the cached seed within
/// the same scope. Returns (text, lnum, cnum) triples like the classic
/// get_matching_raw.
pub fn get_matching(
    state: &State,
    gopts: &GOpts,
    buf: &Buffer,
    bufnr: i32,
    lang: &str,
    ts_id: u64,
    down: bool,
) -> Vec<(String, usize, usize)> {
    let cached = match state.ts.borrow().delim_cache.get(&ts_id) {
        Some(c) if c.bufnr == bufnr => c.clone(),
        _ => return Vec::new(),
    };
    let (tree, text, line_starts, tick) = match ensure_tree(state, bufnr, buf) {
        Some(t) => t,
        None => return Vec::new(),
    };
    if tick != cached.tick {
        // stale scope info; the classic engine has the same exposure via
        // changedtick-invalidated caches
        return Vec::new();
    }
    let q = match query(state, lang) {
        Some(q) => q,
        None => return Vec::new(),
    };
    let nlines = line_starts.len().saturating_sub(1).max(1);
    let cur_row0 = cached.row.max(0) as usize;
    let stopline = gopts.ts_stopline;
    let srow = (cur_row0.saturating_sub(stopline)).max(0);
    let erow = (cur_row0 + stopline).min(nlines);

    let matches = collect_matches(&tree, &text, &line_starts, &q, srow, erow);
    let active = get_active(&matches, gopts.ts_enable_quotes);
    let scopes = get_scopes(&matches);
    let root = tree.root_node();

    let sides: &[MatchType] = if gopts.delim_nomids {
        if down {
            &[MatchType::Close]
        } else {
            &[MatchType::Open]
        }
    } else if down {
        &[MatchType::Mid, MatchType::Close]
    } else {
        &[MatchType::Mid, MatchType::Open]
    };

    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut got_close = false;
    for mt in sides {
        let list = match mt {
            MatchType::Open => &active.open,
            MatchType::Mid => &active.mid,
            _ => &active.close,
        };
        for &i in list {
            if state.perf.timeout_check() {
                return Vec::new();
            }
            let m = &matches[i];
            let id = range_id(m.range);
            if id == cached.info_id {
                continue;
            }
            if active.symbols.get(&id).map(|s| s.as_str()) != Some(cached.key.as_str()) {
                continue;
            }
            let (row, col) = (m.range.0, m.range.1);
            let after = row > cached.row || (row == cached.row && col > cached.col);
            let before = row < cached.row || (row == cached.row && col < cached.col);
            if !(if down { after } else { before }) {
                continue;
            }
            if row < cached.search_rows.0 || row > cached.search_rows.1 {
                continue;
            }
            let scope = match containing_scope(root, &scopes, m.last_node, &cached.key) {
                Some(s) => s,
                None => continue,
            };
            if scope.0 != cached.scope_id {
                continue;
            }
            out.push((
                m.text.clone(),
                (row + 1).max(0) as usize,
                (col + 1).max(0) as usize,
            ));
            if *mt == MatchType::Close {
                got_close = true;
            }
        }
    }

    out.sort_by(|a, b| (a.1, a.2).cmp(&(b.1, b.2)));

    if down && !got_close {
        // no stop marker: empty-match sentinel at the scope end
        out.push((
            String::new(),
            (cached.scope_end.0 + 1).max(0) as usize,
            (cached.scope_end.1 + 1).max(0) as usize,
        ));
    }
    out
}

/// Drop cached trees for a buffer (called on MatchupReload / BufDelete).
pub fn invalidate(state: &State, bufnr: Option<i32>) {
    let mut ts = state.ts.borrow_mut();
    match bufnr {
        Some(b) => {
            ts.trees.remove(&b);
            ts.verdicts.remove(&b);
        }
        None => {
            ts.trees.clear();
            ts.verdicts.clear();
            ts.delim_cache.clear();
            ts.cache_order.clear();
        }
    }
}

#[allow(dead_code)]
fn path_exists(p: &Path) -> bool {
    p.is_file()
}
