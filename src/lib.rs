//! nvim-matchup-rs: Rust rewrite of vim-matchup's classic engine.
//!
//! Lua module surface (require("matchup_rs")):
//!   setup()                     - wire autocmds/keymaps/commands
//!   get_delim(dir, side, opts)  - raw delimiter lookup
//!   get_matching_at(lnum, cnum) - full matching list at position
//!   get_surrounding_at(count)   - surrounding pair
//!   highlight() / clear()       - matchparen
//!   reload() / show_times()

use std::rc::Rc;

use nvim_oxi::api::{self, Buffer, Window};
use nvim_oxi::{Array, Dictionary, Function, Object, Result};

pub mod engine;
pub mod ftplugin;
pub mod matchparen;
pub mod motion;
pub mod nvimrs;
pub mod skip;
pub mod state;
pub mod textobj;
pub mod treesitter;
mod types;
pub mod vimregex;
pub mod words;

use engine::{Ctx, Direction, GetDelimOpts, MatchOpts, SurroundOpts};
use state::{ensure_buf, GOpts, State};
use types::Delim;
use words::SideQuery;

type SharedState = Rc<State>;

/// Log panics to a file: the default hook's stderr output is easily lost
/// inside nvim, and a panic in a Lua callback aborts the process.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/matchup_rs_panic.log")
            {
                let _ = writeln!(f, "{info}");
            }
        }));
    });
}

/// Run a Lua-callback body, converting panics into nil results + error
/// messages instead of aborting nvim.
pub(crate) fn guard<R: Default>(name: &str, f: impl FnOnce() -> R) -> R {
    // SAFETY: single-threaded use; on panic the unwind drops all RefCell
    // guards normally, so no borrow state is poisoned.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            let _ = api::err_writeln(&format!("matchup_rs: panic in {name}: {msg}"));
            R::default()
        }
    }
}

fn parse_direction(s: &str) -> Option<Direction> {
    match s {
        "next" => Some(Direction::Next),
        "prev" => Some(Direction::Prev),
        "current" => Some(Direction::Current),
        _ => None,
    }
}

fn parse_side(s: &str) -> Option<SideQuery> {
    match s {
        "open" => Some(SideQuery::Open),
        "mid" => Some(SideQuery::Mid),
        "close" => Some(SideQuery::Close),
        "both" => Some(SideQuery::Both),
        "both_all" => Some(SideQuery::BothAll),
        "open_mid" => Some(SideQuery::OpenMid),
        "all" | "delim_all" | "delim_tex" => Some(SideQuery::BothAll),
        _ => None,
    }
}

fn obj_bool(o: &Object) -> Option<bool> {
    use nvim_oxi::conversion::FromObject;
    if let Ok(i) = i64::try_from(o.clone()) {
        return Some(i != 0);
    }
    if let Ok(b) = nvim_oxi::Boolean::from_object(o.clone()) {
        return Some(b);
    }
    None
}

fn delim_to_dict(d: &Delim) -> Dictionary {
    Dictionary::from_iter([
        ("lnum", Object::from(d.lnum as i64)),
        ("cnum", Object::from(d.cnum as i64)),
        ("match", Object::from(d.match_.clone())),
        ("side", Object::from(d.side.as_str())),
        ("set", Object::from(d.set as i64)),
        ("word_id", Object::from(d.word_id as i64)),
        ("skip", Object::from(d.skip)),
        ("match_index", Object::from(d.match_index as i64)),
    ])
}

/// Run `f` with a context for the current buffer/window.
pub(crate) fn with_ctx<R>(
    state: &SharedState,
    f: impl FnOnce(&Ctx) -> R,
) -> Option<R> {
    let buf = api::get_current_buf();
    let win = api::get_current_win();
    with_ctx_for(state, &buf, &win, f)
}

pub(crate) fn with_ctx_for<R>(
    state: &SharedState,
    buf: &Buffer,
    win: &Window,
    f: impl FnOnce(&Ctx) -> R,
) -> Option<R> {
    let gopts = state.gopts();
    let ts_words = if gopts.ts_enabled {
        match crate::treesitter::active_lang(state, &gopts, buf.handle()) {
            Some(_) if gopts.ts_include_match_words => state::TsWords::Filter,
            Some(_) => state::TsWords::NoWords,
            None => state::TsWords::None,
        }
    } else {
        state::TsWords::None
    };
    ensure_buf(state, buf, ts_words);
    let h = buf.handle();
    let bufs = state.bufs.borrow();
    let bc = bufs.get(&h)?;
    let ctx = Ctx::new(state, bc, buf.clone(), win.clone(), &gopts);
    Some(f(&ctx))
}

