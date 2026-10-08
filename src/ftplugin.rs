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

use nvim_oxi::api::{self, opts::CreateAutocmdOpts, Buffer};
use nvim_oxi::{Array, Object};

use crate::state::{GOpts, State};

type SharedState = Rc<State>;

// ---------------------------------------------------------------------------
// buffer helpers
// ---------------------------------------------------------------------------

fn buf_str(buf: &Buffer, name: &str) -> Option<String> {
    buf.get_var::<String>(name).ok()
}

fn buf_exists(buf: &Buffer, name: &str) -> bool {
    buf.get_var::<Object>(name).is_ok()
}

fn set_str(buf: &Buffer, name: &str, val: &str) {
    let _ = buf.clone().set_var(name, val.to_string());
}

fn set_i64(buf: &Buffer, name: &str, val: i64) {
    let _ = buf.clone().set_var(name, val);
}

/// Read a buffer option via getbufvar - avoids the deprecated option API and
/// the nvim 0.13-dev call ABI issue (integer handle interpolation only).
fn buf_opt(buf: &Buffer, name: &str) -> String {
    api::eval::<String>(&format!("getbufvar({}, '&{}')", buf.handle(), name))
        .unwrap_or_default()
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

/// Port of matchup#util#patch_match_words: substitute the first literal
/// occurrence of `from` with `to` in b:match_words (no-op if absent/unset).
fn patch(buf: &Buffer, from: &str, to: &str) {
    let mw = match buf_str(buf, "match_words") {
        Some(m) => m,
        None => return,
    };
    if let Some(idx) = mw.find(from) {
        let mut out = String::with_capacity(mw.len());
        out.push_str(&mw[..idx]);
        out.push_str(to);
        out.push_str(&mw[idx + from.len()..]);
        let _ = buf.clone().set_var("match_words", out);
    }
}

/// Port of matchup#util#append_match_words.
fn append(buf: &Buffer, s: &str) {
    let mut mw = buf_str(buf, "match_words").unwrap_or_default();
    if !mw.is_empty() && !mw.ends_with(',') && !s.starts_with(',') {
        mw.push(',');
    }
    mw.push_str(s);
    let _ = buf.clone().set_var("match_words", mw);
}

/// Port of matchup#util#check_match_words: true when b:match_words exists and
/// its sha256() begins with `prefix`. Uses Vim's sha256() so the digest guards
/// (which track the runtime ftplugin's exact b:match_words) stay valid.
fn check(buf: &Buffer, prefix: &str) -> bool {
    if !buf_exists(buf, "match_words") {
        return false;
    }
    // prefix is hex; operate on the current buffer (the FileType target).
    api::eval::<i64>(&format!(
        "sha256(b:match_words) =~# '^{}' ? 1 : 0",
        prefix
    ))
    .unwrap_or(0)
        != 0
}

/// Port of matchup#util#matchpref, reading the setup(opts) matchpref table.
fn matchpref(gopts: &GOpts, ft: &str, id: &str, default: bool) -> bool {
    gopts
        .matchpref
        .get(ft)
        .and_then(|m| m.get(id))
        .copied()
        .unwrap_or(default)
}

/// Append to b:undo_ftplugin so a later filetype change clears our vars.
fn add_undo(buf: &Buffer, cmd: &str) {
    let mut u = buf_str(buf, "undo_ftplugin").unwrap_or_default();
    if !u.is_empty() {
        u.push('|');
    }
    u.push_str(cmd);
    let _ = buf.clone().set_var("undo_ftplugin", u);
}

fn set_midmap(buf: &Buffer, pairs: &[(&str, &str)]) {
    let arr = Array::from_iter(pairs.iter().map(|(a, b)| {
        Object::from(Array::from_iter([Object::from(*a), Object::from(*b)]))
    }));
    let _ = buf.clone().set_var("match_midmap", arr);
}

// ---------------------------------------------------------------------------
// per-filetype definitions
// ---------------------------------------------------------------------------

fn ft_c(buf: &Buffer) {
    if check(buf, "bb2bcbee") {
        append(buf, r"/\*:\*/");
    }
}

fn ft_javascript(buf: &Buffer) {
    if check(buf, "802f71c1") {
        append(buf, r"/\*:\*/");
    }
}

fn ft_cpp(gopts: &GOpts, buf: &Buffer) {
    if matchpref(gopts, "cpp", "template", false) {
        append(buf, r"\%(\s\@<!<\|<\s\@!\)[=(]\@!:\%(\s\@<!>\|>\s\@!\)=\@!");
        // `setlocal matchpairs-=<:>` unless "<:>" sits at index 0 (faithful to
        // the original's `if stridx(&matchpairs, '<:>')`, which is truthy for
        // "absent" (-1) and "found later" (>0) but falsy at index 0).
        let mp = buf_opt(buf, "matchpairs");
        if mp.find("<:>").map(|i| i as i64).unwrap_or(-1) != 0 {
            let kept: Vec<&str> = mp.split(',').filter(|p| *p != "<:>").collect();
            set_buf_opt(buf, "matchpairs", &kept.join(","));
        }
    }
}

fn ft_fortran(buf: &Buffer) {
    patch(buf, r"\<if", r"\<if\>\g{hlend}");
    patch(buf, r"\<else\s*\%(if", r"\<else\g{hlend}\s*\%(if\g{hlend}");
    append(
        buf,
        r"^\s*#\s*if\(\|def\|ndef\)\>:^\s*#\s*elif\>:^\s*#\s*else\>:^\s*#\s*endif\>",
    );
}

fn ft_ruby(buf: &Buffer) {
    patch(buf, "retry", r"retry\|return");
    set_midmap(buf, &[("rubyRepeat", "next"), ("rubyDefine", "return")]);
    if buf_exists(buf, "undo_ftplugin") {
        add_undo(buf, "unlet! b:match_midmap");
    }
}

fn ft_lua(buf: &Buffer) {
    set_midmap(buf, &[("luaFunction", "return")]);
    add_undo(buf, " unlet! b:match_midmap");
    append(buf, r"--\[\(=*\)\[:]\1]");
}

fn ft_janet(buf: &Buffer) {
    append(buf, r"``:``\g{syn;!JanetString}");
}

fn ft_ocaml(buf: &Buffer) {
    set_i64(buf, "matchup_matchparen_timeout", 100);
    add_undo(buf, " unlet! b:matchup_matchparen_timeout");
}

fn ft_vim(buf: &Buffer) {
    set_str(
        buf,
        "match_skip",
        r"s:comment\|string\|vimSynReg\|vimSet\|vimFuncName\|vimNotPatSep\|vimVar\|vimFuncVar\|vimFBVar\|vimOperParen\|vimUserFunc",
    );
    patch(
        buf,
        r"\<aug\%[roup]\s\+\%(END\>\)\@!\S:",
        r"\<aug\%[roup]\ze\s\+\%(END\>\)\@!\S:",
    );
    patch(
        buf,
        r"\|def\)!\=\s\+",
        r"\|\%(export\s\+\)\@<!def\|export\s\+def\)\ze!\=\s\+",
    );
}

/// The four patches shared by html/xml/jsx/tsx under matchpref('tagnameonly').
/// html uses a different set; xml/jsx/tsx share this exact sequence.
fn tagnameonly_xmlish(buf: &Buffer) {
    patch(buf, r"\)\%(", r"\)\g{hlend}\%(");
    patch(buf, r"\)\%(", r"\)\g{hlend}\%(");
    patch(buf, "1>", r"1\g{hlend}>");
    patch(buf, ":/>", r":/\g{hlend}>");
}

fn ft_xml(gopts: &GOpts, buf: &Buffer) {
    if matchpref(gopts, "xml", "tagnameonly", false) {
        tagnameonly_xmlish(buf);
    }
    patch(buf, "[^/>]*", "[^>]*[^/>]");
}

fn ft_tsx(gopts: &GOpts, buf: &Buffer) {
    set_str(buf, "match_skip", r"s:\%(comment\|string\)\%(tsxCloseString\)\@<!");
    if matchpref(gopts, "typescriptreact", "tagnameonly", false) {
        tagnameonly_xmlish(buf);
    }
}

fn ft_jsx(gopts: &GOpts, buf: &Buffer) {
    set_str(buf, "match_skip", r"s:\%(comment\|string\)\%(jsxCloseString\)\@<!");
    if matchpref(gopts, "javascriptreact", "tagnameonly", false) {
        tagnameonly_xmlish(buf);
    }
}

fn ft_html(gopts: &GOpts, buf: &Buffer, ft: &str) {
    patch(
        buf,
        r"[^ \t>]*\)[^>]*\%(>\|$\):<\@<=/\1>",
        r"[^ \t>]*\)\%(>\|$\|[ \t][^>]*\%(>\|$\)\):<\@<=/\1>",
    );
    // default folded from g:matchup_matchpref_html_nolists -> false (clean break).
    // matchpref is keyed on the ACTUAL filetype, so vue/htmlangular look up
    // their own prefs (as html_matchup.vim does via &filetype), not "html".
    if matchpref(gopts, ft, "nolists", false) {
        patch(buf, r"<\@<=[ou]l\>[^>]*\%(>\|$\):<\@<=li\>:<\@<=/[ou]l>", "");
        patch(buf, r"<\@<=dl\>[^>]*\%(>\|$\):<\@<=d[td]\>:<\@<=/dl>", "");
    }
    if matchpref(gopts, ft, "tagnameonly", false) {
        patch(buf, r"\)\%(", r"\)\g{hlend}\%(");
        patch(buf, r"]l\>[", r"]l\>\g{hlend}[");
        patch(buf, r"dl\>", r"dl\>\g{hlend}");
        patch(buf, "1>", r"1\g{hlend}>");
        patch(buf, "]l>", r"]l\g{hlend}>");
        patch(buf, "dl>", r"dl\g{hlend}>");
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

fn tex_setup_match_words(buf: &Buffer) {
    set_buf_opt(buf, "matchpairs", "(:),{:},[:]");
    set_i64(buf, "matchup_delim_nomatchpairs", 1);
    // match_words is set by the caller (needs gopts for matchpref)
    set_str(buf, "match_skip", r"r:\\\@<!\%(\\\\\)*%");
    set_i64(buf, "matchup_regexpengine", 1);
    add_undo(
        buf,
        "unlet! b:matchup_delim_nomatchpairs b:match_words b:match_skip b:matchup_regexpengine",
    );
}

fn ft_tex(gopts: &GOpts, buf: &Buffer) {
    // vimtex detection reads external plugin state (not a matchup option), so
    // it stays a g:/exists() probe; the matchup-side override is a matchpref.
    let vimtex_active = api::eval::<i64>(
        "get(g:, 'vimtex_enabled', exists('*vimtex#init') || exists('g:vimtex_version') ? 1 : 0)",
    )
    .unwrap_or(0)
        != 0;
    let override_vimtex = matchpref(gopts, "tex", "override_vimtex", false);

    if vimtex_active {
        if override_vimtex {
            let _ = api::command(
                "silent! nunmap <buffer> % | silent! xunmap <buffer> % | silent! ounmap <buffer> %",
            );
            let _ = api::set_var("vimtex_matchparen_enabled", 0);
            let _ = api::command("silent! call vimtex#matchparen#disable()");
            tex_setup_match_words(buf);
            let mw = tex_match_words(gopts, buf);
            set_str(buf, "match_words", &mw);
        } else {
            set_i64(buf, "matchup_matchparen_enabled", 0);
            set_i64(buf, "matchup_matchparen_fallback", 0);
        }
    } else {
        tex_setup_match_words(buf);
        let mw = tex_match_words(gopts, buf);
        set_str(buf, "match_words", &mw);
    }
}

// ---------------------------------------------------------------------------
// dispatch + autocmd
// ---------------------------------------------------------------------------

/// Apply the filetype definition for `buf` (its `&filetype`).
fn apply(state: &SharedState, buf: &Buffer) {
    if !buf_exists(buf, "did_ftplugin") {
        return;
    }
    let ft = buf_opt(buf, "filetype");
    let gopts = state.gopts();
    match ft.as_str() {
        "c" => ft_c(buf),
        "cpp" => ft_cpp(&gopts, buf),
        "fortran" => ft_fortran(buf),
        "html" | "vue" | "htmlangular" => ft_html(&gopts, buf, &ft),
        "janet" => ft_janet(buf),
        "javascript" => ft_javascript(buf),
        "javascriptreact" => ft_jsx(&gopts, buf),
        "lua" => ft_lua(buf),
        "ocaml" => ft_ocaml(buf),
        "ruby" => ft_ruby(buf),
        "tex" => ft_tex(&gopts, buf),
        "typescriptreact" => ft_tsx(&gopts, buf),
        "vim" => ft_vim(buf),
        "xml" => ft_xml(&gopts, buf),
        _ => {}
    }
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
    let mut b = CreateAutocmdOpts::builder();
    b.group(group)
        .patterns(["*"])
        .command("lua require('matchup_rs').apply_ftplugin()");
    if let Err(e) = api::create_autocmd(["FileType"], &b.build()) {
        crate::matchparen::trace(&format!("ftplugin autocmd failed: {e:?}"));
    }
    // Apply to the current buffer too (its FileType may have fired pre-setup).
    apply_current(state);
}
