use std::{cell::RefCell, ffi::c_char, ptr};

use crate::{
    callbacks::guard, rapira_grpc_message_alloc, rapira_grpc_message_release,
    rapira_grpc_message_share, zend_string,
};

thread_local! {
    static MESSAGE: RefCell<MessageBuffer> = const { RefCell::new(MessageBuffer {
        string: ptr::null_mut(),
        capacity: 0,
    }) };
}

#[derive(Default)]
struct MessageBuffer {
    string: *mut zend_string,
    capacity: usize,
}

impl MessageBuffer {
    unsafe fn release(&mut self) {
        let string = std::mem::take(self).string;
        if !string.is_null() {
            unsafe { rapira_grpc_message_release(string) };
        }
    }

    unsafe fn copy(&mut self, bytes: &[u8]) -> *mut zend_string {
        unsafe {
            if self.string.is_null()
                || self.capacity < bytes.len()
                || (*self.string).gc.refcount != 1
            {
                self.release();
                // The C shim catches allocation bailouts before they reach this borrow.
                let string = rapira_grpc_message_alloc(bytes.len());
                if string.is_null() {
                    return ptr::null_mut();
                }
                self.string = string;
                self.capacity = bytes.len();
            }
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (*self.string).val.as_mut_ptr().cast(),
                bytes.len(),
            );
            rapira_grpc_message_share(self.string, bytes.len())
        }
    }
}

/// # Safety
/// The engine is active. Bytes are readable and separate from Zend storage.
/// The caller owns the returned reference. Null indicates an allocation bailout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_message_buffer(
    bytes: *const c_char,
    len: usize,
) -> *mut zend_string {
    guard(ptr::null_mut(), || {
        MESSAGE.with_borrow_mut(|buffer| unsafe {
            buffer.copy(std::slice::from_raw_parts(bytes.cast(), len))
        })
    })
}

/// # Safety
/// RSHUTDOWN runs on the PHP thread while the request heap is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_grpc_message_shutdown() {
    guard((), || {
        MESSAGE.with_borrow_mut(|buffer| unsafe { buffer.release() });
    });
}

pub(super) fn reclaim() {
    // Native reclaim follows heap destruction, including after shutdown bailouts.
    // https://github.com/php/php-src/blob/php-8.5.10/main/main.c#L1956-L2051
    MESSAGE.with_borrow_mut(|buffer| *buffer = MessageBuffer::default());
}
