use std::time::Instant;

use tracing::Span;

use crate::{add_assoc_stringl_ex, callbacks::guard, rapira_array_init, zval};

struct TimedSpan {
    span: Span,
    operation: &'static str,
    started: Instant,
}

impl TimedSpan {
    fn new(span: Span, operation: &'static str) -> Self {
        Self {
            span,
            operation,
            started: Instant::now(),
        }
    }
}

impl Drop for TimedSpan {
    fn drop(&mut self) {
        otel::record_duration(self.operation, self.started.elapsed());
    }
}

#[derive(Default)]
pub(crate) struct Telemetry {
    queue: Option<TimedSpan>,
    execute: Option<TimedSpan>,
    pub(crate) carrier: Vec<(String, String)>,
}

impl Telemetry {
    pub(crate) fn queued(parent: &Span) -> Self {
        let span = if parent.is_disabled() {
            Span::none()
        } else {
            tracing::info_span!(parent: parent, "queue.wait")
        };
        Self {
            queue: Some(TimedSpan::new(span, "queue.wait")),
            execute: None,
            carrier: Vec::new(),
        }
    }

    pub(crate) fn dequeued(&mut self) {
        self.queue = None;
    }

    pub(crate) fn execute(&mut self, parent: &Span) -> Span {
        let span = if parent.is_disabled() {
            Span::none()
        } else {
            tracing::info_span!(parent: parent, "php.execute", otel.status_code = tracing::field::Empty)
        };
        self.carrier = otel::trace_context(&span);
        self.execute = Some(TimedSpan::new(span.clone(), "php.execute"));
        span
    }

    pub(crate) fn finish(&mut self, errored: bool) {
        if let Some(execute) = self.execute.take()
            && errored
        {
            execute.span.record("otel.status_code", "ERROR");
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.finish(true);
    }
}

/// # Safety
/// `dst` is writable. The carrier stays owned by the request across PHP allocation bailouts.
pub(crate) unsafe fn emit_carrier(dst: *mut zval, carrier: &[(String, String)]) {
    unsafe {
        rapira_array_init(dst, carrier.len() as u32);
        for (key, value) in carrier {
            add_assoc_stringl_ex(
                dst,
                key.as_ptr().cast(),
                key.len(),
                value.as_ptr().cast(),
                value.len(),
            );
        }
    }
}

/// # Safety
/// `return_value` is writable. PHP is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_trace_context(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        if let Some(ctx) = crate::context::ctx() {
            emit_carrier(return_value, &ctx.telemetry.carrier);
        } else if let Some(ctx) = crate::exchange::active_context() {
            emit_carrier(return_value, &ctx.telemetry.carrier);
        } else {
            rapira_array_init(return_value, 0);
        }
        true
    })
}
