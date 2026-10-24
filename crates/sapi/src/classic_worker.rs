use crate::{
    callbacks::{finalize_response, send_error_head},
    context::{bind_server_context, populate_request_context, unbind_server_context},
    scoreboard::{Event, sb_update},
    start::{pull_unit, run_script},
    types::Context,
    *,
};

pub(crate) fn classic_worker() {
    while let Some(unit) = pull_unit() {
        let Some(mut ctx) = unit.into_cgi() else {
            unreachable!("a unit with no CGI form goes to dispatcher-mode workers only");
        };
        let Executed { event, truncated } = classic_executor(&mut ctx);
        sb_update(event);
        ctx.finish(truncated);
    }
}

/// The result of one classic-mode cycle.
struct Executed {
    event: Event,
    /// The response body was cut, so `Context::finish` sends no content-length.
    truncated: bool,
}

// run_script is also false for exit()/die(), so only unclean_shutdown or a fresh last_error_message marks a real failure.
fn classic_executor(ctx: &mut Context) -> Executed {
    bind_server_context(ctx);
    let (is_errored, truncated) = unsafe {
        populate_request_context(ctx);
        if php_request_startup() == FAILURE {
            send_error_head(ctx, 500);
            rapira_request_shutdown();
            unbind_server_context();
            return Executed {
                event: Event::Handled(true),
                truncated: false,
            };
        }
        crate::context::apply_proto_num(ctx);

        let failed = !run_script(std::path::Path::new(&crate::context::script().filename));
        let exec_err: bool = failed
            && (compiler_globals.unclean_shutdown
                || (!core_globals.last_error_message.is_null()
                    && core_globals.last_error_type & E_FATAL_ERRORS as i32 != 0));
        ctx.tearing_down = true;
        ctx.errored = exec_err;
        rapira_request_shutdown();
        let truncated = finalize_response(ctx, exec_err);
        (exec_err, truncated)
    };

    unbind_server_context();
    Executed {
        event: Event::Handled(is_errored),
        truncated,
    }
}
