-- Correctness diff harness driver: dumps engine results at every cursor
-- position of the sample files as JSON lines. Run once per engine:
--   g:engine   = 'rs' | 'orig'
--   g:repo     = path to nvim-matchup-rs
--   g:diff_out = output file path
-- Both engines must run with g:matchup_treesitter_enabled = 0 so the
-- original uses its classic engine.

local engine = vim.g.engine
local repo = vim.g.repo
local out_path = vim.g.diff_out

local sample_names = { 'sample.vim', 'sample.lua', 'sample.c', 'sample.html' }
if vim.g.diff_filter and vim.g.diff_filter ~= '' then
  local keep = {}
  for part in string.gmatch(vim.g.diff_filter, '[^,]+') do keep[part] = true end
  local filtered = {}
  for _, n in ipairs(sample_names) do
    if keep[n] then filtered[#filtered + 1] = n end
  end
  sample_names = filtered
end

if engine == 'orig' then
  -- normalize in vimscript: delim dicts contain Funcrefs which do not
  -- round-trip through lua
  vim.api.nvim_exec([[
    function! DiffNorm(d) abort
      if empty(a:d) || !has_key(a:d, 'match') | return v:null | endif
      return [a:d.match, a:d.lnum, a:d.cnum, a:d.side]
    endfunction
    function! DiffCur() abort
      return DiffNorm(matchup#delim#get_current('all', 'both_all', {}))
    endfunction
    function! DiffNext() abort
      return DiffNorm(matchup#delim#get_next('all', 'both_all', {}))
    endfunction
    function! DiffPrev() abort
      return DiffNorm(matchup#delim#get_prev('all', 'both_all', {}))
    endfunction
    function! DiffMat() abort
      " the treesitter get_matching bails when the perf budget is 0; the
      " rust raw handlers run with the budget disabled, so use a huge one
      call matchup#perf#timeout_start(100000000)
      let l:d = matchup#delim#get_current('all', 'both_all', {})
      if empty(l:d) | return v:null | endif
      let l:ms = matchup#delim#get_matching(l:d, {})
      if empty(l:ms) | return v:null | endif
      let l:out = []
      for l:m in l:ms
        call add(l:out, [l:m.match, l:m.lnum, l:m.cnum, l:m.side])
      endfor
      return l:out
    endfunction
    function! DiffSur() abort
      call matchup#perf#timeout_start(100000000)
      let l:s = matchup#delim#get_surrounding('all', 1, {'local': 0})
      if empty(l:s[0]) | return v:null | endif
      return [DiffNorm(l:s[0]), DiffNorm(l:s[1])]
    endfunction
  ]], false)
end

local api = {}
if engine == 'rs' then
  local m = require('matchup_rs')
  -- setup registers the FileType autocmd that applies the native ftplugin
  -- definitions (b:match_words etc.); the raw engine ops below don't need
  -- highlighting, so leave matchparen off to keep the sweep lean.
  local tsv = vim.g.matchup_treesitter_enabled
  m.setup({
    matchparen = { enable = false },
    treesitter = { enable = (tsv == true or tsv == 1) },
  })
  api.warmup = function() m.get_delim('current', 'both_all', {}) end
  api.current = function()
    local d = m.get_delim('current', 'both_all', {})
    if d == nil then return nil end
    return { d.match, d.lnum, d.cnum, d.side }
  end
  api.next = function()
    local d = m.get_delim('next', 'both_all', {})
    if d == nil then return nil end
    return { d.match, d.lnum, d.cnum, d.side }
  end
  api.prev = function()
    local d = m.get_delim('prev', 'both_all', {})
    if d == nil then return nil end
    return { d.match, d.lnum, d.cnum, d.side }
  end
  api.matching = function()
    local cur = vim.api.nvim_win_get_cursor(0)
    local r = m.get_matching_at(cur[1], cur[2] + 1, false)
    if r == nil then return nil end
    local out = {}
    for _, e in ipairs(r.delims) do
      out[#out + 1] = { e[1], e[2], e[3], e[4] }
    end
    if #out == 0 then return nil end
    return out
  end
  api.surrounding = function()
    local s = m.get_surrounding_at(1, false)
    if s == nil then return nil end
    local o, c = s[1], s[2]
    local function one(d)
      if d == nil or d.match == nil then return vim.NIL end
      return { d.match, d.lnum, d.cnum, d.side }
    end
    return { one(o), one(c) }
  end
else
  api.warmup = function() vim.fn.DiffCur() end
  api.current = function() return vim.fn.DiffCur() end
  api.next = function() return vim.fn.DiffNext() end
  api.prev = function() return vim.fn.DiffPrev() end
  api.matching = function() return vim.fn.DiffMat() end
  api.surrounding = function() return vim.fn.DiffSur() end
end

local function precondition()
  return {
    match_words = vim.b.match_words or '',
    match_skip = vim.b.match_skip or '',
    matchpairs = vim.bo.matchpairs or '',
    ignorecase = tostring(vim.b.match_ignorecase or ''),
  }
end

vim.defer_fn(function()
  local lines = {}
  for _, name in ipairs(sample_names) do
    local path = repo .. '/tests/diff/samples/' .. name
    vim.cmd('edit ' .. path)
    api.warmup()
    lines[#lines + 1] = vim.json.encode({
      pre = true, f = name, cond = precondition(),
    })
    local nlines = vim.api.nvim_buf_line_count(0)
    for lnum = 1, nlines do
      local line = vim.api.nvim_buf_get_lines(0, lnum - 1, lnum, false)[1] or ''
      local len = #line
      if len == 0 then len = 1 end
      for cnum = 1, len do
        vim.api.nvim_win_set_cursor(0, { lnum, cnum - 1 })
        local rec = {
          f = name, l = lnum, c = cnum,
          cur = api.current() or vim.NIL,
          nxt = api.next() or vim.NIL,
          prv = api.prev() or vim.NIL,
          mat = api.matching() or vim.NIL,
          sur = api.surrounding() or vim.NIL,
        }
        lines[#lines + 1] = vim.json.encode(rec)
      end
    end
  end
  vim.fn.writefile(lines, out_path)
  print(engine .. ': wrote ' .. #lines .. ' records to ' .. out_path)
  vim.cmd('qa!')
end, 100)
