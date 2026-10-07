//! Automatic mode's routing: an "RTP Audio" null sink made the default output, with the
//! streams already playing moved onto it, and everything switched back afterwards. With the
//! browser microphone (`--mic`), also an "RTP Audio Microphone" source made the default input.
//!
//! Every change is recorded in a state file before or right after it's made. Normal shutdown,
//! failed startup and recovery after `kill -9` all undo from that record, and only ever touch
//! the module whose arguments carry this run's random token, never a sink found by name alone.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::pulse::{Module, Pulse};

pub const DISPLAY_NAME: &str = "RTP Audio";
pub const MIC_NAME: &str = "RTP Audio Microphone";
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
    /// The browser microphone: a null sink fed from the browser, and a source made from it.
    pub mic: bool,
    /// The default input before we changed it.
    pub original_default_source: Option<String>,
}

impl State {
    fn new() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()) as u64;
        let token = nanos ^ u64::from(std::process::id()).rotate_left(40);
        Self { token: format!("{token:016x}"), original_default: None, moved: Vec::new(), mic: false, original_default_source: None }
    }

    pub fn sink_name(&self) -> String {
        format!("{SINK_PREFIX}{}", self.token)
    }

    pub fn mic_feed_name(&self) -> String {
        format!("{SINK_PREFIX}mic_{}", self.token)
    }

    pub fn mic_source_name(&self) -> String {
        format!("{SINK_PREFIX}mic_src_{}", self.token)
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
        if self.mic {
            text += "mic=1\n";
        }
        if let Some(source) = &self.original_default_source {
            text += &format!("source={source}\n");
        }
        text
    }

    fn from_text(text: &str) -> Option<Self> {
        let mut state = Self { token: String::new(), original_default: None, moved: Vec::new(), mic: false, original_default_source: None };
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            match line.split_once('=')? {
                ("token", token) => state.token = token.to_string(),
                ("default", sink) => state.original_default = Some(sink.to_string()),
                ("moved", value) => {
                    let (input, sink) = value.split_once(' ')?;
                    state.moved.push((input.parse().ok()?, sink.to_string()));
                }
                ("mic", value) => state.mic = value == "1",
                ("source", source) => state.original_default_source = Some(source.to_string()),
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

    /// Is this module one this run loaded (the output, or the microphone's feed and source)?
    fn owns(&self, module: &Module) -> bool {
        let has = |arg: String| module.argument.split_whitespace().any(|a| a == arg);
        match module.name.as_str() {
            "module-null-sink" => has(format!("sink_name={}", self.sink_name())) || has(format!("sink_name={}", self.mic_feed_name())),
            "module-remap-source" => has(format!("source_name={}", self.mic_source_name())),
            _ => false,
        }
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
            let problems = undo(pulse, &state, path)?;
            if !problems.is_empty() {
                eprintln!("Could not switch everything back: {}. Check the sound settings.", problems.join("; "));
            }
            Ok(())
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
        // With the microphone, this was already noted before its feed could change it.
        if !self.state.mic {
            self.state.original_default = self.pulse.server_info()?.default_sink;
        }
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

    /// Create the browser microphone: a mono null sink to play the browser's sound into, and a
    /// source made from its monitor, which becomes the default input. Returns the sink's name.
    pub fn create_mic(&mut self) -> Result<String, Box<dyn Error>> {
        let info = self.pulse.server_info()?;
        self.state.original_default_source = info.default_source;
        self.state.original_default = info.default_sink;
        self.state.mic = true;
        self.state.save(&self.path)?;
        let (feed, source, token) = (self.state.mic_feed_name(), self.state.mic_source_name(), self.state.token.clone());
        let fail = |err: Box<dyn Error>| -> Box<dyn Error> { format!("could not create the \"{MIC_NAME}\": {err}").into() };
        self.pulse
            .load_module(
                "module-null-sink",
                &format!("sink_name={feed} rate=48000 channels=1 sink_properties='device.description=\"{MIC_NAME} (feed)\" {INSTANCE_PROPERTY}={token}'"),
            )
            .map_err(fail)?;
        let monitor = wait_for(|| Ok(self.pulse.sinks()?.into_iter().find(|s| s.name == feed).map(|s| s.monitor_source)))?
            .ok_or_else(|| fail("its feed did not appear".into()))?;
        // The feed is an output too, and a server whose only output was its dummy makes it the
        // default (and drops the dummy): put the default back where that's still possible.
        if self.pulse.server_info()?.default_sink.as_deref() == Some(&feed)
            && let Some(original) = self.state.original_default.clone()
            && self.pulse.sinks()?.iter().any(|sink| sink.name == original)
        {
            self.pulse.set_default_sink(&original)?;
        }
        self.pulse
            .load_module(
                "module-remap-source",
                &format!("master={monitor} source_name={source} source_properties='device.description=\"{MIC_NAME}\" {INSTANCE_PROPERTY}={token}'"),
            )
            .map_err(fail)?;
        wait_for(|| Ok(self.pulse.sources()?.into_iter().any(|s| s.name == source).then_some(())))?
            .ok_or_else(|| fail("it did not appear".into()))?;
        self.pulse.set_default_source(&source).map_err(fail)?;
        wait_for(|| Ok((self.pulse.server_info()?.default_source.as_deref() == Some(&source)).then_some(())))?
            .ok_or_else(|| fail("it did not become the default input".into()))?;
        Ok(feed)
    }

    /// Is the microphone's feed the default output (sounds played here would go into it)?
    pub fn mic_feed_is_default(&self) -> Result<bool, Box<dyn Error>> {
        Ok(self.state.mic && self.pulse.server_info()?.default_sink.as_deref() == Some(&self.state.mic_feed_name()))
    }

    pub fn make_default(&mut self) -> Result<(), Box<dyn Error>> {
        let sink_name = self.state.sink_name();
        self.pulse
            .set_default_sink(&sink_name)
            .map_err(|err| format!("could not make \"{DISPLAY_NAME}\" the default output: {err}"))?;
        // PipeWire confirms before the change shows. Wait for it, or undoing right away would
        // see the old default, leave it, and the late change would then point at a removed sink.
        wait_for(|| Ok((self.pulse.server_info()?.default_sink.as_deref() == Some(&sink_name)).then_some(())))?
            .ok_or_else(|| format!("\"{DISPLAY_NAME}\" did not become the default output (the sound server accepted, then didn't switch)"))?;
        Ok(())
    }

    /// Move every playing stream to our sink. Returns how many moved.
    pub fn move_streams(&mut self) -> Result<usize, Box<dyn Error>> {
        let sinks = self.pulse.sinks()?;
        let sink_name = self.state.sink_name();
        let ours = sinks.iter().find(|sink| sink.name == sink_name).ok_or("the RTP Audio output disappeared")?;
        let feed = self.state.mic_feed_name();
        for input in self.pulse.sink_inputs()? {
            let Some(from) = sinks.iter().find(|sink| sink.index == input.sink) else { continue };
            // Not ours, and not the microphone playing into its feed.
            if from.index == ours.index || from.name == feed {
                continue;
            }
            // Recorded before moving: a crash in between leaves at worst a note about a stream
            // that didn't move, which undoing skips (it only moves back streams on our output).
            self.state.moved.push((input.index, from.name.clone()));
            self.state.save(&self.path)?;
            // Some streams refuse to move; they keep playing where they are.
            if let Err(err) = self.pulse.move_sink_input(input.index, ours.index) {
                eprintln!("Leaving one stream where it is: {err}");
                self.state.moved.pop();
                self.state.save(&self.path)?;
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
        match result {
            Ok(problems) if problems.is_empty() => {}
            Ok(problems) => eprintln!(
                "rtp-audio: removed its outputs, but could not switch everything back: {}.\n  \
                 Check the sound settings.",
                problems.join("; ")
            ),
            Err(err) => eprintln!(
                "rtp-audio: could not switch sound back ({err}).\n  \
                 Running rtp-audio send again will finish switching it back."
            ),
        }
    }
}

/// Poll `check` (for up to 3 s, as PipeWire announces things a moment late) until it gives
/// something.
fn wait_for<T>(mut check: impl FnMut() -> Result<Option<T>, Box<dyn Error>>) -> Result<Option<T>, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(found) = check()? {
            return Ok(Some(found));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Undo `state` on the server: default input and output, moved streams, then our modules. Each
/// step only acts if things are still the way we left them, so changes the user made meanwhile
/// stay. Errors removing our modules keep the state file, for the next start to retry; problems
/// switching back (nothing a retry could fix once our modules are gone) come back as a list.
fn undo(pulse: &Pulse, state: &State, path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut problems = Vec::new();
    let sink_name = state.sink_name();
    let sinks = pulse.sinks()?;
    let mut modules: Vec<Module> = pulse.modules()?.into_iter().filter(|module| state.owns(module)).collect();
    // The microphone's source sits on its feed's monitor: remove it first.
    modules.sort_by_key(|module| module.name != "module-remap-source");

    // Whether the default input / output were ours and could not be given back: once our modules
    // are gone the server falls back to another device, but keeps remembering ours as the one
    // chosen. Choosing the fallback explicitly then replaces that stale choice.
    let mut source_left = false;
    let mut sink_left = false;

    if state.mic && pulse.server_info()?.default_source.as_deref() == Some(&state.mic_source_name()) {
        let sources = pulse.sources()?;
        match state.original_default_source.as_ref().and_then(|name| sources.iter().find(|s| &s.name == name)) {
            Some(original) => match pulse.set_default_source(&original.name) {
                Ok(()) => eprintln!("Default input switched back to {}", original.description),
                Err(err) => {
                    problems.push(format!("the default input ({err})"));
                    source_left = true;
                }
            },
            None => source_left = true,
        }
    }
    // Our sink, recognised by the token in both its name and its properties.
    let ours = sinks
        .iter()
        .find(|sink| sink.name == sink_name && sink.instance.as_deref().is_none_or(|token| token == state.token));

    if let Some(ours) = ours {
        if pulse.server_info()?.default_sink.as_deref() == Some(&sink_name) {
            match state.original_default.as_ref().and_then(|name| sinks.iter().find(|sink| &sink.name == name)) {
                Some(original) => match pulse.set_default_sink(&original.name) {
                    Ok(()) => eprintln!("Default output switched back to {}", original.description),
                    Err(err) => {
                        problems.push(format!("the default output ({err})"));
                        sink_left = true;
                    }
                },
                None => sink_left = true,
            }
        }
        let mut moved_back = 0;
        for input in pulse.sink_inputs()?.into_iter().filter(|input| input.sink == ours.index) {
            let Some((_, from)) = state.moved.iter().find(|(index, _)| *index == input.index) else { continue };
            let Some(target) = sinks.iter().find(|sink| &sink.name == from) else { continue };
            match pulse.move_sink_input(input.index, target.index) {
                Ok(()) => moved_back += 1,
                Err(err) => problems.push(format!("a stream to {} ({err})", target.description)),
            }
        }
        if moved_back > 0 {
            eprintln!("Moved {moved_back} stream(s) back to their outputs");
        }
    }
    for module in &modules {
        pulse.unload_module(module.index)?;
    }
    if modules.iter().any(|m| m.argument.contains(&format!("sink_name={sink_name} "))) {
        eprintln!("Removed the \"{DISPLAY_NAME}\" output");
    }
    if modules.iter().any(|m| m.name == "module-remap-source") {
        eprintln!("Removed the \"{MIC_NAME}\"");
    }
    if (sink_left || source_left) && !modules.is_empty() {
        settle_defaults(pulse, sink_left, source_left);
    }
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(problems),
    }
}

/// After removing our devices while they were the defaults, with nothing to switch back to (or
/// switching back failed): choose the device the server fell back to, so its remembered choice
/// no longer names a device that's gone.
fn settle_defaults(pulse: &Pulse, sink: bool, source: bool) {
    // The server picks the fallback a moment after the device goes.
    std::thread::sleep(Duration::from_millis(200));
    let Ok(info) = pulse.server_info() else { return };
    if sink && let Some(fallback) = info.default_sink {
        let _ = pulse.set_default_sink(&fallback);
    }
    if source && let Some(fallback) = info.default_source {
        let _ = pulse.set_default_source(&fallback);
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
        state.mic = true;
        state.original_default_source = Some("alsa_input.usb-mic".into());
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
        assert!(state.owns(&module("module-null-sink", format!("sink_name={} channels=1", state.mic_feed_name()))));
        assert!(state.owns(&module("module-remap-source", format!("master=x source_name={}", state.mic_source_name()))));
        assert!(!state.owns(&module("module-remap-source", "source_name=rtp_audio_mic_src_0000000000000000".into())));
    }
}
