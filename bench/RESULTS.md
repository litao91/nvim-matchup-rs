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
  - `surrounding_deep`: get_surrounding from the target line inside the outermost block (walks back past every sibling block). On the 50k synthetic fixtures the outer block sits beyond the search bound (`delim_stopline`), so both engines return not-found there (verified to agree: rust nil, orig empty) - the large ratios reflect the bounded backward-walk speed, not a found-pair speedup. The real 50k vimscript file stays within the bound, so both sides do the full walk there
  - `highlight`: full matchparen cycle incl. extmark writes (cursor alternates between two positions per iteration)
  - `motion_unmatched`: `[%`-style motion from the target line (cursor reset each iteration, outside the timed region)
- 1 warm-up iteration, then 50 (2k files) / 30 (10k) / 6 (50k) / 3 (real 50k file) timed iterations via `vim.uv.hrtime()`; median and p95 reported. The 50k counts are low because the original engine's surrounding_deep costs ~1-89s per call there, so more iterations would not finish
- raw engine ops run with the timeout budget disabled on both sides (`matchup#perf#timeout_start(0)`); highlight and motion use their normal budgets

## nested_c_10k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.31 | 0.02 | 19.7x | 0.39 | 0.05 |
| matching_middle | 0.31 | 0.07 | 4.5x | 0.36 | 0.15 |
| surrounding_deep | 825.71 | 89.83 | 9.2x | 1,268.43 | 110.35 |
| highlight | 0.64 | 0.12 | 5.5x | 1.00 | 0.29 |
| motion_unmatched | 757.72 | 93.32 | 8.1x | 759.05 | 110.09 |

## nested_c_2k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.44 | 0.01 | 30.6x | 0.66 | 0.05 |
| matching_middle | 0.38 | 0.09 | 4.1x | 0.68 | 0.16 |
| surrounding_deep | 179.22 | 19.59 | 9.1x | 202.81 | 25.66 |
| highlight | 0.56 | 0.08 | 6.8x | 0.83 | 0.16 |
| motion_unmatched | 181.59 | 27.09 | 6.7x | 206.28 | 36.30 |

## nested_c_50k.c

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.33 | 0.02 | 20.7x | 0.38 | 0.09 |
| matching_middle | 0.36 | 0.13 | 2.7x | 0.45 | 0.31 |
| surrounding_deep | 5,443.00 | 13.83 | 393.5x | 5,655.26 | 20.68 |
| highlight | 0.53 | 0.66 | 0.8x | 0.57 | 0.96 |
| motion_unmatched | 755.77 | 11.73 | 64.4x | 756.41 | 13.86 |

## nested_lua_10k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.36 | 0.03 | 11.6x | 0.69 | 0.09 |
| matching_middle | 2.86 | 0.22 | 13.3x | 3.65 | 0.35 |
| surrounding_deep | 718.43 | 45.24 | 15.9x | 923.89 | 63.62 |
| highlight | 1.78 | 0.09 | 19.3x | 4.18 | 0.40 |
| motion_unmatched | 756.24 | 56.68 | 13.3x | 758.07 | 72.44 |

## nested_lua_2k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.36 | 0.03 | 12.2x | 0.49 | 0.06 |
| matching_middle | 3.26 | 0.19 | 16.8x | 4.06 | 0.28 |
| surrounding_deep | 169.19 | 8.96 | 18.9x | 208.89 | 12.72 |
| highlight | 1.32 | 0.21 | 6.3x | 4.74 | 0.54 |
| motion_unmatched | 168.21 | 11.42 | 14.7x | 244.70 | 15.35 |

## nested_lua_50k.lua

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 0.34 | 0.06 | 5.9x | 0.42 | 0.08 |
| matching_middle | 2.92 | 0.23 | 12.7x | 2.96 | 0.42 |
| surrounding_deep | 3,769.73 | 6.65 | 567.0x | 4,003.32 | 8.42 |
| highlight | 0.57 | 0.47 | 1.2x | 3.10 | 1.40 |
| motion_unmatched | 756.31 | 8.83 | 85.7x | 757.33 | 10.19 |

## nested_vim_10k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 12.69 | 13.73 | 0.9x | 21.09 | 19.46 |
| matching_middle | 5.84 | 1.26 | 4.6x | 6.60 | 1.79 |
| surrounding_deep | 1,047.42 | 199.87 | 5.2x | 1,278.33 | 292.71 |
| highlight | 1.53 | 0.51 | 3.0x | 6.87 | 2.27 |
| motion_unmatched | 754.76 | 242.03 | 3.1x | 756.14 | 268.43 |

## nested_vim_2k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 13.26 | 11.81 | 1.1x | 16.35 | 14.57 |
| matching_middle | 5.21 | 1.11 | 4.7x | 5.95 | 1.36 |
| surrounding_deep | 284.27 | 120.02 | 2.4x | 393.19 | 151.79 |
| highlight | 0.86 | 1.54 | 0.6x | 4.83 | 4.12 |
| motion_unmatched | 255.23 | 112.93 | 2.3x | 290.48 | 160.27 |

## nested_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 12.26 | 13.05 | 0.9x | 19.16 | 17.47 |
| matching_middle | 5.25 | 1.52 | 3.5x | 6.23 | 1.82 |
| surrounding_deep | 8,838.64 | 15.24 | 580.0x | 9,484.97 | 16.27 |
| highlight | 1.21 | 0.75 | 1.6x | 27.87 | 2.08 |
| motion_unmatched | 753.32 | 16.99 | 44.3x | 761.40 | 20.68 |

## real_vim_50k.vim

| op | orig median (ms) | rust median (ms) | speedup | orig p95 (ms) | rust p95 (ms) |
|---|---:|---:|---:|---:|---:|
| matching_outer | 2.82 | 1.88 | 1.5x | 2.90 | 2.01 |
| matching_middle | 2.64 | 0.67 | 4.0x | 2.76 | 0.76 |
| surrounding_deep | 75,772.60 | 27,602.97 | 2.7x | 77,564.89 | 27,926.51 |
| highlight | 0.89 | 1.47 | 0.6x | 23.63 | 25.89 |
| motion_unmatched | 751.17 | 750.56 | 1.0x | 751.25 | 750.88 |

## Summary (median speedup, geometric mean over files)

| op | speedup |
|---|---:|
| matching_outer | 5.2x |
| matching_middle | 5.8x |
| surrounding_deep | 25.4x |
| highlight | 2.4x |
| motion_unmatched | 10.6x |
| **overall** | **7.2x** |

