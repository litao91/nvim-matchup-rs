//! Motions: %, g%, [%, ]%, z%, Z%, insert-mode <c-g>%.
//! Port of autoload/matchup/motion.vim.

use std::rc::Rc;

use nvim_oxi::api::{self, opts::SetKeymapOpts, types::Mode};
use nvim_oxi::Function;

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
    let _ = win.set_cursor(p.lnum.saturating_sub(1), cnum.saturating_sub(1));
}

fn eval_str(expr: &str) -> String {
    api::eval(expr).unwrap_or_default()
}

/// Port of matchup#motion_force (matchup.vim:167).
fn motion_force() -> String {
    let mode = eval_str("mode(1)");
    if mode.len() >= 3 && mode.starts_with("no") {
        mode[2..3].to_string()
    } else {
        String::new()
    }
}

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

fn read_vars() -> Vars {
    let vals: Vec<nvim_oxi::Object> = api::eval::<nvim_oxi::Array>(
        "[v:count, v:count1, v:operator, v:register, &selection, visualmode(), &foldopen, &startofline]",
    )
    .map(|a| a.into_iter().collect())
    .unwrap_or_default();
    let gi = |i: usize| -> i64 {
        vals.get(i)
            .cloned()
            .and_then(|o| i64::try_from(o).ok())
            .unwrap_or(0)
    };
    let gs = |i: usize| -> String {
        use nvim_oxi::conversion::FromObject;
        vals.get(i)
            .cloned()
            .and_then(|o| String::from_object(o).ok())
            .unwrap_or_default()
    };
    Vars {
        count: gi(0),
        count1: gi(1).max(1),
        operator: gs(2),
        register: gs(3),
        selection: gs(4),
        visualmode: gs(5),
        foldopen: gs(6),
        startofline: gi(7) != 0,
    }
}

/// Ensure visual mode is active for a visual mapping callback (the
/// original re-enters with `normal! gv` after `:<c-u>`).
fn ensure_visual() {
    let m = eval_str("mode()");
    if !m.starts_with('v') && !m.starts_with('V') && !m.contains('\x16') && !m.starts_with("^V") {
        normal("gv");
    }
}

fn in_indentexpr() -> bool {
    api::eval::<i64>("matchup#rs#in_indentexpr()").unwrap_or(0) != 0
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
pub fn find_matching_pair(ctx: &Ctx, visual: bool, down: bool) {
    let vars = read_vars();
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
        return;
    }

    if in_indentexpr() {
        ctx.state.perf.timeout_start(300.0);
        let col: i64 = api::eval("col('.') >= col('$') ? 1 : 0").unwrap_or(0);
        if !vars.startofline && col != 0 {
            normal("^");
        }
    } else {
        ctx.state.perf.timeout_start(1000.0);
    }

    let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
    let mut delim = match engine::get_delim(ctx, &o) {
        Some(d) => d,
        None => {
            o = GetDelimOpts::new(Direction::Next, SideQuery::BothAll);
            match engine::get_delim(ctx, &o) {
                Some(d) => d,
                None => return,
            }
        }
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

    // walk links count1 times
    let mut idx = seed_index(&ml);
    delim.match_index = ml.delims[idx].match_index;
    for _ in 0..vars.count1 {
        idx = if down { ml.next_of(idx) } else { ml.prev_of(idx) };
    }
    let target: Delim = ml.delims[idx].clone();

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
        None => return,
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

    set_cursor(&mut win, ctx, Pos::new(lnum, column));

    if vars.foldopen.contains("percent") {
        normal("zv");
    }
}

// ---------------------------------------------------------------------------
// [% and ]%
// ---------------------------------------------------------------------------

/// Port of matchup#motion#find_unmatched (motion.vim:164).
pub fn find_unmatched(ctx: &Ctx, visual: bool, down: bool, timeout: f64) {
    ctx.state.perf.tic("motion#find_unmatched");
    let vars = read_vars();
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
                return;
            }
        };

        let delim = if down { close } else { open };
        let save_pos = match ctx.cursor() {
            Some(p) => p,
            None => return,
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
        None => return,
    };
    let delim = match found_delim {
        Some(d) => d,
        None => return,
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
}

// ---------------------------------------------------------------------------
// z% and Z%
// ---------------------------------------------------------------------------

