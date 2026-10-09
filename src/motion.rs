//! Motions: %, g%, [%, ]%, z%, Z%, insert-mode <c-g>%.
//! Port of autoload/matchup/motion.vim.

use std::rc::Rc;

use nvim_oxi::api::{self, opts::SetKeymapOpts, types::Mode};
use nvim_oxi::{Array, Object};

use crate::engine::{self, Ctx, Direction, GetDelimOpts, MatchOpts, SurroundOpts};
use crate::state::State;
use crate::types::{pos_next, pos_prev, Delim, MatchingList, Pos};
use crate::words::SideQuery;

type SharedState = Rc<State>;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn normal(cmd: &str) {
    let _ = api::command(&format!("normal! {cmd}"));
}

fn set_cursor(win: &mut nvim_oxi::api::Window, ctx: &Ctx, p: Pos) {
    let mut cnum = p.cnum;
    if let Some(line) = ctx.lines.get1(p.lnum) {
        let len = line.len();
        if cnum > len + 1 {
            cnum = len + 1;
        }
        // clamp to a char boundary
        let mut c0 = cnum - 1;
        while c0 > 0 && !line.is_char_boundary(c0) {
            c0 -= 1;
        }
        cnum = c0 + 1;
    }
    // nvim_win_set_cursor: line is 1-based, col is 0-based
    let r = win.set_cursor(p.lnum, cnum.saturating_sub(1));
    crate::matchparen::trace(&format!(
        "set_cursor({},{}) -> {:?} now {:?}",
        p.lnum,
        cnum,
        r.as_ref().err().map(|e| format!("{e:?}")),
        win.get_cursor()
    ));
}

/// Port of matchup#motion_force (matchup.vim:167).
fn motion_force() -> String {
    let mode: String =
        crate::nvimrs::call_fn_as("mode", &Array::from_iter([Object::from(1i64)]))
            .unwrap_or_default();
    if mode.len() >= 3 && mode.starts_with("no") {
        mode[2..3].to_string()
    } else {
        String::new()
    }
}

#[derive(Debug)]
#[allow(dead_code)] // batched in one eval for port fidelity
struct Vars {
    count: i64,
    count1: i64,
    operator: String,
    register: String,
    selection: String,
    visualmode: String,
    foldopen: String,
    startofline: bool,
}

fn read_vars(ctx: &Ctx) -> Vars {
    use crate::nvimrs::{call_fn0_as, get_option_as, get_vvar_as};
    let count = get_vvar_as::<i64>("count").unwrap_or(0);
    let count1 = get_vvar_as::<i64>("count1").unwrap_or(0).max(1);
    let operator_v = get_vvar_as::<String>("operator").unwrap_or_default();
    let register = get_vvar_as::<String>("register").unwrap_or_default();
    let selection = get_option_as::<String>("selection", 0, 0).unwrap_or_default();
    let visualmode = call_fn0_as::<String>("visualmode").unwrap_or_default();
    let foldopen = get_option_as::<String>("foldopen", 0, 0).unwrap_or_default();
    let startofline = get_option_as::<bool>("startofline", 0, 0).unwrap_or(false);
    // during the op() re-feed, v:operator may be cleared; use the stash
    let stashed = ctx.state.op_operator.borrow().clone();
    let operator = if stashed.is_empty() { operator_v } else { stashed };
    Vars {
        count,
        count1,
        operator,
        register,
        selection,
        visualmode,
        foldopen,
        startofline,
    }
}

/// Ensure visual mode is active for a visual mapping callback (the
/// original re-enters with `normal! gv` after `:<c-u>`).
fn ensure_visual() {
    let m: String = crate::nvimrs::call_fn0_as("mode").unwrap_or_default();
    if !m.starts_with('v') && !m.starts_with('V') && !m.contains('\x16') && !m.starts_with("^V") {
        normal("gv");
    }
}

fn in_indentexpr() -> bool {
    crate::nvimrs::call_fn0_as::<i64>("matchup#rs#in_indentexpr").unwrap_or(0) != 0
}

