" vim match-up (Rust engine) - stateless vimscript shims
"
" These bridge the Rust engine into vimscript for the few operations that must
" run there: timer callbacks, raw b:match_skip eval() at an effective position,
" the operator-pending `normal` re-feed, and the indentexpr throwpoint trick.
" ALL plugin state and config lives in Rust (the `State` struct); nothing here
" stores state.

" Effective-position accessors. The candidate position is held in Rust
" (State.eff_curpos) and set there before matchup#rs#skip_eval runs; these read
" it back so a raw b:match_skip expression's line('.')/col('.')/getline('.')
" resolve at the candidate delimiter rather than the real cursor.
function! matchup#rs#effline(expr) abort
  return a:expr ==# '.' ? luaeval("require('matchup_rs').eff_pos()[1]") : line(a:expr)
endfunction

function! matchup#rs#effcol(expr) abort
  return a:expr ==# '.' ? luaeval("require('matchup_rs').eff_pos()[2]") : col(a:expr)
endfunction

function! matchup#rs#geteffline(expr) abort
  return a:expr ==# '.'
        \ ? getline(luaeval("require('matchup_rs').eff_pos()[1]"))
        \ : getline(a:expr)
endfunction

" Evaluate a raw b:match_skip expression. Rust has already stored the effective
" position, so effline/effcol above resolve correctly during the eval.
" SECURITY NOTE: mirrors vim-matchup, which evaluates b:matchup_delim_skip with
" `execute 'return' ...` (delim.vim:881). The expression comes from buffer-local
" ftplugin config; setting it already implies vimscript execution capability.
function! matchup#rs#skip_eval(expr) abort
  try
    return eval(a:expr) ? 1 : 0
  catch
    return 0
  endtry
endfunction

" Debounce timer callbacks (the timer ids are held in Rust state).
function! matchup#rs#timer_cb(tid) abort
  call luaeval("require('matchup_rs').timer_callback(_A)", a:tid)
endfunction

function! matchup#rs#fade_timer_cb(tid) abort
  call luaeval("require('matchup_rs').fade_timer_callback(_A)", a:tid)
endfunction

" Offscreen statusline scroll refresh: the timer id lives in Rust
" (State.scroll_timer); these just forward to the Rust handlers.
function! matchup#rs#scroll_callback(tid) abort
  call luaeval("require('matchup_rs').scroll_callback(_A)", a:tid)
endfunction

function! matchup#rs#scroll_update(lnum) abort
  return luaeval("require('matchup_rs').scroll_update(_A)", a:lnum)
endfunction

" Re-feed keys for an operator-pending motion (port of matchup#motion#op).
" Args are passed directly from Rust (no global state).
function! matchup#rs#op_exec(wise, count, plug) abort
  execute 'normal' a:wise . (a:count > 0 ? a:count : '')
        \ . "\<Plug>(" . a:plug . ")"
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

" vim: fdm=marker sw=2
