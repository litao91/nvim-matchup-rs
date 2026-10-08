# nvim-matchup-rs benchmarks

Classic-engine Rust rewrite (nvim-oxi + fancy-regex) vs the original vim-matchup vimscript engine, measured in two separate headless nvim instances over identical buffers.

## Environment

- neovim 0.13.0-dev (headless, `--clean -u NONE`)
- cpu: Intel(R) Core(TM) i5-10210U CPU @ 1.60GHz
- os: Linux 6.18.40.1-microsoft-standard-WSL2
- rust: release profile (lto, opt-level 3, codegen-units 1)
- treesitter engine disabled on both sides (`g:matchup_treesitter_enabled = 0`); offscreen rendering disabled (`g:matchup_matchparen_offscreen = {}`) so the highlight op measures the core cycle
- syntax highlighting off in headless runs: `synID()` is empty, so `b:match_skip` syntax filtering is inactive on both sides; this isolates engine regex/scan performance

## Method

- fixtures (`bench/gen.py`): synthetic nested vim/C/lua at ~2k/10k/50k lines - one outer block containing S sibling nested blocks plus a cursor-target line - and a real 50k-line vimscript file (concatenated nvim runtime syntax files)
- ops, each one end-to-end call per iteration:
  - `matching_outer` / `matching_middle`: get_current + get_matching at the outermost / a middle delimiter (full-file depth scan)
  - `surrounding_deep`: get_surrounding from the target line inside the outermost block (walks back past every sibling block)
  - `highlight`: full matchparen cycle incl. extmark writes (cursor alternates between two positions per iteration)
  - `motion_unmatched`: `[%`-style motion from the target line (cursor reset each iteration, outside the timed region)
- 1 warm-up iteration, then 50 (2k files) / 30 (10k) / 15 (50k and real file) timed iterations via `vim.uv.hrtime()`; median and p95 reported
- raw engine ops run with the timeout budget disabled on both sides (`matchup#perf#timeout_start(0)`); highlight and motion use their normal budgets

## nested_c_10k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.31 | 1.42 | 0.2x | 0.43 | 2.19 |
| matching_middle | 0.31 | 1.43 | 0.2x | 0.44 | 1.52 |
| surrounding_deep | 746.79 | 6.30 | 118.5x | 2,466.27 | 6.68 |
| highlight | 0.50 | 1.42 | 0.4x | 0.57 | 1.61 |
| motion_unmatched | 714.30 | 8.49 | 84.1x | 758.32 | 9.44 |

## nested_c_2k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.32 | 0.36 | 0.9x | 0.40 | 0.58 |
| matching_middle | 0.31 | 0.36 | 0.9x | 0.38 | 0.65 |
| surrounding_deep | 196.05 | 1.51 | 130.2x | 956.73 | 1.74 |
| highlight | 1.25 | 0.37 | 3.3x | 10.76 | 0.51 |
| motion_unmatched | 155.71 | 2.12 | 73.5x | 236.96 | 3.62 |

## nested_c_50k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.29 | 0.31 | 0.9x | 0.42 | 0.38 |
| matching_middle | 0.29 | 0.58 | 0.5x | 0.36 | 1.21 |
| surrounding_deep | 5,242.99 | 1.20 | 4,365.1x | 16,092.53 | 2.00 |
| highlight | 0.65 | 0.38 | 1.7x | 1.20 | 0.64 |
| motion_unmatched | 756.32 | 1.56 | 483.4x | 758.65 | 2.53 |

## nested_lua_10k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.36 | 2.04 | 0.2x | 0.54 | 2.78 |
| matching_middle | 2.86 | 1.93 | 1.5x | 3.39 | 2.19 |
| surrounding_deep | 637.67 | 12.02 | 53.1x | 854.06 | 14.34 |
| highlight | 0.60 | 2.04 | 0.3x | 3.29 | 2.70 |
| motion_unmatched | 756.31 | 17.70 | 42.7x | 766.69 | 19.28 |

## nested_lua_2k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.93 | 0.47 | 2.0x | 9.91 | 0.59 |
| matching_middle | 17.74 | 0.53 | 33.4x | 27.83 | 0.72 |
| surrounding_deep | 848.22 | 2.67 | 317.8x | 1,169.04 | 4.44 |
| highlight | 1.79 | 0.59 | 3.0x | 9.98 | 0.70 |
| motion_unmatched | 316.71 | 3.53 | 89.8x | 663.53 | 3.71 |

