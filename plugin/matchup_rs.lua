-- vim match-up (Rust engine) - load guard
--
-- Rust rewrite of vim-matchup. The compiled native module lives at
-- lua/matchup_rs.so and is loaded with require('matchup_rs').
--
-- This plugin is INERT until you call require('matchup_rs').setup{...}: adding
-- the repository to 'runtimepath' only makes require('matchup_rs') available.
-- All configuration is passed to setup() as a Lua table (see README); there are
-- no g:matchup_* option globals.
--
-- Build the native module with:
--   :lua require('matchup_rs.build').build()

if vim.fn.has('nvim-0.11.0') == 0 then
  vim.cmd([[echohl WarningMsg | echom 'matchup-rs requires neovim >= 0.11' | echohl None]])
  return
end

if vim.g.loaded_matchup_rs then
  return
end

-- if the original vim-matchup already loaded, stay out of its way
if vim.fn.exists('*matchup#init') == 1 then
  return
end

vim.g.loaded_matchup_rs = true
