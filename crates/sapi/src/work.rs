use std::mem::MaybeUninit;
use std::ptr::NonNull;

use crate::callbacks::guard;
use crate::pool::{Event, record};
use crate::types::Context;
use crate::zend_class_entry;

pub use crate::queue::{Intake, Refused};

/// A dispatcher-mode unit from submit to free_obj. It is the plugin state behind the object that receive() hands to PHP.
pub trait Held: Send + 'static {
    /// The worker committed the outcome, or discarded the unit.
    fn settled(&self) -> bool;
    /// The unit takes no outcome, because the client left or the plugin closed the unit. Work::isCancelled() returns this value. The pull skips a queued unit that is cancelled.
    fn cancelled(&self) -> bool;
    fn discard(&mut self);
    /// Sends what the client gets in place of the outcome when the object of an unsettled unit is gone. The drop shell skips it when the cycle died.
    fn lost(&mut self);
    /// Sheds the unit with the refusal of the plugin, 503 or UNAVAILABLE, when the worker cannot serve it.
    fn shed(self: Box<Self>);
    /// The class of the object that receive() creates for this unit. The plugin registered it in MINIT, so it is never null.
    fn class(&self) -> *mut zend_class_entry;

    /// Work::isFinalized() returns this value.
    fn is_finalized(&self) -> bool {
        self.settled() || self.cancelled()
    }
}

/// Reclaims the Box that receive() handed out: clears the cycle slot if it still points here, and counts an unsettled unit as handled.
/// # Safety
/// `ptr` came from `Box::leak` in receive and was not reclaimed before.
unsafe fn release(ptr: *mut dyn Held) -> Box<dyn Held> {
    crate::worker::forget_held(ptr.cast::<()>());
    let st = unsafe { Box::from_raw(ptr) };
    if !st.settled() {
        record(Event::Handled { errored: true });
    }
    st
}

/// The unit behind a Work object. It is the first field of the C prefix of every unit class (rapira_held in rapira_sapi.h). zend_object_alloc zeroes the prefix, so a fresh object holds None.
#[repr(transparent)]
pub struct HeldPtr(pub(crate) Option<NonNull<dyn Held>>);

// rapira_held mirrors the two words of the fat pointer, and the zeroed prefix is None.
const _: () = assert!(
    size_of::<HeldPtr>() == 2 * size_of::<*const ()>()
        && align_of::<HeldPtr>() == align_of::<*const ()>()
);
const _: () = assert!(
    unsafe { MaybeUninit::<HeldPtr>::zeroed().assume_init() }
        .0
        .is_none()
);

impl HeldPtr {
    fn get(&self) -> NonNull<dyn Held> {
        self.0.expect("a unit object that receive() never attached")
    }

    /// The plugin state that receive() attached.
    /// # Safety
    /// receive() attached a `T` to this slot, and the object is live.
    pub unsafe fn state<T>(&self) -> *mut T {
        self.get().cast::<T>().as_ptr()
    }
}

/// Work::isFinalized() returns this value.
/// # Safety
/// `held` is the held word of a live unit object that receive() attached.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_is_finalized(held: *const HeldPtr) -> bool {
    guard(false, || unsafe { (*held).get().as_ref().is_finalized() })
}

/// Work::isCancelled() returns this value.
/// # Safety
/// As `rapira_rs_held_is_finalized`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_is_cancelled(held: *const HeldPtr) -> bool {
    guard(false, || unsafe { (*held).get().as_ref().cancelled() })
}

/// The free_obj of a unit class. It reclaims the unit, and an unsettled unit sends its lost outcome. A cycle that died, for example after a fatal error or a timeout, skips the outcome. The plugin then reports the worker death when the sender drops.
/// # Safety
/// `held` is the held word of the object that free_obj got, and the engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_held_drop(held: *mut HeldPtr) {
    guard((), || unsafe {
        // An object that new created holds no unit, because the engine refuses the private constructor after the allocation.
        let Some(ptr) = (*held).0.take() else {
            return;
        };
        let mut held = release(ptr.as_ptr());
        if !held.settled() && !(*crate::rapira_cg()).unclean_shutdown {
            held.lost();
        }
    })
}

/// One unit on the intake: the CGI reply of the classic and worker modes, or a dispatcher-mode unit.
pub enum Unit {
    Cgi(Box<Context>),
    Dispatcher(Box<dyn Held>),
}

impl Unit {
    /// The client left while the unit was queued, so the pull skips it.
    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::Cgi(ctx) => ctx.is_cancelled(),
            Self::Dispatcher(held) => held.cancelled(),
        }
    }

    /// Sheds the unit when the worker cannot serve it: 503 for a CGI unit, the refusal of the plugin for a dispatcher unit.
    pub(crate) fn shed(self) {
        match self {
            Self::Cgi(mut ctx) => ctx.refuse(503),
            Self::Dispatcher(held) => held.shed(),
        }
    }

    /// The classic and worker modes take the CGI reply.
    pub(crate) fn into_cgi(self) -> Box<Context> {
        match self {
            Self::Cgi(ctx) => ctx,
            Self::Dispatcher(_) => unreachable!("a dispatcher unit on a classic or worker intake"),
        }
    }

    /// In dispatcher mode, returns the unit behind the object.
    pub(crate) fn into_held(self) -> Box<dyn Held> {
        match self {
            Self::Dispatcher(held) => held,
            Self::Cgi(_) => unreachable!("a CGI unit on a dispatcher intake"),
        }
    }
}

pub fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shed of a CGI unit is the 503 seal.
    #[test]
    fn a_cgi_shed_refuses_with_503() {
        let (responder, log) = crate::types::tests::responder(false);
        let mut ctx = crate::types::tests::context();
        ctx.responder = Some(responder);
        Unit::Cgi(Box::new(ctx)).shed();
        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["respond 503 truncated=false body="]
        );
    }
}
