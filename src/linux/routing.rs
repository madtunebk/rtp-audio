//! Automatic mode's routing: an "RTP Audio" null sink made the default output, with the
//! streams already playing moved onto it, and everything switched back afterwards.
//!
//! Every change is recorded in a state file before or right after it's made. Normal shutdown,
//! failed startup and recovery after `kill -9` all undo from that record, and only ever touch
//! the module whose arguments carry this run's random token, never a sink found by name alone.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::pulse::{Module, Pulse};

pub const DISPLAY_NAME: &str = "RTP Audio";
/// Sink property holding the token, so the sink can be traced back to the run that made it.
pub const INSTANCE_PROPERTY: &str = "rtp_audio.instance";
const SINK_PREFIX: &str = "rtp_audio_";

#[derive(Debug, PartialEq)]
pub struct State {
    /// Random, unique to one run.
    pub token: String,
    /// The default output before we changed it.
    pub original_default: Option<String>,
    /// Streams we moved to our sink, with the name of the sink each came from.
    pub moved: Vec<(u32, String)>,
}

impl State {
    fn new() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()) as u64;
        let token = nanos ^ u64::from(std::process::id()).rotate_left(40);
        Self { token: format!("{token:016x}"), original_default: None, moved: Vec::new() }
    }

    pub fn sink_name(&self) -> String {
        format!("{SINK_PREFIX}{}", self.token)
    }

    fn to_text(&self) -> String {
        let mut text = String::from("# rtp-audio: what to switch back if the sender stops without cleaning up\n");
        text += &format!("token={}\n", self.token);
        if let Some(default) = &self.original_default {
            text += &format!("default={default}\n");
        }
        for (input, sink) in &self.moved {
            text += &format!("moved={input} {sink}\n");
        }
        text
    }

    fn from_text(text: &str) -> Option<Self> {
        let mut state = Self { token: String::new(), original_default: None, moved: Vec::new() };
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            match line.split_once('=')? {
                ("token", token) => state.token = token.to_string(),
                ("default", sink) => state.original_default = Some(sink.to_string()),
                ("moved", value) => {
                    let (input, sink) = value.split_once(' ')?;
                    state.moved.push((input.parse().ok()?, sink.to_string()));
                }
                _ => {}
            }
        }
        let valid = state.token.len() == 16 && state.token.bytes().all(|b| b.is_ascii_hexdigit());
        valid.then_some(state)
    }

    fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, self.to_text())
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|err| format!("cannot write {}: {err}", path.display()).into())
    }

    /// Is this module the null sink this run loaded?
    fn owns(&self, module: &Module) -> bool {
        let sink_name = format!("sink_name={}", self.sink_name());
        module.name == "module-null-sink" && module.argument.split_whitespace().any(|arg| arg == sink_name)
    }
}

/// The state file for one sound server.
pub fn state_path(dir: &Path, server: &str) -> PathBuf {
    let key: String = server.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    dir.join(format!("routing-{key}.state"))
}

/// Undo what a sender that was killed (or lost its sound server) left behind.
pub fn recover(pulse: &Pulse, path: &Path) -> Result<(), Box<dyn Error>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display()).into()),
    };
    match State::from_text(&text) {
        Some(state) => {
            eprintln!("Switching back sound left over from a sender that did not stop cleanly");
            undo(pulse, &state, path)
        }
        None => {
            eprintln!("Ignoring unreadable {}", path.display());
            Ok(std::fs::remove_file(path)?)
        }
    }
}

pub struct Routing<'a> {
    pulse: &'a Pulse,
    path: PathBuf,
    pub state: State,
}

impl<'a> Routing<'a> {
    pub fn new(pulse: &'a Pulse, path: PathBuf) -> Self {
        Self { pulse, path, state: State::new() }
    }

