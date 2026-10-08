-- Build the native module and deploy it where require('matchup_rs') finds it.
--
--   :lua require('matchup_rs.build').build()
--   :lua require('matchup_rs.build').build({ profile = 'debug' })
--   :lua require('matchup_rs.build').build({ touch = true })  -- WSL2/drvfs mtime
--
-- Pure Lua: this does NOT depend on the compiled .so, so it can build it from
-- scratch. cargo names the cdylib lib<name>.so / lib<name>.dylib on unix and
-- <name>.dll on windows; nvim's package.cpath loads it as lua/matchup_rs.so
-- (unix) or lua/matchup_rs.dll (windows).
--
-- opts:
--   dir      repo root (default: derived from this file's path, else :pwd)
--   profile  'release' (default) | 'debug'
--   touch    touch src/*.rs before building (stale-mtime workaround on drvfs)

local M = {}

local function uv()
  return vim.uv or vim.loop
end

local function sep()
  -- directory separator for the running platform
  return (package.config:sub(1, 1) == '\\') and '\\' or '/'
end

local function is_windows()
  if jit and jit.os then
    return jit.os == 'Windows'
  end
  return package.config:sub(1, 1) == '\\'
end

local function is_mac()
  return jit ~= nil and (jit.os == 'OSX' or jit.os == 'Darwin')
end

-- <root>/lua/matchup_rs/build.lua  ->  <root>
local function repo_root(opts)
  if opts and opts.dir then
    return opts.dir
  end
  local src = debug.getinfo(1, 'S').source or ''
  if src:sub(1, 1) == '@' then
    src = src:sub(2)
  end
  local s = sep()
  local root = src:match('^(.*)' .. s .. 'lua' .. s .. 'matchup_rs' .. s .. 'build%.lua$')
  if root and root ~= '' then
    return root
  end
  return vim.fn.getcwd()
end

-- Copy src over dest atomically: write to dest.tmp then rename, so a .so that
-- is currently mmap'd by this nvim is not clobbered in place (avoids ETXTBSY).
local function copy_over(u, src, dest)
  if not u.fs_stat(src) then
    return nil, 'artifact not found: ' .. src
  end
  local tmp = dest .. '.tmp'
  local ok, err = u.fs_copyfile(src, tmp)
  if not ok then
    return nil, 'copy failed: ' .. tostring(err)
  end
  local ok2, err2 = u.fs_rename(tmp, dest)
  if not ok2 then
    u.fs_unlink(tmp)
    return nil, 'rename failed: ' .. tostring(err2)
  end
  return true
end

function M.build(opts)
  opts = opts or {}
  local u = uv()
  local root = repo_root(opts)
  local s = sep()
  local release = (opts.profile or 'release') == 'release'

  if opts.touch then
    local now = os.time()
    for _, f in ipairs(vim.fn.glob(root .. s .. 'src' .. s .. '*.rs', false, true)) do
      u.fs_utime(f, now, now)
    end
  end

  local args = { 'cargo', 'build' }
  if release then
    args[#args + 1] = '--release'
  end
  args[#args + 1] = '--manifest-path'
  args[#args + 1] = root .. s .. 'Cargo.toml'
  args[#args + 1] = '--target-dir'
  args[#args + 1] = root .. s .. 'target'

  print('build: ' .. table.concat(args, ' '))
  local out = vim.fn.system(args)
  if vim.v.shell_error ~= 0 then
    io.stderr:write('build: cargo failed:\n' .. (out or '') .. '\n')
    return false
  end

  local prof_dir = release and 'release' or 'debug'
  local target = root .. s .. 'target' .. s .. prof_dir
  local artifact, dest
  if is_windows() then
    artifact = target .. s .. 'matchup_rs.dll'
    dest = root .. s .. 'lua' .. s .. 'matchup_rs.dll'
  elseif is_mac() then
    artifact = target .. s .. 'libmatchup_rs.dylib'
    dest = root .. s .. 'lua' .. s .. 'matchup_rs.so'
  else
    artifact = target .. s .. 'libmatchup_rs.so'
    dest = root .. s .. 'lua' .. s .. 'matchup_rs.so'
  end

  local luadir = root .. s .. 'lua'
  if not u.fs_stat(luadir) then
    u.fs_mkdir(luadir, 493) -- 0755
  end

  local ok, err = copy_over(u, artifact, dest)
  if not ok then
    io.stderr:write('build: ' .. err .. '\n')
    return false
  end

  -- build() only (re)writes the file; a running nvim that already require'd the
  -- native module keeps the old code until it is restarted.
  print('build: ' .. artifact .. ' -> ' .. dest)
  return true
end

return M
