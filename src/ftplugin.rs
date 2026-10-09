//! Native port of vim-matchup's `after/ftplugin/*.vim`.
//!
//! Each filetype's delimiter configuration (normally applied by nvim sourcing
//! `after/ftplugin/<ft>_matchup.vim` *after* the runtime ftplugin has set
//! `b:match_words`) is reproduced here in Rust and applied from a `FileType`
//! autocmd registered by `setup()`. Because our autocmd is registered after
//! nvim's `$VIMRUNTIME/ftplugin.vim` loader, `b:match_words`/`b:did_ftplugin`
//! are already present when this runs, matching the original ordering.
//!
//! The transformations mirror `autoload/matchup/util.vim`:
//!   * `patch_match_words(from, to)`  - replace the FIRST literal occurrence
//!   * `append_match_words(str)`      - append with a comma separator
//!   * `check_match_words(sha)`       - guard on `sha256(b:match_words)` prefix
//!   * `matchpref(id, default)`       - per-filetype pref from setup(opts)

use std::rc::Rc;

use nvim_oxi::api::{self, Buffer};
use nvim_oxi::{Array, Object};

use crate::state::{FtConfig, GOpts, State};

type SharedState = Rc<State>;

// ---------------------------------------------------------------------------
// config builder
// ---------------------------------------------------------------------------

/// Mutable context threaded through the per-filetype handlers: the config
/// being built (stored in Rust, never in `b:` vars), plus the buffer for
/// option reads/writes and runtime `b:` inputs, and the setup(opts) prefs.
struct Ft<'a> {
    cfg: FtConfig,
    buf: &'a Buffer,
    gopts: &'a GOpts,
}

fn buf_str(buf: &Buffer, name: &str) -> Option<String> {
    buf.get_var::<String>(name).ok()
}

fn buf_exists(buf: &Buffer, name: &str) -> bool {
    buf.get_var::<Object>(name).is_ok()
}

/// Read a buffer-scoped option natively (nvim_get_option_value).
fn buf_opt(buf: &Buffer, name: &str) -> String {
    crate::nvimrs::get_option_as::<String>(name, buf.handle(), 0).unwrap_or_default()
}

/// Set a buffer option via setbufvar; the value passes through a
/// single-quoted vimscript literal.
fn set_buf_opt(buf: &Buffer, name: &str, val: &str) {
    let v = crate::motion::vim_quote(val);
    let _ = api::command(&format!(
        "call setbufvar({}, '&{}', {})",
        buf.handle(),
        name,
        v
    ));
}

/// The match_words being built, seeded lazily from the runtime base
/// `b:match_words` (nvim/matchit input) on first modification.
fn mw<'f, 'b>(f: &'f mut Ft<'b>) -> &'f mut String {
    if f.cfg.match_words.is_none() {
        f.cfg.match_words = Some(buf_str(f.buf, "match_words").unwrap_or_default());
    }
    f.cfg.match_words.as_mut().unwrap()
}

/// Append a delimiter set with a comma separator (port of
/// matchup#util#append_match_words).
fn append(f: &mut Ft, s: &str) {
    let mw = mw(f);
    if !mw.is_empty() && !mw.ends_with(',') && !s.starts_with(',') {
        mw.push(',');
    }
    mw.push_str(s);
}

/// Replace the FIRST literal occurrence of `from` with `to` (port of
/// matchup#util#patch_match_words).
fn patch(f: &mut Ft, from: &str, to: &str) {
    let mw = mw(f);
    if let Some(idx) = mw.find(from) {
        mw.replace_range(idx..idx + from.len(), to);
    }
}

/// True when the runtime base `b:match_words` sha256 begins with `prefix`
/// (port of matchup#util#check_match_words). Uses Vim's sha256() so the digest
/// guards tracking the runtime ftplugin's exact b:match_words stay valid.
fn check(buf: &Buffer, prefix: &str) -> bool {
    let m = match buf_str(buf, "match_words") {
        Some(m) => m,
        None => return false,
    };
    let hash: String =
        crate::nvimrs::call_fn_as("sha256", &Array::from_iter([Object::from(m)]))
            .unwrap_or_default();
    hash.starts_with(prefix)
}

