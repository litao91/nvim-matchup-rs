//! Text objects: i% and a%.
//! Port of autoload/matchup/text_obj.vim.

use std::rc::Rc;

use nvim_oxi::api::{self, opts::SetKeymapOpts, types::Mode};
use nvim_oxi::{Array, Dictionary, Object};

use crate::engine::{self, Ctx, SurroundOpts};
use crate::state::State;
use crate::types::{pos_next, pos_prev, Delim, Pos};

type SharedState = Rc<State>;

fn normal(cmd: &str) {
    let _ = api::command(&format!("normal! {cmd}"));
}

fn set_cursor(win: &mut nvim_oxi::api::Window, ctx: &Ctx, p: Pos) {
    let mut cnum = p.cnum;
    if let Some(line) = ctx.lines.get1(p.lnum) {
        if cnum > line.len() + 1 {
            cnum = line.len() + 1;
        }
        let mut c0 = cnum - 1;
        while c0 > 0 && !line.is_char_boundary(c0) {
            c0 -= 1;
        }
        cnum = c0 + 1;
    }
    // nvim_win_set_cursor: line is 1-based, col is 0-based
    let _ = win.set_cursor(p.lnum, cnum.saturating_sub(1));
}

fn motion_force() -> String {
    let mode: String = crate::nvimrs::call_fn_as("mode", &Array::from_iter([Object::from(1i64)]))
        .unwrap_or_default();
    if mode.len() >= 3 && mode.starts_with("no") {
        mode[2..3].to_string()
    } else {
        String::new()
    }
}

fn in_indent(ctx: &Ctx, p: Pos) -> bool {
    if p.cnum == 0 {
        return false;
    }
    let line = ctx.lines.get1(p.lnum).unwrap_or("");
    let mut e = p.cnum.min(line.len());
    while e > 0 && !line.is_char_boundary(e) {
        e -= 1;
    }
    line[..e].chars().all(|c| c.is_whitespace())
}

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

/// Port of matchup#util#matchpref: matchpref[&ft][id], read from the
/// setup(opts) configuration (no `g:matchup_matchpref` global).
fn matchpref(ctx: &Ctx, id: &str, default: bool) -> bool {
    let ft: String =
        crate::nvimrs::get_option_as("filetype", ctx.buf.handle(), 0).unwrap_or_default();
    ctx.gopts
        .matchpref
        .get(&ft)
        .and_then(|m| m.get(id))
        .copied()
        .unwrap_or(default)
}

fn ishtmllike() -> bool {
    let ft: String = crate::nvimrs::get_option_as("filetype", api::get_current_buf().handle(), 0)
        .unwrap_or_default();
    let first = ft.split('.').next().unwrap_or("");
    matches!(
        first,
        "tidy"
            | "php"
            | "liquid"
            | "haml"
            | "tt2html"
            | "html"
            | "xhtml"
            | "xml"
            | "jsp"
            | "htmldjango"
            | "aspvbs"
            | "rmd"
            | "markdown"
            | "eruby"
            | "vue"
            | "javascriptreact"
            | "typescriptreact"
            | "svelte"
            | "templ"
    )
}