    /// Create the sink and return the name of its monitor source.
    pub fn create_sink(&mut self) -> Result<String, Box<dyn Error>> {
        self.state.original_default = self.pulse.server_info()?.default_sink;
        // Saved before the module exists, so a crash right after loading it is recoverable.
        self.state.save(&self.path)?;
        let sink_name = self.state.sink_name();
        let argument = format!(
            "sink_name={sink_name} rate=48000 channels=2 \
             sink_properties='device.description=\"{DISPLAY_NAME}\" {INSTANCE_PROPERTY}={}'",
            self.state.token
        );
        self.pulse
            .load_module("module-null-sink", &argument)
            .map_err(|err| format!("could not create the \"{DISPLAY_NAME}\" output: {err}"))?;
        // PipeWire may announce the sink a moment after the module loads.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(sink) = self.pulse.sinks()?.into_iter().find(|sink| sink.name == sink_name) {
                return Ok(sink.monitor_source);
            }
            if Instant::now() > deadline {
                return Err(format!("the \"{DISPLAY_NAME}\" output did not appear after loading module-null-sink").into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn make_default(&mut self) -> Result<(), Box<dyn Error>> {
        let sink_name = self.state.sink_name();
        self.pulse
            .set_default_sink(&sink_name)
            .map_err(|err| format!("could not make \"{DISPLAY_NAME}\" the default output: {err}"))?;
        // PipeWire confirms before the change shows. Wait for it, or undoing right away would
        // see the old default, leave it, and the late change would then point at a removed sink.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.pulse.server_info()?.default_sink.as_deref() != Some(&sink_name) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Move every playing stream to our sink. Returns how many moved.
    pub fn move_streams(&mut self) -> Result<usize, Box<dyn Error>> {
        let sinks = self.pulse.sinks()?;
        let sink_name = self.state.sink_name();
        let ours = sinks.iter().find(|sink| sink.name == sink_name).ok_or("the RTP Audio output disappeared")?;
        for input in self.pulse.sink_inputs()? {
            let Some(from) = sinks.iter().find(|sink| sink.index == input.sink) else { continue };
            if from.index == ours.index {
                continue;
            }
            // Some streams refuse to move; they keep playing where they are.
            match self.pulse.move_sink_input(input.index, ours.index) {
                Ok(()) => {
                    self.state.moved.push((input.index, from.name.clone()));
                    self.state.save(&self.path)?;
                }
                Err(err) => eprintln!("Leaving one stream where it is: {err}"),
            }
        }
        Ok(self.state.moved.len())
    }

    /// Switch everything back. On failure, the state file stays for the next start to retry.
    pub fn restore(self) {
        let result = if self.pulse.is_connected() {
            undo(self.pulse, &self.state, &self.path)
        } else {
            // The connection broke but the server may still be there: try once more.
            Pulse::connect().and_then(|pulse| undo(&pulse, &self.state, &self.path))
        };
        if let Err(err) = result {
            eprintln!(
                "rtp-audio: could not switch sound back ({err}).\n  \
                 Running rtp-audio send again will finish switching it back."
            );
        }
    }
}

/// Undo `state` on the server: default output, moved streams, then our module. Each step only
/// acts if things are still the way we left them, so changes the user made meanwhile stay.
fn undo(pulse: &Pulse, state: &State, path: &Path) -> Result<(), Box<dyn Error>> {
    let sink_name = state.sink_name();
    let sinks = pulse.sinks()?;
    let modules: Vec<Module> = pulse.modules()?.into_iter().filter(|module| state.owns(module)).collect();
    // Our sink, recognised by the token in both its name and its properties.
    let ours = sinks
        .iter()
        .find(|sink| sink.name == sink_name && sink.instance.as_deref().is_none_or(|token| token == state.token));

    if let Some(ours) = ours {
        let current_default = pulse.server_info()?.default_sink;
        let original = state.original_default.as_ref().and_then(|name| sinks.iter().find(|sink| &sink.name == name));
        if current_default.as_deref() == Some(&sink_name)
            && let Some(original) = original
        {
            match pulse.set_default_sink(&original.name) {
                Ok(()) => eprintln!("Default output switched back to {}", original.description),
                Err(err) => eprintln!("Could not switch the default output back: {err}"),
            }
        }
        let mut moved_back = 0;
        for input in pulse.sink_inputs()?.into_iter().filter(|input| input.sink == ours.index) {
            let Some((_, from)) = state.moved.iter().find(|(index, _)| *index == input.index) else { continue };
            let Some(target) = sinks.iter().find(|sink| &sink.name == from) else { continue };
            match pulse.move_sink_input(input.index, target.index) {
                Ok(()) => moved_back += 1,
                Err(err) => eprintln!("Could not move a stream back: {err}"),
            }
        }
        if moved_back > 0 {
            eprintln!("Moved {moved_back} stream(s) back to their outputs");
        }
    }
    for module in &modules {
        pulse.unload_module(module.index)?;
    }
    if !modules.is_empty() {
        eprintln!("Removed the \"{DISPLAY_NAME}\" output");
    }
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Module, State};

    #[test]
    fn state_round_trips() {
        let mut state = State::new();
        state.original_default = Some("alsa_output.pci-0000_00_1f.3.analog-stereo".into());
        state.moved = vec![(12, "alsa_output.a".into()), (40, "bluez_sink.b".into())];
        assert_eq!(State::from_text(&state.to_text()), Some(state));
        assert_eq!(State::from_text("token=xyz\n"), None);
        assert_eq!(State::from_text("garbage"), None);
    }

    #[test]
    fn owns_only_its_own_module() {
        let state = State::new();
        let module = |name: &str, argument: String| Module { index: 1, name: name.into(), argument };
        let ours = format!("sink_name={} rate=48000", state.sink_name());
        assert!(state.owns(&module("module-null-sink", ours.clone())));
        assert!(!state.owns(&module("module-loopback", ours)));
        assert!(!state.owns(&module("module-null-sink", "sink_name=rtp_audio_0000000000000000".into())));
        assert!(!state.owns(&module("module-null-sink", format!("sink_name={}x", state.sink_name()))));
    }
}
