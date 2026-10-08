" vim match-up (Rust engine)
"
" Rust rewrite of vim-matchup's classic delimiter-matching engine,
" built on nvim-oxi. The compiled module lives at lua/matchup_rs.so
" and is loaded with require('matchup_rs').
"
" Options are compatible with vim-matchup's g:matchup_* names.

if !get(g:, 'matchup_enabled', 1) || &cp
  finish
endif

if !has('nvim-0.11.0')
  echohl WarningMsg
  echom 'matchup-rs requires neovim >= 0.11'
  echohl None
  finish
endif

if exists('g:loaded_matchup_rs')
  finish
endif
" if the original vim-matchup already loaded, stay out of its way
if exists('*matchup#init')
  finish
endif
let g:loaded_matchup_rs = 1
" claim vim-matchup's global: the copied after/ftplugin files guard on it,
" and the original plugin skips loading when it is already set
let g:loaded_matchup = 1

let s:save_cpo = &cpo
set cpo&vim

" disable matchit
let g:loaded_matchit = 1

" neuter the bundled matchit plugin (nvim ships it as a default plugin);
" port of vim-matchup's unmatchit.vim
if exists(':MatchDebug')
  delcommand MatchDebug
endif
unlet! g:loaded_matchit
let g:loaded_matchit = 1
silent! unmap %
silent! unmap [%
silent! unmap ]%
silent! unmap a%
silent! unmap g%

" ensure pi_paren is loaded but deactivated (as in vim-matchup)
try
  runtime plugin/matchparen.vim
  au! matchparen
catch /^Vim\%((\a\+)\)\=:E216/
  unlet! g:loaded_matchparen
  runtime plugin/matchparen.vim
  silent! au! matchparen
  let g:loaded_matchparen = 1
endtry

" highlight groups (same defaults as vim-matchup)
hi def link MatchParenCur MatchParen
hi def link MatchWord MatchParen
hi def link MatchBackground ColorColumn

" ---------------------------------------------------------------------------
" option defaults (mirrors vim-matchup's s:init_options)
" ---------------------------------------------------------------------------

function! s:init_option(option, default)
  if !has_key(g:, a:option)
    let g:[a:option] = a:default
  endif
endfunction

call s:init_option('matchup_mappings_enabled', 1)

call s:init_option('matchup_matchparen_enabled',
      \ !(&t_Co < 8 && !has('gui_running')))
let s:offs = {'method': 'status'}
if !get(g:, 'matchup_matchparen_status_offscreen', 1)
  let s:offs = {}
endif
if get(g:, 'matchup_matchparen_status_offscreen_manual', 0)
  let s:offs.method = 'status_manual'
endif
if exists('g:matchup_matchparen_scrolloff')
  let s:offs.scrolloff = g:matchup_matchparen_scrolloff
endif
call s:init_option('matchup_matchparen_offscreen', s:offs)
call s:init_option('matchup_matchparen_singleton', 0)
call s:init_option('matchup_matchparen_deferred', 0)
call s:init_option('matchup_matchparen_deferred_show_delay', 50)
call s:init_option('matchup_matchparen_deferred_hide_delay', 700)
call s:init_option('matchup_matchparen_deferred_fade_time', 0)
call s:init_option('matchup_matchparen_stopline', 400)
call s:init_option('matchup_matchparen_pumvisible', 1)
call s:init_option('matchup_matchparen_nomode', '')
call s:init_option('matchup_matchparen_hi_surround_always', 0)
call s:init_option('matchup_matchparen_hi_background', 0)
call s:init_option('matchup_matchparen_start_sign', '▶')
call s:init_option('matchup_matchparen_end_sign', '◀')
call s:init_option('matchup_matchparen_timeout',
      \ get(g:, 'matchparen_timeout', 300))
call s:init_option('matchup_matchparen_insert_timeout',
      \ get(g:, 'matchparen_insert_timeout', 60))

call s:init_option('matchup_delim_count_fail', 0)
call s:init_option('matchup_delim_count_max', 8)
call s:init_option('matchup_delim_noskips', 0)
call s:init_option('matchup_delim_nomids', 0)
call s:init_option('matchup_delim_stopline', 1500)

call s:init_option('matchup_motion_enabled', 1)
call s:init_option('matchup_motion_cursor_end', 1)
call s:init_option('matchup_motion_override_Npercent', 6)
call s:init_option('matchup_motion_keepjumps', 0)

call s:init_option('matchup_text_obj_enabled', 1)
call s:init_option('matchup_text_obj_linewise_operators', ['d', 'y'])

call s:init_option('matchup_matchpref', {})

" treesitter engine options (the Rust treesitter engine reads these)
call s:init_option('matchup_treesitter_enabled', has('nvim-0.11.2') ? v:true : v:false)
call s:init_option('matchup_treesitter_disabled', [])
call s:init_option('matchup_treesitter_enable_quotes', v:true)
call s:init_option('matchup_treesitter_disable_virtual_text', v:false)
call s:init_option('matchup_treesitter_stopline', 400)

" ---------------------------------------------------------------------------
" commands
" ---------------------------------------------------------------------------

command! NoMatchParen call matchup#rs#toggle(0)
command! DoMatchParen call matchup#rs#toggle(1)
command! MatchupReload call luaeval("require('matchup_rs').reload()")
      \ | call luaeval("require('matchup_rs').update()")
command! MatchupShowTimes call luaeval("require('matchup_rs').show_times()")

" offscreen statusline helper (compat with vim-matchup)
function! MatchupStatusOffscreen() abort
  return get(w:, 'matchup_statusline', '')
endfunction

" ---------------------------------------------------------------------------
" load the Rust module: creates autocmds, keymaps
" ---------------------------------------------------------------------------

lua require('matchup_rs').setup()

let &cpo = s:save_cpo

" vim: fdm=marker sw=2
