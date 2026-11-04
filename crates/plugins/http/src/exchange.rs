//! The Exchange work unit: one HTTP request on a dispatcher-mode intake and its Work impl.

use tokio::sync::mpsc::Sender;

use rapira_sapi::types::{Context, Frame, Request};
use rapira_sapi::work::{Held, Work};
use rapira_sapi::zend_object;

use crate::php::{ExchangeState, exchange_from};

/// Cap 16 lets a buffered Head+Chunk+End trio queue without parking the PHP thread and keeps a 1 MiB body in 64 KiB writes to about one park; tokio allocates one 32-slot block for any cap up to 32.
pub(crate) const FRAME_CAP: usize = 16;

/// One HTTP request on a dispatcher-mode intake.
pub(crate) struct Exchange {
    req: Request,
    /// Sends the reply the transport reads.
    tx: Sender<Frame>,
}

impl Exchange {
    pub(crate) fn new(req: Request, tx: Sender<Frame>) -> Self {
        Self { req, tx }
    }
}

impl Work for Exchange {
    fn cancelled(&self) -> bool {
        self.tx.is_closed()
    }

    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held {
        let ptr = Box::into_raw(Box::new(ExchangeState::new(self.req, self.tx)));
        // SAFETY: the caller passes a live Rapira\Internal\Http\Exchange object.
        unsafe { (*exchange_from(obj)).unit = ptr.cast() };
        ptr
    }

    /// The 503 of a failed boot goes out through the CGI seal.
    fn shed(self: Box<Self>) {
        let Self { req, tx } = *self;
        Context::new(req, tx).refuse(503);
    }
}
