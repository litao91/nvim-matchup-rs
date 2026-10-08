-- Benchmark driver: measures per-op latency for one engine over all
-- fixture files. Run once per engine (see bench/run.sh):
--   g:engine    = 'rs' | 'orig'
--   g:repo      = path to nvim-matchup-rs
--   g:bench_out = output JSON path
-- Offscreen rendering is disabled so the highlight op measures the core
-- engine + extmark cycle on both sides.

local engine = vim.g.engine
local repo = vim.g.repo
local out_path = vim.g.bench_out

local files_dir = repo .. '/bench/files'
local positions = vim.fn.json_decode(
  vim.fn.join(vim.fn.readfile(files_dir .. '/positions.json'), "\n"))

vim.g.matchup_matchparen_offscreen = {}
vim.g.matchup_treesitter_enabled = 0

local api = {}
if engine == 'rs' then
  local m = require('matchup_rs')
  -- registers the FileType autocmd (native ftplugin -> b:match_words) and
  -- configures the engine: classic (treesitter off), offscreen disabled.
  m.setup({
    treesitter = { enable = false },
    matchparen = { offscreen = false },
  })
  api.matching = function()
    local cur = vim.api.nvim_win_get_cursor(0)
    return m.get_matching_at(cur[1], cur[2] + 1, false)
  end
  api.surrounding = function() return m.get_surrounding_at(1, false) end
  api.highlight = function() m.highlight(true) end
  api.motion = function() m.motion_unmatched(0, 0) end
  api.times = function() return vim.g.matchup_rs_times end
else
  -- keep delim dicts inside vimscript: they carry Funcrefs which do not
  -- survive the lua round-trip
  vim.api.nvim_exec([[
    function! BenchMatching() abort
      call matchup#perf#timeout_start(0)
      let l:d = matchup#delim#get_current('all', 'both_all', {})
      if empty(l:d) | return 0 | endif
      call matchup#delim#get_matching(l:d, {})
      return 1
    endfunction
    function! BenchSurrounding() abort
      call matchup#perf#timeout_start(0)
      call matchup#delim#get_surrounding('all', 1, {'local': 0})
      return 1
    endfunction
  ]], false)
  api.matching = function() vim.fn.BenchMatching() end
  api.surrounding = function() vim.fn.BenchSurrounding() end
  api.highlight = function() vim.fn['matchup#matchparen#update']() end
  api.motion = function() vim.fn['matchup#motion#find_unmatched'](0, 0) end
  api.times = function() return vim.g['matchup#perf#times'] end
end

local function set_pos(p)
  vim.api.nvim_win_set_cursor(0, { p[1], math.max(0, p[2] - 1) })
end

local function stats(ts)
  table.sort(ts)
  local n = #ts
  local function pick(q)
    return ts[math.max(1, math.min(n, math.ceil(n * q)))]
  end
  return {
    median = pick(0.5), p95 = pick(0.95),
    min = ts[1], max = ts[n],
  }
end

local results = {}

local function run_op(file, opname, iters, before, fn)
  before(1)
  fn() -- warm-up (also primes syntax/regex caches)
  local ts = {}
  for i = 1, iters do
    before(i)
    local t0 = vim.uv.hrtime()
    fn()
    ts[#ts + 1] = (vim.uv.hrtime() - t0) / 1e6
  end
  local s = stats(ts)
  s.file = file
  s.op = opname
  s.iters = iters
  results[#results + 1] = s
  print(string.format('  %-16s median %9.3f ms  p95 %9.3f ms',
    opname, s.median, s.p95))
end

vim.defer_fn(function()
  local names = {}
  for name in pairs(positions) do names[#names + 1] = name end
  table.sort(names)
  local filter = vim.g.bench_filter
  if filter and filter ~= '' then
    local want = {}
    for part in string.gmatch(filter, '[^,]+') do want[part] = true end
    local kept = {}
    for _, name in ipairs(names) do
      if want[name] then kept[#kept + 1] = name end
    end
    names = kept
  end
  for _, name in ipairs(names) do
    local p = positions[name]
    print(engine .. ': ' .. name .. ' (' .. p.lines .. ' lines, '
      .. p.iters .. ' iters)')
    vim.cmd('edit ' .. files_dir .. '/' .. name)
    local iters = p.iters

    local ok, err = pcall(function()
      run_op(name, 'matching_outer', iters,
        function() set_pos(p.outer) end, api.matching)
      run_op(name, 'matching_middle', iters,
        function() set_pos(p.middle) end, api.matching)
      run_op(name, 'surrounding_deep', iters,
        function() set_pos(p.deep) end, api.surrounding)
      run_op(name, 'highlight', iters,
        function(i) set_pos(i % 2 == 0 and p.middle or p.deep) end,
        api.highlight)
      run_op(name, 'motion_unmatched', iters,
        function() set_pos(p.deep) end, api.motion)
    end)
    if not ok then
      print('  ERROR: ' .. tostring(err))
    end
  end

  local payload = {
    engine = engine,
    nvim = vim.version(),
    results = results,
    times = api.times(),
  }
  vim.fn.writefile({ vim.json.encode(payload) }, out_path)
  print(engine .. ': wrote ' .. out_path)
  vim.cmd('qa!')
end, 200)
