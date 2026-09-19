use std::{fs::File, io::ErrorKind};

use crate::{
    callbacks::{finalize_response, send_error_head},
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    scoreboard::{Event, sb_update},
    start::pull_job,
    types::Job,
    *,
};

fn status_for_open_error(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::NotFound => 404,
        ErrorKind::PermissionDenied => 403,
        _ => 500,
    }
}

pub(crate) fn classic_worker() {
    while let Some(job) = pull_job() {
        let crate::work::Work::Http(mut job) = job else {
            job.unavailable();
            continue;
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

        let exec_err: bool = match File::open(&job.ctx.req.script_filename) {
            Err(e) => {
                send_error_head(&mut job.ctx, status_for_open_error(e.kind()));
                true
            }
            Ok(_) => {
                let failed = !run_script(&job.ctx.req.script_filename);
                let pg = rapira_pg();
                failed
                    && ((*rapira_cg()).unclean_shutdown
                        || (!(*pg).last_error_message.is_null()
                            && (*pg).last_error_type & E_FATAL_ERRORS as i32 != 0))
            }
        };
        job.ctx.tearing_down = true;
        rapira_request_shutdown();

        let truncated = finalize_response(&mut job.ctx, exec_err);

        (exec_err, truncated)
    };

    unbind_server_context();
    (Event::Handled(is_errored), truncated)
}