/// Port of matchup#pos#next_eol (pos.vim:49).
fn pos_next_eol(ctx: &Ctx, p: Pos) -> Pos {
    let line = ctx.lines.get1(p.lnum).unwrap_or("");
    if p.cnum > line.len() {
        return Pos::new(p.lnum + 1, 1);
    }
    let next = pos_next(line, p);
    if next.lnum > p.lnum {
        Pos::new(p.lnum, p.cnum + 1)
    } else {
        next
    }
}

/// Port of matchup#pos#prev_eol (pos.vim:77).
fn pos_prev_eol(ctx: &Ctx, p: Pos) -> Pos {
    if p.cnum <= 1 && p.lnum > 1 {
        let prev = ctx.lines.get1(p.lnum - 1).unwrap_or("");
        Pos::new(p.lnum - 1, prev.len() + 1)
    } else {
        let line = ctx.lines.get1(p.lnum).unwrap_or("");
        let prev_line = ctx.lines.get1(p.lnum.saturating_sub(1)).unwrap_or("");
        pos_prev(line, prev_line, p)
    }
}

fn in_whitespace(ctx: &Ctx, p: Pos) -> bool {
    let line = ctx.lines.get1(p.lnum).unwrap_or("");
    match line.as_bytes().get(p.cnum.saturating_sub(1)) {
        Some(b) => (*b as char).is_whitespace(),
        None => false,
    }
}

fn in_indent(ctx: &Ctx, p: Pos) -> bool {
    if p.cnum == 0 {
        return false;
    }
    let line = ctx.lines.get1(p.lnum).unwrap_or("");
    let end = (p.cnum).min(line.len());
    let mut e = end;
    while e > 0 && !line.is_char_boundary(e) {
        e -= 1;
    }
    line[..e].chars().all(|c| c.is_whitespace())
}

