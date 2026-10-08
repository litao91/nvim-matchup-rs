# nvim-matchup-rs

A Rust rewrite of [vim-matchup](https://github.com/andymass/vim-matchup),
the Neovim plugin for matching delimiters (`if`/`endif`, `function`/`endfunction`,
`{`/`}`, `<div>`/`</div>`, ...). Built on
[nvim-oxi](https://github.com/noib3/nvim-oxi) and exposed to Neovim as the Lua
module `matchup_rs`.

Both of vim-matchup's matching engines are implemented natively in Rust:

- **classic engine** - the `b:match_words`/`&matchpairs` regex engine. Vim-magic
  regular expressions are translated to fancy-regex (with deferred obligations
  for constructs fancy-regex cannot express, e.g. variable-width `\@<=`), and
  the `searchpos`/`searchpairpos` cursor loops are replaced by single-pass
  depth-counting scans. Scan unions are additionally compiled as
  lookaround-free over-approximations for the `regex` crate's DFA; exactness is
  restored by an anchored per-word classify step with first-byte dispatch and
  literal-prefix filtering.
- **treesitter engine** - a pure-Rust port of
  `lua/treesitter-matchup/internal.lua`. Grammar parsers are `dlopen`ed from
  the runtimepath (`libloading`), `matchup.scm` queries are compiled with the
  `tree-sitter` crate (including `; inherits:` concatenation and the `#eq?`,
  `#not-has-parent?`, `#lua-match?`, `#offset!` extensions), and trees are
  cached per buffer and re-parsed incrementally. **No `vim.treesitter` Lua API
  is called.**

## Features

| area | status |
|---|---|
| matchparen highlighting (extmarks, `MatchParen(Cur)`/`MatchWord(Cur)`, fade, deferred debounce, background highlight) | done |
| offscreen matches, `status` method (statusline + scroll-refresh timer) | done |
| motions `%`, `g%`, `[%`, `]%`, `z%`, `N%` override, operator-pending + visual | done |
| text objects `i%`, `a%` (visual + operator-pending, linewise operators, html quirks) | done |
| `b:match_skip` (`s:`/`S:`/`r:`/`R:` and raw expressions), `b:match_words` (backrefs, augments, `\zs`, hlend, midmap) | done |
| treesitter engine (parsers via `libloading`, incremental parsing, scope/open/mid/close model, scope-end virtual text) | done |
| `g:matchup_*` option compatibility, `after/ftplugin` patches, commands (`NoMatchParen`, `DoMatchParen`, `MatchupReload`, `MatchupShowTimes`) | done |
| offscreen `popup` method, surround, transmute, where-am-I, mouse, Vim 8 | not ported |

## Build

Requires Rust (stable) and Neovim >= 0.11. The build was developed against
Neovim 0.13.0-dev; note that this nvim version changed some C API signatures
(`nvim_call_function`, `nvim_echo`) and does not fire Lua-ref autocmd/keymap
callbacks, so the plugin drives those paths through `eval`/command strings and
vimscript shims in `autoload/matchup/rs.vim`.

```sh
cargo build --release
cp target/release/libmatchup_rs.so lua/matchup_rs.so   # linux; .dll/.dylib elsewhere
```

Add the repository to your `runtimepath` (e.g. your plugin manager). The
compiled module must sit at `lua/matchup_rs.so` inside the plugin directory.
Treesitter grammars are picked up from the standard runtimepath `parser/`
directories; the `after/queries/*/matchup.scm` files ship with the plugin.

## Usage

The plugin claims `%`, `g%`, `[%`, `]%`, `z%` (normal/visual/operator-pending)
and `i%`/`a%` (visual/operator-pending), and installs the same `<Plug>` maps
as vim-matchup. It disables `matchit` and `pi_paren` on load, and claims
`g:loaded_matchup` so vim-matchup's engine-agnostic `after/ftplugin` files
work unchanged (if both plugins are on the runtimepath, whichever loads first
wins).

Options are vim-matchup-compatible (`g:matchup_matchparen_*`,
`g:matchup_delim_*`, `g:matchup_motion_*`, `g:matchup_text_obj_*`,
`g:matchup_treesitter_*`, ...).

## Performance

`bench/` contains two harnesses, each running one headless nvim per engine over
identical buffers: `bench/run.sh` (classic engine) and `bench/ts_run.sh`
(treesitter engine, fresh process per engine+file). Full tables in
[bench/RESULTS.md](bench/RESULTS.md); headlines below are medians, geometric
mean over 2k/10k/50k-line synthetic vim/C/lua files plus a real 50k-line
vimscript corpus.

**Classic engine** - overall geomean **6.9x**:

- `get_surrounding` from deep inside a large file: **~116x** faster (geomean;
  up to ~4,500x on 50k-line files, where the original's backward walk takes
  5-83 *seconds* per call)
- `[%`-style motion: **~35x** faster (the original pins its 750 ms timeout on
  large files; the Rust engine finishes in ~1-20 ms)
- full-file `get_matching`: geomean ~2x, ranging ~0.2-12x by filetype - vim's
  regex-heavy `match_words` benefits most, while single-character C/lua
  `&matchpairs` are already fast in vim's C search loop (down to ~0.2x)
- highlight cycle: at parity (~1.0x; sub-millisecond to ~2 ms both sides)

**Treesitter engine** - pure-Rust port vs nvim's native `vim.treesitter`:
**~1.3-2.1x** faster on vim and C buffers (highlight up to 2.0x) and at parity
on lua (~0.8-1.2x), despite nvim's treesitter being C-native while this port
parses entirely in Rust userspace via the `tree-sitter` crate.

## Correctness

`tests/diff/` runs **both engines** headlessly over every cursor position of
sample vim/lua/C/html files and compares `get_current`/`get_next`/`get_prev`/
`get_matching`/`get_surrounding` outputs:

```sh
tests/diff/run.sh          # classic engine: all 1308 positions match exactly
TS=1 tests/diff/run.sh     # treesitter engine: cur/next/prev/matching match
                           # exactly; get_surrounding differs only where the
                           # original degrades (see below)
```

In treesitter mode the original intermittently returns empty surroundings in
long sessions: its `get_surrounding` memo holds delim dicts whose `_id`
entries were evicted from its 150-slot uuid LRU, so `get_matching` then fails.
A fresh-process original agrees with the Rust results at every such position
(the Rust port bypasses the memo while treesitter is active and stays
correct); the comparer reports these as an explicit note instead of a
mismatch.

Also: `cargo test` covers the regex translator, capture-group scanner,
`match_words` parser (backrefs, group renumbering, augments), `&iskeyword`
class builder, skip compilation and position helpers.

## Layout

```
src/
  vimregex.rs    vim-magic -> fancy-regex translator (obligations, scan mode,
                 first-byte/literal-prefix analysis)
  words.rs       b:match_words/&matchpairs parser (loader.vim port)
  state.rs       per-buffer compiled state, caches, options, perf timers
  skip.rs        b:match_skip evaluation
  engine.rs      get_delim / get_matching / get_surrounding / jump_target
  treesitter.rs  pure-Rust treesitter engine
  matchparen.rs  highlighting, offscreen status, deferred debounce
  motion.rs      %, g%, [%, ]%, z% + operator-pending machinery
  textobj.rs     i%, a%
plugin/matchup_rs.vim      defaults, commands, <Plug> maps, setup
autoload/matchup/rs.vim    vimscript shims (timers, skip eval, op re-feed)
autoload/matchup/util.vim  compat subset for the copied ftplugins
after/ftplugin/            copied from vim-matchup (MIT)
after/queries/             copied from vim-matchup (MIT)
tests/diff/                cross-engine correctness harness
bench/                     benchmark harness + results
```

## Security notes

Like vim-matchup, the engine evaluates buffer-local configuration:
expression-valued `b:match_words` (via `eval`, mirroring upstream's
`execute 'let ...'`) and raw `b:match_skip` expressions (via a vimscript shim,
mirroring upstream's `execute 'return ...'`). These values are set by
ftplugins/user config; anyone able to set them can already execute vimscript.
All other dynamic `eval` strings interpolate integers only, or strings passed
through single-quoted vimscript literals.

## Known limitations / future work

- Treesitter: no injected-language (nested parser) support yet - only the
  root language tree is queried; offscreen statusline text uses `synID`
  coloring rather than treesitter highlight groups.
- Incremental treesitter parsing diffs the previous buffer text at line
  granularity (full text re-fetch on each changedtick); an `on_bytes`-style
  edit feed would avoid the fetch on large buffers.
- Offscreen `popup` method, surround, transmute, where-am-I and Vim 8 support
  are not ported.

## License

MIT. `after/ftplugin/` and `after/queries/` are copied verbatim from
vim-matchup (MIT, (c) Andy Massimino).
