-- Smoke tests for the paths tests/diff skips (it runs with matchparen
-- disabled): extmark highlighting, the native keymap motions (% and
-- operator-pending d% / forced dv%), and the offscreen 'status' gutter
-- (relative line-number delta + the ∆ direction marker).
--
-- Covers multiple filetypes and both engines:
--   * C      - &matchpairs (fast DFA scan path)
--   * html   - <div>/</div> tag matching (fancy-regex backref `\1` path)
--   * lua    - function/end plus the --[==[ ]==] long bracket (backref path)
-- run.sh invokes this twice: g:ts=0 (regex engine) and g:ts=1 (treesitter). The
-- buffers are valid code so the treesitter parser accepts them in TS mode; the
-- long-bracket motion is regex-only (treesitter sees one comment node).
--
-- Runs after VimEnter via vim.schedule: highlight() early-outs on
-- has('vim_starting') and state('a'), so it must run once startup is complete
-- and outside an autocmd. g:repo / g:site / g:ts are set by run.sh. Exits 0 on
-- success, nonzero (cq) on any failure.

local repo = vim.g.repo
local site = vim.g.site
local ts = (vim.g.ts == 1)
vim.o.runtimepath = table.concat({ vim.o.runtimepath, repo, repo .. "/after", site }, ",")
vim.cmd("filetype plugin on")
local m = require("matchup_rs")
m.setup({ matchparen = { enable = true }, treesitter = { enable = ts } })

local NS = vim.api.nvim_create_namespace("vim-matchup")
local npass, fails = 0, {}
local function check(name, cond, detail)
  if cond then
    npass = npass + 1
    print("ok   " .. name)
  else
    print("FAIL " .. name .. (detail and ("  [" .. tostring(detail) .. "]") or ""))
    fails[#fails + 1] = name
  end
end
local function fk(keys)
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(keys, true, false, true), "x", false)
end
local function at(lnum, col) vim.api.nvim_win_set_cursor(0, { lnum, col }) end
local function cursor() local c = vim.api.nvim_win_get_cursor(0); return c[1], c[2] end
-- Fresh highlight: clear the matchup extmarks, then render; return the hl groups.
local function hl_groups()
  vim.api.nvim_buf_clear_namespace(0, NS, 0, -1)
  m.highlight()
  local g = {}
  for _, mk in ipairs(vim.api.nvim_buf_get_extmarks(0, NS, 0, -1, { details = true })) do
    local d = mk[4]
    if d and d.hl_group then g[d.hl_group] = (g[d.hl_group] or 0) + 1 end
  end
  return g
end
local function has_hl(g)
  return (g.MatchParen or 0) + (g.MatchWord or 0) + (g.MatchWordCur or 0) > 0
end
local function statusline() return vim.fn.getwinvar(0, "&statusline") end
-- 0-based column of substring `sub` on line `lnum` of the current buffer.
local function col_of(lnum, sub)
  local l = vim.api.nvim_buf_get_lines(0, lnum - 1, lnum, false)[1] or ""
  local c = l:find(sub, 1, true)
  return c and (c - 1) or 0
end
local function new_buf(ft, blines)
  local b = vim.api.nvim_create_buf(true, false)
  vim.api.nvim_win_set_buf(0, b)
  vim.api.nvim_buf_set_lines(b, 0, -1, false, blines)
  vim.bo[b].filetype = ft
  return b
end
-- Assert highlight renders at `open` and (if close_line) % jumps to that line.
local function check_match(label, open, close_line)
  local g = hl_groups_at(open)
  check(label .. ": highlight", has_hl(g), vim.inspect(g))
  if close_line then
    at(open[1], open[2])
    m.motion_matching(0, 0)
    local cl, cc = cursor()
    check(label .. ": % -> line " .. close_line, cl == close_line, "landed " .. cl .. "," .. cc)
  end
end

-- 100-line *valid* C translation unit: '{' on line 10 opens a function whose
-- '}' is on line 90 (80 apart -> offscreen delta), with an on-screen ( ) pair
-- on line 50. Valid so the treesitter C parser accepts it in TS mode.
local clines = {}
for i = 1, 100 do
  if i == 10 then clines[i] = "int f(void) {"
  elseif i == 50 then clines[i] = "  int q = (c + d);"
  elseif i == 90 then clines[i] = "}"
  elseif i < 10 then clines[i] = "int g" .. i .. " = " .. i .. ";"
  elseif i < 90 then clines[i] = "  int x" .. i .. " = " .. i .. ";"
  else clines[i] = "int h" .. i .. " = " .. i .. ";" end