/// Per-filetype pref from setup(opts) (port of matchup#util#matchpref).
fn matchpref(gopts: &GOpts, ft: &str, id: &str, default: bool) -> bool {
    gopts
        .matchpref
        .get(ft)
        .and_then(|m| m.get(id))
        .copied()
        .unwrap_or(default)
}

fn set_midmap(f: &mut Ft, pairs: &[(&str, &str)]) {
    f.cfg.midmap = Some(
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
    );
}

// ---------------------------------------------------------------------------
// per-filetype definitions
// ---------------------------------------------------------------------------

fn ft_c(f: &mut Ft) {
    if check(f.buf, "bb2bcbee") {
        append(f, r"/\*:\*/");
    }
}

fn ft_javascript(f: &mut Ft) {
    if check(f.buf, "802f71c1") {
        append(f, r"/\*:\*/");
    }
}

fn ft_cpp(f: &mut Ft) {
    if matchpref(f.gopts, "cpp", "template", false) {
        append(f, r"\%(\s\@<!<\|<\s\@!\)[=(]\@!:\%(\s\@<!>\|>\s\@!\)=\@!");
        // `setlocal matchpairs-=<:>` unless "<:>" sits at index 0 (faithful to
        // the original's `if stridx(&matchpairs, '<:>')`, which is truthy for
        // "absent" (-1) and "found later" (>0) but falsy at index 0).
        let mp = buf_opt(f.buf, "matchpairs");
        if mp.find("<:>").map(|i| i as i64).unwrap_or(-1) != 0 {
            let kept: Vec<&str> = mp.split(',').filter(|p| *p != "<:>").collect();
            set_buf_opt(f.buf, "matchpairs", &kept.join(","));
        }
    }
}

fn ft_fortran(f: &mut Ft) {
    patch(f, r"\<if", r"\<if\>\g{hlend}");
    patch(f, r"\<else\s*\%(if", r"\<else\g{hlend}\s*\%(if\g{hlend}");
    append(
        f,
        r"^\s*#\s*if\(\|def\|ndef\)\>:^\s*#\s*elif\>:^\s*#\s*else\>:^\s*#\s*endif\>",
    );
}

fn ft_ruby(f: &mut Ft) {
    patch(f, "retry", r"retry\|return");
    set_midmap(f, &[("rubyRepeat", "next"), ("rubyDefine", "return")]);
}

fn ft_lua(f: &mut Ft) {
    set_midmap(f, &[("luaFunction", "return")]);
    append(f, r"--\[\(=*\)\[:]\1]");
}

fn ft_janet(f: &mut Ft) {
    append(f, r"``:``\g{syn;!JanetString}");
}

fn ft_ocaml(f: &mut Ft) {
    f.cfg.matchparen_timeout = Some(100);
}

fn ft_vim(f: &mut Ft) {
    f.cfg.match_skip = Some(
        r"s:comment\|string\|vimSynReg\|vimSet\|vimFuncName\|vimNotPatSep\|vimVar\|vimFuncVar\|vimFBVar\|vimOperParen\|vimUserFunc"
            .to_string(),
    );
    patch(
        f,
        r"\<aug\%[roup]\s\+\%(END\>\)\@!\S:",
        r"\<aug\%[roup]\ze\s\+\%(END\>\)\@!\S:",
    );
    patch(
        f,
        r"\|def\)!\=\s\+",
        r"\|\%(export\s\+\)\@<!def\|export\s\+def\)\ze!\=\s\+",
    );
}

/// The four patches shared by html/xml/jsx/tsx under matchpref('tagnameonly').
/// html uses a different set; xml/jsx/tsx share this exact sequence.
fn tagnameonly_xmlish(f: &mut Ft) {
    patch(f, r"\)\%(", r"\)\g{hlend}\%(");
    patch(f, r"\)\%(", r"\)\g{hlend}\%(");
    patch(f, "1>", r"1\g{hlend}>");
    patch(f, ":/>", r":/\g{hlend}>");
}

fn ft_xml(f: &mut Ft) {
    if matchpref(f.gopts, "xml", "tagnameonly", false) {
        tagnameonly_xmlish(f);
    }
    patch(f, "[^/>]*", "[^>]*[^/>]");
}

