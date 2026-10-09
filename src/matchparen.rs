//! Matchparen highlighting: port of autoload/matchup/matchparen.vim
//! (highlight flow, extmark rendering, offscreen 'status' method,
//! deferred debounce timers).

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use nvim_oxi::api::{self, types::ExtmarkVirtTextPosition};
use nvim_oxi::{Array, Dictionary, Object};

use crate::engine::{self, Ctx, Direction, GetDelimOpts, MatchOpts};
use crate::nvimrs;
use crate::state::State;
use crate::types::{Delim, MatchingList, Pos};
use crate::words::{Side, SideQuery};

type SharedState = Rc<State>;

/// Temporary file-based tracing (err_writeln raises inside callbacks).
pub fn trace(msg: &str) {
    use std::io::Write;
    if std::env::var("MRS_TRACE").is_ok() {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/mrs_trace.log")
        {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// Namespace id, created lazily so highlighting works even if setup()
/// has not run (create_namespace is idempotent by name).
fn ns_id(state: &State) -> u32 {
    {
        let mp = state.matchparen.borrow();
        if let Some(ns) = mp.ns {
            return ns;
        }
    }
    let ns = api::create_namespace("vim-matchup");
    state.matchparen.borrow_mut().ns = Some(ns);
    ns
}

// ---------------------------------------------------------------------------
// Per-window matchparen state
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct WinState {
    pub timer_id: Option<i64>,
    pub paused: bool,
    pub pulse: Option<Instant>,
    pub hi_time: Option<Instant>,
    pub need_clear: bool,
    pub last_cursor: Option<Pos>,
    pub last_tick: Option<u32>,
    pub old_statusline: Option<String>,
    pub statusline_set: bool,
    pub match_ids: Vec<i64>,
    pub vim_winid: i64,
    pub fade_pos: Option<Pos>,
    pub fade_start: Option<Instant>,
    pub fade_timer: Option<i64>,
}

#[derive(Default)]
pub struct MatchParenState {
    pub wins: HashMap<i32, WinState>,
    pub ns: Option<u32>,
}

// ---------------------------------------------------------------------------
// Setup: autocmds
// ---------------------------------------------------------------------------

pub fn setup(state: &SharedState) {
    let ns = ns_id(state);
    let _ = ns;

    let group = match api::create_augroup(
        "matchup_matchparen",
        &api::opts::CreateAugroupOpts::builder().clear(true).build(),
    ) {
        Ok(g) => g,
        Err(_) => return,
    };

    let gid = group as i32;

    // Native LuaRef callbacks via our own FFI layer. nvim-oxi 0.6's
    // create_autocmd keyset predates nvim 0.13's layout, so its `.callback`
    // registers but never fires; nvimrs::create_autocmd_cb uses the correct
    // KeyDict_create_autocmd, so real Rust callbacks work.
    use crate::nvimrs::create_autocmd_cb;
    use nvim_oxi::api::types::AutocmdCallbackArgs;

    let s = Rc::clone(state);
    create_autocmd_cb(
        &["CursorMoved", "CursorMovedI", "TextChanged", "TextChangedI", "TextChangedP"],
        gid,
        "*",
        move |_a: AutocmdCallbackArgs| {
            crate::guard("ac_highlight_deferred", || {
                crate::with_ctx(&s, |ctx| highlight_deferred(ctx));
            });
            false
        },
    );

    let s = Rc::clone(state);
    create_autocmd_cb(&["WinEnter", "InsertLeave"], gid, "*", move |_a| {
        crate::guard("ac_update", || {
            crate::with_ctx(&s, |ctx| highlight(ctx, true, false));
        });
        false
    });

    let s = Rc::clone(state);
    create_autocmd_cb(&["InsertEnter", "InsertChange"], gid, "*", move |_a| {
        crate::guard("ac_update_insert", || {
            crate::with_ctx(&s, |ctx| highlight(ctx, true, true));
        });
        false
    });

    let s = Rc::clone(state);
    create_autocmd_cb(&["OptionSet"], gid, "signcolumn", move |_a| {
        crate::guard("ac_update_signcolumn", || {
            crate::with_ctx(&s, |ctx| highlight(ctx, true, false));
        });
        false
    });

    let s = Rc::clone(state);
    create_autocmd_cb(&["WinLeave", "BufLeave"], gid, "*", move |_a| {
        crate::guard("ac_clear", || {
            crate::with_ctx(&s, |ctx| clear(ctx));
        });
        false
    });

    let s = Rc::clone(state);
    create_autocmd_cb(&["BufDelete", "BufWipeout"], gid, "*", move |a: AutocmdCallbackArgs| {
        crate::guard("ac_drop_buf", || {
            s.drop_buf(a.buffer.handle());
        });
        false
    });
}

// ---------------------------------------------------------------------------
// Option helpers
// ---------------------------------------------------------------------------

fn buf_or_gopt_i64(ctx: &Ctx, bname: &str, gval: i64) -> i64 {
    ctx.buf.get_var::<i64>(bname).unwrap_or(gval)
}

fn buf_or_gopt_bool(ctx: &Ctx, bname: &str, gval: bool) -> bool {
    ctx.buf
        .get_var::<i64>(bname)
        .map(|v| v != 0)
        .unwrap_or(gval)
}

// ---------------------------------------------------------------------------
// highlight
// ---------------------------------------------------------------------------

/// Port of s:matchparen.highlight (matchparen.vim:332).
pub fn highlight(ctx: &Ctx, force_update: bool, changing_insert: bool) {
    let g = ctx.gopts;
    let tr = |why: &str| trace(&format!("HL early-out: {why}"));
    if !g.matchparen_enabled {
        tr("disabled"); return;
    }
    if crate::nvimrs::call_fn_as::<i64>(
        "has",
        &Array::from_iter([Object::from("vim_starting")]),
    )
    .unwrap_or(0)
        != 0
    {
        tr("vim_starting"); return;
    }
    if g.matchparen_pumvisible == 0 && pumvisible() {
        tr("pumvisible"); return;
    }
    if crate::nvimrs::call_fn_as::<String>("state", &Array::from_iter([Object::from("a")]))
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        tr("state(a)"); return;
    }
    if ctx
        .buf
        .get_var::<i64>("matchup_matchparen_enabled")
        .unwrap_or(1)
        == 0
    {
        tr("buf disabled"); return;
    }

    let real_mode: String = if changing_insert {
        crate::nvimrs::get_vvar_as("insertmode").unwrap_or_else(|| "i".to_string())
    } else {
        crate::nvimrs::call_fn0_as("mode").unwrap_or_else(|| "n".to_string())
    };

    let cursor = match ctx.cursor() {
        Some(c) => c,
        None => { tr("no cursor"); return; }
    };
    let tick = ctx.buf.get_changedtick().unwrap_or(0);
    let win_h = ctx.win.handle();

    if !force_update {
        let mp = ctx.state.matchparen.borrow();
        if let Some(ws) = mp.wins.get(&win_h) {
            if ws.last_cursor == Some(cursor) && ws.last_tick == Some(tick) {
                tr("unchanged"); return;
            }
        }
    }
    {
        let mut mp = ctx.state.matchparen.borrow_mut();
        let ws = mp.wins.entry(win_h).or_default();
        ws.last_cursor = Some(cursor);
        ws.last_tick = Some(tick);
    }

    ctx.state.perf.tic("matchparen.highlight");

    // request eventual clearing of stale matches (fade level 0)
    let mut token_save_pos: Option<Pos> = None;
    fade(ctx, 0, None, &mut token_save_pos);

    // mode blacklist
    if g.matchparen_nomode.contains(&real_mode) {
        tr("nomode"); return;
    }

    // visual-block EOL guard: getcurpos()[4] (off) at INT_MAX while in
    // visual / visual-block mode means the cursor sits past the line end.
    {
        let cp: Array = crate::nvimrs::call_fn0_as("getcurpos").unwrap_or_else(Array::new);
        let v: Vec<Object> = cp.into_iter().collect();
        let off = v.get(3).cloned().and_then(|o| i64::try_from(o).ok()).unwrap_or(0);
        let m: String = crate::nvimrs::call_fn0_as("mode").unwrap_or_default();
        if off == 2147483647 && (m == "v" || m == "\x16") {
            tr("visual-block-eol"); return;
        }
    }
    if crate::nvimrs::call_fn_as::<i64>(
        "foldclosed",
        &Array::from_iter([Object::from(cursor.lnum as i64)]),
    )
    .unwrap_or(-1)
        > -1
    {
        tr("foldclosed"); return;
    }
    if ctx.synmaxcol != 0 && cursor.cnum as i64 > ctx.synmaxcol {
        tr("synmaxcol"); return;
    }

    let insertmode = real_mode == "i";
    let timeout = if insertmode {
        buf_or_gopt_i64(
            ctx,
            "matchup_matchparen_insert_timeout",
            g.matchparen_insert_timeout as i64,
        ) as f64
    } else {
        buf_or_gopt_i64(ctx, "matchup_matchparen_timeout", g.matchparen_timeout as i64) as f64
    };
    ctx.state.perf.timeout_start(timeout);

    let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
    o.insertmode = insertmode;
    o.stopline = g.matchparen_stopline;
    o.highlighting = true;
    let current = match engine::get_delim_multi(ctx, &o) {
        Some(d) => d,
        None => { tr("no current delim"); return; }
    };
    ctx.state.perf.toc("matchparen.highlight", "get_current");

    let ml = engine::get_matching(
        ctx,
        &current,
        &MatchOpts {
            stopline: g.matchparen_stopline,
            highlighting: true,
        },
    );
    ctx.state.perf.toc("matchparen.highlight", "get_matching");
    if ml.is_empty() {
        tr("empty matching list"); return;
    }

    // singleton check (matchparen.vim:456-462)
    let min_len = if current.side == Side::Mid { 3 } else { 2 };
    if ml.len() < min_len && !g.matchparen_singleton {
        tr("singleton"); return;
    }

    // prepare for (possibly) new highlights (fade level 1)
    let pos = Pos::new(current.lnum, current.cnum);
    if fade(ctx, 1, Some(pos), &mut token_save_pos) {
        tr("fade cancel"); return;
    }

    {
        let mut mp = ctx.state.matchparen.borrow_mut();
        mp.wins.entry(win_h).or_default().need_clear = true;
    }

    // off-screen matches
    let method = g.matchparen_offscreen_method.clone();
    if !method.is_empty() && method != "none" && !current.skip {
        let scrolling = offscreen_scrolling_disabled(ctx);
        let win_height: i64 =
            nvimrs::call_fn_as("winheight", &Array::from_iter([Object::from(0i64)])).unwrap_or(0);
        if !scrolling && win_height > 0 {
            do_offscreen(ctx, &ml, &current, &method);
        }
    }

    trace(&format!("HL rendering {} delims", ml.len()));
    // pass the seed's list entry: its match_index is the list position,
    // while `current` (from get_current) still has match_index 0
    let seed_entry = ml.delims[ml.seed_index()].clone();
    add_matches(ctx, &ml, Some(&seed_entry));

    if g.matchparen_hi_background {
        highlight_background(ctx, &ml);
    }

    fade(ctx, 2, Some(pos), &mut token_save_pos);
    ctx.state.perf.toc("matchparen.highlight", "end");
}

/// matchparen.offscreen.scrolloff handling (matchparen.vim:474). The config
/// value gates the check; `&scrolloff` below is the window *option* (a
/// different thing) used for the window-edge arithmetic.
fn offscreen_scrolling_disabled(ctx: &Ctx) -> bool {
    if ctx.gopts.matchparen_offscreen_scrolloff == 0 {
        return false;
    }
    let line = |expr: &str| -> i64 {
        nvimrs::call_fn_as("line", &Array::from_iter([Object::from(expr)])).unwrap_or(0)
    };
    let wh: i64 =
        nvimrs::call_fn_as("winheight", &Array::from_iter([Object::from(0i64)])).unwrap_or(0);
    let scrolloff: i64 =
        nvimrs::get_option_as("scrolloff", 0, ctx.win.handle()).unwrap_or(0);
    let cur = line(".");
    let wdollar = line("w$");
    let last = line("$");
    let w0 = line("w0");
    wh > 2 * scrolloff
        && ((cur == wdollar - scrolloff && last != wdollar) || cur == w0 + scrolloff)
}

fn pumvisible() -> bool {
    if nvimrs::call_fn0_as::<i64>("pumvisible").unwrap_or(0) != 0 {
        return true;
    }
    // nvim-cmp visibility (arbitrary Lua, evaluated through vim's `luaeval`).
    let lua = "(function() local ok, cmp = pcall(require, 'cmp') \
               if ok and type(cmp.visible) == 'function' then return cmp.visible() \
               else return false end end)()";
    nvimrs::call_fn_as("luaeval", &Array::from_iter([Object::from(lua)])).unwrap_or(false)
}

/// Port of s:matchparen.fade (matchparen.vim:206). Returns true when
/// highlighting should be canceled (level 1).
fn fade(ctx: &Ctx, level: i32, pos: Option<Pos>, token_save_pos: &mut Option<Pos>) -> bool {
    let fade_time = ctx.gopts.matchparen_deferred_fade_time;
    let win_h = ctx.win.handle();
    if !ctx.gopts.matchparen_deferred || fade_time <= 0 {
        if level <= 0 {
            clear(ctx);
        }
        return false;
    }
    let mut mp = ctx.state.matchparen.borrow_mut();
    let ws = mp.wins.entry(win_h).or_default();
    match level {
        0 => {
            *token_save_pos = ws.fade_pos.take();
            if !ws.need_clear {
                if let Some(tid) = ws.fade_timer {
                    timer_pause(tid, true);
                }
            }
            false
        }
        1 => {
            let p = pos.unwrap();
            if *token_save_pos != Some(p) {
                if let Some(tid) = ws.fade_timer {
                    timer_pause(tid, true);
                }
                drop(mp);
                clear(ctx);
                false
            } else {
                ws.fade_pos = Some(p);
                true
            }
        }
        _ => {
            let p = pos.unwrap();
            if ws.fade_pos != Some(p) {
                ws.fade_pos = Some(p);
                ws.fade_start = Some(Instant::now());
                if ws.fade_timer.is_none() {
                    if let Ok(tid) = timer_start(fade_time, "matchup#rs#fade_timer_cb") {
                        ws.fade_timer = Some(tid);
                        timer_pause(tid, true);
                    }
                } else if let Some(tid) = ws.fade_timer {
                    timer_pause(tid, false);
                }
            }
            false
        }
    }
}

// ---------------------------------------------------------------------------
// highlight_deferred + timers
// ---------------------------------------------------------------------------

/// Port of s:matchparen.highlight_deferred (matchparen.vim:293).
pub fn highlight_deferred(ctx: &Ctx) {
    let deferred = buf_or_gopt_bool(
        ctx,
        "matchup_matchparen_deferred",
        ctx.gopts.matchparen_deferred,
    );
    if !deferred {
        highlight(ctx, false, false);
        return;
    }
    let win_h = ctx.win.handle();
    let show_delay = ctx.gopts.matchparen_deferred_show_delay;
    let mut mp = ctx.state.matchparen.borrow_mut();
    let ws = mp.wins.entry(win_h).or_default();
    if ws.timer_id.is_none() {
        let vim_winid: i64 = nvimrs::call_fn0_as("win_getid").unwrap_or(0);
        ws.vim_winid = vim_winid;
        match timer_start(show_delay, "matchup#rs#timer_cb") {
            Ok(tid) => {
                ws.timer_id = Some(tid);
                ws.paused = false;
            }
            Err(_) => return,
        }
    }
    ws.pulse = Some(Instant::now());
    if ws.paused {
        if let Some(tid) = ws.timer_id {
            timer_pause(tid, false);
        }
        ws.paused = false;
        ws.hi_time = ws.pulse;
    }
}

// Timers are driven through the native nvim_call_function binding (the oxi
// call_function is ABI-broken on 0.13-dev). The callback is the vimscript
// shim `matchup#rs#timer_cb`, which re-enters timer_callback below.

fn timer_start(delay_ms: i64, cb: &str) -> std::result::Result<i64, ()> {
    let opts = Dictionary::from_iter([("repeat", Object::from(-1i64))]);
    let args = Array::from_iter([
        Object::from(delay_ms),
        Object::from(cb),
        Object::from(opts),
    ]);
    nvimrs::call_fn_as::<i64>("timer_start", &args).ok_or(())
}

fn timer_pause(tid: i64, pause: bool) {
    let p = if pause { 1i64 } else { 0i64 };
    let args = Array::from_iter([Object::from(tid), Object::from(p)]);
    let _ = nvimrs::call_fn_as::<i64>("timer_pause", &args);
}

/// Vim timer callback (deferred debounce), port of s:timer_callback
/// (matchparen.vim:173). Invoked via the matchup#rs#timer_cb shim.
pub fn timer_callback(state: &SharedState, tid: i64) {
    let owner = {
        let mp = state.matchparen.borrow();
        mp.wins
            .iter()
            .find(|(_, ws)| ws.timer_id == Some(tid))
            .map(|(h, ws)| (*h, ws.vim_winid))
    };
    let (win_h, vim_winid) = match owner {
        Some(o) => o,
        None => return,
    };
    let cur_winid: i64 = nvimrs::call_fn0_as("win_getid").unwrap_or(-1);
    if cur_winid != vim_winid {
        timer_pause(tid, true);
        if let Some(ws) = state.matchparen.borrow_mut().wins.get_mut(&win_h) {
            ws.paused = true;
        }
        return;
    }

    let g = state.gopts();
    let show_delay = g.matchparen_deferred_show_delay as f64;
    let hide_delay = g.matchparen_deferred_hide_delay as f64;

    let action = {
        let mp = state.matchparen.borrow();
        match mp.wins.get(&win_h) {
            Some(ws) => {
                let elapsed = ws
                    .pulse
                    .map(|p| p.elapsed().as_secs_f64() * 1000.0)
                    .unwrap_or(f64::MAX);
                if elapsed >= show_delay {
                    Some(1u8)
                } else if ws.need_clear {
                    let helapsed = ws
                        .hi_time
                        .map(|p| p.elapsed().as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                    if helapsed >= hide_delay {
                        Some(2u8)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            None => None,
        }
    };

    match action {
        Some(1) => {
            timer_pause(tid, true);
            if let Some(ws) = state.matchparen.borrow_mut().wins.get_mut(&win_h) {
                ws.paused = true;
            }
            crate::with_ctx(state, |ctx| highlight(ctx, false, false));
        }
        Some(2) => {
            crate::with_ctx(state, |ctx| clear(ctx));
        }
        _ => {}
    }
}

/// Fade timer callback (matchparen.vim:273).
pub fn fade_timer_callback(state: &SharedState, tid: i64) {
    let owner = {
        let mp = state.matchparen.borrow();
        mp.wins
            .iter()
            .find(|(_, ws)| ws.fade_timer == Some(tid))
            .map(|(h, ws)| (*h, ws.vim_winid))
    };
    let (win_h, vim_winid) = match owner {
        Some(o) => o,
        None => return,
    };
    let cur_winid: i64 = nvimrs::call_fn0_as("win_getid").unwrap_or(-1);
    if cur_winid != vim_winid {
        timer_pause(tid, true);
        return;
    }
    let fade_time = state.gopts().matchparen_deferred_fade_time as f64;
    let do_clear = {
        let mp = state.matchparen.borrow();
        match mp.wins.get(&win_h) {
            Some(ws) => match (ws.fade_start, ws.need_clear) {
                (Some(start), true) => start.elapsed().as_secs_f64() * 1000.0 >= fade_time,
                _ => false,
            },
            None => false,
        }
    };
    if do_clear {
        crate::with_ctx(state, |ctx| clear(ctx));
        timer_pause(tid, true);
    }
}

// ---------------------------------------------------------------------------
// clear
// ---------------------------------------------------------------------------

/// Port of s:matchparen.clear (matchparen.vim:135).
pub fn clear(ctx: &Ctx) {
    let win_h = ctx.win.handle();
    let ns = Some(ns_id(ctx.state));
    let match_ids: Vec<i64> = {
        let mut mp = ctx.state.matchparen.borrow_mut();
        let ws = mp.wins.entry(win_h).or_default();
        std::mem::take(&mut ws.match_ids)
    };
    for id in match_ids {
        let _ = nvimrs::call_fn_as::<i64>("matchdelete", &Array::from_iter([Object::from(id)]));
    }
    if let Some(ns) = ns {
        let mut buf = ctx.buf.clone();
        let _ = buf.clear_namespace(ns, ..);
    }
    let old = {
        let mut mp = ctx.state.matchparen.borrow_mut();
        let ws = mp.wins.entry(win_h).or_default();
        ws.need_clear = false;
        ws.statusline_set = false;
        ws.old_statusline.take()
    };
    if let Some(old) = old {
        let vim_winid: i64 = nvimrs::call_fn0_as("win_getid").unwrap_or(0);
        let _ = nvimrs::call_function(
            "setwinvar",
            &Array::from_iter([
                Object::from(vim_winid),
                Object::from("&statusline"),
                Object::from(old.as_str()),
            ]),
        );
        let _ = api::command(
            "if exists('#User#MatchupOffscreenLeave') | doautocmd <nomodeline> User MatchupOffscreenLeave | endif",
        );
    }
    let mut win = ctx.win.clone();
    let _ = win.set_var("matchup_statusline", "");
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

fn wordish(d: &Delim) -> bool {
    // match !~? '^[[:punct:]]\{1,3\}$'
    let n = d.match_.chars().count();
    !(1..=3).contains(&n) || !d.match_.chars().all(|c| c.is_ascii_punctuation())
}

/// Port of s:add_matches (matchparen.vim:1188), nvim >= 0.5 path.
pub fn add_matches(ctx: &Ctx, ml: &MatchingList, current: Option<&Delim>) {
    let ns = ns_id(ctx.state);
    let sarg = |s: &str| Array::from_iter([Object::from(s)]);
    let mwc: String = if nvimrs::call_fn_as::<i64>("hlexists", &sarg("MatchWordCur")).unwrap_or(0)
        != 0
    {
        "MatchWordCur".to_string()
    } else {
        let mw = nvimrs::call_fn_as::<i64>("hlID", &sarg("MatchWord")).unwrap_or(0);
        let mw_trans =
            nvimrs::call_fn_as::<i64>("synIDtrans", &Array::from_iter([Object::from(mw)]))
                .unwrap_or(0);
        let mp = nvimrs::call_fn_as::<i64>("hlID", &sarg("MatchParen")).unwrap_or(0);
        if mw_trans == mp {
            "MatchParenCur".to_string()
        } else {
            "MatchWord".to_string()
        }
    };

    let mut buf = ctx.buf.clone();
    for corr in &ml.delims {
        if corr.match_.is_empty() && !ctx.gopts.ts_disable_virtual_text {
            // empty-match sentinel: render as virtual text like the
            // treesitter scope-end marker (matchparen.vim:1210-1221).
            let open_match = ml.delims.first().map(|d| d.match_.as_str()).unwrap_or("");
            let group: String =
                if nvimrs::call_fn_as::<i64>("hlexists", &sarg("MatchupVirtualText")).unwrap_or(0)
                    != 0
                {
                    "MatchupVirtualText".to_string()
                } else {
                    "Normal".to_string()
                };
            let text = format!(" {} {}", ctx.gopts.matchparen_end_sign, open_match);
            let opts = nvim_oxi::api::opts::SetExtmarkOpts::builder()
                .virt_text(vec![(text, group.as_str())])
                .virt_text_pos(ExtmarkVirtTextPosition::Overlay)
                .build();
            let _ = buf.set_extmark(
                ns,
                corr.lnum.saturating_sub(1),
                corr.cnum.saturating_sub(1),
                &opts,
            );
            continue;
        }
        let group: &str = match current {
            Some(cur) if corr.match_index == cur.match_index => {
                if wordish(corr) {
                    mwc.as_str()
                } else {
                    "MatchParenCur"
                }
            }
            _ => {
                if wordish(corr) {
                    "MatchWord"
                } else {
                    "MatchParen"
                }
            }
        };
        let c0 = corr.cnum.saturating_sub(1);
        let _ = buf.add_highlight(
            ns,
            group,
            corr.lnum.saturating_sub(1),
            c0..c0 + corr.match_.len(),
        );
    }
}

/// Port of s:highlight_background (matchparen.vim:1257).
fn highlight_background(ctx: &Ctx, ml: &MatchingList) {
    if ml.len() < 2 {
        return;
    }
    let open = ml.open();
    let close = ml.close();
    if open.match_.is_empty() {
        return;
    }
    let (l1, c1) = (open.lnum, open.cnum);
    let (l2, c2) = (close.lnum, close.cnum + close.match_.len().saturating_sub(1));
    if l1 == l2 && c1 > c2 {
        return;
    }
    let pat = if l1 == l2 {
        format!(r"\%{}l\&\%{}c.*\%{}c.", l1, c1, c2)
    } else {
        format!(
            r"\%>{}l\(.\+\|^$\)\%<{}l\|\%{}l\%>{}c.\+\|\%{}l.\+\%<{}c.",
            l1,
            l2,
            l1,
            c1 - 1,
            l2,
            c2 + 1
        )
    };
    let args = Array::from_iter([
        Object::from("MatchBackground"),
        Object::from(pat.as_str()),
        Object::from(-1i64),
    ]);
    if let Some(id) = nvimrs::call_fn_as::<i64>("matchadd", &args) {
        let mut mp = ctx.state.matchparen.borrow_mut();
        mp.wins
            .entry(ctx.win.handle())
            .or_default()
            .match_ids
            .push(id);
    }
}

// ---------------------------------------------------------------------------
// offscreen 'status' method
// ---------------------------------------------------------------------------

/// Port of s:do_offscreen (matchparen.vim:538).
fn do_offscreen(ctx: &Ctx, ml: &MatchingList, current: &Delim, method: &str) {
    let _ = current;
    let w0: i64 =
        nvimrs::call_fn_as("line", &Array::from_iter([Object::from("w0")])).unwrap_or(1);
    let wdollar: i64 =
        nvimrs::call_fn_as("line", &Array::from_iter([Object::from("w$")])).unwrap_or(i64::MAX);

    let open = ml.open();
    let close = ml.close();
    let mut offscreen: Option<&Delim> = None;
    if (open.lnum as i64) < w0 {
        offscreen = Some(open);
    }
    if (close.lnum as i64) > wdollar {
        // prefer to show close
        offscreen = Some(close);
    }
    let offscreen = match offscreen {
        Some(o) => o,
        None => return,
    };

    match method {
        "status" => do_offscreen_statusline(ctx, ml, offscreen, false),
        "status_manual" => do_offscreen_statusline(ctx, ml, offscreen, true),
        _ => {} // 'popup' is not implemented in the Rust port
    }
}

fn do_offscreen_statusline(ctx: &Ctx, ml: &MatchingList, offscreen: &Delim, manual: bool) {
    let (mut sl, lnum) = status_str(ctx, ml, offscreen, manual);
    // scroll refresh: re-highlight once the offscreen line scrolls into
    // view (matchparen.vim:574-576)
    if !manual {
        let timer_ok: i64 = nvimrs::call_fn0_as("matchup#rs#ensure_scroll_timer").unwrap_or(0);
        if timer_ok != 0 {
            sl.push_str(&format!("%{{matchup#rs#scroll_update({lnum})}}"));
        }
    }
    {
        let mut win = ctx.win.clone();
        let _ = win.set_var("matchup_statusline", sl.clone());
    }
    if !manual {
        let win_h = ctx.win.handle();
        {
            let mut mp = ctx.state.matchparen.borrow_mut();
            let ws = mp.wins.entry(win_h).or_default();
            if !ws.statusline_set {
                let old: String =
                    nvimrs::get_option_local_as("statusline", win_h).unwrap_or_default();
                ws.old_statusline = Some(old);
                ws.statusline_set = true;
            }
        }
        let vim_winid: i64 = nvimrs::call_fn0_as("win_getid").unwrap_or(0);
        let _ = nvimrs::call_function(
            "setwinvar",
            &Array::from_iter([
                Object::from(vim_winid),
                Object::from("&statusline"),
                Object::from(sl.as_str()),
            ]),
        );
        let _ = api::command(
            "if exists('#User#MatchupOffscreenEnter') | doautocmd <nomodeline> User MatchupOffscreenEnter | endif",
        );
    }
}

/// Port of matchup#quirks#status_adjust (quirks.vim:41).
fn status_adjust(ctx: &Ctx, ml: &MatchingList, offscreen: &Delim) -> isize {
    if offscreen.match_ != "{" || !isclike(ctx.buf.handle()) {
        return 0;
    }
    let close = ml.close();
    let indent = |lnum: usize| -> i64 {
        nvimrs::call_fn_as("indent", &Array::from_iter([Object::from(lnum as i64)]))
            .unwrap_or(0)
    };
    let a = indent(offscreen.lnum);
    let b = indent(close.lnum);
    let line = ctx.lines.get1(offscreen.lnum).unwrap_or("");
    let prefix = &line[..offscreen.cnum.saturating_sub(1).min(line.len())];
    let target = if prefix.trim().is_empty() {
        a
    } else if a != b {
        b
    } else {
        return 0;
    };
    for adjust in 1..=9isize {
        let lnum = offscreen.lnum as isize - adjust;
        if lnum < 1 {
            break;
        }
        let l = ctx.lines.get1(lnum as usize).unwrap_or("");
        if l.trim().is_empty() {
            break;
        }
        let t = l.trim_start();
        if indent(lnum as usize) == target
            && !t.starts_with('#')
            && !t.starts_with("/*")
            && !t.starts_with("//")
        {
            return -adjust;
        }
    }
    0
}

fn isclike(buf: i32) -> bool {
    let ft: String = nvimrs::get_option_as("filetype", buf, 0).unwrap_or_default();
    let first = ft.split('.').next().unwrap_or("");
    matches!(
        first,
        "arduino"
            | "c"
            | "cpp"
            | "cuda"
            | "ld"
            | "php"
            | "go"
            | "javascript"
            | "typescript"
            | "javascriptreact"
            | "typescriptreact"
    )
}

/// Port of s:format_gutter (matchparen.vim:1016).
fn format_gutter(ctx: &Ctx, lnum: usize, noshowdir: bool) -> String {
    let win = ctx.win.handle();
    let opt_i = |name: &str| -> i64 { nvimrs::get_option_as(name, 0, win).unwrap_or(0) };
    let wincol: i64 = nvimrs::call_fn0_as("wincol").unwrap_or(1);
    let virtcol: i64 =
        nvimrs::call_fn_as("virtcol", &Array::from_iter([Object::from(".")])).unwrap_or(1);
    let mut padding = wincol - virtcol;
    let number = opt_i("number") != 0;
    let relativenumber = opt_i("relativenumber") != 0;
    let numberwidth = opt_i("numberwidth");
    // strlen() is a byte count; String::len() matches.
    let lastlinelen: i64 =
        nvimrs::call_fn_as::<String>("getline", &Array::from_iter([Object::from("$")]))
            .map(|s| s.len() as i64)
            .unwrap_or(0);
    // &foldcolumn is a string option; vim coerces it numerically ("2"->2, "auto"->0).
    let foldcolumn: i64 = nvimrs::get_option_as::<String>("foldcolumn", 0, win)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let foldlevel: i64 =
        nvimrs::call_fn_as("foldlevel", &Array::from_iter([Object::from(lnum as i64)]))
            .unwrap_or(0);
    let curline: usize =
        nvimrs::call_fn_as::<i64>("line", &Array::from_iter([Object::from(".")])).unwrap_or(0)
            as usize;

    let mut sl = String::new();
    let direction = lnum < curline;

    if number || relativenumber {
        let nw = lastlinelen.max(numberwidth - 1).max(0) as usize;
        let linenr = if relativenumber {
            (lnum as i64 - curline as i64).abs()
        } else {
            lnum as i64
        };
        sl = format!("{linenr:>nw$}");
        if direction && !noshowdir {
            sl = format!("%#Search#{sl}∆%#Normal#");
        } else {
            sl = format!("%#CursorLineNr#{sl} %#Normal#");
        }
        padding -= nw as i64 + 1;
    }

    if sl.is_empty() && direction && !noshowdir {
        sl = "%#Search#∆%#Normal#".to_string();
        padding -= 1;
        if padding == -1 {
            let ind: i64 =
                nvimrs::call_fn_as("indent", &Array::from_iter([Object::from(lnum as i64)]))
                    .unwrap_or(1);
            if ind == 0 {
                padding = 0;
            }
        }
    }

    let mut fdcstr = String::new();
    if foldcolumn > 0 {
        let fdc = (foldcolumn - 1).max(1);
        let inner_bars = if foldlevel <= fdc {
            "|".repeat(foldlevel.max(0) as usize)
        } else {
            "|".repeat(fdc as usize)
        };
        let mut inner = inner_bars;
        while inner.chars().count() < foldcolumn as usize {
            inner.push(' ');
        }
        padding -= inner.chars().count() as i64;
        fdcstr = format!("%#FoldColumn#{inner}%#Normal#");
    } else if sl.is_empty() {
        sl = "%#Normal#".to_string();
    }

    let pad = if padding > 0 {
        " ".repeat(padding as usize)
    } else {
        String::new()
    };
    format!("{fdcstr}{pad}{sl}")
}

/// Port of matchup#matchparen#status_str (matchparen.vim:1067).
/// Returns (statusline string, adjusted lnum).
pub fn status_str(
    ctx: &Ctx,
    ml: &MatchingList,
    offscreen: &Delim,
    compact: bool,
) -> (String, usize) {
    let adjust = status_adjust(ctx, ml, offscreen);
    let lnum = (offscreen.lnum as isize + adjust).max(1) as usize;
    let line = ctx.lines.get1(lnum).unwrap_or("").to_string();

    let mut out: Vec<u8> = Vec::new();
    let mut trimming = false;
    if compact {
        trimming = true;
    } else {
        out.extend_from_slice(format_gutter(ctx, lnum, false).as_bytes());
    }

    let ww: i64 =
        nvimrs::call_fn_as("winwidth", &Array::from_iter([Object::from(0i64)])).unwrap_or(80);
    let wincol: i64 = nvimrs::call_fn0_as("wincol").unwrap_or(1);
    let virtcol: i64 =
        nvimrs::call_fn_as("virtcol", &Array::from_iter([Object::from(".")])).unwrap_or(1);
    let room0: i64 = 300.min(ww) - (wincol - virtcol);
    let mut room = room0;
    if adjust != 0 {
        let w = api::strwidth(&offscreen.match_).unwrap_or(offscreen.match_.len()) as i64;
        room -= 3 + w;
    }

    // Per-column syntax names via native synID/synIDattr (direct FFI, not RPC).
    let syn_names: Vec<String> = if line.is_empty() {
        vec![]
    } else {
        (1..=line.len())
            .map(|col| {
                let id: i64 = nvimrs::call_fn_as(
                    "synID",
                    &Array::from_iter([
                        Object::from(lnum as i64),
                        Object::from(col as i64),
                        Object::from(1i64),
                    ]),
                )
                .unwrap_or(0);
                let n: String = nvimrs::call_fn_as(
                    "synIDattr",
                    &Array::from_iter([Object::from(id), Object::from("name")]),
                )
                .unwrap_or_default();
                if n.is_empty() {
                    "Normal".to_string()
                } else {
                    n
                }
            })
            .collect()
    };

    let bytes = line.as_bytes();
    let mut lasthi = String::new();
    let mut c = 0usize;
    while c < bytes.len() {
        let col1 = c + 1;
        let b = bytes[c];
        let curhi: String = if adjust == 0
            && offscreen.cnum <= col1
            && col1 <= offscreen.cnum - 1 + offscreen.match_.len()
        {
            if wordish(offscreen) {
                "MatchWord".to_string()
            } else {
                "MatchParen".to_string()
            }
        } else if b < 32 {
            "SpecialKey".to_string()
        } else {
            syn_names.get(c).cloned().unwrap_or_else(|| "Normal".to_string())
        };
        if curhi != lasthi {
            out.extend_from_slice(format!("%#{curhi}#").as_bytes());
        }
        let is_ws = b == b' ' || b == b'\t';
        if trimming && !is_ws {
            trimming = false;
        }
        if !trimming {
            room -= 1;
            if room <= 0 {
                break;
            }
            if b == b'\t' {
                let w1 = api::strwidth(&line[..=c]).unwrap_or(c + 1);
                let w0 = api::strwidth(&line[..c]).unwrap_or(c);
                for _ in 0..w1.saturating_sub(w0) {
                    out.push(b' ');
                }
            } else if b < 32 {
                out.extend_from_slice(strtrans(b).as_bytes());
            } else if b == b'%' {
                out.extend_from_slice(b"%%");
            } else {
                out.push(b);
            }
        }
        lasthi = curhi;
        c += 1;
    }
    // trim trailing whitespace, add final markers
    while matches!(out.last(), Some(b' ') | Some(b'\t')) {
        out.pop();
    }
    out.extend_from_slice(b"%<%#Normal#");
    if adjust != 0 {
        out.extend_from_slice(
            format!(
                "%#LineNr# … %#Normal#%#MatchParen#{}%#Normal#",
                offscreen.match_
            )
            .as_bytes(),
        );
    }
    if ml.close().match_.is_empty() {
        let hi = if wordish(ml.open()) {
            "MatchWord"
        } else {
            "MatchParen"
        };
        out.extend_from_slice(
            format!(
                " {} %#{}#{}%#Normal#",
                ctx.gopts.matchparen_end_sign,
                hi,
                ml.open().match_
            )
            .as_bytes(),
        );
    }
    (String::from_utf8_lossy(&out).into_owned(), lnum)
}

fn strtrans(b: u8) -> String {
    match b {
        0 => "^@".to_string(),
        1..=26 => format!("^{}", (b + b'@' - 1) as char),
        27 => "^[".to_string(),
        28 => "^\\".to_string(),
        29 => "^]".to_string(),
        30 => "^^".to_string(),
        31 => "^_".to_string(),
        _ => (b as char).to_string(),
    }
}
