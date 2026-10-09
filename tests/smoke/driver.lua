-- Smoke tests for the paths tests/diff skips (it runs with matchparen
-- disabled): extmark highlighting, the native keymap motions (% and
-- operator-pending d% / forced dv%), and the offscreen 'status' gutter
-- (relative line-number delta + the ∆ direction marker).
--
-- Runs after VimEnter via vim.schedule: highlight() early-outs on
-- has('vim_starting') and state('a'), so it must run once startup is complete
-- and outside an autocmd. g:repo / g:site are set by run.sh. Exits 0 on success,
-- nonzero (cq) on any failure.

local repo = vim.g.repo
local site = vim.g.site
vim.o.runtimepath = table.concat({ vim.o.runtimepath, repo, repo .. "/after", site }, ",")
vim.cmd("filetype plugin on")
local m = require("matchup_rs")
m.setup({ matchparen = { enable = true }, treesitter = { enable = false } })

-- 100-line C buffer: an on-screen pair on line 50 and an off-screen pair
-- (line 10 '{' <-> line 90 '}') with the 10-row window.
local lines = {}
for i = 1, 100 do lines[i] = "filler " .. i end
lines[10] = "aa { bb"
lines[50] = "xx ( yy ) zz"
lines[90] = "cc } dd"
vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
vim.bo.filetype = "c"
vim.api.nvim_win_set_height(0, 10)
vim.wo.number = true
vim.wo.relativenumber = true

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
-- Fresh highlight: clear the matchup extmarks, then render.
local function hl()
  vim.api.nvim_buf_clear_namespace(0, NS, 0, -1)
  m.highlight()
end
local function ext_groups()
  local g = {}
  for _, mk in ipairs(vim.api.nvim_buf_get_extmarks(0, NS, 0, -1, { details = true })) do
    local d = mk[4]
    if d and d.hl_group then g[d.hl_group] = (g[d.hl_group] or 0) + 1 end
  end
  return g
end
local function statusline() return vim.fn.getwinvar(0, "&statusline") end

local function run()
  -- 1) highlight renders MatchParen/MatchWord extmarks at an on-screen delim.
  at(50, 3) -- the '(' on line 50
  hl()
  local g = ext_groups()
  check("highlight extmarks", (g.MatchParen or 0) + (g.MatchWord or 0) > 0, vim.inspect(g))

  -- 2) the '%' keymap jumps to the match.
  at(50, 3)
  local b = vim.api.nvim_win_get_cursor(0)
  fk("%")
  local a = vim.api.nvim_win_get_cursor(0)
  check("% keymap moves", a[1] ~= b[1] or a[2] ~= b[2],
    b[1] .. "," .. b[2] .. " -> " .. a[1] .. "," .. a[2])

  -- 3) motion_matching() direct entry point.
  at(50, 3)
  m.motion_matching(0, 0)
  local c = vim.api.nvim_win_get_cursor(0)
  check("motion_matching moves", c[2] ~= 3, vim.inspect(c))

  -- 4) offscreen gutter, match BELOW the cursor: delta |90-10|=80, no ∆.
  at(10, 3) -- the '{' on line 10; '}' off-screen at line 90
  hl()
  local slA = statusline()
  check("offscreen delta (match below)", slA:find("80", 1, true) ~= nil, slA)
  check("offscreen no ∆ (match below)", slA:find("∆", 1, true) == nil, slA)

  -- 5) offscreen gutter, match ABOVE the cursor: delta |10-90|=80, ∆ present.
  at(90, 3) -- the '}' on line 90; '{' off-screen at line 10
  hl()
  local slB = statusline()
  check("offscreen delta (match above)", slB:find("80", 1, true) ~= nil, slB)
  check("offscreen ∆ (match above)", slB:find("∆", 1, true) ~= nil, slB)

  -- 6) operator-pending d% deletes to the match.
  at(50, 3)
  local l = vim.fn.getline(50)
  fk("d%")
  check("op-pending d% edits", vim.fn.getline(50) ~= l, l .. " -> " .. vim.fn.getline(50))
  vim.cmd("silent! undo")

  -- 7) forced dv% (exercises the motion_force "nov" path).
  at(50, 3)
  local l2 = vim.fn.getline(50)
  fk("dv%")
  check("forced dv% edits", vim.fn.getline(50) ~= l2, l2 .. " -> " .. vim.fn.getline(50))
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
      if #fails == 0 then
        print(string.format("SMOKE ALL OK (%d checks)", npass))
        vim.cmd("qa!")
      else
        print(string.format("SMOKE FAILURES (%d/%d): %s", #fails, #fails + npass,
          table.concat(fails, ", ")))
        vim.cmd("cq")
      end
    end)
  end,
})