fn ft_tsx(f: &mut Ft) {
    f.cfg.match_skip = Some(r"s:\%(comment\|string\)\%(tsxCloseString\)\@<!".to_string());
    if matchpref(f.gopts, "typescriptreact", "tagnameonly", false) {
        tagnameonly_xmlish(f);
    }
}

fn ft_jsx(f: &mut Ft) {
    f.cfg.match_skip = Some(r"s:\%(comment\|string\)\%(jsxCloseString\)\@<!".to_string());
    if matchpref(f.gopts, "javascriptreact", "tagnameonly", false) {
        tagnameonly_xmlish(f);
    }
}

fn ft_html(f: &mut Ft, ft: &str) {
    patch(
        f,
        r"[^ \t>]*\)[^>]*\%(>\|$\):<\@<=/\1>",
        r"[^ \t>]*\)\%(>\|$\|[ \t][^>]*\%(>\|$\)\):<\@<=/\1>",
    );
    // default folded from g:matchup_matchpref_html_nolists -> false (clean break).
    // matchpref is keyed on the ACTUAL filetype, so vue/htmlangular look up
    // their own prefs (as html_matchup.vim does via &filetype), not "html".
    if matchpref(f.gopts, ft, "nolists", false) {
        patch(f, r"<\@<=[ou]l\>[^>]*\%(>\|$\):<\@<=li\>:<\@<=/[ou]l>", "");
        patch(f, r"<\@<=dl\>[^>]*\%(>\|$\):<\@<=d[td]\>:<\@<=/dl>", "");
    }
    if matchpref(f.gopts, ft, "tagnameonly", false) {
        patch(f, r"\)\%(", r"\)\g{hlend}\%(");
        patch(f, r"]l\>[", r"]l\>\g{hlend}[");
        patch(f, r"dl\>", r"dl\>\g{hlend}");
        patch(f, "1>", r"1\g{hlend}>");
        patch(f, "]l>", r"]l\g{hlend}>");
        patch(f, "dl>", r"dl\g{hlend}>");
    }
}

