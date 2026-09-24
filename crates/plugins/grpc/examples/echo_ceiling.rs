//! The gRPC front with a Rust backend that answers without PHP: the transport ceiling of a benchmark.
//! It reads the `[grpc]` table of `rapira.toml` and forks `grpc.pool.processes` workers.
//! Usage: `echo_ceiling serve <rapira.toml>`

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow};
use bytes::Bytes;
use extension_api::{
    Backend, Extension, ListenAddr, Php, PrepareCtx, Reply, Request, Result, RpcStatus, UnaryCall,
    UnaryReply,
};
use http::HeaderMap;
use rapira_config::Listen;
use rapira_grpc::{Config, Schema, Server};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The SIGINT and SIGTERM handler of the parent sets this flag.
static STOP: AtomicBool = AtomicBool::new(false);

const GREETING: &[u8] = b"Hello from worker, ";

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let [_, command, path] = args.as_slice() else {
        usage();
    };
    if command != "serve" {
        usage();
    }
    match serve(Path::new(path)) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("echo_ceiling: {e:#}");
            std::process::exit(1);
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: echo_ceiling serve <rapira.toml>");
    std::process::exit(2);
}

/// Returns the exit code of the parent.
fn serve(path: &Path) -> Result<i32> {
    let settings = rapira_config::resolve(path)?;
    let grpc = settings.grpc.context("the config has no [grpc] table")?;
    let schema = Arc::new(Schema::load(&grpc.descriptor_set, &grpc.services)?);
    let listen = match grpc.listen {
        Listen::Tcp(addr) => ListenAddr::Tcp(addr),
        Listen::Unix(path) => ListenAddr::Unix(path),
    };
    let mut server = Server::init(Config {
        listen,
        schema,
        reflection: false,
        default_timeout: None,
        max_timeout: None,
        drain_grace: settings.supervisor.drain_grace(),
    });
    // The parent keeps `server` and `ctx` until it exits, so the listener stays open in the parent.
    let mut ctx = PrepareCtx::new();
    server.prepare(&mut ctx)?;

    let mut children = Vec::with_capacity(grpc.pool.processes);
    for _ in 0..grpc.pool.processes {
        // SAFETY: the parent has one thread, so the child gets no lock that another thread holds.
        match unsafe { libc::fork() } {
            0 => worker(&mut server),
            -1 => {
                let err = io::Error::last_os_error();
                stop_children(&children);
                return Err(err).context("fork");
            }
            pid => children.push(pid),
        }
    }
    install_stop_handlers()?;
    Ok(supervise(children))
}

fn worker(server: &mut Server) -> ! {
    // SAFETY: prctl with PR_SET_PDEATHSIG reads two integers and writes no memory.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
    // Server::run serves on its own thread and runtime, so a current-thread runtime is enough here.
    let served = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(server.run(Php::new(Arc::new(Echo)))));
    match served {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("echo_ceiling: worker {}: {e:#}", std::process::id());
            std::process::exit(1);
        }
    }
}

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// The handlers have no SA_RESTART, so a signal interrupts waitpid in `supervise`. A handler also replaces the SIG_IGN that a non-interactive shell sets for SIGINT on a command started with `&`. The parent installs the handlers after the fork, so each child keeps the default action for SIGTERM.
fn install_stop_handlers() -> Result {
    // SAFETY: act is fully set before use, and the handler only stores to an atomic.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = on_stop as *const () as usize;
        libc::sigemptyset(&mut act.sa_mask);
        for sig in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(sig, &act, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error()).context("sigaction");
            }
        }
    }
    Ok(())
}

/// Waits for a stop signal or for a child that exits. Returns the exit code of the parent.
fn supervise(mut children: Vec<libc::pid_t>) -> i32 {
    loop {
        // SAFETY: waitpid accepts a null status pointer.
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) };
        let err = io::Error::last_os_error();
        children.retain(|&c| c != pid);
        if STOP.load(Ordering::Relaxed) {
            stop_children(&children);
            return 0;
        }
        if pid > 0 {
            eprintln!("echo_ceiling: worker {pid} exited");
        } else if err.kind() == io::ErrorKind::Interrupted {
            continue;
        } else {
            eprintln!("echo_ceiling: waitpid: {err}");
        }
        stop_children(&children);
        return 1;
    }
}

/// Sends SIGTERM to each child and waits until each one exits.
fn stop_children(children: &[libc::pid_t]) {
    for &pid in children {
        // SAFETY: kill reads two integers and writes no memory.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    for &pid in children {
        // SAFETY: waitpid accepts a null status pointer.
        while unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } == -1
            && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
        {}
    }
}

/// Answers each unary call with the bytes that the PHP worker of the benchmark returns.
struct Echo;

impl Backend for Echo {
    fn exec(&self, _req: Request) -> Pin<Box<dyn Future<Output = Result<Reply>> + Send + '_>> {
        Box::pin(async { Err(anyhow!("the ceiling serves no HTTP")) })
    }

    fn unary(
        &self,
        call: UnaryCall,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UnaryReply>>> + Send + '_>> {
        Box::pin(async move {
            let outcome = match first_field(&call.message) {
                Some(text) => Ok(greeting(text)),
                None => Err(RpcStatus {
                    code: 3,
                    message: "bad request".into(),
                    details: vec![],
                }),
            };
            Ok(Some(UnaryReply {
                headers: HeaderMap::new(),
                trailers: HeaderMap::new(),
                outcome,
            }))
        })
    }
}

/// Returns field 1 of a message that starts with it: the tag 0x0a, a varint length, then the bytes. https://protobuf.dev/programming-guides/encoding/#length-types
fn first_field(message: &[u8]) -> Option<&[u8]> {
    let rest = message.strip_prefix(&[0x0a])?;
    let mut len: u64 = 0;
    for (i, &byte) in rest.iter().take(10).enumerate() {
        len |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return rest[i + 1..].get(..usize::try_from(len).ok()?);
        }
    }
    None
}

/// Encodes `Hello from worker, {text}!` as field 1 of the reply message, in one allocation of the exact size.
fn greeting(text: &[u8]) -> Bytes {
    let len = GREETING.len() + text.len() + 1;
    // A varint holds 7 bits in each byte.
    let mut out = Vec::with_capacity(1 + len.ilog2() as usize / 7 + 1 + len);
    out.push(0x0a);
    let mut rest = len;
    while rest >= 0x80 {
        out.push(rest as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
    out.extend_from_slice(GREETING);
    out.extend_from_slice(text);
    out.push(b'!');
    Bytes::from(out)
}
