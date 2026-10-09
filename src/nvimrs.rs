//! Native FFI bindings to Neovim's C API, matching the **0.13-dev** ABI.
//!
//! nvim-oxi 0.6.0 hardcodes Neovim v0.9/v0.10 signatures and keyset layouts.
//! On 0.13-dev three things break:
//!   * `nvim_call_function` gained a leading `uint64_t channel_id`
//!     (nvim commit 8d51e07fa1) -> oxi's 4-arg binding misaligns -> SIGSEGV.
//!   * `nvim_echo` now returns `Object` (was void) -> oxi never passes the
//!     hidden sret pointer -> broken.
//!   * `nvim_create_autocmd`'s `KeyDict_create_autocmd` layout drifted, so
//!     oxi's `.callback(...)` LuaRef is read at the wrong offset -> the
//!     autocmd registers but the callback never fires.
//!
//! This module declares those functions with their true current signatures
//! (verified against ~/repos/neovim/src/nvim/api/*.c) and the exact generated
//! keyset layouts (~/repos/neovim/build/.../keysets_defs.generated.h), reusing
//! nvim-oxi's layout-compatible `Object`/`Array`/`Dictionary` for data.
//! Everything else (handles, `get_var`, `win.get_cursor`, extmarks, ...) keeps
//! using nvim-oxi, whose bindings for those are still correct.

use std::ffi::{c_char, c_void, CString};
use std::ptr;

use nvim_oxi::conversion::FromObject;
use nvim_oxi::{Array, Object};

/// `LUA_INTERNAL_CALL` = `(1<<63) + 1` (defs.h: INTERNAL_CALL_MASK, VIML/LUA).
const LUA_INTERNAL_CALL: u64 = (1u64 << 63) + 1;

// --- repr(C) primitives matching Neovim's layout ---------------------------

/// Neovim `String` = `{ char *data; size_t size; }`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CStr {
    data: *const c_char,
    size: usize,
}

/// Neovim `Error` = `{ ErrorType type; char *msg; }`, `kErrorTypeNone == -1`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CError {
    etype: i32,
    msg: *mut c_char,
}

impl CError {
    const fn new() -> Self {
        CError { etype: -1, msg: ptr::null_mut() }
    }
    fn is_err(&self) -> bool {
        self.etype != -1
    }
}

/// Neovim `Array` (kvec) = `{ size_t size; size_t capacity; Object *items; }`.
/// Borrowed view: no `Drop`, so the owning `nvim_oxi::Array` stays in charge.
#[repr(C)]
#[derive(Clone, Copy)]
struct CArray {
    size: usize,
    capacity: usize,
    items: *const Object,
}

impl CArray {
    fn borrow(a: &Array) -> CArray {
        CArray { size: a.len(), capacity: a.len(), items: a.as_ptr() }
    }
}

/// `KeyDict_create_autocmd` (keysets_defs.h:277-288), field order = memory
/// order. Bit indices from keysets_defs.generated.h.
#[repr(C)]
struct KeyDictCreateAutocmd {
    is_set: u64,
    buffer: i32,   // Buffer (deprecated)
    buf: i32,      // Buffer
    callback: Object, // Union(String, LuaRef)
    command: CStr,
    desc: CStr,
    group: Object,    // Union(Integer, String)
    nested: bool,
    once: bool,
    pattern: Object,  // Union(String, ArrayOf(String))
}
const OPTIDX_AUTOCMD_GROUP: u64 = 4;
const OPTIDX_AUTOCMD_COMMAND: u64 = 7;
const OPTIDX_AUTOCMD_PATTERN: u64 = 8;
const OPTIDX_AUTOCMD_CALLBACK: u64 = 9;

impl KeyDictCreateAutocmd {
    fn zeroed() -> Self {
        // SAFETY: every field is a plain POD / zero-valid Object (Nil).
        unsafe { std::mem::zeroed() }
    }
}

/// `KeyDict_option` (keysets_defs.h:171-180).
#[repr(C)]
struct KeyDictOption {
    is_set: u64,
    scope: CStr,
    win: i32,
    buf: i32,
    tab: i32,
    filetype: CStr,
    operation: CStr,
    dry_run: bool,
}
const OPTIDX_OPTION_BUF: u64 = 1;
const OPTIDX_OPTION_WIN: u64 = 3;
const OPTIDX_OPTION_SCOPE: u64 = 4;

/// `KeyDict_echo_opts` (keysets_defs.h:370-382); passed zeroed (all defaults).
#[repr(C)]
struct KeyDictEchoOpts {
    is_set: u64,
    err: bool,
    verbose: bool,
    truncate: bool,
    kind: CStr,
    id: Object,
    title: CStr,
    status: CStr,
    percent: i64,
    source: CStr,
    data: CArray, // Dict = kvec(KeyValuePair); zeroed = empty, never read (is_set=0)
}

