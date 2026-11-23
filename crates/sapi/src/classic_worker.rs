use crate::{
    callbacks::fatal_error_recorded,
    context::{bind_server_context, populate_request_context, unbind_server_context},
    pool::{Event, record},
    start::{Pull, Wait, pull, run_script},
    types::Context,
    *,
};
use std::path::Path;

pub(crate) fn classic_worker(entrypoint: &Path) {
    loop {
        crate::context::resolve_script(entrypoint);
        crate::context::clean_realpath_cache();
        loop {
            match pull(Wait::Forever) {
                Pull::Unit(unit) => record(classic_executor(&mut unit.into_cgi())),
                Pull::Reload => {
                    record(Event::Starting);
                    crate::quota::renew();
                    tracing::info!(target: "rapira", "reload requested; resolving the entrypoint again");
                    break;
                }
                Pull::Closed | Pull::NoUnit => return,
            }
        }
    }
}

// run_script is also false for exit() and die(), so only a recorded fatal error marks a failure.
fn classic_executor(ctx: &mut Context) -> Event {
    // The reset comes before bind_server_context, because the request that makes the reset call has no client and runs no script.
    if !crate::rapira_worker::reset_opcache_before_request() {
        tracing::error!(target: "rapira", "OPcache reset request failed; refusing the request");
        ctx.refuse(500);
        return Event::Handled { errored: true };
    }
    bind_server_context(ctx);
    let errored = unsafe {
        populate_request_context(ctx);
        if php_request_startup() == FAILURE {
            // The refusal comes before the shutdown, because the php_header() in RSHUTDOWN would commit a 200 head first.
            ctx.refuse(500);
            rapira_request_shutdown();
            unbind_server_context();
            return Event::Handled { errored: true };
        }
        crate::context::apply_proto_num(ctx);

        let failed = !run_script(crate::context::script().filename());
        let exec_err: bool = failed && fatal_error_recorded();
        ctx.tearing_down = true;
        ctx.errored = exec_err;
        rapira_request_shutdown();
        // RSHUTDOWN seals inside a zend_try; a bailout there leaves the seal to this call.
        ctx.end(exec_err);
        exec_err
    };

    unbind_server_context();
    Event::Handled { errored }
}