/// Port of matchup#text_obj#delimited (text_obj.vim:10).
pub fn delimited(ctx: &Ctx, is_inner: bool, visual: bool) {
    let v_motion_force = motion_force();

    use crate::nvimrs::{call_fn0_as, call_fn_as, get_option_as, get_vvar_as};
    let count = get_vvar_as::<i64>("count").unwrap_or(0);
    let count1 = get_vvar_as::<i64>("count1").unwrap_or(0).max(1);
    let operator = get_vvar_as::<String>("operator").unwrap_or_default();
    let save_reg = get_vvar_as::<String>("register").unwrap_or_default();
    let selection_opt = get_option_as::<String>("selection", 0, 0).unwrap_or_default();
    let visualmode = call_fn0_as::<String>("visualmode").unwrap_or_default();
    // getpos("'<")[1:2] / getpos("'>")[1:2] -> (lnum, col) both 1-based
    let mark_pos = |m: &str| -> Pos {
        let arr: Array =
            call_fn_as("getpos", &Array::from_iter([Object::from(m)])).unwrap_or_else(Array::new);
        let v: Vec<Object> = arr.into_iter().collect();
        let lnum = v
            .get(1)
            .cloned()
            .and_then(|o| i64::try_from(o).ok())
            .unwrap_or(0);
        let col = v
            .get(2)
            .cloned()
            .and_then(|o| i64::try_from(o).ok())
            .unwrap_or(0);
        Pos::new(lnum as usize, col as usize)
    };
    let sel_start = mark_pos("'<");
    let sel_end = mark_pos("'>");

    let mut win = ctx.win.clone();

    // move to the start of the current selection
    if visual {
        set_cursor(&mut win, ctx, sel_start);
    }

    let mut forced = if visual {
        String::new()
    } else {
        v_motion_force.clone()
    };

    // determine if operator is able to act line-wise (for inner)
    let mut linewise_op = ctx
        .gopts
        .text_obj_linewise_operators
        .iter()
        .any(|o| *o == operator);
    if operator == "g@" {
        // '^g@\%(,\(.\+\)\)\?' spec against the joined option string
        let spec: String = ctx.gopts.text_obj_linewise_operators.join(",");
        if let Some(rest) = spec.strip_prefix("g@") {
            if rest.is_empty() {
                linewise_op = true;
            } else if let Some(expr) = rest.strip_prefix(',') {
                // arbitrary user-config vimscript expression -> native nvim_eval
                linewise_op = crate::nvimrs::eval_as::<i64>(expr).unwrap_or(0) != 0;
            }
        }
    } else if operator == ":"
        && ctx
            .gopts
            .text_obj_linewise_operators
            .iter()
            .any(|o| *o == visualmode)
    {
        linewise_op = true;
    }

    ctx.state.perf.timeout_start(725.0);

    // the [local, try_again] schedule
    let simple = count == 1 || count > ctx.gopts.delim_count_max as i64;
    let schedule: Vec<(bool, i64)> = if simple {
        if is_inner {
            vec![(false, 0), (false, 1), (false, 2), (false, 3)]
        } else {
            vec![(false, 0), (false, 1), (false, 2)]
        }
    } else if is_inner {
        vec![
            (true, 0),
            (false, 0),
            (true, 1),
            (false, 1),
            (true, 2),
            (false, 2),
        ]
    } else {
        vec![(true, 0), (false, 0), (true, 1), (false, 1)]
    };

    let mut l1: usize = 0;
    let mut c1: usize = 0;
    let mut l2: usize = 0;
    let mut c2: usize = 0;
    let mut completed = false;

    for (local, try_again) in schedule {
        let cnt = (count1 + try_again).max(0) as usize;
        let opts = SurroundOpts {
            local: Some(false),
            stopline: 0,
            check_skip: false,
            highlighting: false,
        };
        let (_open, close_, ml) = match engine::get_surrounding(ctx, cnt, &opts) {
            Some(r) => r,
            None => {
                if visual {
                    normal("gv");
                } else {
                    // invalid text object: drop into normal mode and undo
                    // any entered text (text_obj.vim:66-74)
                    let keys = nvim_oxi::String::from("\u{1c}\u{1e}\u{1b}");
                    let mode = nvim_oxi::String::from("n");
                    api::feedkeys(&keys, &mode, false);
                    let seq: i64 = crate::nvimrs::call_fn0_as::<Dictionary>("undotree")
                        .and_then(|d| d.get("seq_cur").cloned())
                        .and_then(|o| i64::try_from(o).ok())
                        .unwrap_or(0);
                    let keys = nvim_oxi::String::from(format!(
                        ":call matchup#rs#text_obj_undo({seq})\r:\u{3}",
                        seq = seq
                    ));
                    let mode = nvim_oxi::String::from("n");
                    api::feedkeys(&keys, &mode, false);
                }
                return;
            }
        };
        let _ = close_;

        let seed_idx = ml
            .delims
            .iter()
            .position(|d| d.word_id != crate::types::MID_SENTINEL)
            .unwrap_or(0);

        let (open, close): (Delim, Delim) = if local {
            let cur = ctx.cursor().unwrap_or(sel_start);
            match engine::get_surround_nearest(&ml, seed_idx, cur) {
                Some((pi, ni)) => (ml.delims[pi].clone(), ml.delims[ni].clone()),
                None => (
                    ml.delims[seed_idx].clone(),
                    ml.delims[ml.next_of(seed_idx)].clone(),
                ),
            }
        } else {
            (ml.delims[seed_idx].clone(), ml.close().clone())
        };

        // no way to specify an empty region: use tricks (text_obj.vim:88)
        let mut epos = Pos::new(open.lnum, open.cnum + open.end_offset());
        {
            let line = ctx.lines.get1(epos.lnum).unwrap_or("");
            epos = pos_next(line, epos);
        }
        if !visual && is_inner && close.pos() == epos {
            if operator == "c" {
                set_cursor(&mut win, ctx, close.pos());
                let _ = api::command("silent! execute \"normal! i \\<esc>v\"");
            } else if !"<>".contains(&operator) {
                let lb: i64 = crate::nvimrs::call_fn_as(
                    "line2byte",
                    &Array::from_iter([Object::from(close.lnum as i64)]),
                )
                .unwrap_or(0);
                let byte: i64 = lb + close.cnum as i64 - 1;
                let keys = nvim_oxi::String::from(format!("{byte}go"));
                let mode = nvim_oxi::String::from("n");
                api::feedkeys(&keys, &mode, false);
            }
            return;
        }

        l1 = open.lnum;
        c1 = open.cnum;
        l2 = close.lnum;
        c2 = close.cnum;

        let line_count = l2.saturating_sub(l1) + 1;

        // if inner and the selection coincides with open/close, try again
        if visual && is_inner && sel_start == Pos::new(l1, c1) && sel_end == Pos::new(l2, c2) {
            continue;
        }

        if is_inner {
            c1 += open.end_offset();
            {
                let line = ctx.lines.get1(l1).unwrap_or("");
                let p = pos_next(line, Pos::new(l1, c1));
                l1 = p.lnum;
                c1 = p.cnum;
            }
            let mut sol = c2 <= 1;
            {
                let line = ctx.lines.get1(l2).unwrap_or("");
                let pline = ctx.lines.get1(l2.saturating_sub(1)).unwrap_or("");
                let p = pos_prev(line, pline, Pos::new(l2, c2));
                l2 = p.lnum;
                c2 = p.cnum;
            }

            // make *i% more like *it for html
            if line_count < 2
                && ishtmllike()
                && !matchpref(ctx, "classic_textobj", false)
                && html_close_like(&close.match_)
                && !(visual && Pos::new(l1, c1) == Pos::new(l2, c2))
            {
                let line = ctx.lines.get1(l2).unwrap_or("");
                let pline = ctx.lines.get1(l2.saturating_sub(1)).unwrap_or("");
                let p = pos_prev(line, pline, Pos::new(l2, c2));
                l2 = p.lnum;
                c2 = p.cnum;
                if !open.match_.to_lowercase().ends_with('>') {
                    let line = ctx.lines.get1(l1).unwrap_or("");
                    let p = pos_next(line, Pos::new(l1, c1));
                    l1 = p.lnum;
                    c1 = p.cnum;
                }
            }

            // don't select only indent at close
            while in_indent(ctx, Pos::new(l2, c2)) {
                c2 = 1;
                let line = ctx.lines.get1(l2).unwrap_or("");
                let pline = ctx.lines.get1(l2.saturating_sub(1)).unwrap_or("");
                let p = pos_prev(line, pline, Pos::new(l2, c2));
                l2 = p.lnum;
                c2 = p.cnum;
                sol = true;
            }

            // include the line break if we had wrapped around
            if visual && sol {
                c2 = ctx.lines.get1(l2).map(|l| l.len()).unwrap_or(0) + 1;
            }

            if !visual {
                if sol {
                    let line = ctx.lines.get1(l2).unwrap_or("");
                    let p = pos_next(line, Pos::new(l2, c2));
                    l2 = p.lnum;
                    c2 = p.cnum;
                }

                // toggle exclusive: difference between di% and dvi%
                let mut inclusive = !sol && Pos::new(l1, c1).val() <= Pos::new(l2, c2).val();
                if forced == "v" {
                    inclusive = !inclusive;
                }

                // sometimes operate in visual line motion (re-purpose force)
                if v_motion_force.is_empty() && c2 <= 1 && line_count > 1 && !inclusive {
                    l2 -= 1;
                    if c1 <= 1 || in_indent(ctx, Pos::new(l1, c1.saturating_sub(1))) {
                        forced = "V".to_string();
                        inclusive = true;
                    } else {
                        // end_adjusted
                        c2 = ctx.lines.get1(l2).map(|l| l.len()).unwrap_or(0) + 1;
                        if c2 > 1 {
                            c2 -= 1;
                            inclusive = true;
                        }
                    }
                }

                if !inclusive {
                    let line = ctx.lines.get1(l2).unwrap_or("");
                    let pline = ctx.lines.get1(l2.saturating_sub(1)).unwrap_or("");
                    let p = pos_prev(line, pline, Pos::new(l2, c2));
                    l2 = p.lnum;
                    c2 = p.cnum;
                }
            }

            // line-wise special case
            if line_count > 2 && linewise_op && close.match_.len() > 1 {
                if c1 != 1 {
                    l1 += 1;
                    c1 = 1;
                }
                l2 = close.lnum - 1;
                c2 = ctx.lines.get1(l2).map(|l| l.len()).unwrap_or(0) + 1;
            }

            // empty selection fallback
            if !visual && (l2 < l1 || (l1 == l2 && c1 > c2)) {
                if operator == "c" {
                    set_cursor(&mut win, ctx, Pos::new(l1, c1));
                    let _ = api::command("silent! execute \"normal! i \\<esc>v\"");
                } else if !"<>".contains(&operator) {
                    let lb: i64 = crate::nvimrs::call_fn_as(
                        "line2byte",
                        &Array::from_iter([Object::from(l1 as i64)]),
                    )
                    .unwrap_or(0);
                    let byte: i64 = lb + c1 as i64 - 1;
                    let keys = nvim_oxi::String::from(format!("{byte}go"));
                    let mode = nvim_oxi::String::from("n");
                    api::feedkeys(&keys, &mode, false);
                }
                return;
            }
        } else {
            c2 += close.end_offset();

            // make *a% more like *at for html
            if ishtmllike()
                && !matchpref(ctx, "classic_textobj", false)
                && html_close_like(&close.match_)
            {
                c1 = c1.saturating_sub(1);
                if !close.match_.to_lowercase().ends_with('>') {
                    c2 += 1;
                }
            }

            // special case for delete operator
            if !visual && operator == "d" && line_count > 1 {
                let line2 = ctx.lines.get1(l2).unwrap_or("");
                let after = &line2[c2.min(line2.len())..];
                let before = &line2[..(c1 - 1).min(line2.len())];
                if after.chars().all(|c| c.is_whitespace())
                    && before.chars().all(|c| c.is_whitespace())
                {
                    c1 = 1;
                    c2 = line2.len() + 1;
                }
            }
        }

        // in visual line mode, force new selection to not be smaller
        if visual && visualmode == "V" && (l1 > sel_start.lnum || l2 < sel_end.lnum) {
            continue;
        }

        // in other visual modes, try again if we didn't reach a bigger range
        if visual
            && visualmode != "V"
            && sel_start != sel_end
            && ((sel_start == Pos::new(l1, c1) && sel_end == Pos::new(l2, c2))
                || Pos::new(l1, c1).larger(&sel_start)
                || sel_end.larger(&Pos::new(l2, c2)))
        {
            continue;
        }

        completed = true;
        break;
    }

    let _ = completed;
    if l1 == 0 {
        return;
    }

    // set the proper visual mode for this selection
    let select_mode = if operator == ":" {
        visualmode.clone()
    } else if !forced.is_empty() {
        forced.clone()
    } else {
        "v".to_string()
    };

    if selection_opt == "exclusive" {
        let p = pos_next_eol(ctx, Pos::new(l2, c2));
        l2 = p.lnum;
        c2 = p.cnum;
    }

    // apply selection
    normal(&select_mode);
    normal("o");
    set_cursor(&mut win, ctx, Pos::new(l1, c1));
    normal("o");
    set_cursor(&mut win, ctx, Pos::new(l2, c2));
    if operator == "g@" && !save_reg.is_empty() {
        normal(&format!("\"{save_reg}"));
    }
}

