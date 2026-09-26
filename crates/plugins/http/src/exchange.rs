use tokio::sync::mpsc::{self, Sender};

use rapira_sapi::callbacks::send_error_head;
use rapira_sapi::types::{Context, Frame, Request};
use rapira_sapi::work::{Held, Work, now_unix_f64};
use rapira_sapi::zend_object;

use crate::php::{ExchangeState, exchange_from};

// cap 4 lets a buffered Head+Chunk+End trio, plus a stray interim head, queue without parking the PHP thread
const FRAME_CAP: usize = 4;

/// One HTTP request on the intake.
pub struct Exchange {
    /// The request, the frame sender, and the CGI view when `new` built one.
    ctx: Context,
}

impl Exchange {
    /// `rx` is the reply the transport reads. Stamps received_at when the transport left it None.
    /// `superglobals` builds the CGI view here, on the transport thread: true in the classic and worker modes.
    pub fn new(mut req: Request, superglobals: bool) -> (Self, mpsc::Receiver<Frame>) {
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel(FRAME_CAP);
        let ctx = Context::new(req, tx, superglobals);
        (Self { ctx }, rx)
    }
}

impl Work for Exchange {
    fn cancelled(&self) -> bool {
        self.ctx.sender.as_ref().is_some_and(Sender::is_closed)
    }

    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held {
        let Context { req, sender, .. } = self.ctx;
        let tx = sender.expect("a queued exchange holds its sender");
        let ptr = Box::into_raw(Box::new(ExchangeState::new(req, tx)));
        // SAFETY: the caller passes a live Rapira\Internal\Http\Exchange object.
        unsafe { (*exchange_from(obj)).job = ptr.cast() };
        ptr
    }

    fn into_cgi(self: Box<Self>) -> Option<Context> {
        Some(self.ctx)
    }

    fn shed(self: Box<Self>) {
        let mut ctx = self.ctx;
        send_error_head(&mut ctx, 503);
        ctx.finish(false);
    }
}
