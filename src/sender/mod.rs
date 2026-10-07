//! Sending this computer's sound, through PulseAudio or PipeWire (pipewire-pulse): Linux only.
//! The receiver, in `receiver`, runs everywhere.

mod capture;
mod lock;
mod playback;
mod pulse;
mod routing;
pub mod send;
pub mod service;
pub mod web;

use std::error::Error;

/// Something that ends a running sender.
pub enum Event {
    Signal,
    Capture(String),
    Network(std::io::Error),
    /// Playing the browser's microphone into its feed failed.
    Mic(String),
}

/// `rtp-audio sources`: works while a sender is running, as it takes no lock.
pub fn list_sources() -> Result<(), Box<dyn Error>> {
    let pulse = pulse::Pulse::connect()?;
    let sources = pulse.sources()?;
    let width = sources.iter().map(|source| source.name.len()).max().unwrap_or(0).max(4);
    println!("{:>4}  {:width$}  DESCRIPTION", "ID", "NAME");
    for source in &sources {
        println!("{:>4}  {:width$}  {}", source.index, source.name, source.description);
    }
    println!(
        "\nSend one with: rtp-audio send HOST:46000 --source ID_OR_NAME\n\
         A \"Monitor of ...\" source sends a copy of what that output plays, and it keeps playing here."
    );
    Ok(())
}

/// Debug builds only: fail at a named startup step if RTP_AUDIO_FAIL_AT says so, to test that
/// partial startups are undone.
fn fail_point(step: &str) -> Result<(), Box<dyn Error>> {
    if cfg!(debug_assertions) && std::env::var("RTP_AUDIO_FAIL_AT").is_ok_and(|at| at == step) {
        return Err(format!("simulated failure at '{step}' (RTP_AUDIO_FAIL_AT)").into());
    }
    Ok(())
}