/// One-time activation, guarded by Rust state (`State.activated`): highlight
/// groups, neutralize matchit / the bundled pi_paren, and define the user
/// commands. The globals it touches belong to *other* plugins (matchit,
/// pi_paren) and are set from Rust; matchup-rs keeps no state in vimscript.
fn activate(state: &SharedState) {
    if state.is_activated() {
        return;
    }
    state.set_activated();

    // highlight group links (native nvim_set_hl with `default`, so an existing
    // user definition is not overridden - the `hi def link` equivalent)
    nvimrs::set_hl_link("MatchParenCur", "MatchParen");
    nvimrs::set_hl_link("MatchWord", "MatchParen");
    nvimrs::set_hl_link("MatchBackground", "ColorColumn");

    // Disable matchit and its bundled mappings. nvim ships matchit loaded (%
    // mapped to <Plug>(MatchitNormalForward)); clear it so matchup's own
    // motion/text-object maps can claim %/[%/]%/a% (motion::setup only maps an
    // lhs that is currently free).
    let _ = api::set_var("loaded_matchit", 1);
    nvimrs::del_user_command("MatchDebug");
    for m in ["%", "[%", "]%", "a%", "g%"] {
        let _ = api::command(&format!("silent! unmap {m}"));
    }

    // ensure pi_paren is loaded, then deactivate it (clear its autocmds and
    // claim its global so it does not re-arm)
    let _ = api::command("runtime plugin/matchparen.vim");
    let _ = api::command("silent! au! matchparen");
    let _ = api::set_var("loaded_matchparen", 1);

    // user commands via native nvim_create_user_command with Rust callbacks
    // (no `command!` vimscript strings, no lua bodies).
    let s = Rc::clone(state);
    nvimrs::create_user_command_cb("NoMatchParen", "Disable matchup highlighting", move |_| {
        guard("cmd_NoMatchParen", || {
            s.set_matchparen_enabled(false);
            with_ctx(&s, |ctx| matchparen::clear(ctx));
        });
    });
    let s = Rc::clone(state);
    nvimrs::create_user_command_cb("DoMatchParen", "Enable matchup highlighting", move |_| {
        guard("cmd_DoMatchParen", || {
            s.set_matchparen_enabled(true);
            with_ctx(&s, |ctx| matchparen::clear(ctx));
            with_ctx(&s, |ctx| matchparen::highlight(ctx, true, false));
        });
    });
    let s = Rc::clone(state);
    nvimrs::create_user_command_cb("MatchupReload", "Reload matchup per-buffer state", move |_| {
        guard("cmd_MatchupReload", || {
            s.reload();
            with_ctx(&s, |ctx| matchparen::highlight(ctx, true, false));
        });
    });
    let s = Rc::clone(state);
    nvimrs::create_user_command_cb("MatchupShowTimes", "Show matchup perf timings", move |_| {
        guard("cmd_MatchupShowTimes", || emit_times(&s));
    });
}

/// Format and echo the perf timings (shared by the `show_times` export and the
/// `:MatchupShowTimes` command callback).
fn emit_times(state: &State) {
    let times = state.perf.times.borrow().clone();
    let mut keys: Vec<&String> = times.keys().collect();
    keys.sort();
    let mut out = String::from("matchup-rs times (emavg / last / max):\n");
    for k in keys {
        let e = &times[k];
        out.push_str(&format!(
            "  {:<40} {:>8.3}ms {:>8.3}ms {:>8.3}ms\n",
            k,
            e.emavg * 1000.0,
            e.last * 1000.0,
            e.maximum * 1000.0
        ));
    }
    // native nvim_echo (oxi's api::echo is ABI-broken on 0.13-dev).
    nvimrs::echo(&out);
}

