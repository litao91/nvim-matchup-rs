//! The classic matching engine: get_delim, get_matching, get_surrounding.
//! Port of vim-matchup's autoload/matchup/delim.vim, replacing vim's
//! cursor-moving searchpos/searchpairpos loops with direct scans over
//! buffer text fetched once per operation.

use std::collections::HashMap;

use fancy_regex::Regex;
use nvim_oxi::api::{Buffer, Window as NvimWindow};
use nvim_oxi::{Array, Object};

use crate::skip::{in_synstack, skip_at, MidSkip};
use crate::state::{BufCompiled, GOpts, SharedRegex, State, Union};
use crate::types::*;
use crate::vimregex::{fill_backrefs_vim, Opts};
use crate::words::{process_hlend, remove_capture_groups, Side, SideQuery};

// ---------------------------------------------------------------------------
// Buffer lines
// ---------------------------------------------------------------------------

/// Buffer text fetched once per operation. Columns are byte offsets.
pub struct Lines {
    /// 0-based index of the first held line.
    pub start0: usize,
    pub lines: Vec<String>,
    /// Total number of lines in the buffer.
    pub total: usize,
}

/// Buffers up to this size are fetched in full; larger buffers use a
/// window around the cursor.
const FULL_FETCH_LIMIT: usize = 20_000;