fn seed_index(ml: &MatchingList) -> usize {
    ml.delims
        .iter()
        .position(|d| d.word_id != crate::types::MID_SENTINEL)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// % and g%
// ---------------------------------------------------------------------------

/// Port of matchup#motion#find_matching_pair (motion.vim:24).
pub fn find_matching_pair(ctx: &Ctx, visual: bool, down: bool) -> bool {
    use crate::matchparen::trace;
    let vars = read_vars(ctx);
    trace(&format!("FMP start visual={visual} down={down} count={} count1={} op={vars:?} cursor={:?}", vars.count, vars.count1, ctx.cursor()));
    let force = motion_force();
    let is_oper = !vars.operator.is_empty();

    if visual && !is_oper {
        ensure_visual();
    }

    if down
        && ctx.gopts.motion_override_npercent < 100
        && vars.count > ctx.gopts.motion_override_npercent
    {
        if visual && is_oper {
            normal("V");
        }
        let _ = api::command(&format!("normal! {}%", vars.count));
        return true;
    }

    if in_indentexpr() {
        ctx.state.perf.timeout_start(300.0);
        let cur: i64 = crate::nvimrs::call_fn_as("col", &Array::from_iter([Object::from(".")]))
            .unwrap_or(0);
        let end: i64 = crate::nvimrs::call_fn_as("col", &Array::from_iter([Object::from("$")]))
            .unwrap_or(0);
        let col: i64 = if cur >= end { 1 } else { 0 };
        if !vars.startofline && col != 0 {
            normal("^");
        }
    } else {
        ctx.state.perf.timeout_start(1000.0);
    }

    let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
    let mut delim = match engine::get_delim_multi(ctx, &o) {
        Some(d) => d,
        None => {
            trace("FMP current empty, trying next");
            o = GetDelimOpts::new(Direction::Next, SideQuery::BothAll);
            match engine::get_delim_multi(ctx, &o) {
                Some(d) => d,
                None => { trace("FMP no delim at all"); return false; }
            }
        }
    };
    trace(&format!("FMP delim {} {} {:?} side={:?}", delim.lnum, delim.cnum, delim.match_, delim.side));

    let ml = engine::get_matching(
        ctx,
        &delim,
        &MatchOpts {
            stopline: 0,
            highlighting: false,
        },
    );
    let min_len = if delim.side == crate::words::Side::Mid {
        3
    } else {
        2
    };
    trace(&format!("FMP ml len {}", ml.len()));
    if ml.len() < min_len {
        trace("FMP ml too short");
        return false;
    }

    // walk links count1 times
    let mut idx = seed_index(&ml);
    trace(&format!("FMP seed_idx {idx}"));
    delim.match_index = ml.delims[idx].match_index;
    for _ in 0..vars.count1 {
        idx = if down { ml.next_of(idx) } else { ml.prev_of(idx) };
    }
    let target: Delim = ml.delims[idx].clone();
    trace(&format!("FMP target {} {} {:?} side={:?} idx={idx}", target.lnum, target.cnum, target.match_, target.side));

    if visual && is_oper {
        ensure_visual();
    }

    let exclusive = is_oper && force == "v";
    let forward = (down && target.side != crate::words::Side::Open)
        || target.side == crate::words::Side::Close;

    // go to the end of the delimiter, if necessary
    let mut column = target.cnum;
    if ctx.gopts.motion_cursor_end && !is_oper && forward {
        column = engine::jump_target(ctx, &target);
    }

    let start_pos = match ctx.cursor() {
        Some(p) => p,
        None => return false,
    };

    if !ctx.gopts.motion_keepjumps {
        normal("m`");
    }

    // column position of last character in match
    let eom = target.cnum + target.end_offset();

    if is_oper && forward {
        column = if exclusive {
            column.saturating_sub(1)
        } else {
            eom
        };
    }

    let mut win = ctx.win.clone();

    if is_oper && exclusive && target.pos().smaller(&start_pos) {
        normal("o");
        let pl = ctx.lines.get1(start_pos.lnum).unwrap_or("");
        let ppl = ctx
            .lines
            .get1(start_pos.lnum.saturating_sub(1))
            .unwrap_or("");
        let p = pos_prev(pl, ppl, start_pos);
        set_cursor(&mut win, ctx, p);
        normal("o");
    }

    // special handling for d% (motion.vim:107-130)
    if vars.operator == "d" && start_pos.lnum != target.lnum && force.is_empty() {
        let (tl, br, swap) = if start_pos.lnum <= target.lnum {
            ((start_pos.lnum, start_pos.cnum), (target.lnum, eom), false)
        } else {
            ((target.lnum, target.cnum), (start_pos.lnum, start_pos.cnum), true)
        };
        let tl_line = ctx.lines.get1(tl.0).unwrap_or("");
        let br_line = ctx.lines.get1(br.0).unwrap_or("");
        let tl_ok = {
            let end = (tl.1 - 1).min(tl_line.len());
            tl_line[..end].chars().all(|c| c == ' ' || c == '\t')
        };
        let br_ok = {
            let start = br.1.min(br_line.len());
            br_line[start..].chars().all(|c| c == ' ' || c == '\t')
        };
        if tl_ok && br_ok {
            if swap {
                normal("o");
                set_cursor(&mut win, ctx, Pos::new(br.0, br_line.len() + 1));
                normal("o");
                column = 1;
            } else {
                normal("o");
                set_cursor(&mut win, ctx, Pos::new(tl.0, 1));
                normal("o");
                column = br_line.len() + 1;
            }
        }
    }

    let mut lnum = target.lnum;

    // adjustments for 'selection' option
    if forward && visual && vars.selection == "exclusive" {
        let p = pos_next_eol(ctx, Pos::new(lnum, column));
        lnum = p.lnum;
        column = p.cnum;
    }
    if !forward && is_oper && vars.selection == "exclusive" {
        normal("o");
        let cur = ctx.cursor().unwrap_or(start_pos);
        let p = pos_next_eol(ctx, cur);
        set_cursor(&mut win, ctx, p);
        normal("o");
    }

    trace(&format!("FMP set_cursor ({lnum},{column})"));
    set_cursor(&mut win, ctx, Pos::new(lnum, column));
    trace(&format!("FMP after set_cursor: {:?}", ctx.cursor()));

    if vars.foldopen.contains("percent") {
        normal("zv");
        trace(&format!("FMP after zv: {:?}", ctx.cursor()));
    }
    true
}

// ---------------------------------------------------------------------------
// [% and ]%
// ---------------------------------------------------------------------------

/// Port of matchup#motion#find_unmatched (motion.vim:164).
pub fn find_unmatched(ctx: &Ctx, visual: bool, down: bool, timeout: f64) -> bool {
    ctx.state.perf.tic("motion#find_unmatched");
    let vars = read_vars(ctx);
    let force = motion_force();
    let is_oper = !vars.operator.is_empty();
    let exclusive = is_oper && force != "v" && force != "\x16";

    let mut count = vars.count1;

    if visual {
        ensure_visual();
    }

    ctx.state.perf.timeout_start(timeout);

    let mut new_pos: Option<Pos> = None;
    let mut found_delim: Option<Delim> = None;

    for tries in 0..3i64 {
        let c = if tries > 0 { count } else { 1 } as usize;
        let opts = SurroundOpts {
            local: Some(false),
            stopline: 0,
            check_skip: false,
            highlighting: false,
        };
        let (open, close, _ml) = match engine::get_surrounding(ctx, c, &opts) {
            Some(r) => r,
            None => {
                ctx.state
                    .perf
                    .toc("motion#find_unmatched", &format!("fail{tries}"));
                return false;
            }
        };

        let delim = if down { close } else { open };
        let save_pos = match ctx.cursor() {
            Some(p) => p,
            None => return false,
        };
        let mut np = Pos::new(delim.lnum, delim.cnum);

        // this is an exclusive motion, ]%
        if delim.side == crate::words::Side::Close {
            if exclusive {
                np = pos_prev_eol(ctx, np);
            } else {
                np.cnum += delim.end_offset();
            }
        }

        // if the cursor didn't move, increment count
        if save_pos == np {
            count += 1;
        } else if tries > 0 {
            new_pos = Some(np);
            found_delim = Some(delim);
            break;
        }

        if count <= 1 {
            new_pos = Some(np);
            found_delim = Some(delim);
            break;
        }
        new_pos = Some(np);
        found_delim = Some(delim);
    }

    let mut new_pos = match new_pos {
        Some(p) => p,
        None => return false,
    };
    let delim = match found_delim {
        Some(d) => d,
        None => return false,
    };

    let mut win = ctx.win.clone();

    if down && !is_oper {
        new_pos.cnum = engine::jump_target(ctx, &delim);
    }

    // this is an exclusive motion, [%
    if !down && exclusive {
        normal("o");
        let cur = ctx.cursor().unwrap_or(new_pos);
        let pl = ctx.lines.get1(cur.lnum).unwrap_or("");
        let ppl = ctx.lines.get1(cur.lnum.saturating_sub(1)).unwrap_or("");
        let p = pos_prev(pl, ppl, cur);
        set_cursor(&mut win, ctx, p);
        normal("o");
    }

    // 'selection' exclusive going backwards
    if !down && is_oper && vars.selection == "exclusive" {
        normal("o");
        let cur = ctx.cursor().unwrap_or(new_pos);
        let p = pos_next_eol(ctx, cur);
        set_cursor(&mut win, ctx, p);
        normal("o");
    }

    // 'selection' exclusive going forwards
    if down && is_oper && vars.selection == "exclusive" {
        new_pos = pos_next_eol(ctx, new_pos);
    }

    if !ctx.gopts.motion_keepjumps {
        normal("m`");
    }
    set_cursor(&mut win, ctx, new_pos);

    ctx.state.perf.toc("motion#find_unmatched", "done");
    true
}

// ---------------------------------------------------------------------------
// z% and Z%
// ---------------------------------------------------------------------------

/// Port of matchup#motion#jump_inside (motion.vim:254).
pub fn jump_inside(ctx: &Ctx, visual: bool) -> bool {
    let vars = read_vars(ctx);
    let force = motion_force();
    let save_pos = match ctx.cursor() {
        Some(p) => p,
        None => return false,
    };

    ctx.state.perf.timeout_start(750.0);

    if visual {
        ensure_visual();
    }

    let mut new_pos: Option<Pos> = None;
    for counter in 0..vars.count1 {
        let delim = if counter > 0 {
            let o = GetDelimOpts::new(Direction::Next, SideQuery::Open);
            engine::get_delim_multi(ctx, &o)
        } else {
            let o = GetDelimOpts::new(Direction::Current, SideQuery::Open);
            match engine::get_delim_multi(ctx, &o) {
                Some(d) => Some(d),
                None => {
                    let o = GetDelimOpts::new(Direction::Next, SideQuery::Open);
                    engine::get_delim_multi(ctx, &o)
                }
            }
        };
        let delim = match delim {
            Some(d) => d,
            None => {
                let mut win = ctx.win.clone();
                set_cursor(&mut win, ctx, save_pos);
                return false;
            }
        };
        let mut np = Pos::new(delim.lnum, delim.cnum + delim.end_offset());
        let line = ctx.lines.get1(np.lnum).unwrap_or("");
        np = pos_next(line, np);
        new_pos = Some(np);
    }

    let mut win = ctx.win.clone();
    set_cursor(&mut win, ctx, save_pos);

    let mut new_pos = match new_pos {
        Some(p) => p,
        None => return false,
    };

    // exclusive motion except when dealing with whitespace
    let is_oper = !vars.operator.is_empty();
    if is_oper && force != "v" && force != "\x16" {
        while in_whitespace(ctx, new_pos) {
            let line = ctx.lines.get1(new_pos.lnum).unwrap_or("");
            new_pos = pos_next(line, new_pos);
        }
        let pl = ctx.lines.get1(new_pos.lnum).unwrap_or("");
        let ppl = ctx
            .lines
            .get1(new_pos.lnum.saturating_sub(1))
            .unwrap_or("");
        new_pos = pos_prev(pl, ppl, new_pos);
    }

    // jump ahead if inside indent
    if !is_oper && in_indent(ctx, new_pos) {
        let line = ctx.lines.get1(new_pos.lnum).unwrap_or("");
        let indent_len = line
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum::<usize>();
        new_pos.cnum = 1 + indent_len;
    }

    // 'selection' exclusive (motion only goes forwards)
    if visual && vars.selection == "exclusive" {
        new_pos = pos_next_eol(ctx, new_pos);
    }

    if !ctx.gopts.motion_keepjumps {
        normal("m`");
    }
    set_cursor(&mut win, ctx, new_pos);
    true
}

/// Port of matchup#motion#jump_inside_prev (motion.vim:317).
pub fn jump_inside_prev(ctx: &Ctx, visual: bool) -> bool {
    let vars = read_vars(ctx);
    let save_pos = match ctx.cursor() {
        Some(p) => p,
        None => return false,
    };

    ctx.state.perf.timeout_start(750.0);

    if visual {
        ensure_visual();
    }

    let mut new_pos: Option<Pos> = None;
    'outer: for _ in 0..vars.count1 {
        let o = GetDelimOpts::new(Direction::Current, SideQuery::Open);
        if let Some(d) = engine::get_delim_multi(ctx, &o) {
            let mut win = ctx.win.clone();
            let pl = ctx.lines.get1(d.lnum).unwrap_or("");
            let ppl = ctx.lines.get1(d.lnum.saturating_sub(1)).unwrap_or("");
            let p = pos_prev(pl, ppl, d.pos());
            set_cursor(&mut win, ctx, p);
        }

        for _tries in 0..2 {
            let o = GetDelimOpts::new(Direction::Prev, SideQuery::Open);
            let delim = match engine::get_delim_multi(ctx, &o) {
                Some(d) => d,
                None => {
                    let mut win = ctx.win.clone();
                    set_cursor(&mut win, ctx, save_pos);
                    return false;
                }
            };
            let mut np = Pos::new(delim.lnum, delim.cnum + delim.end_offset());
            let line = ctx.lines.get1(np.lnum).unwrap_or("");
            np = pos_next(line, np);

            if in_indent(ctx, np) {
                let line = ctx.lines.get1(np.lnum).unwrap_or("");
                let indent_len = line
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                np.cnum = 1 + indent_len;
            }

            if np.smaller(&save_pos) {
                new_pos = Some(np);
                break 'outer;
            }

            let mut win = ctx.win.clone();
            let p = pos_prev_eol(ctx, delim.pos());
            set_cursor(&mut win, ctx, p);
        }
    }

    let mut win = ctx.win.clone();
    set_cursor(&mut win, ctx, save_pos);

    let mut new_pos = match new_pos {
        Some(p) => p,
        None => return false,
    };

    let is_oper = !vars.operator.is_empty();
    if is_oper && vars.selection == "exclusive" {
        new_pos = pos_next_eol(ctx, new_pos);
    }

    if !ctx.gopts.motion_keepjumps {
        normal("m`");
    }
    set_cursor(&mut win, ctx, new_pos);
    true
}

