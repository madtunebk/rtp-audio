//! Play sound from other machines (WSL, Linux boxes) on this one, over the network as RTP,
//! and on Linux, send this machine's sound. See `cli::USAGE`.

mod cli;
mod jitter;
#[cfg(target_os = "linux")]
mod linux;
mod receive;
mod rtp;
mod transport;
#[cfg(target_os = "linux")]
mod web;

use std::error::Error;
use std::process::ExitCode;

use cli::Command;

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rtp-audio: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), Box<dyn Error>> {
    match cli::parse(args)? {
        Command::Help => {
            println!("{}", cli::USAGE);
            Ok(())
        }
        Command::Receive(options) => receive::run(options),
        Command::Send(options) if options.stdin => {
            let destination = options.destination.ok_or("--stdin needs the receiver's address")?;
            transport::send_stdin(destination, options.rate, options.channels)
        }
        #[cfg(target_os = "linux")]
        Command::Sources => linux::list_sources(),
        #[cfg(target_os = "linux")]
        Command::Send(options) => linux::sender::run(options.destination, options.source.as_deref(), options.web),
        #[cfg(not(target_os = "linux"))]
        Command::Sources | Command::Send(_) => {
            Err("capturing sound is only supported on Linux; here, pipe raw audio into `send HOST:PORT --stdin`".into())
        }
    }
}
