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
| `setup(opts)` configuration, native per-filetype definitions applied via `FileType` autocmd, commands (`NoMatchParen`, `DoMatchParen`, `MatchupReload`, `MatchupShowTimes`) | done |
| offscreen `popup` method, surround, transmute, where-am-I, mouse, Vim 8 | not ported |

## Build

Requires Rust (stable) and Neovim >= 0.11. The build was developed against
Neovim 0.13.0-dev, whose C ABI drifted from what nvim-oxi 0.6 assumes:
`nvim_call_function` gained a leading channel id, `nvim_echo`'s return type
changed, and the `nvim_create_autocmd` keyset layout shifted so oxi's Lua-ref
callbacks register but never fire. `src/nvimrs.rs` re-declares those functions
with their true 0.13 signatures, so the plugin uses direct native calls and
real Rust autocmd callbacks throughout - it never routes through `api::eval`. A
native `nvim_eval` wrapper remains only for the three genuinely-arbitrary
vimscript expressions (expression-valued `b:match_words`, raw `b:match_skip`,
and the linewise-operator config); everything else is a typed native call.

Build the native module from inside nvim with the Lua helper:

```vim
:lua require('matchup_rs.build').build()
```

or headlessly:

```sh
nvim --headless "+lua require('matchup_rs.build').build()" +qa
```

`build()` runs `cargo build --release` (via `--manifest-path`/`--target-dir`, so
your working directory is untouched), picks the right artifact per platform
(`libmatchup_rs.so` on Linux, `libmatchup_rs.dylib` on macOS, `matchup_rs.dll`
on Windows), and atomically copies it to `lua/matchup_rs.so` (`.dll` on Windows)
where `require('matchup_rs')` finds it. Options:
`build({ profile = 'debug' })`, `build({ dir = '<repo root>' })`, and
`build({ touch = true })` (touch `src/*.rs` first - a stale-mtime workaround on
WSL2/drvfs). It needs a Rust toolchain on `PATH`. Equivalent manual steps:

```sh
cargo build --release
cp target/release/libmatchup_rs.so lua/matchup_rs.so   # .dylib on macOS, .dll on Windows
```

Add the repository to your `runtimepath` (e.g. your plugin manager). The
compiled module must sit at `lua/matchup_rs.so` inside the plugin directory.
Treesitter grammars are picked up from the standard runtimepath `parser/`
directories; the `after/queries/*/matchup.scm` files ship with the plugin.

## Usage

Add the repository to your `runtimepath`. The plugin is **inert until you call
`setup`** - putting it on the runtimepath only makes `require('matchup_rs')`
available; nothing is mapped, highlighted or configured until you call:

```lua
require('matchup_rs').setup({
  -- every key is optional; omitted keys use these defaults
  mappings = true,                       -- master switch for all keymaps
  matchpref = {},                        -- e.g. { html = { nolists = true } }
  delim = {
    noskips = 0, nomids = false, stopline = 1500, count_fail = false, count_max = 8,
  },
  matchparen = {
    enable = true, stopline = 400, timeout = 300, insert_timeout = 60,
    singleton = false, pumvisible = true, nomode = '', hi_background = false,
    offscreen = { method = 'status' },   -- or `false` to disable offscreen
    start_sign = '▶', end_sign = '◀',
    deferred = false, deferred_show_delay = 50, deferred_hide_delay = 700,
    deferred_fade_time = 0,
  },
  motion   = { enable = true, cursor_end = true, override_Npercent = 6, keepjumps = false },
  text_obj = { enable = true, linewise_operators = { 'd', 'y' } },
  treesitter = {
    enable = true,                       -- default follows has('nvim-0.11.2')
    disabled = {}, stopline = 400, enable_quotes = true,
    include_match_words = false, disable_virtual_text = false,
  },
})
```