/// Port of tex_matchup.vim's s:get_match_words(). Vim single-quoted literals
/// and Rust raw strings both keep backslashes verbatim, so each piece is a
/// 1:1 transcription.
fn tex_match_words(gopts: &GOpts, buf: &Buffer) -> String {
    let not_bslash = r"\v%(\\@<!%(\\\\)*)@4<=\m";
    let delim = r"\%(\\\w\+\>\|\\[|{}]\|.\)";
    let wdelim = r"\%(angle\|floor\|ceil\|[vV]ert\|brace\)\>";
    let nomod = r"\%(\\left\|\\right\|\[\@1<!\\[bB]igg\?[lr]\?\)\@6<!";
    let mmod = r"\(\\[bB]igg\?\)";

    let mut mw = String::new();
    // left/middle/right modifiers, any delimiters
    mw.push_str(r"\\left\>");
    mw.push_str(delim);
    mw.push_str(r":\\middle\>");
    mw.push_str(delim);
    mw.push_str(r":\\right\>");
    mw.push_str(delim);
    mw.push_str(r",\(\\[bB]igg\?\)l\>");
    mw.push_str(delim);
    mw.push_str(r":\1m\>");
    mw.push_str(delim);
    mw.push_str(r":\1r\>");
    mw.push_str(delim);

    // un-sided sized, left and right delimiters
    let mtopt = r"\%(\%(\w\[\)\@2<!\|\%(\\[bB]igg\?\[\)\@6<=\)";
    mw.push(',');
    mw.push_str(mmod);
    mw.push_str(r"\%(\\l");
    mw.push_str(wdelim);
    mw.push_str(r"\|\\[lu]lcorner\>\|(\|\[\|\\{");
    mw.push_str(r"\)");
    mw.push_str(r":\1");
    mw.push_str(r"\%(\\vert\>\||\|\\|\)");
    mw.push_str(":");
    mw.push_str(mtopt);
    mw.push_str(r"\1");
    mw.push_str(r"\%(\\r");
    mw.push_str(wdelim);
    mw.push_str(r"\|\\[lu]rcorner\>\|)\|]\|\\}\)");

    // unmodified delimiters
    for pair in [
        (r"\\{", r"\\}"),
        (r"\[", "]"),
        ("(", ")"),
        (r"\\[lu]lcorner", r"\\[lu]rcorner"),
    ] {
        mw.push(',');
        mw.push_str(nomod);
        mw.push_str(not_bslash);
        mw.push_str(pair.0);
        mw.push(':');
        mw.push_str(nomod);
        mw.push_str(not_bslash);
        mw.push_str(pair.1);
    }
    mw.push(',');
    mw.push_str(nomod);
    mw.push_str(not_bslash);
    mw.push_str(r"\\l\(");
    mw.push_str(wdelim);
    mw.push_str(r"\)");
    mw.push(':');
    mw.push_str(nomod);
    mw.push_str(not_bslash);
    mw.push_str(r"\\r\1\>");

    // the curly braces
    mw.push_str(",{:}");

    // latex equation markers
    mw.push_str(r",\\(:\\),");
    mw.push_str(not_bslash);
    mw.push_str(r"\\\[");
    mw.push_str(r":\\]");

    // latex3 file i/o
    mw.push_str(r",\\ior_open\:NnT\?F\?\s*\\\([^\s]*\):\\ior_close\:N\s*\\\1");
    mw.push_str(r",\\ior_open\:cnT\?F\?\s*{\s*\([^\s\\}]*\)\s*}:\\ior_close\:c\s*{\s*\1\s*}");
    mw.push_str(r",\\iow_open\:Nn\s*\\\([^\s]*\):\\iow_close\:N\s*\\\1");
    mw.push_str(r",\\iow_open\:cn\s*{\s*\([^\s\\}]*\)\s*}:\\iow_close\:c\s*{\s*\1\s*}");

    // simple blocks
    mw.push_str(r",\\if\%(\:w\|\%(\w\|@\)*\)\>:\\else\:\?\>:\\fi\:\?\>");
    mw.push_str(r",\\if_\%(true\|false\)\::\\else\::\\fi\:");
    mw.push_str(r",\\if_mode_\%(horizontal\|vertical\|math\|inner\)\::\\else\::\\fi\:");
    mw.push_str(r",\\if_\%(charcode\|catcode\|dim\)\:w:\\else\::\\fi\:");
    mw.push_str(r",\\if_cs_exist\:w:\\cs_end\::\\else\::\\fi\:");
    mw.push_str(r",\\if_cs_exist\:N:\\else\::\\fi\:");
    mw.push_str(r",\\if_[hv]box\:N:\\else\::\\fi\:");
    mw.push_str(r",\\if_box_empty\:N:\\else\::\\fi\:");
    mw.push_str(r",\\cs\:w:\\cs_end\:");
    mw.push_str(r",\\makeatletter:\\makeatother");
    mw.push_str(r",\\ExplSyntaxOn:\\ExplSyntaxOff");
    mw.push_str(r",\\debug_suspend\::\\debug_resume\:");
    mw.push_str(r",\\begingroup:\\endgroup,\\bgroup:\\egroup");
    mw.push_str(r",\\group_begin\::\\group_end\:");
    mw.push_str(r",\\group_align_safe_begin\::\\group_align_safe_end\:");
    mw.push_str(r",\\color_group_begin\::\\color_group_end\:");
    mw.push_str(r",\\cctab_begin\:[Nc]:\\cctab_end\:");
    mw.push_str(r",\\exp\:w:\\exp_end\(_continue_f\:n\?w\|\:\)");

    // environments
    mw.push_str(r",\\begin{tabular}");
    mw.push_str(r":\\toprule\>:\\midrule\>:\\bottomrule\>");
    mw.push_str(r":\\end{tabular}");
    mw.push_str(r",\\begin\s*{\(enumerate\*\=\|itemize\*\=\)}");
    mw.push_str(r":\\item\>:\\end\s*{\1}");

    // generic environment
    if matchpref(gopts, "tex", "relax_env", false) {
        mw.push_str(r",\\begin\s*{\([^}]*\)}:\\end\s*{\([^}]*\)}");
    } else {
        mw.push_str(r",\\begin\s*{\([^}]*\)}:\\end\s*{\1}");
    }

    // dollar sign math
    if buf_exists(buf, "vimtex") {
        mw.push_str(r",\$:\$\g{syn;!texMathZoneTI}");
    } else {
        mw.push_str(r",\$:\$\g{syn;!texMathZoneX}");
    }

    mw
}

