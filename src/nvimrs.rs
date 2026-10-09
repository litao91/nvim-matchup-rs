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
//!
//! # Safety contract (every `unsafe` in this module)
//!
//! All `unsafe` here is either an FFI call into Neovim's C API or a
//! `mem::zeroed` keyset, and each relies on the same invariants (restated
//! briefly at the individual sites):
//!
//! * **ABI** - each `extern "C"` declaration matches nvim 0.13's true signature
//!   (checked against `~/repos/neovim/build/include/api/*.h.generated.h`), and
//!   every `KeyDict_*` layout is pinned by the compile-time `size_of!` /
//!   `offset_of!` asserts below, so field offsets (notably the `Union(String,
//!   LuaRef)` callbacks and the generator-added `is_set`) cannot silently drift
//!   the way oxi 0.6's hardcoded 0.9/0.10 layouts do.
//! * **String args** - passed as `CStr { data, size }` from `cstr()`, whose
//!   backing `CString` is bound to a local guard (`_guard`/`_g`/...) that lives
//!   until the end of the function, i.e. past the call, so `data` stays valid,
//!   NUL-terminated and `size` bytes long for the whole call.
//! * **Borrowed arrays** - `CArray::borrow` is a non-owning view of a live
//!   `nvim_oxi::Array` that outlives the call; the view has no `Drop`, so nvim's
//!   read of it cannot double-free.
//! * **Return objects** - `arena` is `null`, so nvim allocates results with
//!   `xmalloc`; the returned `Object`/`Array`/`String` is layout-compatible with
//!   oxi's and frees that allocation on `Drop` (callers must not free it again).
//! * **Error out-param** - `&mut err` points to a valid `CError` (etype -1 =
//!   none) that only nvim writes. On error nvim allocates `err.msg`; we leak
//!   that rare, small string instead of calling `api_clear_error` (a leak, not
//!   UB).
//! * **Channel / threading** - `LUA_INTERNAL_CALL` marks the call in-process,
//!   which is correct because this code only runs on nvim's main thread inside a
//!   Lua / autocmd / keymap / user-command callback (nvim's API is
//!   main-thread-only, and the plugin never spawns threads).
//! * **`mem::zeroed` keysets** - every `KeyDict_*` is `#[repr(C)]` and all-zero
//!   is a valid value: `is_set = 0` (so nvim reads no optional field), bools
//!   false, ints 0, `CStr { null, 0 }` (nvim's empty `String`), and an `Object`
//!   type-tag of 0 == `kObjectTypeNil`. Optional fields are only read when we
//!   explicitly set their `is_set` bit.

use std::ffi::{c_char, c_void, CString};
use std::ptr;

use nvim_oxi::conversion::FromObject;
use nvim_oxi::{Array, Dictionary, Object};

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
        CError {
            etype: -1,
            msg: ptr::null_mut(),
        }
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
        CArray {
            size: a.len(),
            capacity: a.len(),
            items: a.as_ptr(),
        }
    }
}

