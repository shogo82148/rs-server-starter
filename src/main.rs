//! The `start_server` command-line superdaemon.

use std::process::ExitCode;

use server_starter::{parse_args, restart_server, start_server, stop_server, usage, Action};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("start_server: {e}");
            eprintln!("try `start_server --help` for usage.");
            return ExitCode::from(1);
        }
    };

    match opts.action {
        Action::Help => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!(
                "start_server (rs-server-starter) {}",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
        Action::Restart => report(restart_server(&opts)),
        Action::Stop => report(stop_server(&opts)),
        Action::Run => report(start_server(opts)),
    }
}

fn report(result: std::io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("start_server: {e}");
            ExitCode::from(1)
        }
    }
}
