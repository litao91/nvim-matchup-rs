-- Build the native module and deploy it where require('matchup_rs') finds it.
--
--   :lua require('matchup_rs.build').ensure()                 -- build only if outdated
--   :lua require('matchup_rs.build').build()                  -- always (re)build
--   :lua require('matchup_rs.build').build({ profile = 'debug' })
--   :lua require('matchup_rs.build').build({ touch = true })  -- WSL2/drvfs mtime
--   :lua require('matchup_rs.build').build_log()              -- open the build log
--
-- Follows the blink.cmp fuzzy-build convention: an async `vim.system` cargo
-- run (non-blocking), progress notifications, and the compiler output captured
-- to a log file you can open with build_log(). Pure Lua - this does NOT depend
-- on the compiled .so, so it can build it from scratch. cargo names the cdylib
-- lib<name>.so / lib<name>.dylib on unix and <name>.dll on windows; nvim's
-- package.cpath loads it as lua/matchup_rs.so (unix) or lua/matchup_rs.dll.
--
-- The build is asynchronous: build() returns immediately and notifies on
-- completion. A running nvim that already require'd the native module keeps the
-- old code until restarted.
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

local function log_path()
  return vim.fn.stdpath('cache') .. sep() .. 'matchup_rs_build.log'
end

local function notify(level, msg)
  vim.notify(msg, level, { title = 'matchup_rs' })
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

-- Where require('matchup_rs') loads the native module from (the deploy target).
local function dest_path(root)
  local s = sep()
  return root .. s .. 'lua' .. s .. (is_windows() and 'matchup_rs.dll' or 'matchup_rs.so')
end

-- cargo's output for the cdylib, per platform/profile.
local function artifact_path(root, release)
  local s = sep()
  local target = root .. s .. 'target' .. s .. (release and 'release' or 'debug')
  if is_windows() then
    return target .. s .. 'matchup_rs.dll'
  elseif is_mac() then
    return target .. s .. 'libmatchup_rs.dylib'
  end
  return target .. s .. 'libmatchup_rs.so'
end

-- Deploy the built cdylib to lua/matchup_rs.{so,dll}.
local function deploy(u, root, release)
  local s = sep()
  local luadir = root .. s .. 'lua'
  if not u.fs_stat(luadir) then
    u.fs_mkdir(luadir, 493) -- 0755
  end
  local artifact = artifact_path(root, release)
  local dest = dest_path(root)
  local ok, err = copy_over(u, artifact, dest)
  if not ok then
    return nil, err
  end
  return dest
end

local function mtime(u, path)
  local st = u.fs_stat(path)
  if not st then
    return nil
  end
  return st.mtime.sec + st.mtime.nsec * 1e-9
end

-- Source inputs whose change should force a rebuild.
local function source_files(root)
  local s = sep()
  local files = vim.fn.glob(root .. s .. 'src' .. s .. '**' .. s .. '*.rs', false, true)
  for _, extra in ipairs({ 'Cargo.toml', 'Cargo.lock', 'build.rs' }) do
    local p = root .. s .. extra
    if vim.uv.fs_stat(p) then
      files[#files + 1] = p
    end
  end
  return files
end

-- True when the deployed native module is missing or older than any source
-- input (i.e. the binary is outdated and should be rebuilt).
local function is_outdated(u, root, release)
  local so_mtime = mtime(u, dest_path(root))
  if not so_mtime then
    return true -- not built yet
  end
  for _, f in ipairs(source_files(root)) do
    local m = mtime(u, f)
    if m and m > so_mtime then
      return true
    end
  end
  return false
end

--- Build the native module from source (asynchronous).
--- @param opts? { dir?: string, profile?: 'release'|'debug', touch?: boolean }
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

  local log = io.open(log_path(), 'w')
  if log then
    log:write('Working Directory: ' .. root .. '\n')
    log:write('Command: ' .. table.concat(args, ' ') .. '\n\n---\n\n')
    log:flush()
  end

  notify(vim.log.levels.INFO, 'Building native module from source...')

  vim.system(args, {
    cwd = root,
    text = true,
    stdout = function(_, data)
      if log and data then
        log:write(data)
        log:flush()
      end
    end,
    stderr = function(_, data)
      if log and data then
        log:write(data)
        log:flush()
      end
    end,
  }, vim.schedule_wrap(function(res)
    if log then
      log:close()
      log = nil
    end
    if res.code ~= 0 then
      notify(vim.log.levels.ERROR,
        'Failed to build native module (see require("matchup_rs.build").build_log())')
      return
    end
    local dest, err = deploy(u, root, release)
    if not dest then
      notify(vim.log.levels.ERROR, 'Build succeeded but deploy failed: ' .. tostring(err))
      return
    end
    notify(vim.log.levels.INFO, 'Successfully built native module (restart nvim to load it).')
  end))
end

--- Ensure the native module is present and up to date: rebuild (async) when the
--- deployed binary is missing or older than any source input, then report
--- whether it can be loaded *now*. A rebuild takes effect on the next start
--- (a running nvim keeps the currently loaded module), so when this returns
--- false the caller should skip setup and let the user restart once the build
--- notification arrives.
--- @param opts? { dir?: string, profile?: 'release'|'debug', touch?: boolean }
--- @return boolean loadable true if require('matchup_rs') succeeds this session
function M.ensure(opts)
  opts = opts or {}
  local u = uv()
  local root = repo_root(opts)
  local release = (opts.profile or 'release') == 'release'
  if is_outdated(u, root, release) then
    M.build(opts)
  end
  return pcall(require, 'matchup_rs')
end

--- Open the captured build log.
function M.build_log()
  local p = log_path()
  if not uv().fs_stat(p) then
    notify(vim.log.levels.WARN, 'No build log yet: ' .. p)
    return
  end
  vim.cmd('edit ' .. vim.fn.fnameescape(p))
end

return M
