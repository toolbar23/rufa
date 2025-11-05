mod cli;
mod config;
mod debug_update;
mod ipc;
mod logging;
mod runner;
mod state;
mod watch;

use anyhow::Result;
use clap::Parser;
use std::env;
use std::ffi::OsString;

#[tokio::main]
async fn main() -> Result<()> {
    logging::init_tracing();
    let mut args = env::args_os().collect::<Vec<_>>();
    if should_default_to_info(&args) {
        args.insert(1, OsString::from("info"));
    }
    let cli = cli::Cli::parse_from(args);
    cli.execute().await
}

fn should_default_to_info(args: &[OsString]) -> bool {
    if args.len() <= 1 {
        return true;
    }

    // Respect top-level help/version invocations
    for arg in args.iter().skip(1) {
        if let Some(s) = arg.to_str() {
            if matches!(s, "-h" | "--help" | "-V" | "--version") {
                return false;
            }
        }
    }

    let commands = [
        "start",
        "run",
        "kill",
        "info",
        "log",
        "restart",
        "stop",
        "completions",
        "daemon",
        "__daemon",
    ];

    for arg in args.iter().skip(1) {
        if let Some(s) = arg.to_str() {
            if commands.contains(&s) {
                return false;
            }
        }
    }

    true
}
