" vim match-up (Rust engine) - vimscript shims for the Rust module
"
" These functions bridge the Rust engine (matchup_rs) back into
" vimscript for the few things that must run there: timer callbacks,
" raw skip-expression evaluation at effective positions, and the
" indentexpr detection trick.

let s:eff_curpos = [1, 1]

function! matchup#rs#effline(expr) abort
  return a:expr ==# '.' ? s:eff_curpos[0] : line(a:expr)
endfunction

function! matchup#rs#effcol(expr) abort
  return a:expr ==# '.' ? s:eff_curpos[1] : col(a:expr)
endfunction

function! matchup#rs#geteffline(expr) abort
  return a:expr ==# '.' ? getline(s:eff_curpos[0]) : getline(a:expr)
endfunction

" Evaluate a raw b:match_skip expression at an effective position.
" SECURITY NOTE: mirrors vim-matchup, which evaluates b:matchup_delim_skip
" with `execute 'return' ...` (delim.vim:881). The expression comes from
" buffer-local ftplugin config; setting it already implies vimscript
" execution capability.
function! matchup#rs#skip_eval(expr, lnum, cnum) abort
  let s:eff_curpos = [a:lnum, a:cnum]
  try
    return eval(a:expr) ? 1 : 0
  catch
    return 0
  endtry
endfunction

" deferred-highlight debounce timer callback
function! matchup#rs#timer_cb(tid) abort
  call luaeval("require('matchup_rs').timer_callback(_A)", a:tid)
endfunction

" fade timer callback
function! matchup#rs#fade_timer_cb(tid) abort
  call luaeval("require('matchup_rs').fade_timer_callback(_A)", a:tid)
endfunction

" true while evaluating an indent expression (motion.vim:152 trick)
function! matchup#rs#in_indentexpr() abort
  try
    throw '_'
  catch /_/
    if v:throwpoint =~# 'GetVimIndent'
      return 1
    endif
  endtry
  return 0
endfunction

" undo helper for the invalid-text-object case (text_obj.vim:279)
function! matchup#rs#text_obj_undo(seq) abort
  if undotree().seq_cur > a:seq
    silent! undo
  endif
endfunction

" :NoMatchParen / :DoMatchParen
function! matchup#rs#toggle(val) abort
  let g:matchup_matchparen_enabled = a:val
  call luaeval("require('matchup_rs').clear()")
  if a:val
    call luaeval("require('matchup_rs').update()")
  endif
endfunction

" vim: fdm=marker sw=2
