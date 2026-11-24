//! PHP reads the `putenv()` writes of its thread, then the pool values, then the process environment at startup. Children inherit only the process environment.
//! A pool holds its values and the process environment as one table of permanent interned strings. Its workers share the table, and PHP reads a string of the table with no copy.
//! No Rust borrow spans a PHP allocation, because the allocation can bail out.

use crate::context::{Permanent, permanent};
use crate::{
    php_import_environment_variables, rapira_zval_stringl, zend_hash_index_del,
    zend_hash_index_update, zend_hash_str_del, zend_hash_str_update, zend_hash_update, zend_long,
    zend_string, zend_ulong, zval,
};
use std::cell::{OnceCell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, c_char};
use std::fmt::NumBuffer;
use std::os::unix::ffi::OsStringExt;
use std::ptr::{null, null_mut};
use std::sync::Arc;

type Bytes = Box<[u8]>;

/// The key of a variable in an array of the environment import.
enum Key {
    Name(Permanent),
    Index(zend_long),
}

/// The process environment and the values of one pool.
pub(crate) struct PoolEnv {
    /// The pool pairs as written, for the `$_SERVER` of a request.
    pool: Vec<(Permanent, Permanent)>,
    /// The variables of the environment import: the process environment in its order with the pool values in place, then the other pool names.
    imported: Vec<(Key, Permanent)>,
    /// The value of each name, for `getenv($name)`.
    values: HashMap<Bytes, Permanent>,
}

impl PoolEnv {
    /// Reads the process environment and puts the pool values over it.
    pub(crate) fn new(pool: &BTreeMap<String, String>) -> Self {
        let mut names: Vec<Bytes> = Vec::new();
        let mut values: HashMap<Bytes, Permanent> = HashMap::new();
        // For duplicate names, libc getenv returns the first value.
        for (name, value) in std::env::vars_os() {
            let name: Bytes = name.into_vec().into();
            if let Entry::Vacant(slot) = values.entry(name.clone()) {
                slot.insert(permanent(&value.into_vec()));
                names.push(name);
            }
        }
        let mut pairs = Vec::with_capacity(pool.len());
        for (name, value) in pool {
            let value = permanent(value.as_bytes());
            pairs.push((permanent(name.as_bytes()), value));
            if values.insert(name.as_bytes().into(), value).is_none() {
                names.push(name.as_bytes().into());
            }
        }
        let mut imported = Vec::new();
        for name in &names {
            if skipped(name) {
                continue;
            }
            let key = match index_key(name) {
                Some(index) => Key::Index(index),
                None => Key::Name(permanent(name)),
            };
            imported.push((key, values[name]));
        }
        PoolEnv {
            pool: pairs,
            imported,
            values,
        }
    }
}

/// The environment import of PHP skips these names. https://github.com/php/php-src/blob/php-8.6.0/main/php_variables.c#L616-L651
fn skipped(name: &[u8]) -> bool {
    name.first().is_none_or(|b| *b == b'=') || name.iter().any(|&b| matches!(b, b' ' | b'.' | b'['))
}

/// PHP stores a name as an integer key when the name is the canonical decimal form of an integer. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_hash.c#L3477-L3514
fn index_key(name: &[u8]) -> Option<zend_long> {
    let text = std::str::from_utf8(name).ok()?;
    let index: zend_long = text.parse().ok()?;
    (index.format_into(&mut NumBuffer::new()) == text).then_some(index)
}