/// close.match =~? '^/\w\+\s*>\=$'
fn html_close_like(m: &str) -> bool {
    let b = m.as_bytes();
    if b.first() != Some(&b'/') {
        return false;
    }
    let mut i = 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    if i == 1 {
        return false;
    }
    let tail = &m[i..];
    let t = tail.trim_start_matches(|c: char| c.is_whitespace());
    t.is_empty() || t == ">"
}

// ---------------------------------------------------------------------------
// keymaps
// ---------------------------------------------------------------------------

pub fn setup(state: &SharedState) {
    let g = state.gopts();
    if !g.text_obj_enabled || !g.mappings_enabled {
        return;
    }

    let opts = SetKeymapOpts::builder().noremap(true).silent(true).build();

    for (lhs, inner) in [("i%", 1), ("a%", 0)] {
        let plug = format!("<Plug>(matchup-{lhs})");
        for (mode, mode_s, visual) in [(Mode::Visual, "x", 1), (Mode::OperatorPending, "o", 0)] {
            // visual mode must drop out of visual before the callback (as the
            // original's `:<c-u>` plug does) so that `normal! v` inside
            // re-enters visual mode to apply the new selection; <cmd> would
            // keep visual active and toggle it off instead
            let rhs = if visual == 1 {
                format!(":<c-u>lua require('matchup_rs').textobj({inner},1)<cr>")
            } else {
                format!("<cmd>lua require('matchup_rs').textobj({inner},{visual})<cr>")
            };
            let _ = api::set_keymap(mode, &plug, &rhs, &opts);
            let unmapped: String = crate::nvimrs::call_fn_as(
                "maparg",
                &Array::from_iter([Object::from(lhs), Object::from(mode_s)]),
            )
            .unwrap_or_default();
            let has: i64 = crate::nvimrs::call_fn_as(
                "hasmapto",
                &Array::from_iter([Object::from(plug.as_str()), Object::from(mode_s)]),
            )
            .unwrap_or(0);
            if unmapped.is_empty() && has == 0 {
                let _ = api::set_keymap(mode, lhs, &rhs, &opts);
            }
        }
    }
}