fn tex_setup_match_words(f: &mut Ft) {
    set_buf_opt(f.buf, "matchpairs", "(:),{:},[:]");
    f.cfg.nomatchpairs = true;
    // match_words is set by the caller (needs gopts for matchpref)
    f.cfg.match_skip = Some(r"r:\\\@<!\%(\\\\\)*%".to_string());
}

fn ft_tex(f: &mut Ft) {
    // vimtex detection reads external plugin state (not a matchup option):
    // g:vimtex_enabled override, else exists('*vimtex#init')/g:vimtex_version.
    let vimtex_active = crate::nvimrs::get_var_as::<i64>("vimtex_enabled")
        .map(|v| v != 0)
        .unwrap_or_else(|| {
            crate::nvimrs::call_fn_as::<i64>(
                "exists",
                &Array::from_iter([Object::from("*vimtex#init")]),
            )
            .unwrap_or(0)
                != 0
                || crate::nvimrs::get_var_as::<Object>("vimtex_version").is_some()
        });
    let override_vimtex = matchpref(f.gopts, "tex", "override_vimtex", false);

    if vimtex_active {
        if override_vimtex {
            let _ = api::command(
                "silent! nunmap <buffer> % | silent! xunmap <buffer> % | silent! ounmap <buffer> %",
            );
            let _ = api::set_var("vimtex_matchparen_enabled", 0);
            let _ = api::command("silent! call vimtex#matchparen#disable()");
            tex_setup_match_words(f);
            let mww = tex_match_words(f.gopts, f.buf);
            f.cfg.match_words = Some(mww);
        } else {
            f.cfg.matchparen_enabled = Some(false);
        }
    } else {
        tex_setup_match_words(f);
        let mww = tex_match_words(f.gopts, f.buf);
        f.cfg.match_words = Some(mww);
    }
}

// ---------------------------------------------------------------------------
// dispatch + autocmd
// ---------------------------------------------------------------------------

/// Apply the filetype definition for `buf` (its `&filetype`), storing the
/// derived config in Rust (`State.ft_config`) rather than in `b:` vars.
fn apply(state: &SharedState, buf: &Buffer) {
    if !buf_exists(buf, "did_ftplugin") {
        return;
    }
    let ft = buf_opt(buf, "filetype");
    let gopts = state.gopts();
    let mut f = Ft { cfg: FtConfig::default(), buf, gopts: &gopts };
    match ft.as_str() {
        "c" => ft_c(&mut f),
        "cpp" => ft_cpp(&mut f),
        "fortran" => ft_fortran(&mut f),
        "html" | "vue" | "htmlangular" => ft_html(&mut f, &ft),
        "janet" => ft_janet(&mut f),
        "javascript" => ft_javascript(&mut f),
        "javascriptreact" => ft_jsx(&mut f),
        "lua" => ft_lua(&mut f),
        "ocaml" => ft_ocaml(&mut f),
        "ruby" => ft_ruby(&mut f),
        "tex" => ft_tex(&mut f),
        "typescriptreact" => ft_tsx(&mut f),
        "vim" => ft_vim(&mut f),
        "xml" => ft_xml(&mut f),
        _ => {}
    }
    state.set_ft_config(buf.handle(), f.cfg);
}

/// Entry point for the FileType autocmd / manual apply: current buffer.
pub fn apply_current(state: &SharedState) {
    let buf = api::get_current_buf();
    apply(state, &buf);
}

pub fn setup(state: &SharedState) {
    let group = match api::create_augroup(
        "matchup_ftplugin",
        &api::opts::CreateAugroupOpts::builder().clear(true).build(),
    ) {
        Ok(g) => g,
        Err(_) => return,
    };
    // Native FileType callback (see nvimrs: oxi 0.6's .callback never fires on
    // nvim 0.13). Applies the ft's definition to the event's buffer.
    let s = Rc::clone(state);
    crate::nvimrs::create_autocmd_cb(
        &["FileType"],
        group as i32,
        "*",
        move |a: nvim_oxi::api::types::AutocmdCallbackArgs| {
            crate::guard("ac_ftplugin", || {
                apply(&s, &a.buffer);
            });
            false
        },
    );
    // Apply to the current buffer too (its FileType may have fired pre-setup).
    apply_current(state);
}