thread_local! {
    static POOL: OnceCell<Arc<PoolEnv>> = const { OnceCell::new() };
    /// The putenv() writes, one per name in the order of the first write. None is a removal. A value is a C string, because the getenv callback of the SAPI returns it as one.
    static WRITES: RefCell<Vec<(Bytes, Option<CString>)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "C" {
    fn rapira_env_install();
}

/// Install the import hook and the PHP handlers before the workers start.
pub(crate) fn install() {
    // SAFETY: MINIT has run, and no PHP thread exists yet.
    unsafe {
        php_import_environment_variables = Some(import);
        rapira_env_install();
    }
}

/// All workers of a pool read the same immutable table.
pub(crate) fn enter_pool(env: Arc<PoolEnv>) {
    POOL.with(|slot| assert!(slot.set(env).is_ok()));
}

/// Gives `f` the pool pairs as written, for the `$_SERVER` of a request.
pub(crate) fn with_pool_pairs(f: impl FnOnce(&[(Permanent, Permanent)])) {
    POOL.with(|slot| {
        f(slot
            .get()
            .map(|env| env.pool.as_slice())
            .unwrap_or_default())
    });
}

/// The import hook of PHP. It fills `$_ENV`, the `$_SERVER` of a boot and the array of `getenv()`: the table of the pool goes in with no copy, then the `putenv()` writes of this thread.
unsafe extern "C" fn import(array: *mut zval) {
    // SAFETY: PHP gives a new array.
    let ht = unsafe { (*array).value.arr };
    POOL.with(|slot| {
        let Some(env) = slot.get() else {
            return;
        };
        for (key, value) in &env.imported {
            let mut value = value.zval();
            // SAFETY: ht is a live array, and a permanent interned string outlives it.
            unsafe {
                match key {
                    Key::Name(name) => zend_hash_update(ht, name.as_ptr(), &mut value),
                    Key::Index(index) => {
                        zend_hash_index_update(ht, *index as zend_ulong, &mut value)
                    }
                };
            }
        }
    });
    // SAFETY: only this thread reads and writes the table. The array is new, so no call below destroys an object and runs PHP code. The plain reference holds no RefCell borrow, so a bailout of a PHP allocation leaves the cell free.
    let writes = unsafe { &*WRITES.with(RefCell::as_ptr) };
    for (name, value) in writes {
        if skipped(name) {
            continue;
        }
        let index = index_key(name);
        // SAFETY: ht is a live array, and each buffer holds the given length.
        unsafe {
            let Some(value) = value else {
                match index {
                    Some(index) => zend_hash_index_del(ht, index as zend_ulong),
                    None => zend_hash_str_del(ht, name.as_ptr().cast(), name.len()),
                };
                continue;
            };
            let mut copy: zval = std::mem::zeroed();
            rapira_zval_stringl(&mut copy, value.as_ptr(), value.count_bytes());
            match index {
                Some(index) => zend_hash_index_update(ht, index as zend_ulong, &mut copy),
                None => zend_hash_str_update(ht, name.as_ptr().cast(), name.len(), &mut copy),
            };
        }
    }
}

/// # Safety
/// `ptr` is non-null and points to `len` readable bytes.
unsafe fn bytes<'a>(ptr: *const c_char, len: usize) -> &'a [u8] {
    // SAFETY: the caller gives a live buffer of len bytes.
    unsafe { std::slice::from_raw_parts(ptr.cast(), len) }
}

/// Returns the permanent string of `name` from the table of the pool, or null. A `putenv()` write hides the table: a written value goes to the output pointers, and a removal leaves them unchanged. The written value is a C string. It stays valid until the next `putenv()` or the end of this PHP request.
///
/// # Safety
/// `name` points to `name_len` readable bytes. `value` and `value_len` are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_env_get(
    name: *const c_char,
    name_len: usize,
    value: *mut *const c_char,
    value_len: *mut usize,
) -> *mut zend_string {
    // SAFETY: the caller gives a live buffer of name_len bytes.
    let name = unsafe { bytes(name, name_len) };
    let written = WRITES.with_borrow(|writes| {
        let Some((_, write)) = writes.iter().find(|(n, _)| **n == *name) else {
            return false;
        };
        if let Some(write) = write {
            // SAFETY: the caller gives writable out pointers.
            unsafe {
                *value = write.as_ptr();
                *value_len = write.count_bytes();
            }
        }
        true
    });
    if written {
        return null_mut();
    }
    POOL.with(|slot| {
        slot.get()
            .and_then(|env| env.values.get(name))
            .map_or(null_mut(), |value| value.as_ptr())
    })
}

/// The getenv callback of the SAPI. PHP reads it for `${NAME}` in INI text and copies the string at once. For null, PHP reads the process environment: this is the result for a name that `putenv()` removed, and for each name on a thread with no pool. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_ini_parser.y#L172-L193
///
/// # Safety
/// `name` points to `name_len` readable bytes.
pub(crate) unsafe extern "C" fn getenv(name: *const c_char, name_len: usize) -> *mut c_char {
    let mut value = null();
    let mut value_len = 0;
    // SAFETY: the caller gives a live buffer of name_len bytes, and the out pointers are writable.
    let fixed = unsafe { rapira_rs_env_get(name, name_len, &mut value, &mut value_len) };
    if fixed.is_null() {
        return value.cast_mut();
    }
    // SAFETY: a permanent string is live and ends with a NUL byte.
    unsafe { (&raw mut (*fixed).val).cast() }
}

/// Writes `name` into the putenv() layer. A null `value` is a removal.
///
/// # Safety
/// `name` points to `name_len` readable bytes. A non-null `value` is a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_env_put(
    name: *const c_char,
    name_len: usize,
    value: *const c_char,
) {
    // SAFETY: the caller gives a live buffer of name_len bytes.
    let name = unsafe { bytes(name, name_len) };
    // SAFETY: the caller gives a C string when value is not null.
    let value = (!value.is_null()).then(|| unsafe { CStr::from_ptr(value) }.into());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The expected keys follow ZEND_HANDLE_NUMERIC_STR. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_hash.h#L439-L455
    #[test]
    fn index_key_follows_the_numeric_string_rule_of_php() {
        struct Case {
            name: &'static str,
            input: &'static [u8],
            want: Option<zend_long>,
        }
        let cases = [
            Case {
                name: "a decimal integer",
                input: b"4242",
                want: Some(4242),
            },
            Case {
                name: "zero",
                input: b"0",
                want: Some(0),
            },
            Case {
                name: "a negative integer",
                input: b"-7",
                want: Some(-7),
            },
            Case {
                name: "the highest integer",
                input: b"9223372036854775807",
                want: Some(zend_long::MAX),
            },
            Case {
                name: "the lowest integer",
                input: b"-9223372036854775808",
                want: Some(zend_long::MIN),
            },
            Case {
                name: "one above the highest integer",
                input: b"9223372036854775808",
                want: None,
            },
            Case {
                name: "a leading zero",
                input: b"042",
                want: None,
            },
            Case {
                name: "minus zero",
                input: b"-0",
                want: None,
            },
            Case {
                name: "a plus sign",
                input: b"+5",
                want: None,
            },
            Case {
                name: "a minus sign alone",
                input: b"-",
                want: None,
            },
            Case {
                name: "a letter after the digits",
                input: b"42A",
                want: None,
            },
            Case {
                name: "a name with no digit",
                input: b"PATH",
                want: None,
            },
        ];
        for case in cases {
            assert_eq!(index_key(case.input), case.want, "{}", case.name);
        }
    }
}