#[nvim_oxi::plugin]
fn matchup_rs() -> Result<Dictionary> {
    install_panic_hook();
    let state: SharedState = Rc::new(State::new());

    // ---- raw engine API (tests, benchmarks, interop) ----

    let s = Rc::clone(&state);
    let get_delim: Function<(String, String, Option<Dictionary>), Object> =
        Function::from_fn(
            move |(dir, side, opts): (String, String, Option<Dictionary>)| -> nvim_oxi::Result<Object> {
        Ok(guard("get_delim", || -> Object {
        let direction = match parse_direction(&dir) {
            Some(d) => d,
            None => return Object::nil(),
        };
        let sideq = parse_side(&side).unwrap_or(SideQuery::BothAll);
        let mut o = GetDelimOpts::new(direction, sideq);
        if let Some(opts) = opts {
            if let Some(v) = opts.get("insertmode").and_then(obj_bool) {
                o.insertmode = v;
            }
            if let Some(v) = opts.get("highlighting").and_then(obj_bool) {
                o.highlighting = v;
            }
            if let Some(v) = opts.get("stopline").and_then(|v| i64::try_from(v.clone()).ok()) {
                o.stopline = v.max(0) as usize;
            }
            if let Some(v) = opts.get("check_skip").and_then(obj_bool) {
                o.check_skip = Some(v);
            }
            if let (Some(l), Some(c)) = (
                opts.get("lnum").and_then(|v| i64::try_from(v.clone()).ok()),
                opts.get("cnum").and_then(|v| i64::try_from(v.clone()).ok()),
            ) {
                o.at = Some(types::Pos::new(l.max(1) as usize, c.max(1) as usize));
            }
        }
        let r = with_ctx(&s, |ctx| {
            s.perf.timeout_start(0.0); // no budget for raw calls
            engine::get_delim_multi(ctx, &o).map(|d| Object::from(delim_to_dict(&d)))
        });
        r.flatten().unwrap_or_else(Object::nil)
        }))
    });

    let s = Rc::clone(&state);
    let get_matching_at: Function<(i64, i64, Option<bool>), Object> =
        Function::from_fn(
            move |(lnum, cnum, highlighting): (i64, i64, Option<bool>)| -> nvim_oxi::Result<Object> {
        Ok(guard("get_matching_at", || -> Object {
        let r = with_ctx(&s, |ctx| {
            s.perf.timeout_start(0.0);
            let mut o = GetDelimOpts::new(Direction::Current, SideQuery::BothAll);
            o.at = Some(types::Pos::new(lnum.max(1) as usize, cnum.max(1) as usize));
            o.highlighting = highlighting.unwrap_or(false);
            let seed = match engine::get_delim_multi(ctx, &o) {
                Some(d) => d,
                None => return Object::nil(),
            };
            let ml = engine::get_matching(
                ctx,
                &seed,
                &MatchOpts {
                    stopline: 0,
                    highlighting: o.highlighting,
                },
            );
            let arr: Array = ml
                .delims
                .iter()
                .map(|d| {
                    Object::from(Array::from_iter([
                        Object::from(d.match_.clone()),
                        Object::from(d.lnum as i64),
                        Object::from(d.cnum as i64),
                        Object::from(d.side.as_str()),
                        Object::from(d.match_index as i64),
                    ]))
                })
                .collect();
            Object::from(Dictionary::from_iter([
                ("delims", Object::from(arr)),
                ("seed_index", Object::from(ml.seed_index() as i64)),
            ]))
        });
        r.unwrap_or_else(Object::nil)
        }))
    });

    let s = Rc::clone(&state);
    let get_surrounding_at: Function<(Option<i64>, Option<bool>), Object> =
        Function::from_fn(
            move |(count, local): (Option<i64>, Option<bool>)| -> nvim_oxi::Result<Object> {
        Ok(guard("get_surrounding_at", || -> Object {
        let r = with_ctx(&s, |ctx| {
            s.perf.timeout_start(0.0);
            let opts = SurroundOpts {
                local,
                stopline: 0,
                check_skip: false,
                highlighting: false,
            };
            match engine::get_surrounding(ctx, count.unwrap_or(1).max(0) as usize, &opts) {
                Some((open, close, _ml)) => Object::from(Array::from_iter([
                    Object::from(delim_to_dict(&open)),
                    Object::from(delim_to_dict(&close)),
                ])),
                None => Object::nil(),
            }
        });
        r.unwrap_or_else(Object::nil)
        }))
    });

    // ---- matchparen ----

    let s = Rc::clone(&state);
    let highlight: Function<(Option<bool>,), ()> = Function::from_fn(move |(force,): (Option<bool>,)| -> nvim_oxi::Result<()> {
        let f = force.unwrap_or(false);
        guard("highlight", || {
            with_ctx(&s, |ctx| matchparen::highlight(ctx, f, false));
        });
        Ok(())
    });

    let s = Rc::clone(&state);
    let clear: Function<(), ()> = Function::from_fn(move |()| -> nvim_oxi::Result<()> {
        guard("clear", || {
            with_ctx(&s, |ctx| matchparen::clear(ctx));
        });
        Ok(())
    });

    let s = Rc::clone(&state);
    let reload: Function<(), ()> = Function::from_fn(move |()| -> nvim_oxi::Result<()> {
        s.reload();
        Ok(())
    });

    let s = Rc::clone(&state);
    let drop_buf: Function<(i64,), ()> = Function::from_fn(move |(b,): (i64,)| -> nvim_oxi::Result<()> {
        s.drop_buf(b as i32);
        Ok(())
    });

    // :NoMatchParen / :DoMatchParen runtime toggle (via autoload/matchup/rs.vim).
    let s = Rc::clone(&state);
    let set_matchparen_enabled: Function<(bool,), ()> =
        Function::from_fn(move |(on,): (bool,)| -> nvim_oxi::Result<()> {
            s.set_matchparen_enabled(on);
            Ok(())
        });

    // matchup#util#matchpref bridge for the ftplugin definitions: looks up
    // <ft>.<id> in the setup(opts) matchpref table.
    let s = Rc::clone(&state);
    let matchpref: Function<(String, String, bool), bool> =
        Function::from_fn(move |(ft, id, dflt): (String, String, bool)| -> nvim_oxi::Result<bool> {
            let g = s.gopts();
            Ok(g
                .matchpref
                .get(&ft)
                .and_then(|m| m.get(&id))
                .copied()
                .unwrap_or(dflt))
        });

    // FileType autocmd handler: apply the native ftplugin definition.
    let s = Rc::clone(&state);
    let apply_ftplugin: Function<(), ()> = Function::from_fn(move |()| -> nvim_oxi::Result<()> {
        guard("apply_ftplugin", || {
            ftplugin::apply_current(&s);
        });
        Ok(())
    });

    let s = Rc::clone(&state);
    let show_times: Function<(), ()> = Function::from_fn(move |()| -> nvim_oxi::Result<()> {
        guard("show_times", || emit_times(&s));
        Ok(())
    });

    // ---- setup: autocmds, keymaps, commands ----

    let s = Rc::clone(&state);
    let setup: Function<(Option<Dictionary>,), ()> =
        Function::from_fn(move |(opts,): (Option<Dictionary>,)| -> nvim_oxi::Result<()> {
        install_panic_hook();
        guard("setup", || {
            let st = Rc::clone(&s);
            if nvimrs::call_fn_as::<i64>(
                "has",
                &Array::from_iter([Object::from("nvim-0.11.0")]),
            )
            .unwrap_or(0)
                == 0
            {
                nvimrs::echo("matchup-rs requires neovim >= 0.11");
            }
            let gopts = GOpts::from_opts(&GOpts::default(), opts.as_ref());
            st.set_gopts(gopts);
            // One-time activation (highlight groups, matchit/pi_paren
            // neutralization, user commands). Idempotent across re-setup.
            activate(&st);
            ftplugin::setup(&st);
            matchparen::setup(&st);
            motion::setup(&st);
            textobj::setup(&st);
        });
        Ok(())
    });

    // ---- deferred timer callbacks (via vimscript shim) ----

    let s = Rc::clone(&state);
    let timer_callback: Function<(i64,), ()> =
        Function::from_fn(move |(tid,)| -> nvim_oxi::Result<()> {
            guard("timer_callback", || {
                matchparen::timer_callback(&s, tid);
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let fade_timer_callback: Function<(i64,), ()> =
        Function::from_fn(move |(tid,)| -> nvim_oxi::Result<()> {
            guard("fade_timer_callback", || {
                matchparen::fade_timer_callback(&s, tid);
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let highlight_deferred: Function<(), ()> =
        Function::from_fn(move |()| -> nvim_oxi::Result<()> {
            guard("highlight_deferred", || {
                with_ctx(&s, |ctx| matchparen::highlight_deferred(ctx));
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let update: Function<(), ()> = Function::from_fn(move |()| -> nvim_oxi::Result<()> {
        guard("update", || {
            with_ctx(&s, |ctx| matchparen::highlight(ctx, true, false));
        });
        Ok(())
    });





    // ---- motions & text objects (called from <cmd> keymap rhs) ----

    /// Run a motion; if it made no progress while an operator is pending,
    /// feed <esc> so vim does not hang waiting for a motion.
    fn run_motion(state: &SharedState, name: &str, f: impl Fn(&Ctx) -> bool) {
        guard(name, || {
            with_ctx(state, |ctx| {
                let moved = f(ctx);
                if !moved {
                    let m: String = nvimrs::call_fn_as(
                        "mode",
                        &Array::from_iter([Object::from(1i64)]),
                    )
                    .unwrap_or_default();
                    if m.starts_with("no") {
                        let k = nvim_oxi::String::from("\x1b");
                        let md = nvim_oxi::String::from("n");
                        api::feedkeys(&k, &md, false);
                    }
                }
            });
        });
    }

    let s = Rc::clone(&state);
    let motion_matching: Function<(i64, i64), ()> =
        Function::from_fn(move |(visual, down): (i64, i64)| -> nvim_oxi::Result<()> {
            run_motion(&s, "motion_matching", |ctx| {
                motion::find_matching_pair(ctx, visual != 0, down != 0)
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let motion_unmatched: Function<(i64, i64), ()> =
        Function::from_fn(move |(visual, down): (i64, i64)| -> nvim_oxi::Result<()> {
            run_motion(&s, "motion_unmatched", |ctx| {
                motion::find_unmatched(ctx, visual != 0, down != 0, 750.0)
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let motion_jump_inside: Function<(i64,), ()> =
        Function::from_fn(move |(visual,): (i64,)| -> nvim_oxi::Result<()> {
            run_motion(&s, "motion_jump_inside", |ctx| {
                motion::jump_inside(ctx, visual != 0)
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let motion_jump_inside_prev: Function<(i64,), ()> =
        Function::from_fn(move |(visual,): (i64,)| -> nvim_oxi::Result<()> {
            run_motion(&s, "motion_jump_inside_prev", |ctx| {
                motion::jump_inside_prev(ctx, visual != 0)
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let motion_insert: Function<(), ()> =
        Function::from_fn(move |()| -> nvim_oxi::Result<()> {
            guard("motion_insert", || {
                with_ctx(&s, |ctx| motion::insert_mode(ctx));
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let op_motion: Function<(String,), ()> =
        Function::from_fn(move |(plug,): (String,)| -> nvim_oxi::Result<()> {
            guard("op_motion", || {
                with_ctx(&s, |ctx| motion::op_motion(ctx, &plug));
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let textobj: Function<(i64, i64), ()> =
        Function::from_fn(move |(inner, visual): (i64, i64)| -> nvim_oxi::Result<()> {
            guard("textobj", || {
                with_ctx(&s, |ctx| {
                    textobj::delimited(ctx, inner != 0, visual != 0)
                });
            });
            Ok(())
        });

    let s = Rc::clone(&state);
    let update_insert: Function<(), ()> =
        Function::from_fn(move |()| -> nvim_oxi::Result<()> {
            guard("update_insert", || {
                with_ctx(&s, |ctx| matchparen::highlight(ctx, true, true));
            });
            Ok(())
        });

    // Introspection: the resolved per-buffer config the engine actually uses
    // (Rust FtConfig, falling back to nvim-runtime/user b: inputs). Used by the
    // test harnesses now that the plugin no longer writes b: config vars.
    let s = Rc::clone(&state);
    let buffer_config: Function<(), Object> =
        Function::from_fn(move |()| -> nvim_oxi::Result<Object> {
        Ok(guard("buffer_config", || -> Object {
            let buf = api::get_current_buf();
            let h = buf.handle();
            let ftc = s.ft_config(h);
            let bvar = |n: &str| buf.get_var::<String>(n).unwrap_or_default();
            let mw = ftc.match_words.clone().unwrap_or_else(|| bvar("match_words"));
            let ms = ftc.match_skip.clone().unwrap_or_else(|| bvar("match_skip"));
            let mp = nvimrs::get_option_as::<String>("matchpairs", h, 0).unwrap_or_default();
            let ic = buf
                .get_var::<i64>("match_ignorecase")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let mpe = match ftc.matchparen_enabled {
                Some(b) => if b { "1" } else { "0" }.to_string(),
                None => String::new(),
            };
            let mm = ftc.midmap.map(|pairs| {
                Object::from(Array::from_iter(pairs.into_iter().map(|(a, b)| {
                    Object::from(Array::from_iter([Object::from(a), Object::from(b)]))
                })))
            });
            let mut d = Dictionary::from_iter([
                ("match_words", Object::from(mw)),
                ("match_skip", Object::from(ms)),
                ("matchpairs", Object::from(mp)),
                ("ignorecase", Object::from(ic)),
                ("nomatchpairs", Object::from(ftc.nomatchpairs)),
                ("matchparen_enabled", Object::from(mpe)),
            ]);
            if let Some(mm) = mm {
                d.insert("midmap", mm);
            }
            Object::from(d)
        }))
        });

    // Effective cursor position for raw b:match_skip evaluation, held in Rust
    // (State.eff_curpos); the matchup#rs#effline/effcol shims read it back.
    let s = Rc::clone(&state);
    let eff_pos: Function<(), Object> = Function::from_fn(move |()| -> nvim_oxi::Result<Object> {
        Ok(guard("eff_pos", || -> Object {
            let (l, c) = s.eff_pos();
            Object::from(Array::from_iter([Object::from(l), Object::from(c)]))
        }))
    });

    // Offscreen statusline scroll-refresh timer (id held in Rust state).
    let s = Rc::clone(&state);
    let scroll_callback: Function<(i64,), ()> =
        Function::from_fn(move |(tid,)| -> nvim_oxi::Result<()> {
            guard("scroll_callback", || matchparen::scroll_callback(&s, tid));
            Ok(())
        });

    let s = Rc::clone(&state);
    let scroll_update: Function<(i64,), String> =
        Function::from_fn(move |(lnum,)| -> nvim_oxi::Result<String> {
            Ok(guard("scroll_update", || matchparen::scroll_update(&s, lnum)))
        });

    Ok(Dictionary::from_iter([
        ("version", Object::from(env!("CARGO_PKG_VERSION"))),
        ("setup", Object::from(setup)),
        ("get_delim", Object::from(get_delim)),
        ("get_matching_at", Object::from(get_matching_at)),
        ("get_surrounding_at", Object::from(get_surrounding_at)),
        ("highlight", Object::from(highlight)),
        ("highlight_deferred", Object::from(highlight_deferred)),
        ("update", Object::from(update)),
        ("clear", Object::from(clear)),
        ("reload", Object::from(reload)),
        ("drop_buf", Object::from(drop_buf)),
        ("set_matchparen_enabled", Object::from(set_matchparen_enabled)),
        ("matchpref", Object::from(matchpref)),
        ("apply_ftplugin", Object::from(apply_ftplugin)),
        ("show_times", Object::from(show_times)),
        ("buffer_config", Object::from(buffer_config)),
        ("eff_pos", Object::from(eff_pos)),
        ("scroll_callback", Object::from(scroll_callback)),
        ("scroll_update", Object::from(scroll_update)),
        ("timer_callback", Object::from(timer_callback)),
        ("fade_timer_callback", Object::from(fade_timer_callback)),
        ("update_insert", Object::from(update_insert)),
        ("motion_matching", Object::from(motion_matching)),
        ("motion_unmatched", Object::from(motion_unmatched)),
        ("motion_jump_inside", Object::from(motion_jump_inside)),
        ("motion_jump_inside_prev", Object::from(motion_jump_inside_prev)),
        ("motion_insert", Object::from(motion_insert)),
        ("op_motion", Object::from(op_motion)),
        ("textobj", Object::from(textobj)),
    ]))
}