/// Port of matchup#motion#jump_inside (motion.vim:254).
pub fn jump_inside(ctx: &Ctx, visual: bool) {
    let vars = read_vars();
    let force = motion_force();
    let save_pos = match ctx.cursor() {
        Some(p) => p,
        None => return,
    };

    ctx.state.perf.timeout_start(750.0);

    if visual {
        ensure_visual();
    }

    let mut new_pos: Option<Pos> = None;
    for counter in 0..vars.count1 {
        let delim = if counter > 0 {
            let o = GetDelimOpts::new(Direction::Next, SideQuery::Open);
            engine::get_delim(ctx, &o)
        } else {
            let o = GetDelimOpts::new(Direction::Current, SideQuery::Open);
            match engine::get_delim(ctx, &o) {
                Some(d) => Some(d),
                None => {
                    let o = GetDelimOpts::new(Direction::Next, SideQuery::Open);
                    engine::get_delim(ctx, &o)
                }
            }
        };
        let delim = match delim {
            Some(d) => d,
            None => {
                let mut win = ctx.win.clone();
                set_cursor(&mut win, ctx, save_pos);
                return;
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
        None => return,
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
}

/// Port of matchup#motion#jump_inside_prev (motion.vim:317).
pub fn jump_inside_prev(ctx: &Ctx, visual: bool) {
    let vars = read_vars();
    let save_pos = match ctx.cursor() {
        Some(p) => p,
        None => return,
    };

    ctx.state.perf.timeout_start(750.0);

    if visual {
        ensure_visual();
    }

    let mut new_pos: Option<Pos> = None;
    'outer: for _ in 0..vars.count1 {
        let o = GetDelimOpts::new(Direction::Current, SideQuery::Open);
        if let Some(d) = engine::get_delim(ctx, &o) {
            let mut win = ctx.win.clone();
            let pl = ctx.lines.get1(d.lnum).unwrap_or("");
            let ppl = ctx.lines.get1(d.lnum.saturating_sub(1)).unwrap_or("");
            let p = pos_prev(pl, ppl, d.pos());
            set_cursor(&mut win, ctx, p);
        }

        for _tries in 0..2 {
            let o = GetDelimOpts::new(Direction::Prev, SideQuery::Open);
            let delim = match engine::get_delim(ctx, &o) {
                Some(d) => d,
                None => {
                    let mut win = ctx.win.clone();
                    set_cursor(&mut win, ctx, save_pos);
                    return;
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
        None => return,
    };

    let is_oper = !vars.operator.is_empty();
    if is_oper && vars.selection == "exclusive" {
        new_pos = pos_next_eol(ctx, new_pos);
    }

    if !ctx.gopts.motion_keepjumps {
        normal("m`");
    }
    set_cursor(&mut win, ctx, new_pos);
}

/// Port of matchup#motion#insert_mode (motion.vim:376).
pub fn insert_mode(ctx: &Ctx) {
    ctx.state.perf.timeout_start(0.0);
    let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
    o.insertmode = true;
    let delim = match engine::get_delim(ctx, &o) {
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
    let unmapped: String = api::eval(&format!(
        "maparg({}, {})",
        vim_quote(lhs),
        vim_quote(mode)
    ))
    .unwrap_or_default();
    let has: i64 = api::eval(&format!(
        "hasmapto({}, {})",
        vim_quote(&plug),
        vim_quote(mode)
    ))
    .unwrap_or(0);
    unmapped.is_empty() && has == 0
}

fn map(
    mode: Mode,
    mode_s: &str,
    lhs: &str,
    state: &SharedState,
    f: impl Fn(&Ctx) + 'static,
) {
    let s = Rc::clone(state);
    let cb = Function::from_fn(move |()| {
        crate::with_ctx(&s, |ctx| f(ctx));
    });
    let opts = SetKeymapOpts::builder()
        .noremap(true)
        .silent(true)
        .callback(cb)
        .build();
    // <Plug> mapping (always)
    let plug = format!("<Plug>(matchup-{lhs})");
    let _ = api::set_keymap(mode, &plug, "", &opts);
    // default mapping (guarded)
    if lhs_free(lhs, mode_s) {
        let _ = api::set_keymap(mode, lhs, "", &opts);
    }
}

pub fn setup(state: &SharedState) {
    let motion_enabled: i64 = api::get_var("matchup_motion_enabled").unwrap_or(1);
    let mappings_enabled: i64 = api::get_var("matchup_mappings_enabled").unwrap_or(1);
    if motion_enabled == 0 || mappings_enabled == 0 {
        return;
    }

    // % (down) and g% (up)
    for (lhs, down) in [("%", true), ("g%", false)] {
        let d = down;
        map(Mode::Normal, "n", lhs, state, move |ctx| {
            find_matching_pair(ctx, false, d)
        });
        let d = down;
        map(Mode::Visual, "x", lhs, state, move |ctx| {
            find_matching_pair(ctx, true, d)
        });
        let d = down;
        map(Mode::OperatorPending, "o", lhs, state, move |ctx| {
            find_matching_pair(ctx, false, d)
        });
    }

    // ]% and [%
    for (lhs, down) in [("]%", true), ("[%", false)] {
        let d = down;
        map(Mode::Normal, "n", lhs, state, move |ctx| {
            find_unmatched(ctx, false, d, 750.0)
        });
        let d = down;
        map(Mode::Visual, "x", lhs, state, move |ctx| {
            find_unmatched(ctx, true, d, 750.0)
        });
        let d = down;
        map(Mode::OperatorPending, "o", lhs, state, move |ctx| {
            find_unmatched(ctx, false, d, 750.0)
        });
    }

    // z%
    map(Mode::Normal, "n", "z%", state, |ctx| {
        jump_inside(ctx, false)
    });
    map(Mode::Visual, "x", "z%", state, |ctx| {
        jump_inside(ctx, true)
    });
    map(Mode::OperatorPending, "o", "z%", state, |ctx| {
        jump_inside(ctx, false)
    });

    // Z% (unmapped <Plug> only, like the original)
    {
        let s = Rc::clone(state);
        let cb = Function::from_fn(move |()| {
            crate::with_ctx(&s, |ctx| jump_inside_prev(ctx, false));
        });
        let opts = SetKeymapOpts::builder()
            .noremap(true)
            .silent(true)
            .callback(cb)
            .build();
        let _ = api::set_keymap(Mode::Normal, "<Plug>(matchup-Z%)", "", &opts);
    }

    // insert mode <c-g>%
    {
        let s = Rc::clone(state);
        let cb = Function::from_fn(move |()| {
            crate::with_ctx(&s, |ctx| insert_mode(ctx));
        });
        let opts = SetKeymapOpts::builder()
            .noremap(true)
            .silent(true)
            .callback(cb)
            .build();
        let _ = api::set_keymap(Mode::Insert, "<Plug>(matchup-c_g%)", "", &opts);
        if lhs_free("<c-g>%", "i") {
            let _ = api::set_keymap(Mode::Insert, "<C-G>%", "", &opts);
        }
    }
}