unsafe extern "C" {
    fn nvim_eval(expr: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_call_function(
        channel_id: u64,
        name: CStr,
        args: CArray,
        arena: *mut c_void,
        err: *mut CError,
    ) -> Object;
    fn nvim_echo(
        chunks: CArray,
        history: bool,
        opts: *const KeyDictEchoOpts,
        err: *mut CError,
    ) -> Object;
    fn nvim_create_autocmd(
        channel_id: u64,
        event: Object,
        opts: *const KeyDictCreateAutocmd,
        arena: *mut c_void,
        err: *mut CError,
    ) -> i64;
    fn nvim_get_option_value(
        name: CStr,
        opts: *const KeyDictOption,
        err: *mut CError,
    ) -> Object;
    fn nvim_get_var(name: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_get_vvar(name: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_buf_get_var(buf: i32, name: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_get_runtime_file(
        name: CStr,
        all: bool,
        arena: *mut c_void,
        err: *mut CError,
    ) -> Array;
}

// --- safe wrappers ---------------------------------------------------------

fn cstr(s: &str) -> Option<(CString, CStr)> {
    let c = CString::new(s).ok()?;
    let cs = CStr { data: c.as_ptr(), size: s.len() };
    Some((c, cs))
}

/// Native `nvim_eval` - ONLY for the genuinely-arbitrary vimscript expressions
/// (expression-valued `b:match_words`, raw `b:match_skip`, linewise-op config).
/// Everything else uses a dedicated native call.
pub fn eval(expr: &str) -> Option<Object> {
    let (_guard, cexpr) = cstr(expr)?;
    let mut err = CError::new();
    let obj = unsafe { nvim_eval(cexpr, ptr::null_mut(), &mut err) };
    if err.is_err() {
        return None;
    }
    Some(obj)
}

pub fn eval_as<V: FromObject>(expr: &str) -> Option<V> {
    let obj = eval(expr)?;
    V::from_object(obj).ok()
}

/// Native `nvim_call_function` (correct 0.13 signature with leading
/// channel_id). `args` is borrowed for the duration of the call.
pub fn call_function(name: &str, args: &Array) -> Option<Object> {
    let (_guard, cname) = cstr(name)?;
    let cargs = CArray::borrow(args);
    let mut err = CError::new();
    let obj =
        unsafe { nvim_call_function(LUA_INTERNAL_CALL, cname, cargs, ptr::null_mut(), &mut err) };
    if err.is_err() {
        return None;
    }
    Some(obj)
}

pub fn call_fn_as<V: FromObject>(name: &str, args: &Array) -> Option<V> {
    let obj = call_function(name, args)?;
    V::from_object(obj).ok()
}

/// `call_function(name, [])`
pub fn call_fn0_as<V: FromObject>(name: &str) -> Option<V> {
    call_fn_as(name, &Array::new())
}

/// Native `nvim_echo` with a single plain-text chunk (history=true).
pub fn echo(text: &str) {
    let chunks = Array::from_iter([Object::from(Array::from_iter([Object::from(text)]))]);
    let cchunks = CArray::borrow(&chunks);
    // zeroed echo opts -> all defaults
    let opts: KeyDictEchoOpts = unsafe { std::mem::zeroed() };
    let mut err = CError::new();
    let _ret = unsafe { nvim_echo(cchunks, true, &opts, &mut err) };
    // _ret (Object) drops here, freeing nvim's allocation.
}

/// Native option read. `buf`/`win` = 0 means global scope.
pub fn get_option_value(name: &str, buf: i32, win: i32) -> Option<Object> {
    get_option_scoped(name, "", buf, win)
}

/// Native option read with an explicit `scope` ("", "local", "global").
fn get_option_scoped(name: &str, scope: &str, buf: i32, win: i32) -> Option<Object> {
    let (_guard, cname) = cstr(name)?;
    let (_sguard, cscope) = cstr(scope)?;
    let mut opts: KeyDictOption = unsafe { std::mem::zeroed() };
    if !scope.is_empty() {
        opts.scope = cscope;
        opts.is_set |= 1 << OPTIDX_OPTION_SCOPE;
    }
    if buf != 0 {
        opts.buf = buf;
        opts.is_set |= 1 << OPTIDX_OPTION_BUF;
    }
    if win != 0 {
        opts.win = win;
        opts.is_set |= 1 << OPTIDX_OPTION_WIN;
    }
    let mut err = CError::new();
    let obj = unsafe { nvim_get_option_value(cname, &opts, &mut err) };
    if err.is_err() {
        return None;
    }
    Some(obj)
}

pub fn get_option_as<V: FromObject>(name: &str, buf: i32, win: i32) -> Option<V> {
    let obj = get_option_value(name, buf, win)?;
    V::from_object(obj).ok()
}

/// Window-local option read (`&l:{name}` in vimscript).
pub fn get_option_local_as<V: FromObject>(name: &str, win: i32) -> Option<V> {
    let obj = get_option_scoped(name, "local", 0, win)?;
    V::from_object(obj).ok()
}

/// Native `nvim_get_var` (g: scope).
pub fn get_var_as<V: FromObject>(name: &str) -> Option<V> {
    let (_g, cname) = cstr(name)?;
    let mut err = CError::new();
    let obj = unsafe { nvim_get_var(cname, ptr::null_mut(), &mut err) };
    if err.is_err() {
        None
    } else {
        V::from_object(obj).ok()
    }
}

/// Native `nvim_get_vvar` (v: scope, e.g. count/operator/register/insertmode).
pub fn get_vvar_as<V: FromObject>(name: &str) -> Option<V> {
    let (_g, cname) = cstr(name)?;
    let mut err = CError::new();
    let obj = unsafe { nvim_get_vvar(cname, ptr::null_mut(), &mut err) };
    if err.is_err() {
        None
    } else {
        V::from_object(obj).ok()
    }
}

/// Native `nvim_buf_get_var` (b: scope) by buffer handle.
pub fn buf_get_var_as<V: FromObject>(buf: i32, name: &str) -> Option<V> {
    let (_g, cname) = cstr(name)?;
    let mut err = CError::new();
    let obj = unsafe { nvim_buf_get_var(buf, cname, ptr::null_mut(), &mut err) };
    if err.is_err() {
        None
    } else {
        V::from_object(obj).ok()
    }
}

/// Native `nvim_get_runtime_file`.
pub fn get_runtime_file(name: &str, all: bool) -> Option<Array> {
    let (_g, cname) = cstr(name)?;
    let mut err = CError::new();
    let arr = unsafe { nvim_get_runtime_file(cname, all, ptr::null_mut(), &mut err) };
    if err.is_err() {
        None
    } else {
        Some(arr)
    }
}

/// Register an autocmd whose handler is a native Rust callback (a real LuaRef
/// built by nvim-oxi's registry, packed into a correctly-laid-out
/// `KeyDict_create_autocmd`). `f` returns whether to delete the autocmd.
pub fn create_autocmd_cb<F>(events: &[&str], group: i32, pattern: &str, f: F) -> bool
where
    F: Fn(nvim_oxi::api::types::AutocmdCallbackArgs) -> bool + 'static,
{
    use nvim_oxi::Function;
    let func: Function<nvim_oxi::api::types::AutocmdCallbackArgs, bool> =
        Function::from_fn(move |args| -> nvim_oxi::Result<bool> { Ok(f(args)) });
    let cb_obj = Object::from(func); // tagged LuaRef object

    let event_arr = Array::from_iter(events.iter().map(|e| Object::from(*e)));
    let event = Object::from(event_arr);

    let mut opts = KeyDictCreateAutocmd::zeroed();
    opts.callback = cb_obj;
    opts.is_set |= 1 << OPTIDX_AUTOCMD_CALLBACK;
    opts.group = Object::from(group as i64);
    opts.is_set |= 1 << OPTIDX_AUTOCMD_GROUP;
    opts.pattern = Object::from(pattern.to_string());
    opts.is_set |= 1 << OPTIDX_AUTOCMD_PATTERN;

    let mut err = CError::new();
    let id = unsafe {
        nvim_create_autocmd(LUA_INTERNAL_CALL, event, &opts, ptr::null_mut(), &mut err)
    };
    !err.is_err() && id > 0
}

/// Register a command-string autocmd through the same corrected keyset (used
/// where a native callback is not wanted).
pub fn create_autocmd_cmd(events: &[&str], group: i32, pattern: &str, command: &str) -> bool {
    let event_arr = Array::from_iter(events.iter().map(|e| Object::from(*e)));
    let event = Object::from(event_arr);
    // The CString guard lives until after the call; nvim copies `command`.
    let (_cmd_guard, ccmd) = match cstr(command) {
        Some(x) => x,
        None => return false,
    };
    let mut opts = KeyDictCreateAutocmd::zeroed();
    opts.command = ccmd;
    opts.is_set |= 1 << OPTIDX_AUTOCMD_COMMAND;
    opts.group = Object::from(group as i64);
    opts.is_set |= 1 << OPTIDX_AUTOCMD_GROUP;
    opts.pattern = Object::from(pattern.to_string());
    opts.is_set |= 1 << OPTIDX_AUTOCMD_PATTERN;
    let mut err = CError::new();
    let id =
        unsafe { nvim_create_autocmd(LUA_INTERNAL_CALL, event, &opts, ptr::null_mut(), &mut err) };
    !err.is_err() && id > 0
}
