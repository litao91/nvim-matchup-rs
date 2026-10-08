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

" offscreen statusline scroll refresh (port of matchparen.vim:1140-1168):
" a paused 50ms repeating timer, unpaused by the %{...scroll_update(N)}
" statusline expression when the offscreen line scrolls into view
function! matchup#rs#ensure_scroll_timer() abort
  if has('timers') && exists('*timer_pause')
    if !exists('s:scroll_timer')
      let s:scroll_timer = timer_start(50,
            \ 'matchup#rs#scroll_callback', { 'repeat': -1 })
      call timer_pause(s:scroll_timer, 1)
    endif
  endif
  return exists('s:scroll_timer')
endfunction

function! matchup#rs#scroll_callback(tid) abort
  call timer_pause(a:tid, 1)
  lua require('matchup_rs').highlight(true)
endfunction

function! matchup#rs#scroll_update(lnum) abort
  if line('w0') <= a:lnum && a:lnum <= line('w$')
        \ && exists('s:scroll_timer')
    call timer_pause(s:scroll_timer, 0)
  endif
  return ''
endfunction

" Re-feed keys for an operator-pending motion (port of matchup#motion#op):
" g:mrs_op_args = [wise, count, plugname]
function! matchup#rs#op_exec() abort
  let [l:wise, l:count, l:plug] = g:mrs_op_args
  execute 'normal' l:wise . (l:count > 0 ? l:count : '')
        \ . "\<Plug>(" . l:plug . ")"
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