/// Port of matchup#motion#insert_mode (motion.vim:376).
pub fn insert_mode(ctx: &Ctx) {
    ctx.state.perf.timeout_start(0.0);
    let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
    o.insertmode = true;
    let delim = match engine::get_delim_multi(ctx, &o) {
        Some(d) => d,
        None => return,
    };
    let ml = engine::get_matching(
        ctx,
        &delim,
        &MatchOpts {
            stopline: 0,
            highlighting: false,
        },
    );
    let min_len = if delim.side == crate::words::Side::Mid {
        3
    } else {
        2
    };
    if ml.len() < min_len {
        return;
    }
    let idx = seed_index(&ml);
    let target = &ml.delims[ml.next_of(idx)];
    let np = Pos::new(target.lnum, target.cnum + target.end_offset());
    let np = pos_next_eol(ctx, np);
    let mut win = ctx.win.clone();
    set_cursor(&mut win, ctx, np);
}

// ---------------------------------------------------------------------------
// keymaps
// ---------------------------------------------------------------------------

/// Single-quote a string for vimscript eval (inputs are our own static
/// mapping names; nvim_call_function is ABI-broken on nvim 0.13-dev).
pub fn vim_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn lhs_free(lhs: &str, mode: &str) -> bool {
    // mirror s:map: only map when lhs is unmapped and nothing is mapped
    // to the <Plug> sequence
    let plug = format!("<Plug>(matchup-{lhs})");
    let unmapped: String = crate::nvimrs::call_fn_as(
        "maparg",
        &Array::from_iter([Object::from(lhs), Object::from(mode)]),
    )
    .unwrap_or_default();
    let has: i64 = crate::nvimrs::call_fn_as(
        "hasmapto",
        &Array::from_iter([Object::from(plug.as_str()), Object::from(mode)]),
    )
    .unwrap_or(0);
    unmapped.is_empty() && has == 0
}

