" vim match-up (Rust engine)
"
" Rust rewrite of vim-matchup. The compiled module lives at lua/matchup_rs.so
" and is loaded with require('matchup_rs').
"
" This plugin is INERT until you call require('matchup_rs').setup{...}.
" All configuration is passed to setup() as a Lua table (see README); there
" are no g:matchup_* option globals. setup() activates the highlight groups,
" matchit neutralization, user commands, autocmds and keymaps.

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

let s:save_cpo = &cpo
set cpo&vim

" Nothing is mapped, highlighted or configured here on purpose: adding this
" repository to 'runtimepath' only makes require('matchup_rs') available.
" Call require('matchup_rs').setup{...} from your config to activate, e.g.:
"
"   require('matchup_rs').setup{
"     treesitter = { enable = true },
"     matchparen = { deferred = true },
"   }

let &cpo = s:save_cpo

" vim: fdm=marker sw=2