## nested_lua_50k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.76 | 0.35 | 2.2x | 1.41 | 0.38 |
| matching_middle | 4.87 | 0.67 | 7.2x | 6.34 | 0.76 |
| surrounding_deep | 8,044.23 | 1.77 | 4,532.3x | 9,770.19 | 1.87 |
| highlight | 1.23 | 0.38 | 3.2x | 4.74 | 1.17 |
| motion_unmatched | 756.63 | 2.46 | 308.1x | 760.11 | 2.91 |

## nested_vim_10k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 15.58 | 2.69 | 5.8x | 18.75 | 3.44 |
| matching_middle | 6.67 | 3.29 | 2.0x | 7.80 | 3.52 |
| surrounding_deep | 1,336.83 | 203.21 | 6.6x | 1,530.43 | 235.46 |
| highlight | 1.33 | 4.01 | 0.3x | 9.00 | 6.57 |
| motion_unmatched | 754.83 | 216.66 | 3.5x | 756.29 | 282.00 |

## nested_vim_2k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 13.48 | 1.18 | 11.4x | 14.91 | 1.74 |
| matching_middle | 7.57 | 2.02 | 3.7x | 10.73 | 2.89 |
| surrounding_deep | 346.35 | 79.65 | 4.3x | 400.45 | 92.31 |
| highlight | 1.77 | 1.99 | 0.9x | 8.47 | 3.30 |
| motion_unmatched | 405.57 | 83.82 | 4.8x | 468.97 | 99.05 |

## nested_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 12.89 | 1.08 | 12.0x | 13.79 | 1.66 |
| matching_middle | 6.01 | 2.21 | 2.7x | 8.61 | 2.97 |
| surrounding_deep | 14,747.52 | 16.24 | 908.1x | 17,016.15 | 17.87 |
| highlight | 0.95 | 0.66 | 1.5x | 31.41 | 2.68 |
| motion_unmatched | 756.42 | 15.85 | 47.7x | 765.96 | 16.66 |

## real_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 3.88 | 1.40 | 2.8x | 5.13 | 1.56 |
| matching_middle | 4.25 | 1.96 | 2.2x | 5.01 | 2.28 |
| surrounding_deep | 83,232.56 | 25,542.34 | 3.3x | 112,797.59 | 52,126.44 |
| highlight | 1.39 | 3.08 | 0.5x | 38.85 | 30.80 |
| motion_unmatched | 751.45 | 752.19 | 1.0x | 1,003.44 | 754.88 |

## Summary (median speedup, geometric mean over files)

| op | speedup |
|---|---:|
| matching_outer | 1.8x |
| matching_middle | 2.1x |
| surrounding_deep | 115.8x |
| highlight | 1.0x |
| motion_unmatched | 35.1x |
| **overall** | **6.9x** |


---

## Treesitter engine (pure-Rust port vs vim.treesitter)

Both engines run with `g:matchup_treesitter_enabled = v:true`, offscreen rendering disabled, in a fresh headless process per engine and file (the original's uuid-LRU cache degrades its own results in long sessions). The Rust side loads grammar parsers from the runtimepath via `libloading` and parses with the `tree-sitter` crate - no `vim.treesitter` API calls. `cold first call` includes the initial full parse (Rust) vs nvim's incremental LanguageTree parse (original); warm ops dominate interactive use.

### nested_c_10k.c

cold first call: orig 113.7 ms, rust 133.7 ms

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| ts_current | 10.61 | 6.37 | 1.7x | 14.68 | 8.37 |
| ts_matching | 23.72 | 11.55 | 2.1x | 27.99 | 18.30 |
| ts_highlight | 12.59 | 6.80 | 1.9x | 27.00 | 15.34 |

### nested_lua_10k.lua

cold first call: orig 75.0 ms, rust 81.0 ms

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| ts_current | 6.12 | 6.49 | 0.9x | 9.98 | 8.06 |
| ts_matching | 11.25 | 13.44 | 0.8x | 15.31 | 17.65 |
| ts_highlight | 7.53 | 6.44 | 1.2x | 15.89 | 16.46 |

### nested_vim_10k.vim

cold first call: orig 51.9 ms, rust 59.1 ms

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| ts_current | 4.38 | 3.45 | 1.3x | 6.93 | 5.39 |
| ts_matching | 9.40 | 6.73 | 1.4x | 12.94 | 7.33 |
| ts_highlight | 12.28 | 6.14 | 2.0x | 18.10 | 8.24 |

