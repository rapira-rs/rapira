use crate::{
    callbacks::{finalize_response, send_error_head},
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    scoreboard::{Event, sb_update},
    start::pull_job,
    types::Job,
    *,
};

pub(crate) fn classic_worker() {
    while let Some(unit) = pull_job() {
        let Some(mut job) = unit.into_http() else {
            unreachable!("gRPC units go to dispatcher-mode workers only");
        };
        let (event, truncated) = classic_executor(&mut job);
        sb_update(event);
        job.ctx.finish(truncated);
    }
}

// run_script is also false for exit()/die(), so only unclean_shutdown or a fresh last_error_message marks a real failure.
fn classic_executor(job: &mut Job) -> (Event, bool) {
    bind_server_context(&mut job.ctx);
    let (is_errored, truncated) = unsafe {
        populate_request_context(&mut job.ctx);
        if php_request_startup() == FAILURE {
            send_error_head(&mut job.ctx, 500);
            rapira_request_shutdown();
            unbind_server_context();
            return (Event::Handled(true), false);
        }
        crate::context::apply_proto_num(&job.ctx);

        let failed = !run_script(std::path::Path::new(crate::context::script().filename));
        let pg = rapira_pg();
        let exec_err: bool = failed
            && ((*rapira_cg()).unclean_shutdown
                || (!(*pg).last_error_message.is_null()
                    && (*pg).last_error_type & E_FATAL_ERRORS as i32 != 0));
        job.ctx.tearing_down = true;
        rapira_request_shutdown();

        let truncated = finalize_response(&mut job.ctx, exec_err);

        (exec_err, truncated)
    };

    unbind_server_context();
    (Event::Handled(is_errored), truncated)
}
