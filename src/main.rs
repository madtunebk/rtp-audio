//! Play sound from other machines (WSL, Linux boxes) on this one, over the network as RTP,
//! and on Linux, send this machine's sound. See `cli::USAGE`.

mod cli;
mod json;
mod net;
mod receiver;
// The sender (and its web server) captures through PulseAudio/PipeWire: Linux only.
#[cfg(target_os = "linux")]
mod sender;

use std::error::Error;
use std::io::IsTerminal;
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
        Command::Version => {
            println!("rtp-audio {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Receive(options) => receiver::run(options),
        Command::Devices { json } => receiver::list_devices(json),
        Command::Keygen => {
            println!("{}", net::secure::generate()?);
            // Only when the key went to the screen: `keygen > file` needs no advice.
            if std::io::stdout().is_terminal() {
                eprintln!(
                    "Save it to a file on both computers, readable only by you, e.g.\n  \
                     rtp-audio keygen > ~/.rtp-audio.key && chmod 600 ~/.rtp-audio.key\n\
                     then use --key-file ~/.rtp-audio.key with send and the receiver."
                );
            }
            Ok(())
        }
        Command::Find(port) => net::discover::print_found(port),
        Command::Send(mut options) if options.stdin => {
            if let Some(port) = options.auto {
                options.destinations.push(net::discover::pick(port)?);
            }
            net::transport::send_stdin(options.destinations[0], options.rate, options.channels, options.encoding)
        }
        #[cfg(target_os = "linux")]
        Command::Sources { json } => sender::list_sources(json),
        #[cfg(target_os = "linux")]
        Command::Service { action, send_args } => sender::service::run(&action, &send_args),
        #[cfg(target_os = "linux")]
        Command::Send(mut options) => {
            if let Some(port) = options.auto {
                options.destinations.push(net::discover::pick(port)?);
            }
            sender::send::run(&options.destinations, options.encoding, options.source.as_deref(), options.web, options.mic)
        }
        #[cfg(not(target_os = "linux"))]
        Command::Sources { .. } | Command::Send(_) | Command::Service { .. } => {
            Err("capturing sound is only supported on Linux; here, pipe raw audio into `send HOST:PORT --stdin`".into())
        }
    }
}