impl Lines {
    pub fn fetch(buf: &Buffer, from1: usize, to1: usize) -> Lines {
        let total = buf.line_count().unwrap_or(0);
        let s = from1.saturating_sub(1).min(total);
        let e = to1.min(total).max(s);
        let lines = buf
            .get_lines(s..e, false)
            .map(|it| {
                it.map(|x| x.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Lines {
            start0: s,
            lines,
            total,
        }
    }

    pub fn fetch_for_cursor(buf: &Buffer, cursor_lnum: usize, margin: usize) -> Lines {
        let total = buf.line_count().unwrap_or(0);
        if total <= FULL_FETCH_LIMIT {
            Lines::fetch(buf, 1, total)
        } else {
            Lines::fetch(
                buf,
                cursor_lnum.saturating_sub(margin).max(1),
                cursor_lnum + margin,
            )
        }
    }

    pub fn get1(&self, lnum: usize) -> Option<&str> {
        let i = lnum.checked_sub(self.start0 + 1)?;
        self.lines.get(i).map(|s| s.as_str())
    }

    pub fn max_lnum(&self) -> usize {
        self.start0 + self.lines.len()
    }
}

fn bound_up(line: &str, mut p: usize) -> usize {
    while p < line.len() && !line.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// Next scan resume position after a match starting at `s` (advances one
/// character, allowing overlapping matches like vim with `cpo-=c`).
fn next_pos(line: &str, s: usize) -> Option<usize> {
    let p = bound_up(line, s + 1);
    if p <= line.len() && p > s {
        Some(p)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Operation context
// ---------------------------------------------------------------------------

pub struct Ctx<'a> {
    pub state: &'a State,
    pub bc: &'a BufCompiled,
    pub buf: Buffer,
    pub win: NvimWindow,
    pub gopts: &'a GOpts,
    pub lines: Lines,
    pub mode: String,
    pub synmaxcol: i64,
    /// Whether vim syntax highlighting is loaded (`g:syntax_on`); when
    /// false every synID() is 0 and syntax-based skips are constant.
    pub syntax_on: bool,
    /// Treesitter language for this buffer when the TS engine is active.
    pub ts_lang: Option<String>,
}

impl<'a> Ctx<'a> {
    pub fn new(
        state: &'a State,
        bc: &'a BufCompiled,
        buf: Buffer,
        win: NvimWindow,
        gopts: &'a GOpts,
    ) -> Ctx<'a> {
        // nvim_win_get_cursor: line is 1-based, col is 0-based
        let cursor = win.get_cursor().map(|(r, _)| r).unwrap_or(1);
        let margin = gopts.delim_stopline.max(gopts.matchparen_stopline) + 100;
        let lines = Lines::fetch_for_cursor(&buf, cursor, margin);
        let mode: String =
            crate::nvimrs::call_fn_as("mode", &Array::from_iter([Object::from(1i64)]))
                .unwrap_or_else(|| "n".to_string());
        let synmaxcol: i64 =
            crate::nvimrs::get_option_as("synmaxcol", buf.handle(), 0).unwrap_or(0);
        let syntax_on: i64 = if crate::nvimrs::get_var_as::<Object>("syntax_on").is_some() {
            1
        } else {
            0
        };
        let ts_lang = if gopts.ts_enabled {
            crate::treesitter::active_lang(state, gopts, buf.handle())
        } else {
            None
        };
        Ctx {
            state,
            bc,
            buf,
            win,
            gopts,
            lines,
            mode,
            synmaxcol,
            syntax_on: syntax_on != 0,
            ts_lang,
        }
    }

    pub fn cursor(&self) -> Option<Pos> {
        // nvim_win_get_cursor: line is 1-based, col is 0-based
        let (r, c) = self.win.get_cursor().ok()?;
        Some(Pos::new(r, c + 1))
    }

    pub fn mode_char(&self) -> char {
        self.mode.chars().next().unwrap_or('n')
    }

    fn translate_opts(&self, captures: bool) -> Opts {
        Opts {
            word: self.bc.word.clone(),
            ignorecase: self.bc.ignorecase,
            captures,
            scan: false,
        }
    }

    /// Compile a (possibly dynamically filled) vim pattern for scanning.
    fn compile_pat(&self, vim: &str) -> Option<CPat> {
        let (re, checks, main) = self
            .state
            .compile_checked(vim, &self.translate_opts(false))?;
        let fast_main = {
            let mut o = self.translate_opts(false);
            o.scan = true;
            // no per-call compile validation: an invalid part makes the
            // combined compile_fast fail and the scan falls back to fancy
            self.state.translate_cached(vim, &o).map(|t| t.pattern)
        };
        Some(CPat {
            main,
            re,
            checks,
            fast_main,
        })
    }
}

pub struct CPat {
    pub main: String,
    pub re: SharedRegex,
    pub checks: Vec<(SharedRegex, bool)>,
    /// Lookaround-free DFA pattern for scan loops (over-approximates
    /// positions; side_at verifies exactly). None when untranslatable.
    pub fast_main: Option<String>,
}

/// Scan-loop regex: the DFA variant when available, fancy otherwise.
enum ScanRe<'a> {
    Fast(&'a regex::Regex),
    Fancy(&'a Regex),
}

impl ScanRe<'_> {
    fn find_at(&self, line: &str, pos: usize) -> Option<(usize, usize)> {
        match self {
            ScanRe::Fast(r) => r.find_at(line, pos).map(|m| (m.start(), m.end())),
            ScanRe::Fancy(r) => r
                .find_from_pos(line, pos)
                .ok()
                .flatten()
                .map(|m| (m.start(), m.end())),
        }
    }
}

fn checks_ok(checks: &[(SharedRegex, bool)], line: &str, start0: usize) -> bool {
    if checks.is_empty() {
        return true;
    }
    let prefix = &line[..start0.min(line.len())];
    checks.iter().all(|(re, neg)| {
        let m = re.is_match(prefix).unwrap_or(false);
        if *neg {
            !m
        } else {
            m
        }
    })
}

// ---------------------------------------------------------------------------
// get_delim
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Next,
    Prev,
    Current,
}

#[derive(Clone)]
pub struct GetDelimOpts {
    pub direction: Direction,
    pub side: SideQuery,
    pub insertmode: bool,
    pub check_skip: Option<bool>,
    pub stopline: usize,
    pub highlighting: bool,
    pub at: Option<Pos>,
}

impl GetDelimOpts {
    pub fn new(direction: Direction, side: SideQuery) -> GetDelimOpts {
        GetDelimOpts {
            direction,
            side,
            insertmode: false,
            check_skip: None,
            stopline: 0, // 0 = use gopts.delim_stopline
            highlighting: false,
            at: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Hit {
    lnum: usize,
    start0: usize,
    end0: usize,
}

impl Hit {
    fn cnum(&self) -> usize {
        self.start0 + 1
    }
}

/// True when the byte at 0-based column `c0` is not punctuation
/// (vim's `getline(lnum)[cnum-1] =~? '[^[:punct:]]'`).
fn char_not_punct(line: &str, c0: usize) -> bool {
    match line.as_bytes().get(c0) {
        None => false,
        Some(&b) if b < 0x80 => !(b as char).is_ascii_punctuation(),
        Some(_) => true,
    }
}

/// Visual modes per vim's `stridx("vV\<c-v>", mode()) > -1`; nvim reports
/// visual-block as either "\x16" or "^V".
fn is_visual_mode(mode: &str) -> bool {
    mode.starts_with('v')
        || mode.starts_with('V')
        || mode.contains('\x16')
        || mode.starts_with("^V")
}

/// Port of s:get_delim (delim.vim:324).
pub fn get_delim(ctx: &Ctx, opts: &GetDelimOpts) -> Option<Delim> {
    if ctx.bc.sets.is_empty() {
        return None;
    }
    // async events pending (delim.vim:363)
    if crate::nvimrs::call_fn_as::<String>("state", &Array::from_iter([Object::from("a")]))
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return None;
    }
    ctx.state.perf.tic("s:get_delim");

    let union = ctx.bc.unions.get(&opts.side)?;
    if union.empty {
        return None;
    }

    let cur = match opts.at {
        Some(p) => p,
        None => ctx.cursor()?,
    };
    let raw0 = cur.cnum.saturating_sub(1);
    let mut cursorpos = cur.cnum; // 1-based, adjusted
    if cursorpos > 1 && opts.insertmode {
        cursorpos -= 1;
    }
    let visual_eol = {
        let line_len = ctx.lines.get1(cur.lnum).map(|l| l.len()).unwrap_or(0);
        cursorpos > line_len && is_visual_mode(&ctx.mode)
    };
    if visual_eol {
        cursorpos -= 1;
    }
    let cur0 = cursorpos.saturating_sub(1);

    let stopline = if opts.stopline > 0 {
        opts.stopline
    } else {
        ctx.gopts.delim_stopline
    };

    // check_skip determination (delim.vim:387-400)
    let cursor_skip = {
        let line = ctx.lines.get1(cur.lnum).unwrap_or("");
        skip_at(
            ctx.state,
            &ctx.bc.skip,
            line,
            cur.lnum,
            cursorpos.min(line.len().max(1)),
            ctx.syntax_on,
        )
    };
    let noskips = ctx.gopts.delim_noskips;
    let check_skip = opts.check_skip.unwrap_or(match opts.direction {
        Direction::Current => {
            noskips >= 2
                || (noskips >= 1 && char_not_punct(ctx.lines.get1(cur.lnum).unwrap_or(""), cur0))
        }
        _ => !cursor_skip || noskips >= 2,
    });
    if opts.direction == Direction::Current && check_skip && cursor_skip {
        return None;
    }

    ctx.state.perf.toc("s:get_delim", "setup");

    // ---- first pass: locate the match and classify it ----
    // Next/Prev scan with the lookaround-free DFA union plus the exotic
    // fancy union: the DFA over-approximates match positions, and a hit
    // that no word claims (obligations, word boundaries) is a position
    // vim's engine would not have stopped at, so the scan continues. A
    // claimed hit that the parser rejects (syn requirement, \ze extent)
    // mirrors vim's empty parser result: get_delim fails without retry.
    // Current keeps the exact fancy union: its contains-filter depends on
    // true match extents.
    let mut line_hits: Vec<Hit> = Vec::new();
    let found: Option<(Hit, Delim)> = match opts.direction {
        Direction::Current => {
            let ure = match union.re.as_ref() {
                Some(r) => r,
                None => return None,
            };
            let line = ctx.lines.get1(cur.lnum)?;
            let mut cont: Vec<Hit> = Vec::new();
            let mut pos = 0usize;
            loop {
                let m = match ure.find_from_pos(line, pos) {
                    Ok(Some(m)) => m,
                    _ => break,
                };
                let (s, e) = (m.start(), m.end());
                let contains = if opts.insertmode {
                    s < raw0 && e > cur0
                } else {
                    s <= raw0 && e > cur0
                };
                if contains {
                    cont.push(Hit {
                        lnum: cur.lnum,
                        start0: s,
                        end0: e,
                    });
                }
                match next_pos(line, s) {
                    Some(p) => pos = p,
                    None => break,
                }
            }
            let mut out = None;
            for h in cont.iter().rev() {
                match classify(ctx, line, *h, opts, cur0) {
                    Classify::Claimed(d) => {
                        out = Some((*h, d));
                        break;
                    }
                    Classify::Rejected => return None,
                    Classify::Unclaimed => continue,
                }
            }
            out
        }
        Direction::Next => {
            let end_lnum = (cur.lnum + stopline).min(ctx.lines.max_lnum());
            let mut out = None;
            'outer: for lnum in cur.lnum..=end_lnum {
                let line = match ctx.lines.get1(lnum) {
                    Some(l) => l,
                    None => break,
                };
                let from0 = if lnum == cur.lnum { raw0 } else { 0 };
                union_line_hits(union, lnum, line, from0, &mut line_hits);
                for &h in line_hits.iter() {
                    // classify before the skip check: the DFA union
                    // over-approximates, and only positions a word actually
                    // claims correspond to matches vim's searchpos would
                    // stop at (and thus skip-check)
                    match classify(ctx, line, h, opts, cur0) {
                        Classify::Claimed(d) => {
                            if reject_by_skip(ctx, h, line, check_skip, true) {
                                continue;
                            }
                            out = Some((h, d));
                            break 'outer;
                        }
                        Classify::Rejected => {
                            if reject_by_skip(ctx, h, line, check_skip, true) {
                                continue;
                            }
                            return None;
                        }
                        Classify::Unclaimed => {}
                    }
                }
            }
            out
        }
        Direction::Prev => {
            let start_lnum = cur.lnum.saturating_sub(stopline).max(1);
            let mut out = None;
            'outer: for lnum in (start_lnum..=cur.lnum).rev() {
                let line = match ctx.lines.get1(lnum) {
                    Some(l) => l,
                    None => continue,
                };
                union_line_hits(union, lnum, line, 0, &mut line_hits);
                if lnum == cur.lnum {
                    line_hits.retain(|h| h.start0 <= raw0);
                }
                for h in line_hits.iter().rev().copied() {
                    match classify(ctx, line, h, opts, cur0) {
                        Classify::Claimed(d) => {
                            if reject_by_skip(ctx, h, line, check_skip, false) {
                                continue;
                            }
                            out = Some((h, d));
                            break 'outer;
                        }
                        Classify::Rejected => {
                            if reject_by_skip(ctx, h, line, check_skip, false) {
                                continue;
                            }
                            return None;
                        }
                        Classify::Unclaimed => continue,
                    }
                }
            }
            out
        }
    };

    ctx.state.perf.toc("s:get_delim", "first_pass");

    let (hit, delim) = found?;
    if ctx.state.perf.timeout_check() {
        return None;
    }
    let line = ctx.lines.get1(hit.lnum)?;

    // skip state recorded on the delim (delim.vim:486-496)
    let mut skip_state = false;
    if !check_skip && (ctx.synmaxcol == 0 || hit.cnum() as i64 <= ctx.synmaxcol) {
        skip_state = skip_at(
            ctx.state,
            &ctx.bc.skip,
            line,
            hit.lnum,
            hit.cnum(),
            ctx.syntax_on,
        );
    }

    ctx.state.perf.toc("s:get_delim", "got_results");
    Some(Delim {
        skip: skip_state,
        ..delim
    })
}

/// Port of s:get_delim_multi (delim.vim:43): merge the treesitter and
/// classic engine results. Current: treesitter wins when non-empty (the
/// original returns the first non-empty engine result, treesitter first);
/// next/prev: the position closest to the cursor in the scan direction.
pub fn get_delim_multi(ctx: &Ctx, opts: &GetDelimOpts) -> Option<Delim> {
    let ts = ctx.ts_lang.as_ref().and_then(|lang| {
        crate::treesitter::get_delim(ctx.state, ctx.gopts, &ctx.buf, ctx.buf.handle(), lang, opts)
    });
    match opts.direction {
        Direction::Current => ts.or_else(|| get_delim(ctx, opts)),
        direction => {
            let classic = get_delim(ctx, opts);
            match (ts, classic) {
                (Some(t), Some(c)) => {
                    let tv = Pos::new(t.lnum, t.cnum).val();
                    let cv = Pos::new(c.lnum, c.cnum).val();
                    let take_ts = if direction == Direction::Next {
                        tv <= cv
                    } else {
                        tv >= cv
                    };
                    Some(if take_ts { t } else { c })
                }
                (t, c) => t.or(c),
            }
        }
    }
}

/// Skip-based rejection during next/prev scans (delim.vim:443-457).
fn reject_by_skip(ctx: &Ctx, h: Hit, line: &str, check_skip: bool, forward: bool) -> bool {
    let noskips = ctx.gopts.delim_noskips;
    let should_check =
        check_skip || (noskips == 1 && char_not_punct(line, h.start0)) || noskips >= 2;
    if !should_check {
        return false;
    }
    if !skip_at(
        ctx.state,
        &ctx.bc.skip,
        line,
        h.lnum,
        h.cnum(),
        ctx.syntax_on,
    ) {
        return false;
    }
    // at buffer edges, accept anyway (delim.vim:448-449)
    let at_edge = if forward {
        h.lnum >= ctx.lines.total
            && h.cnum()
                >= ctx
                    .lines
                    .get1(ctx.lines.total)
                    .map(|l| l.len())
                    .unwrap_or(0)
    } else {
        h.lnum <= 1 && h.cnum() <= 1
    };
    !at_edge
}

/// Port of s:parser_delim_new (delim.vim:527): identify which (set, side,
/// word) matches at the hit and extract capture groups.
/// Result of classifying a first-pass hit.
enum Classify {
    /// A word claimed the position and the delim was built.
    Claimed(Delim),
    /// A word's anchored pattern matched (obligations pass) but a
    /// parser-level check failed (syn requirement, \ze extent): vim's
    /// parser returns {} here, so get_delim fails without retrying.
    Rejected,
    /// No word claimed the position: a false positive of the
    /// obligation-free union regex; vim's searchpos would have kept
    /// scanning, so the caller continues.
    Unclaimed,
}

fn classify(ctx: &Ctx, line: &str, hit: Hit, opts: &GetDelimOpts, cur0: usize) -> Classify {
    let dispatch = match ctx.bc.dispatch.get(&opts.side) {
        Some(d) => d,
        None => return Classify::Unclaimed,
    };
    // candidates whose pattern can start with this byte; hits at EOL
    // (empty matches) fall back to all words
    let candidates: &[crate::state::WordRef] = match line.as_bytes().get(hit.start0) {
        Some(b) => &dispatch.by_byte[*b as usize],
        None => &dispatch.all,
    };
    let prefix = &line[..hit.start0.min(line.len())];
    let mut any_claimed = false;
    for wref in candidates {
        let si = wref.set;
        let side = wref.side;
        let mid_id = wref.mid_id;
        let cset = &ctx.bc.sets[si];
        let lset = &ctx.bc.lists.sets[si];
        let cw = match cset.word(side, mid_id) {
            Some(w) => w,
            None => continue,
        };
        if cw.classify.is_none() {
            continue;
        }
        let extra_idx = match side {
            Side::Open => 0,
            Side::Mid => mid_id,
            Side::Close => lset.regextwo.extra_list.len().saturating_sub(1),
        };
        let extra = lset.regextwo.extra_list.get(extra_idx);
        let has_hlend = extra.map(|e| e.contains_key("hlend")).unwrap_or(false);
        let use_hlend = has_hlend && opts.highlighting;

        let (re, checks) = if use_hlend {
            (
                cw.hlend_classify.as_ref().or(cw.classify.as_ref()),
                if cw.hlend_classify.is_some() {
                    &cw.hlend_checks
                } else {
                    &cw.checks
                },
            )
        } else {
            (cw.classify.as_ref(), &cw.checks)
        };
        let re = match re {
            Some(r) => r,
            None => continue,
        };

        // cheap obligation pre-filter: a positive prefix check cannot match
        // a line prefix that lacks its leading literal
        let lits = if use_hlend {
            &cw.hlend_req_lits
        } else {
            &cw.req_lits
        };
        if !lits.iter().all(|l| prefix.contains(l.as_str())) {
            continue;
        }
        // cheap literal-prefix pre-filter: the anchored classify regex
        // would otherwise scan to the end of the line before failing
        if let Some((pref, ic)) = &cw.lit_prefix {
            let end = hit.start0 + pref.len();
            if end > line.len() || !line.is_char_boundary(end) {
                continue;
            }
            let seg = &line[hit.start0..end];
            let ok = if *ic {
                seg.eq_ignore_ascii_case(pref)
            } else {
                seg == pref.as_str()
            };
            if !ok {
                continue;
            }
        }

        let caps = match re.captures_from_pos(line, hit.start0) {
            Ok(Some(c)) => c,
            _ => continue,
        };
        let m0 = match caps.get(0) {
            Some(m) => m,
            None => continue,
        };
        if m0.start() != hit.start0 {
            continue;
        }
        if !checks_ok(checks, line, hit.start0) {
            continue;
        }
        // a word's anchored pattern matches here: this is a position
        // vim's searchpos would have stopped at
        any_claimed = true;
        // for current, reject matches the cursor is outside of
        // (matters for \ze; delim.vim:583-586)
        if !use_hlend && opts.direction == Direction::Current && m0.end() <= cur0 {
            continue;
        }
        // \g{syn;...} requirement (delim.vim:594-607)
        if let Some(e) = extra {
            if let Some(syn_arg) = e.get("syn") {
                let (pat, offs) = match syn_arg.split_once(';') {
                    Some((p, a)) => (p, a.parse::<usize>().unwrap_or(0)),
                    None => (syn_arg.as_str(), 0),
                };
                if !in_synstack(pat, hit.lnum, hit.cnum() + offs, &ctx.bc.word) {
                    continue;
                }
            }
        }

        // build the delim
        let match_text = line[m0.start()..m0.end()].to_string();
        let two = &lset.regextwo;
        let word_id = match side {
            Side::Open => 0,
            Side::Mid => mid_id,
            Side::Close => two.mid_list.len() + 1,
        };
        let mut groups: HashMap<u32, String> = HashMap::new();
        let mut augment_str = String::new();
        let mut augment_unresolved = Default::default();
        if side == Side::Open {
            for &br in &two.need_grp {
                if let Some(gm) = caps.get(br as usize) {
                    if !gm.as_str().is_empty() {
                        groups.insert(br, gm.as_str().to_string());
                    }
                }
            }
        } else {
            if let Some(renu) = two.grp_renu.get(&word_id) {
                for (&br, &to) in renu {
                    let txt = caps.get(br as usize).map(|m| m.as_str()).unwrap_or("");
                    groups.insert(to, txt.to_string());
                }
            }
            if let Some(aug) = two.aug_comp.get(&word_id).and_then(|v| v.first()) {
                augment_str = fill_backrefs_vim(&aug.str, &groups);
                augment_unresolved = aug.outputmap.clone();
            }
        }

        return Classify::Claimed(Delim {
            lnum: hit.lnum,
            cnum: hit.cnum(),
            match_: match_text,
            side,
            set: si,
            word_id,
            skip: false,
            groups,
            augment_str,
            augment_unresolved,
            highlighting: opts.highlighting,
            match_index: 0,
            ts_id: 0,
        });
    }
    if any_claimed {
        Classify::Rejected
    } else {
        Classify::Unclaimed
    }
}

// ---------------------------------------------------------------------------
// get_matching
// ---------------------------------------------------------------------------

fn sentinel() -> Vec<(String, usize, usize)> {
    vec![(String::new(), 0, 0)]
}

/// Determine which candidate pattern matches at (line, start0), trying
/// candidates in order. Returns the candidate index.
fn side_at(line: &str, start0: usize, cands: &[&CPat]) -> Option<usize> {
    for (i, c) in cands.iter().enumerate() {
        if let Ok(Some(m)) = c.re.find_from_pos(line, start0) {
            if m.start() == start0 && checks_ok(&c.checks, line, start0) {
                return Some(i);
            }
        }
    }
    None
}

/// Port of s:get_matching_delims (delim.vim:681). `delim` is mutated:
/// groups are enriched from the counterpart match (as vim mutates the
/// delim dict), which the mid -> up -> down flow depends on.
pub fn get_matching_raw(
    ctx: &Ctx,
    delim: &mut Delim,
    down: bool,
    stopline: usize,
) -> Vec<(String, usize, usize)> {
    ctx.state.perf.tic("get_matching_delims");

    let lset = match ctx.bc.lists.sets.get(delim.set) {
        Some(s) => s,
        None => return sentinel(),
    };
    let cset = &ctx.bc.sets[delim.set];

    // pattern selection (delim.vim:690-717)
    let (mut open_v, mut close_v) = (lset.regexone.open.clone(), lset.regexone.close.clone());
    if !down && !delim.augment_str.is_empty() {
        open_v = delim.augment_str.clone();
    }
    if down && delim.side == Side::Mid && !delim.augment_unresolved.is_empty() {
        open_v = lset.regextwo.open.clone();
        close_v = lset.regextwo.close.clone();
    }
    open_v = remove_capture_groups(&open_v);
    close_v = remove_capture_groups(&close_v);
    open_v = fill_backrefs_vim(&open_v, &delim.groups);
    close_v = fill_backrefs_vim(&close_v, &delim.groups);

    // midmap disambiguation (delim.vim:727-745)
    let mid_skip = compute_mid_skip(ctx, delim);
    let invert = delim.skip;

    if ctx.state.perf.timeout_check() {
        return sentinel();
    }

    let open_p = match ctx.compile_pat(&open_v) {
        Some(p) => p,
        None => return sentinel(),
    };
    let close_p = match ctx.compile_pat(&close_v) {
        Some(p) => p,
        None => return sentinel(),
    };
    let same = open_v == close_v;

    let skipfn = |lnum: usize, cnum: usize, line: &str| -> bool {
        let base = || skip_at(ctx.state, &ctx.bc.skip, line, lnum, cnum, ctx.syntax_on) != invert;
        match &mid_skip {
            Some(ms) => ms.eval(line, lnum, cnum, base),
            None => base(),
        }
    };

    // ---- phase 1: find the counterpart ----
    let seed0 = delim.cnum.saturating_sub(1);
    let comb1_fast = match (&open_p.fast_main, &close_p.fast_main) {
        (Some(o), Some(c)) => ctx.state.compile_fast(&format!("(?:{o})|(?:{c})")),
        _ => None,
    };
    // the fancy comb is only needed when the DFA variant is unavailable;
    // compiling it is expensive, so build it lazily and cache by pattern
    let comb1_fancy = if comb1_fast.is_none() {
        let combined1 = format!("(?:{})|(?:{})", open_p.main, close_p.main);
        match ctx.state.compile_fancy(&combined1) {
            Some(r) => Some(r),
            None => return sentinel(),
        }
    } else {
        None
    };
    let scan1 = match (&comb1_fast, &comb1_fancy) {
        (Some(f), _) => ScanRe::Fast(f),
        (None, Some(r)) => ScanRe::Fancy(r),
        (None, None) => return sentinel(),
    };
    let corr: Option<Hit> = if same {
        // 'same' matches (delim.vim:763): plain next/prev occurrence,
        // no skip evaluation, no depth counting.
        let same_fast = open_p
            .fast_main
            .as_deref()
            .and_then(|fm| ctx.state.compile_fast(fm));
        let sr = match &same_fast {
            Some(f) => ScanRe::Fast(f),
            None => ScanRe::Fancy(&open_p.re),
        };
        scan_same(ctx, &sr, delim.lnum, seed0, down, stopline)
    } else {
        scan_first(
            ctx,
            &scan1,
            &[&open_p, &close_p],
            delim.lnum,
            seed0,
            down,
            stopline,
            &skipfn,
        )
    };

    ctx.state.perf.toc("get_matching_delims", "initial_pair");

    let corr = match corr {
        Some(h) => h,
        None => return sentinel(),
    };

    // ---- re-match the counterpart for text + groups (delim.vim:778-807)
    let extra_list = &lset.regextwo.extra_list;
    let hlend_side_idx = if down {
        extra_list.len().saturating_sub(1)
    } else {
        0
    };
    let word = if down { &cset.close } else { &cset.open };
    let use_hlend = word.has_hlend
        && delim.highlighting
        && extra_list
            .get(hlend_side_idx)
            .map(|e| e.contains_key("hlend"))
            .unwrap_or(false);
    let (re2, checks2) = if use_hlend && word.hlend_classify.is_some() {
        (
            word.hlend_classify.as_ref().unwrap(),
            word.hlend_checks.as_slice(),
        )
    } else if word.classify.is_some() {
        (word.classify.as_ref().unwrap(), word.checks.as_slice())
    } else {
        return sentinel();
    };

    let corr_line = match ctx.lines.get1(corr.lnum) {
        Some(l) => l,
        None => return sentinel(),
    };
    let mut match_corr = corr_line
        .get(corr.start0..corr.end0)
        .unwrap_or("")
        .to_string();
    if let Ok(Some(caps)) = re2.captures_from_pos(corr_line, corr.start0) {
        if let Some(m0) = caps.get(0) {
            if m0.start() == corr.start0 && checks_ok(checks2, corr_line, corr.start0) {
                match_corr = m0.as_str().to_string();
                if down {
                    let id = lset.regextwo.mid_list.len() + 1;
                    if let Some(renu) = lset.regextwo.grp_renu.get(&id) {
                        for (&from, &to) in renu {
                            if !delim.groups.contains_key(&to) {
                                if let Some(gm) = caps.get(from as usize) {
                                    if !gm.as_str().is_empty() {
                                        delim.groups.insert(to, gm.as_str().to_string());
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for &to in &lset.regextwo.need_grp {
                        if !delim.groups.contains_key(&to) {
                            if let Some(gm) = caps.get(to as usize) {
                                if !gm.as_str().is_empty() {
                                    delim.groups.insert(to, gm.as_str().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    ctx.state.perf.toc("get_matching_delims", "get_matches");

    // ---- phase 2: collect mids between seed and counterpart
    // (delim.vim:811-864)
    let mut list: Vec<(String, usize, usize)> = Vec::new();
    let mids_vim = &lset.regexone.mid;
    if !mids_vim.is_empty() && !ctx.gopts.delim_nomids && !same {
        let mut mids_filled = fill_backrefs_vim(&remove_capture_groups(mids_vim), &delim.groups);
        if lset.regextwo.extra_info.mid_hlend && delim.highlighting {
            mids_filled = process_hlend(&mids_filled, -1);
        }
        if let Some(mids_p) = ctx.compile_pat(&mids_filled) {
            let comb2_fast = match (&open_p.fast_main, &mids_p.fast_main, &close_p.fast_main) {
                (Some(o), Some(mi), Some(c)) => {
                    ctx.state.compile_fast(&format!("(?:{o})|(?:{mi})|(?:{c})"))
                }
                _ => None,
            };
            let comb2_fancy = if comb2_fast.is_none() {
                let combined2 = format!(
                    "(?:{})|(?:{})|(?:{})",
                    open_p.main, mids_p.main, close_p.main
                );
                ctx.state.compile_fancy(&combined2)
            } else {
                None
            };
            let scan2 = match (&comb2_fast, &comb2_fancy) {
                (Some(f), _) => Some(ScanRe::Fast(f)),
                (None, Some(r)) => Some(ScanRe::Fancy(r)),
                _ => None,
            };
            if let Some(scan2) = scan2 {
                let mids = scan_mids(
                    ctx,
                    &scan2,
                    &[&open_p, &mids_p, &close_p],
                    delim.lnum,
                    seed0,
                    corr,
                    down,
                    &skipfn,
                );
                list.extend(mids);
            }
        }
    }

    list.push((match_corr, corr.lnum, corr.cnum()));
    if !down {
        list.reverse();
    }

    ctx.state.perf.toc("get_matching_delims", "mids");
    list
}

/// 'Same' open/close patterns (e.g. quotes): the counterpart is simply
/// the next/previous occurrence (delim.vim:763-765).
fn scan_same(
    ctx: &Ctx,
    re: &ScanRe,
    seed_lnum: usize,
    seed0: usize,
    down: bool,
    stopline: usize,
) -> Option<Hit> {
    if down {
        let lmax = (seed_lnum + stopline).min(ctx.lines.max_lnum());
        for lnum in seed_lnum..=lmax {
            if ctx.state.perf.timeout_check() {
                return None;
            }
            let line = match ctx.lines.get1(lnum) {
                Some(l) => l,
                None => break,
            };
            let pos = if lnum == seed_lnum {
                bound_up(line, seed0 + 1)
            } else {
                0
            };
            if let Some((s0, e0)) = re.find_at(line, pos) {
                return Some(Hit {
                    lnum,
                    start0: s0,
                    end0: e0,
                });
            }
        }
    } else {
        let lmin = seed_lnum.saturating_sub(stopline).max(1);
        for lnum in (lmin..=seed_lnum).rev() {
            if ctx.state.perf.timeout_check() {
                return None;
            }
            let line = match ctx.lines.get1(lnum) {
                Some(l) => l,
                None => continue,
            };
            let hits = enum_line(re, line, 0);
            for h in hits.iter().rev() {
                if lnum == seed_lnum && h.start0 >= seed0 {
                    continue;
                }
                return Some(Hit { lnum, ..*h });
            }
        }
    }
    None
}

/// Depth-counting scan for the counterpart (replaces searchpairpos).
/// `cands` gives side determination: index 0 = open, 1 = close.
fn scan_first<F>(
    ctx: &Ctx,
    comb: &ScanRe,
    cands: &[&CPat],
    seed_lnum: usize,
    seed0: usize,
    down: bool,
    stopline: usize,
    skipfn: &F,
) -> Option<Hit>
where
    F: Fn(usize, usize, &str) -> bool,
{
    let mut depth: i32 = 0;
    if down {
        let lmax = (seed_lnum + stopline).min(ctx.lines.max_lnum());
        for lnum in seed_lnum..=lmax {
            if ctx.state.perf.timeout_check() {
                return None;
            }
            let line = match ctx.lines.get1(lnum) {
                Some(l) => l,
                None => break,
            };
            let mut pos = if lnum == seed_lnum {
                bound_up(line, seed0 + 1)
            } else {
                0
            };
            loop {
                let (s, e) = match comb.find_at(line, pos) {
                    Some(x) => x,
                    None => break,
                };
                if skipfn(lnum, s + 1, line) {
                    match next_pos(line, s) {
                        Some(p) => pos = p,
                        None => break,
                    }
                    continue;
                }
                match side_at(line, s, cands) {
                    Some(0) => depth += 1,
                    Some(1) => {
                        if depth == 0 {
                            return Some(Hit {
                                lnum,
                                start0: s,
                                end0: e,
                            });
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
                match next_pos(line, s) {
                    Some(p) => pos = p,
                    None => break,
                }
            }
        }
    } else {
        let lmin = seed_lnum.saturating_sub(stopline).max(1);
        for lnum in (lmin..=seed_lnum).rev() {
            if ctx.state.perf.timeout_check() {
                return None;
            }
            let line = match ctx.lines.get1(lnum) {
                Some(l) => l,
                None => continue,
            };
            let hits = enum_line(comb, line, 0);
            for h in hits.iter().rev() {
                if lnum == seed_lnum && h.start0 >= seed0 {
                    // 'bW' without 'c': match must start before the seed
                    continue;
                }
                if skipfn(lnum, h.start0 + 1, line) {
                    continue;
                }
                match side_at(line, h.start0, cands) {
                    Some(1) => depth += 1, // close
                    Some(0) => {
                        if depth == 0 {
                            return Some(Hit { lnum, ..*h });
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

/// Phase-2 scan collecting mids at depth 0 between seed and counterpart.
/// cands: [open, mids, close].
fn scan_mids<F>(
    ctx: &Ctx,
    comb: &ScanRe,
    cands: &[&CPat],
    seed_lnum: usize,
    seed0: usize,
    corr: Hit,
    down: bool,
    skipfn: &F,
) -> Vec<(String, usize, usize)>
where
    F: Fn(usize, usize, &str) -> bool,
{
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut depth: i32 = 0;
    let past_corr = |lnum: usize, s0: usize| -> bool {
        if down {
            lnum > corr.lnum || (lnum == corr.lnum && s0 + 1 >= corr.cnum())
        } else {
            lnum < corr.lnum || (lnum == corr.lnum && s0 + 1 <= corr.cnum())
        }
    };
    if down {
        let lmax = corr.lnum.min(ctx.lines.max_lnum());
        for lnum in seed_lnum..=lmax {
            if ctx.state.perf.timeout_check() {
                break;
            }
            let line = match ctx.lines.get1(lnum) {
                Some(l) => l,
                None => break,
            };
            let mut pos = if lnum == seed_lnum {
                bound_up(line, seed0 + 1)
            } else {
                0
            };
            loop {
                let (s, e) = match comb.find_at(line, pos) {
                    Some(x) => x,
                    None => break,
                };
                if past_corr(lnum, s) {
                    return out;
                }
                if skipfn(lnum, s + 1, line) {
                    match next_pos(line, s) {
                        Some(p) => pos = p,
                        None => break,
                    }
                    continue;
                }
                match side_at(line, s, cands) {
                    Some(0) => depth += 1,
                    Some(1) => {
                        if depth == 0 {
                            // mid: extent from the mids pattern itself
                            let extent = cands[1]
                                .re
                                .find_from_pos(line, s)
                                .ok()
                                .flatten()
                                .filter(|mm| mm.start() == s)
                                .map(|mm| line[mm.start()..mm.end()].to_string())
                                .unwrap_or_else(|| line[s..e].to_string());
                            out.push((extent, lnum, s + 1));
                        }
                    }
                    Some(2) => {
                        if depth > 0 {
                            depth -= 1;
                        }
                    }
                    _ => {}
                }
                match next_pos(line, s) {
                    Some(p) => pos = p,
                    None => break,
                }
            }
        }
    } else {
        let lmin = corr.lnum.max(1);
        for lnum in (lmin..=seed_lnum).rev() {
            if ctx.state.perf.timeout_check() {
                break;
            }
            let line = match ctx.lines.get1(lnum) {
                Some(l) => l,
                None => continue,
            };
            let hits = enum_line(comb, line, 0);
            for h in hits.iter().rev() {
                if lnum == seed_lnum && h.start0 + 1 >= seed0 + 1 {
                    continue;
                }
                if past_corr(lnum, h.start0) {
                    return out;
                }
                if skipfn(lnum, h.start0 + 1, line) {
                    continue;
                }
                match side_at(line, h.start0, cands) {
                    Some(2) => depth += 1, // close
                    Some(1) => {
                        if depth == 0 {
                            let extent = cands[1]
                                .re
                                .find_from_pos(line, h.start0)
                                .ok()
                                .flatten()
                                .filter(|mm| mm.start() == h.start0)
                                .map(|mm| line[mm.start()..mm.end()].to_string())
                                .unwrap_or_else(|| line[h.start0..h.end0].to_string());
                            out.push((extent, lnum, h.start0 + 1));
                        }
                    }
                    Some(0) => {
                        if depth > 0 {
                            depth -= 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// Enumerate union hits on one line for Next/Prev scans: the DFA union
/// (over-approximate positions, exact starts) plus the exotic fancy union,
/// merged and deduplicated by start position.
fn union_line_hits(union: &Union, lnum: usize, line: &str, from0: usize, out: &mut Vec<Hit>) {
    out.clear();
    if let Some(fast) = union.fast.as_ref() {
        let mut pos = bound_up(line, from0);
        loop {
            let m = match fast.find_at(line, pos) {
                Some(m) => m,
                None => break,
            };
            out.push(Hit {
                lnum,
                start0: m.start(),
                end0: m.end(),
            });
            match next_pos(line, m.start()) {
                Some(p) => pos = p,
                None => break,
            }
        }
    }
    if let Some(ex) = union.exotic.as_ref() {
        let mut pos = bound_up(line, from0);
        loop {
            let m = match ex.find_from_pos(line, pos) {
                Ok(Some(m)) => m,
                _ => break,
            };
            out.push(Hit {
                lnum,
                start0: m.start(),
                end0: m.end(),
            });
            match next_pos(line, m.start()) {
                Some(p) => pos = p,
                None => break,
            }
        }
    }
    out.sort_by_key(|h| h.start0);
    out.dedup_by_key(|h| h.start0);
}

fn enum_line(re: &ScanRe, line: &str, from0: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut pos = bound_up(line, from0);
    loop {
        let (s0, e0) = match re.find_at(line, pos) {
            Some(x) => x,
            None => break,
        };
        hits.push(Hit {
            lnum: 0,
            start0: s0,
            end0: e0,
        });
        match next_pos(line, s0) {
            Some(p) => pos = p,
            None => break,
        }
    }
    hits
}

fn compute_mid_skip(ctx: &Ctx, delim: &Delim) -> Option<MidSkip> {
    let mm = ctx.bc.lists.midmap.as_ref()?;
    let unanchored = |pat: &str, text: &str| -> bool {
        ctx.state
            .compile(pat, &ctx.translate_opts(false))
            .map(|re| re.is_match(text).unwrap_or(false))
            .unwrap_or(false)
    };
    let chosen = if delim.side == Side::Mid {
        mm.elements
            .iter()
            .find(|(_, w)| unanchored(w, &delim.match_))
    } else {
        let syn = crate::skip::syn_name(delim.lnum, delim.cnum, false);
        mm.elements.iter().find(|(s, _)| unanchored(s, &syn))
    };
    match chosen {
        Some((s, w)) => MidSkip::compile_skip1(s, w, &ctx.bc.word),
        None => MidSkip::compile_skip2(&mm.strike, &ctx.bc.word),
    }
}

pub struct MatchOpts {
    pub stopline: usize,
    pub highlighting: bool,
}

/// Port of matchup#delim#get_matching (delim.vim:64): builds the full
/// matching list with circular links.
pub fn get_matching(ctx: &Ctx, seed: &Delim, opts: &MatchOpts) -> MatchingList {
    if seed.lnum == 0 {
        return MatchingList::default();
    }
    let stopline = if opts.stopline > 0 {
        opts.stopline
    } else {
        ctx.gopts.delim_stopline
    };

    let mut work = seed.clone();
    let mut matches: Vec<Option<(String, usize, usize)>> = Vec::new();
    let downs: &[bool] = match seed.side {
        Side::Open => &[true],
        Side::Close => &[false],
        Side::Mid => &[false, true],
    };
    for &down in downs {
        if !matches.is_empty() {
            matches.push(None);
        }
        let res = if seed.ts_id != 0 {
            match &ctx.ts_lang {
                Some(lang) => crate::treesitter::get_matching(
                    ctx.state,
                    ctx.gopts,
                    &ctx.buf,
                    ctx.buf.handle(),
                    lang,
                    seed.ts_id,
                    down,
                ),
                None => Vec::new(),
            }
        } else {
            get_matching_raw(ctx, &mut work, down, stopline)
        };
        if res.is_empty() {
            continue;
        }
        if res[0].1 > 0 {
            matches.extend(res.into_iter().map(Some));
        } else if down {
            matches.clear();
        }
    }
    match seed.side {
        Side::Open => matches.insert(0, None),
        Side::Close => matches.push(None),
        Side::Mid => {}
    }

    let len = matches.len();
    if len == 0 {
        return MatchingList::default();
    }
    let mut delims: Vec<Delim> = Vec::with_capacity(len);
    for (i, entry) in matches.iter().enumerate() {
        match entry {
            None => {
                let mut d = seed.clone();
                d.match_index = i;
                delims.push(d);
            }
            Some((m, l, c)) => {
                let mut d = seed.clone();
                d.lnum = *l;
                d.cnum = *c;
                d.match_ = m.clone();
                d.side = if i == 0 {
                    Side::Open
                } else if i == len - 1 {
                    Side::Close
                } else {
                    Side::Mid
                };
                d.word_id = MID_SENTINEL;
                d.match_index = i;
                delims.push(d);
            }
        }
    }

    let mut next = vec![0usize; len];
    let mut prev = vec![0usize; len];
    for i in 0..len {
        next[i] = (i + 1) % len;
        prev[i] = if i == 0 { len - 1 } else { i - 1 };
    }

    // allow empty marker ending (delim.vim:138-144)
    if len >= 2 && delims[len - 1].match_.is_empty() {
        if seed.highlighting && len <= 2 {
            return MatchingList::default();
        }
        prev[0] = len - 2;
        next[len - 2] = 0;
    }

    MatchingList { delims, next, prev }
}

// ---------------------------------------------------------------------------
// get_surrounding
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SurroundOpts {
    pub local: Option<bool>,
    pub stopline: usize,
    pub check_skip: bool,
    pub highlighting: bool,
}

impl Default for SurroundOpts {
    fn default() -> Self {
        SurroundOpts {
            local: None,
            stopline: 0,
            check_skip: false,
            highlighting: false,
        }
    }
}

/// Port of matchup#delim#get_surrounding_impl (delim.vim:151).
/// Returns (open, close, matching list of the accepted pair).
pub fn get_surrounding(
    ctx: &Ctx,
    count: usize,
    opts: &SurroundOpts,
) -> Option<(Delim, Delim, MatchingList)> {
    ctx.state.perf.tic("delim#get_surrounding");
    let cursor = ctx.cursor()?;
    let pos_val_cursor = cursor.val();
    let mut pos_val_last = pos_val_cursor;
    let mut pos_val_open = pos_val_cursor - 1;

    let mut counter = count as i64;
    let local = opts.local.unwrap_or(count == 0);
    let stopline = if opts.stopline > 0 {
        opts.stopline
    } else {
        ctx.gopts.delim_stopline
    };

    let cursor_skip = {
        let line = ctx.lines.get1(cursor.lnum).unwrap_or("");
        skip_at(
            ctx.state,
            &ctx.bc.skip,
            line,
            cursor.lnum,
            cursor.cnum,
            ctx.syntax_on,
        )
    };
    let check_skip = if opts.check_skip {
        Some(true)
    } else if cursor_skip {
        Some(false)
    } else {
        None
    };

    let mut best: Option<(Delim, Delim)> = None;
    let mut walk = cursor;
    let mode = ctx.mode_char();
    let tick = ctx.buf.get_changedtick().unwrap_or(0);
    let bufh = ctx.buf.handle();

    // memo validity
    {
        let mut memo = ctx.state.surround_memo.borrow_mut();
        let entry = memo.entry(bufh).or_insert_with(|| (tick, HashMap::new()));
        if entry.0 != tick {
            entry.0 = tick;
            entry.1.clear();
        }
    }

    let mut result: Option<(Delim, Delim, MatchingList)> = None;
    while pos_val_open < pos_val_last {
        let key = crate::state::MemoKey {
            lnum: walk.lnum,
            cnum: walk.cnum,
            mode,
        };
        // the treesitter engine's delim-info cache is a small LRU keyed by
        // uuid; a memoized TS delim can outlive its cache entry, so the
        // memo is only used for classic-engine walks (the original's memo
        // keys include curswant and rarely hit across calls)
        let use_memo = ctx.ts_lang.is_none();
        let cached = if use_memo {
            ctx.state
                .surround_memo
                .borrow()
                .get(&bufh)
                .and_then(|(_, m)| m.get(&key))
                .cloned()
        } else {
            None
        };
        let open_opt = match cached {
            Some(v) => v,
            None => {
                let mut o = GetDelimOpts::new(
                    Direction::Prev,
                    if local {
                        SideQuery::OpenMid
                    } else {
                        SideQuery::Open
                    },
                );
                o.check_skip = check_skip;
                o.stopline = stopline;
                o.at = Some(walk);
                let d = get_delim_multi(ctx, &o);
                if use_memo {
                    ctx.state
                        .surround_memo
                        .borrow_mut()
                        .entry(bufh)
                        .or_insert_with(|| (tick, HashMap::new()))
                        .1
                        .insert(key, d.clone());
                }
                d
            }
        };
        let open = match open_opt {
            Some(o) => o,
            None => break,
        };

        if ctx.state.perf.timeout_check() && !ctx.gopts.delim_count_fail {
            break;
        }

        let ml = get_matching(
            ctx,
            &open,
            &MatchOpts {
                stopline,
                highlighting: opts.highlighting,
            },
        );
        let ml = if ml.len() == 1 {
            MatchingList::default()
        } else {
            ml
        };

        if !ml.is_empty() {
            let seed_idx = ml
                .delims
                .iter()
                .position(|d| d.word_id != MID_SENTINEL)
                .unwrap_or(0);
            let close_idx = if local {
                ml.next_of(seed_idx)
            } else {
                ml.len() - 1
            };
            let close = &ml.delims[close_idx];
            let pos_val_try = close.pos().val() + close.end_offset() as i64;
            if pos_val_try >= pos_val_cursor {
                if counter <= 1 {
                    result = Some((ml.delims[seed_idx].clone(), close.clone(), ml));
                    break;
                }
                counter -= 1;
                best = Some((open.clone(), close.clone()));
            } else {
                pos_val_last = pos_val_open;
                pos_val_open = open.pos().val();
            }
        } else {
            pos_val_last = pos_val_open;
            pos_val_open = open.pos().val();
        }

        if open.lnum == 1 && open.cnum == 1 {
            break;
        }
        let open_line = ctx.lines.get1(open.lnum).unwrap_or("");
        let prev_line = ctx.lines.get1(open.lnum.saturating_sub(1)).unwrap_or("");
        walk = pos_prev(open_line, prev_line, open.pos());
        if ctx.state.perf.timeout_check() && !ctx.gopts.delim_count_fail {
            break;
        }
    }

    if let Some(r) = result {
        ctx.state.perf.toc("delim#get_surrounding", "accept");
        return Some(r);
    }
    if let Some((o, c)) = best {
        if ctx.gopts.delim_count_fail {
            ctx.state.perf.toc("delim#get_surrounding", "bad_count");
            return Some((o, c, MatchingList::default()));
        }
    }
    ctx.state.perf.toc("delim#get_surrounding", "fail");
    None
}

/// Port of matchup#delim#get_surround_nearest (delim.vim:259): finds the
/// consecutive pair of matching-list entries surrounding `cur`.
pub fn get_surround_nearest(
    ml: &MatchingList,
    open_idx: usize,
    cur: Pos,
) -> Option<(usize, usize)> {
    let pos_val_open = ml.delims[open_idx].pos().val();
    let mut pos_val_prev = pos_val_open;
    let mut i = ml.next_of(open_idx);
    let mut pos_val_next = ml.delims[i].pos().val();
    let cur_val = cur.val();
    while pos_val_next > pos_val_open {
        let end_offset = ml.delims[i].end_offset() as i64;
        if pos_val_prev <= cur_val && pos_val_next + end_offset >= cur_val {
            return Some((ml.prev_of(i), i));
        }
        pos_val_prev = pos_val_next;
        i = ml.next_of(i);
        pos_val_next = ml.delims[i].pos().val();
    }
    None
}

/// Port of matchup#delim#jump_target (delim.vim:285): the column of the
/// last character of the delim, probing for overlapping delims.
pub fn jump_target(ctx: &Ctx, delim: &Delim) -> usize {
    let mut column = delim.cnum as isize + delim.match_.len() as isize - 1;
    if delim.match_.len() < 2 {
        return column.max(1) as usize;
    }
    for _ in 0..delim.match_.len() - 1 {
        let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
        o.at = Some(Pos::new(delim.lnum, column.max(1) as usize));
        match get_delim_multi(ctx, &o) {
            None => break,
            Some(t) if t.set == delim.set => break,
            Some(_) => column -= 1,
        }
    }
    column.max(1) as usize
}