/// Operator-pending entry (port of matchup#motion#op, motion.vim:11).
/// First invocation stashes v:operator and re-feeds
/// `{wise}{count}<Plug>(plug)`; the re-entrant invocation (in_op set)
/// performs the actual motion so vim resolves the pending operator from
/// the cursor displacement with the forced motion type.
pub fn op_motion(ctx: &Ctx, plug: &str) {
    if ctx.state.in_op.get() {
        match plug {
            "matchup-%" => {
                find_matching_pair(ctx, false, true);
            }
            "matchup-g%" => {
                find_matching_pair(ctx, false, false);
            }
            "matchup-]%" => {
                find_unmatched(ctx, false, true, 750.0);
            }
            "matchup-[%" => {
                find_unmatched(ctx, false, false, 750.0);
            }
            "matchup-z%" => {
                jump_inside(ctx, false);
            }
            _ => {}
        }
        return;
    }

    let force = motion_force();
    let operator: String = crate::nvimrs::get_vvar_as("operator").unwrap_or_default();
    *ctx.state.op_operator.borrow_mut() = operator;

    let wise = if force.is_empty() { "v" } else { force.as_str() };
    let count: i64 = crate::nvimrs::get_vvar_as("count").unwrap_or(0);
    let _ = api::set_var(
        "mrs_op_args",
        nvim_oxi::Array::from_iter([
            nvim_oxi::Object::from(wise),
            nvim_oxi::Object::from(count),
            nvim_oxi::Object::from(plug),
        ]),
    );
    ctx.state.in_op.set(true);
    let _ = crate::nvimrs::call_fn0_as::<i64>("matchup#rs#op_exec");
    ctx.state.in_op.set(false);
    *ctx.state.op_operator.borrow_mut() = String::new();
}

