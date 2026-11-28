use clap::{Args, CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

mod logging;
mod pidfile;
mod server;
mod settings;
mod signals;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The class register functions of the linked plugins. MINIT calls them in this order.
static PLUGIN_REGISTERS: [unsafe extern "C" fn(); 2] = [
    rapira_http::rapira_http_register_classes,
    rapira_grpc::rapira_grpc_register_classes,
];

#[derive(Parser)]
#[command(name = "rapira", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the server and serve requests.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Path to rapira.toml. Relative paths inside the file resolve against its directory.
    #[arg(value_name = "CONFIG")]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let signals = signals::Signals::block()?;
    rapira_sapi::block_timer_signal()?;
    // Transparent huge pages increase the resident size of allocator regions. https://man7.org/linux/man-pages/man2/PR_SET_THP_DISABLE.2const.html
    #[cfg(target_os = "linux")]
    // SAFETY: prctl with integer arguments only; the kernel reads all four as unsigned long.
    unsafe {
        libc::prctl(
            libc::PR_SET_THP_DISABLE,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };

    match Cli::parse().command {
        Some(Commands::Serve(args)) => {
            let settings = settings::resolve(&args.config)?;
            logging::init(&settings.log);
            server::serve(settings, signals)
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}