`setup()` claims `%`, `g%`, `[%`, `]%`, `z%` (normal/visual/operator-pending)
and `i%`/`a%` (visual/operator-pending), installs the `<Plug>(matchup-*)` maps,
defines the `MatchParenCur`/`MatchWord`/`MatchBackground` highlight groups and
the `NoMatchParen`/`DoMatchParen`/`MatchupReload`/`MatchupShowTimes` commands,
and neutralizes `matchit`/`pi_paren`. It is re-runnable, so calling it again
reconfigures. There are **no `g:matchup_*` option globals** - configuration
lives entirely in the `setup` table. Option names mirror vim-matchup's
`g:matchup_<group>_<name>`, nested and de-prefixed (e.g.
`g:matchup_treesitter_disable_virtual_text` -> `treesitter.disable_virtual_text`).

Per-filetype delimiter definitions (`match_words`, `match_skip`, `midmap`, ...)
are built into the Rust module (`src/ftplugin.rs`, a port of vim-matchup's
`after/ftplugin/*.vim`) and applied from a `FileType` autocmd, so no vimscript
ftplugin files are shipped. **All plugin config and runtime state lives in
Rust** (`State.ft_config` and the `State` cells) - the plugin never writes `b:`/
`g:`/`w:` config variables. It only *reads* nvim's own runtime/matchit inputs
(base `b:match_words`, `b:match_ignorecase`, `&matchpairs`) as a fallback for
filetypes without a native definition, so a user-set `b:match_words` is still
honored. There is no vim-matchup compatibility layer (`matchup#util#*`,
`MatchupStatusOffscreen()`, the `g:loaded_matchup` claim are all gone); the only
globals it sets belong to *other* plugins (`g:loaded_matchit`,
`g:loaded_matchparen`, `g:vimtex_matchparen_enabled`) and are set from Rust.

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

The native filetype definitions (`src/ftplugin.rs`) are verified byte-for-byte
against vim-matchup's `after/ftplugin/*.vim`: a headless harness sets each of
the 16 supported filetypes under both plugins and diffs the resulting config -
read from Rust via the `buffer_config()` introspection export on the Rust side
and from `b:match_words`/`b:match_skip`/`b:match_midmap`/`&matchpairs` on the
original - both with default prefs and with the `matchpref` branches
(`nolists`/`tagnameonly`/`template`/`relax_env`) enabled. All 16 match exactly
in both modes.

## Layout

```
src/
  vimregex.rs    vim-magic -> fancy-regex translator (obligations, scan mode,
                 first-byte/literal-prefix analysis)
  words.rs       b:match_words/&matchpairs parser (loader.vim port)
  state.rs       per-buffer compiled state, caches, setup(opts) config, the
                 Rust FtConfig store + all runtime state cells, perf
  skip.rs        b:match_skip evaluation
  engine.rs      get_delim / get_matching / get_surrounding / jump_target
  treesitter.rs  pure-Rust treesitter engine
  ftplugin.rs    native per-filetype definitions (after/ftplugin port); builds
                 FtConfig from a FileType autocmd (no b: vars written)
  matchparen.rs  highlighting, offscreen status, deferred + scroll timers
  motion.rs      %, g%, [%, ]%, z% + operator-pending machinery
  textobj.rs     i%, a%
  nvimrs.rs      native nvim C-API FFI (0.13-dev ABI) - call_function, autocmd
                 callbacks, options/vars, echo; replaces the ABI-broken oxi paths
  lib.rs         Lua module surface (setup, activation, raw engine API,
                 autocmd/keymap wiring)
lua/matchup_rs/build.lua     Lua build helper: require('matchup_rs.build').build()
autoload/matchup/rs.vim      stateless vimscript shims only: timers, raw skip
                             eval, effective-position accessors, op re-feed,
                             indentexpr trick (no plugin state)
after/queries/               treesitter queries, copied from vim-matchup (MIT)
tests/diff/                  cross-engine correctness harness
bench/                       benchmark harness + results
```

The plugin is a pure Lua module: adding it to `runtimepath` does nothing until
you call `require('matchup_rs').setup{...}` (there is no `plugin/` script). All
mutable state - config, timers, activation, effective cursor, offscreen
statusline - is held in the Rust `State`; vimscript/lua hold none.

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

MIT. `after/queries/` is copied verbatim from vim-matchup (MIT, (c) Andy
Massimino); `src/ftplugin.rs` is a port of vim-matchup's `after/ftplugin/*.vim`
delimiter definitions.