fn map_rhs(mode: Mode, mode_s: &str, plug_suffix: &str, rhs: &str, default_lhs: Option<&str>) {
    let opts = SetKeymapOpts::builder()
        .noremap(true)
        .silent(true)
        .build();
    let plug = format!("<Plug>(matchup-{plug_suffix})");
    let _ = api::set_keymap(mode, &plug, rhs, &opts);
    if let Some(lhs) = default_lhs {
        if lhs_free(lhs, mode_s) {
            let _ = api::set_keymap(mode, lhs, rhs, &opts);
        }
    }
}

pub fn setup(state: &SharedState) {
    let g = state.gopts();
    if !g.motion_enabled || !g.mappings_enabled {
        return;
    }

    // NOTE: rhs strings use <cmd> (no mode transition); LuaRef callbacks
    // are broken on nvim 0.13-dev (registered but never invoked).

    // % and g%
    for (suffix, down) in [("%", 1), ("g%", 0)] {
        let n_rhs = format!("<cmd>lua require('matchup_rs').motion_matching(0,{down})<cr>");
        let x_rhs = format!("<cmd>lua require('matchup_rs').motion_matching(1,{down})<cr>");
        let o_rhs = format!(
            "<cmd>lua require('matchup_rs').op_motion('matchup-{suffix}')<cr>"
        );
        map_rhs(Mode::Normal, "n", suffix, &n_rhs, Some(suffix));
        map_rhs(Mode::Visual, "x", suffix, &x_rhs, Some(suffix));
        map_rhs(Mode::OperatorPending, "o", suffix, &o_rhs, Some(suffix));
    }

    // ]% and [%
    for (suffix, down) in [("]%", 1), ("[%", 0)] {
        let n_rhs = format!("<cmd>lua require('matchup_rs').motion_unmatched(0,{down})<cr>");
        let x_rhs = format!("<cmd>lua require('matchup_rs').motion_unmatched(1,{down})<cr>");
        let o_rhs = format!(
            "<cmd>lua require('matchup_rs').op_motion('matchup-{suffix}')<cr>"
        );
        map_rhs(Mode::Normal, "n", suffix, &n_rhs, Some(suffix));
        map_rhs(Mode::Visual, "x", suffix, &x_rhs, Some(suffix));
        map_rhs(Mode::OperatorPending, "o", suffix, &o_rhs, Some(suffix));
    }

    // z%
    map_rhs(
        Mode::Normal,
        "n",
        "z%",
        "<cmd>lua require('matchup_rs').motion_jump_inside(0)<cr>",
        Some("z%"),
    );
    map_rhs(
        Mode::Visual,
        "x",
        "z%",
        "<cmd>lua require('matchup_rs').motion_jump_inside(1)<cr>",
        Some("z%"),
    );
    map_rhs(
        Mode::OperatorPending,
        "o",
        "z%",
        "<cmd>lua require('matchup_rs').op_motion('matchup-z%')<cr>",
        Some("z%"),
    );

    // Z% (<Plug> only, like the original)
    map_rhs(
        Mode::Normal,
        "n",
        "Z%",
        "<cmd>lua require('matchup_rs').motion_jump_inside_prev(0)<cr>",
        None,
    );

    // insert mode <c-g>%
    map_rhs(
        Mode::Insert,
        "i",
        "c_g%",
        "<cmd>lua require('matchup_rs').motion_insert()<cr>",
        Some("<c-g>%"),
    );
}