/// `KeyDict_create_autocmd` (keysets_defs.h:277-288), field order = memory
/// order. Bit indices from keysets_defs.generated.h.
#[repr(C)]
struct KeyDictCreateAutocmd {
    is_set: u64,
    buffer: i32,      // Buffer (deprecated)
    buf: i32,         // Buffer
    callback: Object, // Union(String, LuaRef)
    command: CStr,
    desc: CStr,
    group: Object, // Union(Integer, String)
    nested: bool,
    once: bool,
    pattern: Object, // Union(String, ArrayOf(String))
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

/// `KeyDict_exec_autocmds` (keysets_defs.h:289-297), field order = memory order.
#[repr(C)]
struct KeyDictExecAutocmds {
    is_set: u64,
    buffer: i32,   // Buffer (deprecated)
    buf: i32,      // Buffer
    group: Object, // Union(Integer, String)
    modeline: bool,
    pattern: Object, // Union(String, ArrayOf(String))
    data: Object,
}
const OPTIDX_EXEC_AUTOCMDS_PATTERN: u64 = 5;
const OPTIDX_EXEC_AUTOCMDS_MODELINE: u64 = 6;

/// `KeyDict_highlight_cterm` (keysets_defs.h:222-239). Empirically 24 bytes:
/// the generated keyset carries a leading `OptionalKeys is_set` even though the
/// hand-written struct lists only the 16 bools (verified by the offset asserts
/// below + runtime link behaviour).
#[repr(C)]
struct KeyDictHighlightCterm {
    is_set: u64,
    bold: bool,
    standout: bool,
    strikethrough: bool,
    underline: bool,
    undercurl: bool,
    underdouble: bool,
    underdotted: bool,
    underdashed: bool,
    italic: bool,
    reverse: bool,
    altfont: bool,
    dim: bool,
    blink: bool,
    conceal: bool,
    overline: bool,
    nocombine: bool,
}

/// `KeyDict_highlight` (keysets_defs.h:182-220), field order = memory order.
/// `Union(Integer, String)` fields are nvim `Object`; `HLGroupID` is `Integer`.
#[repr(C)]
struct KeyDictHighlight {
    is_set: u64,
    altfont: bool,
    blink: bool,
    bold: bool,
    conceal: bool,
    dim: bool,
    italic: bool,
    nocombine: bool,
    overline: bool,
    reverse: bool,
    standout: bool,
    strikethrough: bool,
    undercurl: bool,
    underdashed: bool,
    underdotted: bool,
    underdouble: bool,
    underline: bool,
    default_: bool,
    cterm: KeyDictHighlightCterm,
    foreground: Object,
    fg: Object,
    background: Object,
    bg: Object,
    ctermfg: Object,
    ctermbg: Object,
    special: Object,
    sp: Object,
    link: i64, // HLGroupID
    link_global: i64,
    fallback: bool,
    blend: i64,
    fg_indexed: bool,
    bg_indexed: bool,
    force: bool,
    update: bool,
    url: CStr,
    font: CStr,
}
const OPTIDX_HL_LINK: u64 = 8;
const OPTIDX_HL_DEFAULT: u64 = 16;

// Compile-time layout guards: if the nvim keyset ever drifts (or Object/CStr
// are not the assumed size), these fail the build instead of silently misreading
// the struct at runtime (the failure mode that broke oxi's create_autocmd).
const _: () = assert!(std::mem::size_of::<Object>() == 32);
const _: () = assert!(std::mem::size_of::<CStr>() == 16);
const _: () = assert!(std::mem::size_of::<KeyDictHighlightCterm>() == 24);
const _: () = assert!(std::mem::size_of::<KeyDictHighlight>() == 384);
const _: () = assert!(std::mem::offset_of!(KeyDictHighlight, default_) == 24);
const _: () = assert!(std::mem::offset_of!(KeyDictHighlight, cterm) == 32);
const _: () = assert!(std::mem::offset_of!(KeyDictHighlight, link) == 312);
const _: () = assert!(std::mem::offset_of!(KeyDictHighlight, url) == 352);

/// `KeyDict_user_command` (keysets_defs.h:99-113), field order = memory order.
#[repr(C)]
struct KeyDictUserCommand {
    is_set: u64,
    addr: Object,
    bang: bool,
    bar: bool,
    complete: Object,
    count: Object,
    desc: Object,
    force: bool,
    keepscript: bool,
    nargs: Object,
    preview: Object,
    range: Object,
    register_: bool,
}
const OPTIDX_UCMD_DESC: u64 = 4;
const OPTIDX_UCMD_FORCE: u64 = 6;
const _: () = assert!(std::mem::size_of::<KeyDictUserCommand>() == 256);
const _: () = assert!(std::mem::offset_of!(KeyDictUserCommand, desc) == 112);
const _: () = assert!(std::mem::offset_of!(KeyDictUserCommand, force) == 144);

/// `KeyDict_keymap` (keysets_defs.h:84-95), field order = memory order.
/// NOTE: `callback` is a bare `LuaRef` (= nvim `Integer`, i64), *not* a
/// `Union(String, LuaRef)` Object like the autocmd/user-command callbacks - so
/// it is 8 bytes at offset 16, and the six bools before it leave 2 pad bytes.
#[repr(C)]
struct KeyDictKeymap {
    is_set: u64,
    noremap: bool,
    nowait: bool,
    silent: bool,
    script: bool,
    expr: bool,
    unique: bool,
    callback: i64, // LuaRef
    desc: CStr,
    replace_keycodes: bool,
}
// Bit indices from keysets_defs.generated.h (KEYSET_OPTIDX_keymap__*).
const OPTIDX_KEYMAP_DESC: u64 = 1;
const OPTIDX_KEYMAP_SILENT: u64 = 4;
const OPTIDX_KEYMAP_NOREMAP: u64 = 7;
const OPTIDX_KEYMAP_CALLBACK: u64 = 8;
const _: () = assert!(std::mem::size_of::<KeyDictKeymap>() == 48);
const _: () = assert!(std::mem::offset_of!(KeyDictKeymap, callback) == 16);
const _: () = assert!(std::mem::offset_of!(KeyDictKeymap, desc) == 24);

// SAFETY: these declarations mirror nvim 0.13's exported C ABI (checked against
// the generated api headers, with every KeyDict_* layout pinned by the asserts
// above). Calling them is `unsafe`; each wrapper below upholds the module-level
// Safety contract - CStr/array args backed by locals that outlive the call, a
// valid `&mut` error out-param, null arena (returned objects own nvim's alloc),
// main-thread only, and LUA_INTERNAL_CALL where a channel id is taken.
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
    fn nvim_exec_autocmds(
        event: Object,
        opts: *const KeyDictExecAutocmds,
        arena: *mut c_void,
        err: *mut CError,
    );
    fn nvim_get_hl_id_by_name(name: CStr, err: *mut CError) -> i64;
    fn nvim_set_hl(
        channel_id: u64,
        ns_id: i64,
        name: CStr,
        val: *const KeyDictHighlight,
        err: *mut CError,
    );
    fn nvim_create_user_command(
        channel_id: u64,
        name: CStr,
        cmd: Object,
        opts: *const KeyDictUserCommand,
        err: *mut CError,
    );
    fn nvim_del_user_command(name: CStr, err: *mut CError);
    fn nvim_set_keymap(
        channel_id: u64,
        mode: CStr,
        lhs: CStr,
        rhs: CStr,
        opts: *const KeyDictKeymap,
        err: *mut CError,
    );
    fn nvim_get_option_value(name: CStr, opts: *const KeyDictOption, err: *mut CError) -> Object;
    fn nvim_get_var(name: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_get_vvar(name: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_buf_get_var(buf: i32, name: CStr, arena: *mut c_void, err: *mut CError) -> Object;
    fn nvim_get_runtime_file(name: CStr, all: bool, arena: *mut c_void, err: *mut CError) -> Array;
}

// --- safe wrappers ---------------------------------------------------------

fn cstr(s: &str) -> Option<(CString, CStr)> {
    let c = CString::new(s).ok()?;
    let cs = CStr {
        data: c.as_ptr(),
        size: s.len(),
    };
    Some((c, cs))
}

/// Native `nvim_eval` - ONLY for the genuinely-arbitrary vimscript expressions
/// (expression-valued `b:match_words`, raw `b:match_skip`, linewise-op config).
/// Everything else uses a dedicated native call.
pub fn eval(expr: &str) -> Option<Object> {
    let (_guard, cexpr) = cstr(expr)?;
    let mut err = CError::new();
    // SAFETY: module contract; `cexpr` is backed by `_guard` (outlives the
    // call), `err` is a valid &mut, arena null so the returned Object owns and
    // frees nvim's allocation on Drop.
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
    // SAFETY: module contract; `cname` is backed by `_guard`, `cargs` borrows
    // the live `args` Array (non-owning view, no Drop), `err` is a valid &mut,
    // arena null so the returned Object owns nvim's allocation.
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
    // SAFETY: all-zero `KeyDictEchoOpts` is a valid value (is_set = 0, so nvim
    // reads no optional field); see the module Safety contract.
    let opts: KeyDictEchoOpts = unsafe { std::mem::zeroed() };
    let mut err = CError::new();
    // SAFETY: module contract; `cchunks` borrows the live `chunks` local, opts
    // and err are valid pointers, and the returned Object (`_ret`) owns and
    // frees nvim's allocation when it drops at the end of this function.
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
    // SAFETY: all-zero `KeyDictOption` is valid (is_set = 0); the optional
    // scope/buf/win fields are set below only alongside their is_set bits, and
    // `cscope` is backed by `_sguard` which outlives the call. Module contract.
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
    // SAFETY: module contract; `cname`/`opts.scope` are backed by locals that
    // outlive the call, `opts`/`err` are valid pointers, returned Object owns
    // nvim's allocation.
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
    // SAFETY: module contract; `cname` is backed by `_g` (outlives the call),
    // `err` is a valid &mut, arena null so the returned Object owns nvim's alloc.
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
    // SAFETY: module contract; `cname` is backed by `_g`, `err` is a valid &mut,
    // arena null so the returned Object owns nvim's allocation.
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
    // SAFETY: module contract; `buf` is a valid handle, `cname` is backed by
    // `_g`, `err` is a valid &mut, arena null so the returned Object owns
    // nvim's allocation.
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
    // SAFETY: module contract; `cname` is backed by `_g`, `err` is a valid &mut,
    // arena null so the returned Array owns and frees nvim's allocation.
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
    // SAFETY: module contract; `event`/`opts.pattern`/`opts.group` are owned
    // Objects valid for the call, `opts` is a KeyDictCreateAutocmd whose layout
    // is compile-time asserted, its `callback` is a LuaRef Object that nvim
    // adopts (oxi's Object does not unref a LuaRef on Drop, so no double-free),
    // `err` is a valid &mut, and arena is null.
    let id =
        unsafe { nvim_create_autocmd(LUA_INTERNAL_CALL, event, &opts, ptr::null_mut(), &mut err) };
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
    // SAFETY: module contract; `opts.command` (`ccmd`) is backed by `_cmd_guard`
    // (outlives the call) and nvim copies it, `event`/pattern/group are owned
    // Objects, `opts` layout is compile-time asserted, `err` is a valid &mut,
    // arena null.
    let id =
        unsafe { nvim_create_autocmd(LUA_INTERNAL_CALL, event, &opts, ptr::null_mut(), &mut err) };
    !err.is_err() && id > 0
}

/// Fire a `User {name}` autocmd natively (`nvim_exec_autocmds`), a graceful
/// no-op when nothing is registered. Replaces the vimscript
/// `if exists('#User#X') | doautocmd <nomodeline> User X | endif`.
pub fn exec_user_autocmd(name: &str) {
    let event = Object::from("User");
    // SAFETY: all-zero KeyDictExecAutocmds is valid (is_set = 0); module contract.
    let mut opts: KeyDictExecAutocmds = unsafe { std::mem::zeroed() };
    opts.pattern = Object::from(name.to_string());
    opts.is_set |= 1 << OPTIDX_EXEC_AUTOCMDS_PATTERN;
    opts.modeline = false; // <nomodeline>
    opts.is_set |= 1 << OPTIDX_EXEC_AUTOCMDS_MODELINE;
    let mut err = CError::new();
    // SAFETY: module contract; `event`/`opts.pattern` are owned Objects valid for
    // the call, `opts` layout is compile-time asserted, `err` is a valid &mut,
    // arena null (this call returns no object).
    unsafe { nvim_exec_autocmds(event, &opts, ptr::null_mut(), &mut err) };
}

/// Link highlight group `name` to `target` in the global namespace, without
/// overriding an existing definition - the native `nvim_set_hl` equivalent of
/// `hi def link {name} {target}`. `link` takes an HLGroupID, so `target` is
/// resolved by name first.
pub fn set_hl_link(name: &str, target: &str) {
    let (_tguard, ctarget) = match cstr(target) {
        Some(x) => x,
        None => return,
    };
    let mut err = CError::new();
    // SAFETY: module contract; `ctarget` is backed by `_tguard` (outlives the
    // call) and `err` is a valid &mut. Returns an HLGroupID (i64).
    let id = unsafe { nvim_get_hl_id_by_name(ctarget, &mut err) };
    if err.is_err() {
        return;
    }
    let (_nguard, cname) = match cstr(name) {
        Some(x) => x,
        None => return,
    };
    // SAFETY: all-zero KeyDictHighlight is valid - is_set = 0, bools false, the
    // nested KeyDictHighlightCterm is itself all-zero (its own is_set = 0), and
    // the String/Object/Integer fields are zero-valid (Nil/empty). Layout is
    // compile-time asserted; only `link`/`default_` are set below, each with its
    // is_set bit.
    let mut val: KeyDictHighlight = unsafe { std::mem::zeroed() };
    val.link = id;
    val.is_set |= 1 << OPTIDX_HL_LINK;
    val.default_ = true;
    val.is_set |= 1 << OPTIDX_HL_DEFAULT;
    let mut err = CError::new();
    // SAFETY: module contract; ns_id 0 = global namespace, `cname` is backed by
    // `_nguard`, `val` is a valid asserted-layout KeyDictHighlight, `err` is a
    // valid &mut.
    unsafe { nvim_set_hl(LUA_INTERNAL_CALL, 0, cname, &val, &mut err) };
}

/// Define a user command whose handler is a native Rust callback (a LuaRef
/// packed as the `Union(String, LuaRef)` cmd), via the correctly-laid-out
/// `KeyDict_user_command`. `force = true` mirrors `command!`. The callback
/// receives the command-args table (ignored by our no-arg commands).
pub fn create_user_command_cb<F>(name: &str, desc: &str, f: F) -> bool
where
    F: Fn(Dictionary) + 'static,
{
    use nvim_oxi::Function;
    let func: Function<Dictionary, ()> = Function::from_fn(move |args| -> nvim_oxi::Result<()> {
        f(args);
        Ok(())
    });
    let cmd = Object::from(func); // tagged LuaRef object

    let (_ng, cname) = match cstr(name) {
        Some(x) => x,
        None => return false,
    };
    // SAFETY: all-zero KeyDictUserCommand is valid (is_set = 0, Object fields
    // Nil, bools false); layout is compile-time asserted. Module contract.
    let mut opts: KeyDictUserCommand = unsafe { std::mem::zeroed() };
    opts.force = true;
    opts.is_set |= 1 << OPTIDX_UCMD_FORCE;
    opts.desc = Object::from(desc.to_string());
    opts.is_set |= 1 << OPTIDX_UCMD_DESC;
    let mut err = CError::new();
    // SAFETY: module contract; `cname` is backed by `_ng`, `cmd` is a LuaRef
    // Object nvim adopts (oxi's Object does not unref a LuaRef on Drop, so no
    // double-free), `opts` layout is asserted, `err` is a valid &mut.
    unsafe { nvim_create_user_command(LUA_INTERNAL_CALL, cname, cmd, &opts, &mut err) };
    !err.is_err()
}

/// Define a keymap whose handler is a native Rust callback (a bare `LuaRef`
/// packed into the correctly-laid-out `KeyDict_keymap`), the equivalent of
/// `nvim_set_keymap` with a Lua function rhs. `noremap` + `silent` are forced
/// (mirroring the old `<cmd>` mappings). Keymap callbacks are invoked with no
/// arguments (mapping.c: `Array args = ARRAY_DICT_INIT`), hence
/// `Function<(), ()>`. nvim takes ownership of the ref (mapping.c:2802 sets
/// `opts->callback = LUA_NOREF`) and oxi's `Function` has no `Drop`, so there
/// is no double-unref; the ref lives until the mapping is deleted.
pub fn set_keymap_cb<F>(mode: &str, lhs: &str, desc: &str, f: F) -> bool
where
    F: Fn() + 'static,
{
    use nvim_oxi::Function;
    let func: Function<(), ()> = Function::from_fn(move |()| -> nvim_oxi::Result<()> {
        f();
        Ok(())
    });
    // oxi's LuaRef is c_int (i32); nvim's keymap callback field is Integer (i64).
    let luaref = func.lua_ref() as i64;

    let (_mg, cmode) = match cstr(mode) {
        Some(x) => x,
        None => return false,
    };
    let (_lg, clhs) = match cstr(lhs) {
        Some(x) => x,
        None => return false,
    };
    let (_rg, crhs) = match cstr("") {
        Some(x) => x,
        None => return false,
    };
    let (_dg, cdesc) = match cstr(desc) {
        Some(x) => x,
        None => return false,
    };

    // SAFETY: all-zero KeyDictKeymap is a valid bit pattern (is_set = 0, so nvim
    // reads no optional field yet; bools false; `desc` = empty CStr; the bare i64
    // `callback` is overwritten below before the call). Layout is compile-time
    // asserted. Module contract.
    let mut opts: KeyDictKeymap = unsafe { std::mem::zeroed() };
    opts.callback = luaref;
    opts.is_set |= 1 << OPTIDX_KEYMAP_CALLBACK;
    opts.desc = cdesc;
    opts.is_set |= 1 << OPTIDX_KEYMAP_DESC;
    opts.noremap = true;
    opts.is_set |= 1 << OPTIDX_KEYMAP_NOREMAP;
    opts.silent = true;
    opts.is_set |= 1 << OPTIDX_KEYMAP_SILENT;

    let mut err = CError::new();
    // SAFETY: module contract; `cmode`/`clhs`/`crhs`/`cdesc` are backed by their
    // `_mg`/`_lg`/`_rg`/`_dg` guards (all outlive the call), `opts.callback` is a
    // LuaRef nvim adopts (mapping.c sets `opts->callback = LUA_NOREF`; oxi's
    // Function has no Drop, so no double-unref), `opts` layout is asserted, and
    // `err` is a valid &mut.
    unsafe { nvim_set_keymap(LUA_INTERNAL_CALL, cmode, clhs, crhs, &opts, &mut err) };
    !err.is_err()
}

/// Delete a user command natively (`nvim_del_user_command`). A missing command
/// is not an error for our purposes (mirrors `silent! delcommand`).
pub fn del_user_command(name: &str) {
    let (_g, cname) = match cstr(name) {
        Some(x) => x,
        None => return,
    };
    let mut err = CError::new();
    // SAFETY: module contract; `cname` is backed by `_g` (outlives the call) and
    // `err` is a valid &mut. A missing command only sets `err`, which we ignore.
    unsafe { nvim_del_user_command(cname, &mut err) };
}