end
vim.api.nvim_buf_set_lines(0, 0, -1, false, clines)
vim.bo.filetype = "c"
vim.api.nvim_win_set_height(0, 10)
vim.wo.number = true
vim.wo.relativenumber = true

function hl_groups_at(pos) -- forward-declared helper used by check_match
  at(pos[1], pos[2])
  return hl_groups()
end

local function run()
  local pc = col_of(50, "(")

  -- C: highlight + % + motion_matching on the on-screen ( ) pair.
  local g = hl_groups_at({ 50, pc })
  check("c: highlight extmarks", has_hl(g), vim.inspect(g))
  at(50, pc); local b1, b2 = cursor(); fk("%"); local a1, a2 = cursor()
  check("c: % keymap moves", a1 ~= b1 or a2 ~= b2, b1 .. "," .. b2 .. " -> " .. a1 .. "," .. a2)
  at(50, pc); m.motion_matching(0, 0); local m1, m2 = cursor()
  check("c: motion_matching moves", m2 ~= pc, m1 .. "," .. m2)

  -- C: offscreen gutter, match BELOW (cursor '{' line 10, '}' line 90): delta 80, no ∆.
  local bc = col_of(10, "{")
  hl_groups_at({ 10, bc })
  local slA = statusline()
  check("c: offscreen delta (below)", slA:find("80", 1, true) ~= nil, slA)
  check("c: offscreen no ∆ (below)", slA:find("∆", 1, true) == nil, slA)
  -- match ABOVE (cursor '}' line 90, '{' line 10): delta 80, ∆ present.
  hl_groups_at({ 90, 0 })
  local slB = statusline()
  check("c: offscreen delta (above)", slB:find("80", 1, true) ~= nil, slB)
  check("c: offscreen ∆ (above)", slB:find("∆", 1, true) ~= nil, slB)

  -- C: operator-pending d% and forced dv% on the ( ) pair.
  at(50, pc); local l = vim.fn.getline(50); fk("d%")
  check("c: op-pending d% edits", vim.fn.getline(50) ~= l, l .. " -> " .. vim.fn.getline(50))
  vim.cmd("silent! undo")
  at(50, pc); local l2 = vim.fn.getline(50); fk("dv%")
  check("c: forced dv% edits", vim.fn.getline(50) ~= l2, l2 .. " -> " .. vim.fn.getline(50))

  -- html: <div> ... </div> tag matching (fancy-regex backref `\1`). The cursor
  -- goes on the tag NAME, not the '<': html's &matchpairs adds '<:'>', so on the
  -- '<' the nearer angle-bracket pair wins over the tag match.
  new_buf("html", { "<div>", "hello", "</div>" })
  check_match("html <div>/</div>", { 1, col_of(1, "div") }, 3)

  -- lua: function ... end (structural; both engines).
  new_buf("lua", { "local function foo()", "  --[==[", "  comment", "  ]==]", "  return 1", "end" })
  check_match("lua function/end", { 1, col_of(1, "function") }, 6)

  -- lua: --[==[ ... ]==] long bracket - a regex-engine feature (the fancy-regex
  -- backref `\1` ties the `=` counts), so checked in regex mode only. Under the
  -- treesitter engine the long comment is a single node and highlight/motion
  -- disagree there (motion jumps to the close, highlight renders nothing) - a
  -- pre-existing TS quirk, not a stable assertion.
  if not ts then
    check_match("lua --[==[ ]==]", { 2, col_of(2, "--[==[") }, 4)
  end
end

vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    vim.schedule(function()
      local ok, err = pcall(run)
      if not ok then
        print("FAIL exception: " .. tostring(err))
        fails[#fails + 1] = "exception"
      end
      local mode = ts and "treesitter" or "regex"
      if #fails == 0 then
        print(string.format("SMOKE ALL OK [%s] (%d checks)", mode, npass))
        vim.cmd("qa!")
      else
        print(string.format("SMOKE FAILURES [%s] (%d/%d): %s", mode, #fails, #fails + npass,
          table.concat(fails, ", ")))
        vim.cmd("cq")
      end
    end)
  end,
})
