use tokio::sync::mpsc::Sender;

use rapira_sapi::callbacks::send_error_head;
use rapira_sapi::types::{Context, Frame, ReqC, Request};
use rapira_sapi::work::{Held, Work};
use rapira_sapi::zend_object;

use crate::php::{ExchangeState, exchange_from};

// cap 16 lets a buffered Head+Chunk+End trio, plus a stray interim head, queue without parking the PHP thread, and keeps a 1 MiB body in 64 KiB writes to about one park; tokio allocates one 32-slot block for any cap up to 32
pub(crate) const FRAME_CAP: usize = 16;

/// One HTTP request on the intake.
pub(crate) struct Exchange {
    req: Request,
    /// Sends the reply the transport reads.
    tx: Sender<Frame>,
    /// The CGI view, built on the transport thread in the classic and worker modes.
    cgi: Option<ReqC>,
}

impl Exchange {
    /// `superglobals` builds the CGI view here, on the transport thread: true in the classic and worker modes.
    pub(crate) fn new(req: Request, tx: Sender<Frame>, superglobals: bool) -> Self {
        let cgi = superglobals.then(|| ReqC::build(&req));
        Self { req, tx, cgi }
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

    fn into_cgi(self: Box<Self>) -> Context {
        let Self { req, tx, cgi } = *self;
        let cgi = cgi.unwrap_or_else(|| ReqC::build(&req));
        Context::new(req, tx, cgi)
    }

    /// A dispatcher-mode unit has no CGI view yet; the shed after a failed boot builds one to write the 503.
    fn shed(self: Box<Self>) {
        let mut ctx = self.into_cgi();
        send_error_head(&mut ctx, 503);
        ctx.finish(false);
    }
}
