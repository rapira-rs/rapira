//! PHP reads the `putenv()` writes of its thread, then the pool values, then the process environment at startup. Children inherit only the process environment.
//! C copies the result into PHP strings after each Rust call returns. No Rust borrow spans a PHP allocation.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_char;
use std::os::unix::ffi::OsStringExt;
use std::sync::{Arc, OnceLock};

type Bytes = Box<[u8]>;

/// Keep the order of the process environment. For duplicate names, libc `getenv` returns the first value.
struct Boot {
    pairs: Vec<(Bytes, Bytes)>,
    index: HashMap<Bytes, usize>,
}

static BOOT: OnceLock<Boot> = OnceLock::new();

thread_local! {
    static POOL: OnceCell<Arc<BTreeMap<String, String>>> = const { OnceCell::new() };
    /// The putenv() writes, one per name in the order of the first write. None is a removal.
    static WRITES: RefCell<Vec<(Bytes, Option<Bytes>)>> = const { RefCell::new(Vec::new()) };
    static VIEW: RefCell<Vec<EnvPair>> = const { RefCell::new(Vec::new()) };
}

/// `rapira_env_pair` in `rapira_env.c`. A null value is a removal.
#[repr(C)]
pub struct EnvPair {
    name: *const c_char,
    name_len: usize,
    value: *const c_char,
    value_len: usize,
}

impl EnvPair {
    fn new(name: &[u8], value: Option<&[u8]>) -> Self {
        EnvPair {
            name: name.as_ptr().cast(),
            name_len: name.len(),
            value: value.map_or(std::ptr::null(), |v| v.as_ptr().cast()),
            value_len: value.map_or(0, <[u8]>::len),
        }
    }
}

unsafe extern "C" {
    fn rapira_env_install();
}

/// Save the process environment and install the PHP handlers before the workers start.
pub(crate) fn install() {
    BOOT.get_or_init(|| {
        let mut boot = Boot {
            pairs: Vec::new(),
            index: HashMap::new(),
        };
        for (name, value) in std::env::vars_os() {
            let name: Bytes = name.into_vec().into();
            if !boot.index.contains_key(&name) {
                boot.index.insert(name.clone(), boot.pairs.len());
                boot.pairs.push((name, value.into_vec().into()));
            }
        }
        boot
    });
    // SAFETY: MINIT has run, and no PHP thread exists yet.
    unsafe { rapira_env_install() };
}

/// All workers of a pool read the same immutable table.
pub(crate) fn enter_pool(env: Arc<BTreeMap<String, String>>) {
    POOL.with(|slot| assert!(slot.set(env).is_ok()));
}

/// # Safety
/// `ptr` is non-null and points to `len` readable bytes.
unsafe fn bytes<'a>(ptr: *const c_char, len: usize) -> &'a [u8] {
    // SAFETY: the caller gives a live buffer of len bytes.
    unsafe { std::slice::from_raw_parts(ptr.cast(), len) }
}

/// Return the value of `name` through the output pointers. A missing name returns false. The bytes stay valid until the next `putenv()` or the end of this PHP request.
///
/// # Safety
/// `name` points to `name_len` readable bytes. `value` and `value_len` are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_env_get(
    name: *const c_char,
    name_len: usize,
    value: *mut *const c_char,
    value_len: *mut usize,
) -> bool {
    // SAFETY: the caller gives a live buffer of name_len bytes.
    let name = unsafe { bytes(name, name_len) };
    let write = WRITES.with_borrow(|writes| {
        writes
            .iter()
            .find(|(n, _)| **n == *name)
            .map(|(_, v)| v.as_deref().map(|v| (v.as_ptr(), v.len())))
    });
    let found = write.unwrap_or_else(|| {
        let pool = std::str::from_utf8(name).ok().and_then(|name| {
            POOL.with(|pool| pool.get()?.get(name).map(|v| (v.as_ptr(), v.len())))
        });
        pool.or_else(|| {
            let boot = BOOT.get()?;
            let (_, v) = &boot.pairs[*boot.index.get(name)?];
            Some((v.as_ptr(), v.len()))
        })
    });
    let Some((ptr, len)) = found else {
        return false;
    };
    // SAFETY: the caller gives writable out pointers.
    unsafe {
        *value = ptr.cast();
        *value_len = len;
    }
    true
}

/// Writes `name` into the putenv() layer. A null `value` is a removal.
///
/// # Safety
/// `name` points to `name_len` readable bytes. A non-null `value` points to `value_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_env_put(
    name: *const c_char,
    name_len: usize,
    value: *const c_char,
    value_len: usize,
) {
    // SAFETY: the caller gives a live buffer of name_len bytes.
    let name = unsafe { bytes(name, name_len) };
    // SAFETY: the caller gives a live buffer of value_len bytes when value is not null.
    let value = (!value.is_null()).then(|| unsafe { bytes(value, value_len) }.into());
    WRITES.with_borrow_mut(
        |writes| match writes.iter_mut().find(|(n, _)| **n == *name) {
            Some((_, old)) => *old = value,
            None => writes.push((name.into(), value)),
        },
    );
}

/// Drops the putenv() writes at the end of the PHP request.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_env_clear() {
    WRITES.set(Vec::new());
}

/// Return the entries in import order: boot, pool, then `putenv()`. C applies them to one PHP array. The entries stay valid until the next import, the next `putenv()` or the end of this PHP request.
///
/// # Safety
/// `len` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_env_view(len: *mut usize) -> *const EnvPair {
    VIEW.with_borrow_mut(|view| {
        view.clear();
        if let Some(boot) = BOOT.get() {
            view.extend(boot.pairs.iter().map(|(n, v)| EnvPair::new(n, Some(v))));
        }
        POOL.with(|slot| {
            if let Some(pool) = slot.get() {
                view.extend(
                    pool.iter()
                        .map(|(n, v)| EnvPair::new(n.as_bytes(), Some(v.as_bytes()))),
                );
            }
        });
        WRITES.with_borrow(|writes| {
            view.extend(writes.iter().map(|(n, v)| EnvPair::new(n, v.as_deref())));
        });
        // SAFETY: the caller gives a writable len.
        unsafe { *len = view.len() };
        view.as_ptr()
    })
}
