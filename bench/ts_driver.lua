-- Treesitter-engine benchmark driver (one engine, one file per process to
-- avoid cross-call history effects; the original's uuid-LRU degrades its
-- get_surrounding in long sessions).
--   g:engine    = 'rs' | 'orig'
--   g:repo      = path to nvim-matchup-rs
--   g:bench_file = file name under bench/files
--   g:bench_iters = iterations per op
--   g:bench_out = output JSON path

local engine = vim.g.engine
local repo = vim.g.repo
local fname = vim.g.bench_file
local iters = vim.g.bench_iters or 15
local out_path = vim.g.bench_out

local files_dir = repo .. '/bench/files'
local positions = vim.fn.json_decode(
  vim.fn.join(vim.fn.readfile(files_dir .. '/positions.json'), "\n"))[fname]

vim.g.matchup_matchparen_offscreen = {}
vim.g.matchup_treesitter_enabled = vim.g.matchup_treesitter_enabled or true

local api = {}
if engine == 'rs' then
  local m = require('matchup_rs')
  local tsv = vim.g.matchup_treesitter_enabled
  m.setup({
    treesitter = { enable = (tsv == true or tsv == 1) },
    matchparen = { offscreen = false },
  })
  api.current = function()
    local c = vim.api.nvim_win_get_cursor(0)
    return m.get_delim('current', 'both_all', { lnum = c[1], cnum = c[2] + 1 })
  end
  api.matching = function()
    local c = vim.api.nvim_win_get_cursor(0)
    return m.get_matching_at(c[1], c[2] + 1, false)
  end
  api.highlight = function() m.highlight(true) end
else
  vim.api.nvim_exec([[
    function! TsCur() abort
      call matchup#perf#timeout_start(100000000)
      return empty(matchup#delim#get_current('all', 'both_all', {})) ? 0 : 1
    endfunction
    function! TsMat() abort
      call matchup#perf#timeout_start(100000000)
      let l:d = matchup#delim#get_current('all', 'both_all', {})
      if empty(l:d) | return 0 | endif
      call matchup#delim#get_matching(l:d, {})
      return 1
    endfunction
  ]], false)
  api.current = function() vim.fn.TsCur() end
  api.matching = function() vim.fn.TsMat() end
  api.highlight = function() vim.fn['matchup#matchparen#update']() end
end

local function set_pos(p)
  vim.api.nvim_win_set_cursor(0, { p[1], math.max(0, p[2] - 1) })
end

local function stats(ts)
  table.sort(ts)
  local n = #ts
  local function pick(q) return ts[math.max(1, math.min(n, math.ceil(n * q)))] end
  return { median = pick(0.5), p95 = pick(0.95), min = ts[1], max = ts[n] }
end

local results = {}
local function run_op(opname, before, fn)
  before(1)
  fn()
  fn() -- second warm-up: first call includes the cold parse
  local ts = {}
  for i = 1, iters do
    before(i)
    local t0 = vim.uv.hrtime()
    fn()
    ts[#ts + 1] = (vim.uv.hrtime() - t0) / 1e6
  end
  local s = stats(ts)
  s.file = fname
  s.op = opname
  s.iters = iters
  results[#results + 1] = s
  print(string.format('  %-14s median %9.3f ms  p95 %9.3f ms', opname, s.median, s.p95))
end

vim.defer_fn(function()
  print(engine .. ': ' .. fname)
  vim.cmd('edit ' .. files_dir .. '/' .. fname)

  local t0 = vim.uv.hrtime()
  api.current()
  local cold_ms = (vim.uv.hrtime() - t0) / 1e6
  print(string.format('  cold first call (incl. parse): %.3f ms', cold_ms))

  run_op('ts_current', function() set_pos(positions.middle) end, api.current)
  run_op('ts_matching', function() set_pos(positions.middle) end, api.matching)
  run_op('ts_highlight',
    function(i) set_pos(i % 2 == 0 and positions.middle or positions.outer) end,
    api.highlight)

  vim.fn.writefile({ vim.json.encode({
    engine = engine, file = fname, cold_ms = cold_ms, results = results,
  }) }, out_path)
  vim.cmd('qa!')
end, 200)
