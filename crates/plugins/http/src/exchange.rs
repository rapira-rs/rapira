//! The Exchange work unit: one HTTP request on a dispatcher-mode intake and its Work impl.

use tokio::sync::mpsc::Sender;

use rapira_sapi::types::{Context, Frame, Request};
use rapira_sapi::work::{Held, Work};

use crate::php::ExchangeState;

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

    fn into_held(self: Box<Self>) -> Box<dyn Held> {
        Box::new(ExchangeState::new(self.req, self.tx))
    }

    /// The 503 of a failed boot goes out through the CGI seal.
    fn shed(self: Box<Self>) {
        let Self { req, tx } = *self;
        Context::new(req, tx).refuse(503);
    }
}
