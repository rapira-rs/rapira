use tokio::sync::mpsc::{self, Sender};

use crate::callbacks::send_error_head;
use crate::exchange::{ExchangeState, exchange_from};
use crate::types::{Context, Frame, Request};
use crate::work::{Held, Work, now_unix_f64};
use crate::zend_object;

// cap 4 lets a buffered Head+Chunk+End trio, plus a stray interim head, queue without parking the PHP thread
const FRAME_CAP: usize = 4;

/// One HTTP request on the intake.
pub struct Exchange {
    req: Request,
    tx: Sender<Frame>,
}

impl Exchange {
    /// `rx` is the reply the transport reads. Stamps received_at when the transport left it None.
    pub fn new(mut req: Request) -> (Self, mpsc::Receiver<Frame>) {
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel(FRAME_CAP);
        (Self { req, tx }, rx)
    }

    pub fn request(&self) -> &Request {
        &self.req
    }

    /// The frame sender PHP writes the reply to.
    pub fn reply_sender(&self) -> Sender<Frame> {
        self.tx.clone()
    }
}

impl Work for Exchange {
    fn cancelled(&self) -> bool {
        self.tx.is_closed()
    }

    unsafe fn attach(self: Box<Self>, obj: *mut zend_object) -> *mut dyn Held {
        let Self { req, tx } = *self;
        let ptr = Box::into_raw(Box::new(ExchangeState::new(req, tx)));
        // SAFETY: the caller passes a live Rapira\Internal\Http\Exchange object.
        unsafe { (*exchange_from(obj)).job = ptr.cast() };
        ptr
    }

    fn into_cgi(self: Box<Self>) -> Option<Context> {
        Some(Context::new(self.req, self.tx, true))
    }

    fn shed(self: Box<Self>) {
        let mut ctx = Context::new(self.req, self.tx, false);
        send_error_head(&mut ctx, 503);
        ctx.finish(false);
    }
}
