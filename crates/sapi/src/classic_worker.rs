use crate::{
    callbacks::fatal_error_recorded,
    context::{bind_server_context, populate_request_context, unbind_server_context},
    scoreboard::{Event, sb_update},
    start::{Pull, Wait, pull, run_script},
    types::Context,
    *,
};

pub(crate) fn classic_worker() {
    while let Pull::Unit(unit) = pull(Wait::Forever) {
        let mut ctx = unit.into_cgi();
        sb_update(classic_executor(&mut ctx));
    }
}

// run_script is also false for exit()/die(), so only a recorded fatal error marks a failure.
fn classic_executor(ctx: &mut Context) -> Event {
    bind_server_context(ctx);
    let errored = unsafe {
        populate_request_context(ctx);
        if php_request_startup() == FAILURE {
            // Before the shutdown: the php_header() in RSHUTDOWN would commit a 200 head first.
            ctx.refuse(500);
            rapira_request_shutdown();
            unbind_server_context();
            return Event::Handled { errored: true };
        }
        crate::context::apply_proto_num(ctx);

        let failed = !run_script(std::path::Path::new(&crate::context::script().filename));
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
