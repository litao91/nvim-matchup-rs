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
| matching_outer | 0.47 | 1.30 | 0.4x | 0.66 | 1.66 |
| matching_middle | 0.47 | 1.26 | 0.4x | 0.56 | 1.43 |
| surrounding_deep | 675.93 | 6.15 | 109.9x | 798.06 | 10.01 |
| highlight | 0.66 | 1.35 | 0.5x | 1.08 | 1.46 |
| motion_unmatched | 689.53 | 8.21 | 84.0x | 757.72 | 10.45 |

## nested_c_2k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.33 | 0.28 | 1.2x | 0.47 | 0.43 |
| matching_middle | 0.32 | 0.29 | 1.1x | 0.49 | 0.40 |
| surrounding_deep | 164.28 | 1.38 | 119.2x | 208.72 | 1.98 |
| highlight | 0.58 | 0.31 | 1.9x | 0.66 | 0.47 |
| motion_unmatched | 182.82 | 2.01 | 90.9x | 242.25 | 2.79 |

## nested_c_50k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.53 | 0.45 | 1.2x | 0.78 | 0.51 |
| matching_middle | 0.53 | 0.55 | 1.0x | 0.90 | 0.66 |
| surrounding_deep | 4,830.22 | 1.10 | 4,408.0x | 5,326.15 | 1.44 |
| highlight | 0.51 | 0.40 | 1.3x | 0.70 | 0.62 |
| motion_unmatched | 756.21 | 1.44 | 526.8x | 758.18 | 1.96 |

## nested_lua_10k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.70 | 1.62 | 0.4x | 1.05 | 1.90 |
| matching_middle | 4.49 | 1.70 | 2.6x | 5.84 | 2.66 |
| surrounding_deep | 632.81 | 11.41 | 55.4x | 769.06 | 13.66 |
| highlight | 0.76 | 2.10 | 0.4x | 3.67 | 3.43 |
| motion_unmatched | 658.15 | 19.42 | 33.9x | 757.13 | 29.04 |

## nested_lua_2k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.31 | 0.43 | 0.7x | 0.47 | 0.52 |
| matching_middle | 2.64 | 0.48 | 5.5x | 4.02 | 0.58 |
| surrounding_deep | 129.23 | 2.56 | 50.5x | 157.39 | 2.73 |
| highlight | 0.95 | 0.51 | 1.8x | 3.76 | 0.68 |
| motion_unmatched | 139.92 | 3.36 | 41.6x | 162.10 | 3.50 |

## nested_lua_50k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.34 | 0.29 | 1.2x | 0.56 | 0.44 |
| matching_middle | 2.71 | 0.58 | 4.7x | 3.35 | 0.72 |
| surrounding_deep | 3,134.14 | 1.71 | 1,830.8x | 3,695.00 | 1.81 |
| highlight | 1.00 | 0.38 | 2.7x | 4.02 | 0.89 |
| motion_unmatched | 756.40 | 2.37 | 319.6x | 757.40 | 3.50 |

## nested_vim_10k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 10.60 | 2.37 | 4.5x | 11.18 | 2.58 |
| matching_middle | 4.43 | 3.15 | 1.4x | 4.85 | 3.51 |
| surrounding_deep | 1,018.75 | 219.05 | 4.7x | 1,185.60 | 271.68 |
| highlight | 0.81 | 2.64 | 0.3x | 5.10 | 5.38 |
| motion_unmatched | 754.53 | 199.73 | 3.8x | 755.49 | 259.13 |

## nested_vim_2k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 11.90 | 1.26 | 9.5x | 16.04 | 2.09 |
| matching_middle | 4.55 | 2.02 | 2.3x | 5.25 | 2.78 |
| surrounding_deep | 267.45 | 78.31 | 3.4x | 324.75 | 92.53 |
| highlight | 0.97 | 1.82 | 0.5x | 5.04 | 3.35 |
| motion_unmatched | 280.65 | 87.90 | 3.2x | 341.68 | 102.14 |

## nested_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 10.26 | 0.99 | 10.4x | 10.62 | 1.20 |
| matching_middle | 4.17 | 2.09 | 2.0x | 5.02 | 2.73 |
| surrounding_deep | 9,570.67 | 16.06 | 595.9x | 11,438.90 | 28.29 |
| highlight | 0.94 | 0.61 | 1.6x | 21.91 | 2.01 |
| motion_unmatched | 754.50 | 17.11 | 44.1x | 765.69 | 21.69 |

## real_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 3.41 | 1.62 | 2.1x | 5.08 | 1.83 |
| matching_middle | 3.68 | 1.73 | 2.1x | 4.78 | 2.39 |
| surrounding_deep | 91,441.05 | 29,276.72 | 3.1x | 104,486.40 | 39,639.71 |
| highlight | 1.22 | 2.02 | 0.6x | 27.00 | 21.96 |
| motion_unmatched | 752.07 | 751.39 | 1.0x | 1,076.84 | 753.44 |

## Summary (median speedup, geometric mean over files)

| op | speedup |
|---|---:|
| matching_outer | 1.7x |
| matching_middle | 1.8x |
| surrounding_deep | 78.4x |
| highlight | 0.9x |
| motion_unmatched | 31.5x |
| **overall** | **5.8x** |

