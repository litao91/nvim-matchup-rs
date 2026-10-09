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
- 1 warm-up iteration, then 50 (2k files) / 30 (10k) / 6 (50k) / 3 (real 50k file) timed iterations via `vim.uv.hrtime()`; median and p95 reported. The 50k counts are low because the original engine's surrounding_deep costs ~1-89s per call there, so more iterations would not finish
- raw engine ops run with the timeout budget disabled on both sides (`matchup#perf#timeout_start(0)`); highlight and motion use their normal budgets

## nested_c_10k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.29 | 1.74 | 0.2x | 0.49 | 2.64 |
| matching_middle | 0.27 | 1.78 | 0.2x | 0.39 | 2.33 |
| surrounding_deep | 585.95 | 70.07 | 8.4x | 734.91 | 78.78 |
| highlight | 0.73 | 2.24 | 0.3x | 1.04 | 3.07 |
| motion_unmatched | 636.23 | 66.18 | 9.6x | 754.31 | 79.77 |

## nested_c_2k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.29 | 0.40 | 0.7x | 0.42 | 0.62 |
| matching_middle | 0.29 | 0.42 | 0.7x | 0.38 | 0.66 |
| surrounding_deep | 125.14 | 16.72 | 7.5x | 149.13 | 20.28 |
| highlight | 0.49 | 0.39 | 1.3x | 0.74 | 0.49 |
| motion_unmatched | 131.92 | 19.33 | 6.8x | 161.04 | 25.40 |

## nested_c_50k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.28 | 0.32 | 0.9x | 0.36 | 0.34 |
| matching_middle | 0.28 | 0.76 | 0.4x | 0.49 | 0.83 |
| surrounding_deep | 4,319.51 | 11.14 | 387.6x | 4,548.03 | 13.68 |
| highlight | 0.59 | 0.38 | 1.5x | 0.68 | 0.93 |
| motion_unmatched | 755.37 | 12.06 | 62.6x | 756.02 | 16.32 |

## nested_lua_10k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.27 | 2.27 | 0.1x | 0.32 | 2.89 |
| matching_middle | 2.32 | 2.45 | 0.9x | 2.52 | 3.17 |
| surrounding_deep | 546.31 | 40.53 | 13.5x | 625.66 | 50.35 |
| highlight | 0.80 | 2.65 | 0.3x | 3.29 | 3.39 |
| motion_unmatched | 558.72 | 51.00 | 11.0x | 647.22 | 65.78 |

## nested_lua_2k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.27 | 0.40 | 0.7x | 0.36 | 0.45 |
| matching_middle | 2.26 | 0.53 | 4.3x | 2.41 | 0.67 |
| surrounding_deep | 111.11 | 6.76 | 16.4x | 127.59 | 10.61 |
| highlight | 0.88 | 0.76 | 1.1x | 4.05 | 1.34 |
| motion_unmatched | 127.72 | 13.34 | 9.6x | 166.41 | 19.03 |

## nested_lua_50k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.31 | 0.51 | 0.6x | 0.45 | 0.81 |
| matching_middle | 2.81 | 0.93 | 3.0x | 3.06 | 1.24 |
| surrounding_deep | 2,717.16 | 7.13 | 381.4x | 2,841.07 | 7.51 |
| highlight | 0.71 | 0.46 | 1.6x | 3.36 | 1.30 |
| motion_unmatched | 756.25 | 8.18 | 92.5x | 756.50 | 11.83 |

## nested_vim_10k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 15.62 | 14.35 | 1.1x | 18.11 | 17.08 |
| matching_middle | 5.80 | 3.74 | 1.6x | 7.87 | 4.24 |
| surrounding_deep | 930.20 | 183.85 | 5.1x | 1,114.38 | 209.34 |
| highlight | 1.13 | 4.54 | 0.2x | 8.95 | 9.15 |
| motion_unmatched | 754.58 | 191.97 | 3.9x | 755.35 | 225.78 |

## nested_vim_2k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 10.34 | 11.87 | 0.9x | 12.35 | 15.58 |
| matching_middle | 3.93 | 1.37 | 2.9x | 4.59 | 1.69 |
| surrounding_deep | 242.98 | 89.97 | 2.7x | 290.83 | 130.67 |
| highlight | 1.18 | 1.15 | 1.0x | 5.84 | 2.36 |
| motion_unmatched | 246.26 | 86.05 | 2.9x | 293.18 | 101.43 |

## nested_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 8.63 | 10.81 | 0.8x | 9.11 | 11.80 |
| matching_middle | 4.05 | 1.85 | 2.2x | 4.62 | 2.47 |
| surrounding_deep | 8,606.27 | 12.84 | 670.4x | 9,421.30 | 15.63 |
| highlight | 0.74 | 0.65 | 1.1x | 24.15 | 2.22 |
| motion_unmatched | 753.71 | 13.92 | 54.1x | 755.14 | 17.71 |

## real_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 3.29 | 1.44 | 2.3x | 3.73 | 1.45 |
| matching_middle | 3.59 | 1.72 | 2.1x | 4.21 | 1.79 |
| surrounding_deep | 93,990.87 | 21,349.53 | 4.4x | 99,801.50 | 21,798.43 |
| highlight | 1.13 | 1.07 | 1.1x | 29.62 | 18.01 |
| motion_unmatched | 751.33 | 751.32 | 1.0x | 751.41 | 752.01 |

## Summary (median speedup, geometric mean over files)

| op | speedup |
|---|---:|
| matching_outer | 0.6x |
| matching_middle | 1.3x |
| surrounding_deep | 24.6x |
| highlight | 0.8x |
| motion_unmatched | 10.9x |
| **overall** | **2.8x** |

