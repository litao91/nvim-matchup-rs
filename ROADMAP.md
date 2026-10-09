# Roadmap

Follow-up improvements after the buffer-lines snapshot cache (`1109dd3`) and the
native `mode()` / `line('.')` / `line("$")` swaps (`33f8edf`). Each item lists
the motivation, the concrete change, and its status.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done

## 1. Automated coverage for the highlight / motion / offscreen paths — [x]

**Done:** `tests/smoke/{run.sh,driver.lua}` — 9 headless checks (extmark
highlight, `%` keymap, `motion_matching`, operator-pending `d%`, forced `dv%`,
and the offscreen gutter delta + `∆` marker above/below). `run.sh` builds +
deploys the `.so` then runs post-`VimEnter` via `vim.schedule`. All pass.

`tests/diff` drives the raw engine ops with `matchparen.enable = false`, so the
hottest real-world code has **no** automated tests: extmark highlighting, the
native keymap motions (`%` / `g%` / `[%` / `]%` / `z%`), and the offscreen
statusline renderer. Add a headless smoke suite that asserts observable
behavior:

- highlight renders `MatchParen` / `MatchWord` extmarks at the cursor delimiter
- `%` and `motion_matching` jump to the match; operator-pending `d%` and forced
  `dv%` (the `motion_force` `"nov"` path) edit the buffer correctly
- offscreen `status` method renders the gutter — line-number column (relative
  delta) and the `∆` direction marker — for matches above and below the viewport

Harness notes (all worked out): run **after** `VimEnter` via `vim.schedule`
(highlight early-outs on `has('vim_starting')` and `state('a')`), and pump the
debounce timer with `vim.wait`. Highlight lands as **extmarks**, not
`matchadd`, so assert on `nvim_buf_get_extmarks`, not `getmatches()`.

## 2. Replace the remaining window FFI calls with native C-API — [x]

**Done:** `win_getid()` → `ctx.win.handle()` (or `api::get_current_win()` in the
timer callbacks that have no `ctx`); `winheight(0)` / `winwidth(0)` →
`Window::get_height()` / `get_width()`; `setwinvar('&statusline')` → a
hand-rolled `nvimrs::set_option_local` (`nvim_set_option_value`). oxi 0.6's
`nvim_set_option_value` binding targets v0.10 (missing the `arena` param and the
`Object` return), so it is ABI-broken on 0.13 — hence the hand-roll. Verified by
the item-1 offscreen-gutter checks (which set + read back the statusline).

- `win_getid()` → the window handle already held in `ctx.win` (or
  `api::get_current_win()`); ~5 sites
- `winheight(0)` / `winwidth(0)` → `Window::get_height()` / `get_width()`; ~4 sites
- `setwinvar(w, '&statusline', v)` → native window-option set
  (`nvim_set_option_value`); add `set_option_local` to `nvimrs.rs` mirroring
  `get_option_local_as`; 2 sites

`line('w0')` / `line('w$')` stay vimscript: topline/botline are nvim-internal
scroll state with no C-API equivalent (`nvim_win_get_view` does not exist in
0.13) and are not derivable — identical cursor/position/height yield different
`w0` after `zt` vs `zb` vs `zz`.

## 3. Memoize `has('vim_starting')` — [x]

**Done:** `matchparen::vim_starting()` caches the terminal `false` in a
`thread_local Cell` (same pattern as the existing `TRACE_ENABLED`); the
highlight gate skips the vimscript `has()` call in steady state. `state('a')`
beside it stays live (dynamic).

`highlight()` calls `has('vim_starting')` on every invocation to skip work
during startup, but it flips `true → false` exactly once. Cache the first
`false` (e.g. an `AtomicBool` / `Cell`) so the steady-state hot path drops one
vimscript round-trip per highlight. `state('a')` next to it must stay live (it
is dynamic).

## 4. Re-baseline `bench/RESULTS.md` — [x]

**Done:** re-ran `bench/run.sh`. The post-cache numbers replace the stale
pre-cache ones — `matching_outer` 0.6x→**5.2x**, `highlight` 0.8x→**2.4x**,
overall 2.8x→**7.2x**. Sanity-checked the 50k `surrounding_deep`: at the deep
target both engines return *not-found* (rust `nil`, orig empty — verified to
agree), so the 393–580x ratios are bounded-backward-walk speed, not a
found-pair speedup; `real_vim_50k` stays within the bound (rust 27.6s of real
work vs orig 75.8s). Documented in the Method section via `aggregate.py` so it
survives regeneration.

RESULTS.md predates the snapshot cache — it shows rust `matching_outer` /
`highlight` *slower* than the original (0.2x / 0.3x), which the cache reversed
(they are now ~0.006 ms). Re-run `bench/run.sh` on the current build and
regenerate. Sanity-check that the 50k `surrounding_deep` numbers reflect real
work rather than windowed `nil` returns on `> FULL_FETCH_LIMIT` buffers. Run
**last**, on the final `.so`, so items 2–3 are reflected.
