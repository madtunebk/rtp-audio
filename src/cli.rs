//! Command line parsing and validation.

use std::net::{SocketAddr, ToSocketAddrs};

use crate::receiver;
use crate::net::secure::Key;
use crate::net::transport::Encoding;

pub const USAGE: &str = "\
usage:
  rtp-audio [receive] [--port 46000] [--latency 60] [--device NAME[,NAME…]] [--volume 100]
                     [--group 239.255.46.1] [--no-discovery] [--json]
      play RTP audio (16-bit PCM) arriving on a UDP port; --latency is the buffer in ms,
      --device an output from `rtp-audio devices` (several, separated by commas, play the same
      sound at once: --device 'HDMI, Headphones'; each can end with a delay and a volume of its
      own: 'HDMI+80ms, Bose@50%'), --volume in percent, --group also listens to a multicast
      group, --json prints the status as a JSON line every half second (for programs)
  rtp-audio [receive] ws://HOST:PORT [--latency 60] [--device NAME[,NAME…]] [--volume 100] [--json]
      play the stream of `rtp-audio send --web` over TCP instead: nothing is lost, and it goes
      through an SSH tunnel (ssh -L 46080:localhost:46080 SERVER, then ws://localhost:46080)
  rtp-audio devices [--json]
      list the sound outputs the receiver can play on
  rtp-audio find [--port 46000]
      list the receivers on this local network (they answer unless --no-discovery)
  rtp-audio keygen
      make a key for --key: then sender and receiver need the same key, e.g.
      rtp-audio send 192.168.1.20:46000 --opus --key-file ~/.rtp-audio.key
      rtp-audio --key-file rtp-audio.key
  rtp-audio sources [--json]
      list this computer's sound sources (Linux)
  rtp-audio send HOST:PORT [HOST:PORT…]
      send all of this computer's sound (Linux): adds an \"RTP Audio\" output, makes it the
      default and moves playing apps to it; Ctrl+C switches everything back. HOST:PORT can be
      several receivers, a multicast group (e.g. 239.255.46.1:46000), or `auto`: the one
      receiver `rtp-audio find` sees
  rtp-audio send HOST:PORT --source NAME_OR_ID
      send one source from `rtp-audio sources` instead, without changing any output
  rtp-audio send HOST:PORT [--opus] [--key KEY | --key-file FILE]
      --opus: about 128 kbit/s instead of 1.5 Mbit/s (needs an rtp-audio receiver);
      --key/--key-file: encrypt (the receiver needs the same key)
  rtp-audio send --web 127.0.0.1:46080 [--mic] [HOST:PORT] [--source NAME_OR_ID]
      (also) serve the sound to web browsers as Opus over a WebSocket, with a noVNC player;
      put it behind NGINX for HTTPS and a login (see docs/web.md). --mic: browsers can also
      send their microphone, which apps here hear as \"RTP Audio Microphone\"
  rtp-audio service install SEND_OPTIONS
      run `rtp-audio send SEND_OPTIONS` as a user service that starts with the desktop,
      e.g. rtp-audio service install --web 46080
  rtp-audio service uninstall | status | start | stop | restart
  rtp-audio version
      print the version (also --version)
  rtp-audio gui
      open the window (also --gui), when rtp-audio was built with it
  rtp-audio send HOST:PORT --stdin [--rate 48000] [--channels 2]
      send raw big-endian 16-bit PCM read from stdin

HOST is the receiving computer, e.g. rtp-audio send 192.168.1.20:46000";

pub enum Command {
    Help,
    /// Open the window.
    Gui,
    Version,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Service { action: String, send_args: Vec<String> },
    Receive(receiver::Options),
    Sources {
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        json: bool,
    },
    Devices { json: bool },
    Keygen,
    Find(u16),
    Send(SendOptions),
}

pub struct SendOptions {
    pub destinations: Vec<SocketAddr>,
    /// `auto`: use the receiver answering on this port.
    pub auto: Option<u16>,
    pub encoding: Encoding,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub web: Option<SocketAddr>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub mic: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub source: Option<String>,
    pub stdin: bool,
    pub rate: u32,
    pub channels: usize,
}

pub fn parse(args: Vec<String>) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("service") => return parse_service(args),
        Some("version") => return Ok(Command::Version),
        Some("gui" | "--gui") if args.len() == 1 => return Ok(Command::Gui),
        _ => {}
    }
    let mut command: Option<String> = None;
    let mut positional = Vec::new();
    let (mut port, mut latency_ms, mut rate, mut channels) = (None, None, None, None);
    let (mut source, mut stdin, mut web) = (None, false, None);
    let (mut devices, mut volume): (Vec<String>, _) = (Vec::new(), None);
    let (mut opus, mut key, mut key_file, mut mic) = (false, None, None, false);
    let (mut group, mut discovery, mut json) = (None, true, false);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" | "help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--port" => port = Some(number::<u16>(&arg, &value()?)?),
            "--latency" => latency_ms = Some(number::<u32>(&arg, &value()?)?),
            "--rate" => rate = Some(number::<u32>(&arg, &value()?)?),
            "--channels" => channels = Some(number::<usize>(&arg, &value()?)?),
            "-s" | "--source" => source = Some(value()?),
            "--stdin" => stdin = true,
            "--web" => web = Some(value()?),
            // Several outputs: one --device with a comma-separated list.
            "--device" if !devices.is_empty() => {
                return Err("--device is given once: list several outputs separated by commas, e.g. --device \"HDMI, Headphones\"".into())
            }
            "--device" => devices.push(value()?),
            "--opus" => opus = true,
            "--mic" => mic = true,
            "--group" => group = Some(value()?),
            "--no-discovery" => discovery = false,
            "--json" => json = true,
            "--key" => key = Some(value()?),
            "--key-file" => key_file = Some(value()?),
            "--volume" => volume = Some(number::<f32>(&arg, &value()?)?),
            _ if arg.starts_with('-') => return Err(format!("unknown option '{arg}'\n\n{USAGE}")),
            "receive" | "send" | "sources" | "devices" | "keygen" | "find" if command.is_none() => command = Some(arg),
            _ => positional.push(arg),
        }
    }
    if channels.is_some_and(|c| !(1..=2).contains(&c)) {
        return Err("--channels must be 1 or 2".into());
    }
    let only = |allowed: &[&str]| -> Result<(), String> {
        let given = [
            ("--port", port.is_some()),
            ("--latency", latency_ms.is_some()),
            ("--rate", rate.is_some()),
            ("--channels", channels.is_some()),
            ("--source", source.is_some()),
            ("--stdin", stdin),
            ("--web", web.is_some()),
            ("--device", !devices.is_empty()),
            ("--volume", volume.is_some()),
            ("--opus", opus),
            ("--key", key.is_some()),
            ("--key-file", key_file.is_some()),
            ("--mic", mic),
            ("--group", group.is_some()),
            ("--no-discovery", !discovery),
            ("--json", json),
        ];
        match given.iter().find(|(name, set)| *set && !allowed.contains(name)) {
            Some((name, _)) => Err(format!("{name} can't be used here\n\n{USAGE}")),
            None => Ok(()),
        }
    };
    let extra = |what: &str| format!("unexpected argument '{what}'\n\n{USAGE}");

    match command.as_deref() {
        Some("sources") => {
            only(&["--json"])?;
            match positional.first() {
                Some(arg) => Err(extra(arg)),
                None => Ok(Command::Sources { json }),
            }
        }
        Some("find") => {
            only(&["--port"])?;
            match positional.first() {
                Some(arg) => Err(extra(arg)),
                None => Ok(Command::Find(nonzero("--port", port.unwrap_or(46000))?)),
            }
        }
        Some("keygen") => {
            only(&[])?;
            match positional.first() {
                Some(arg) => Err(extra(arg)),
                None => Ok(Command::Keygen),
            }
        }
        Some("devices") => {
            only(&["--json"])?;
            match positional.first() {
                Some(arg) => Err(extra(arg)),
                None => Ok(Command::Devices { json }),
            }
        }
        Some("send") => {
            let mut auto = None;
            let mut destinations = Vec::new();
            for arg in &positional {
                if arg == "auto" || arg.starts_with("auto:") {
                    let port = arg.strip_prefix("auto:").map_or(Ok(46000), |p| number::<u16>("auto", p))?;
                    auto = Some(nonzero("auto port", port)?);
                } else {
                    destinations.push(parse_destination(arg)?);
                }
            }
            let has_destination = !destinations.is_empty() || auto.is_some();
            if !has_destination && web.is_none() {
                return Err(format!(
                    "send needs the receiver's address (e.g. rtp-audio send 192.168.1.20:46000) or --web\n\n{USAGE}"
                ));
            }
            if stdin {
                only(&["--stdin", "--rate", "--channels", "--opus", "--key", "--key-file"])?;
                if destinations.len() + usize::from(auto.is_some()) != 1 {
                    return Err("--stdin sends to exactly one receiver".into());
                }
            } else {
                only(&["--source", "--web", "--opus", "--key", "--key-file", "--mic"])?;
                if mic && web.is_none() {
                    return Err("--mic takes the microphone from browsers: it needs --web".into());
                }
            }
            if !has_destination && (opus || key.is_some() || key_file.is_some()) {
                return Err("--opus and --key are for the UDP stream to a receiver; the browser mode is \
                    already Opus, and NGINX's HTTPS protects it"
                    .into());
            }
            if source.as_deref().is_some_and(|s| s.trim().is_empty()) {
                return Err("--source needs a source name or ID (see rtp-audio sources)".into());
            }
            Ok(Command::Send(SendOptions {
                destinations,
                auto,
                encoding: Encoding { opus, key: read_key(key, key_file)? },
                web: web.as_deref().map(parse_web).transpose()?,
                mic,
                source,
                stdin,
                rate: within("--rate", rate.unwrap_or(48_000), RATES, "Hz")?,
                channels: channels.unwrap_or(2),
            }))
        }
        _ => {
            only(&["--port", "--latency", "--rate", "--channels", "--device", "--volume", "--key", "--key-file", "--group", "--no-discovery", "--json"])?;
            let mut url = None;
            for arg in &positional {
                if url.is_none() && (arg.starts_with("ws://") || arg.starts_with("wss://")) {
                    url = Some(arg.clone());
                } else {
                    return Err(extra(arg));
                }
            }
            if url.is_some() {
                // The stream is Opus at 48 kHz, protected by the tunnel (or proxy) it goes through.
                only(&["--latency", "--channels", "--device", "--volume", "--json"])?;
            }
            let group = match group {
                None => None,
                Some(text) => match text.parse::<std::net::Ipv4Addr>() {
                    Ok(ip) if ip.is_multicast() => Some(ip),
                    _ => return Err(format!("--group: '{text}' is not a multicast address (224.0.0.0 to 239.255.255.255)")),
                },
            };
            let volume = volume.unwrap_or(100.0);
            if !(0.0..=400.0).contains(&volume) {
                return Err("--volume must be between 0 and 400 (percent)".into());
            }
            Ok(Command::Receive(receiver::Options {
                port: nonzero("--port", port.unwrap_or(46000))?,
                latency_ms: within("--latency", latency_ms.unwrap_or(60), LATENCIES, "ms")?,
                rate: within("--rate", rate.unwrap_or(48_000), RATES, "Hz")?,
                channels: channels.unwrap_or(2),
                devices,
                volume: volume / 100.0,
                key: read_key(key, key_file)?,
                group,
                discovery,
                url,
                json,
            }))
        }
    }
}

/// `service ACTION [send options]`; install checks the options like `send` would.
fn parse_service(args: Vec<String>) -> Result<Command, String> {
    let mut args = args.into_iter().skip(1);
    let action = args.next().ok_or(format!("service needs an action: install, uninstall, status, start, stop or restart\n\n{USAGE}"))?;
    let send_args: Vec<String> = args.collect();
    match action.as_str() {
        "-h" | "--help" => Ok(Command::Help),
        "install" => match parse(std::iter::once("send".to_string()).chain(send_args.iter().cloned()).collect())? {
            Command::Send(options) if !options.stdin => Ok(Command::Service { action, send_args }),
            Command::Send(_) => Err("the service can't read from --stdin".into()),
            _ => Ok(Command::Help),
        },
        "uninstall" | "status" | "start" | "stop" | "restart" if send_args.is_empty() => Ok(Command::Service { action, send_args }),
        "uninstall" | "status" | "start" | "stop" | "restart" => Err(format!("service {action} takes no options")),
        _ => Err(format!("unknown service action '{action}'\n\n{USAGE}")),
    }
}

fn number<T: std::str::FromStr>(option: &str, text: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("{option}: '{text}' is not a valid number"))
}

fn nonzero<T: Default + PartialEq>(option: &str, value: T) -> Result<T, String> {
    if value == T::default() { Err(format!("{option} must be above 0")) } else { Ok(value) }
}

/// Sample rates sound cards and Opus use: a smaller or larger one is a typo, and would make
/// empty or oversized packets.
pub const RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
/// The receiver's buffer: at least one 20 ms Opus packet, at most two seconds.
const LATENCIES: std::ops::RangeInclusive<u32> = 20..=2_000;

fn within(option: &str, value: u32, range: std::ops::RangeInclusive<u32>, unit: &str) -> Result<u32, String> {
    if range.contains(&value) {
        Ok(value)
    } else {
        Err(format!("{option} must be between {} and {} {unit}", range.start(), range.end()))
    }
}

fn read_key(key: Option<String>, key_file: Option<String>) -> Result<Option<Key>, String> {
    match (key, key_file) {
        (Some(_), Some(_)) => Err("use --key or --key-file, not both".into()),
        (Some(key), None) => Key::parse(&key).map(Some),
        (None, Some(path)) => Key::from_file(&path).map(Some),
        (None, None) => Ok(None),
    }
}

/// Where to serve browsers: `IP:PORT`, or just `PORT` for 127.0.0.1.
fn parse_web(text: &str) -> Result<SocketAddr, String> {
    if let Ok(port) = text.parse::<u16>() {
        return if port == 0 { Err("--web: port must be 1-65535".into()) } else { Ok(SocketAddr::from(([127, 0, 0, 1], port))) };
    }
    parse_destination(text).map_err(|err| format!("--web: {err}"))
}

/// `HOST:PORT`, `IP:PORT` or `[IPv6]:PORT`, resolved to an address.
pub fn parse_destination(text: &str) -> Result<SocketAddr, String> {
    let example = "e.g. 192.168.1.20:46000";
    let (host, port) = text
        .rsplit_once(':')
        .ok_or(format!("destination '{text}' has no port; use HOST:PORT, {example}"))?;
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.strip_suffix(']').ok_or(format!("destination '{text}': unclosed '['"))?,
        None if host.contains(':') => {
            return Err(format!("destination '{text}': write IPv6 addresses in brackets, e.g. [fd00::2]:46000"));
        }
        None => host,
    };
    if host.is_empty() {
        return Err(format!("destination '{text}' has no host; use HOST:PORT, {example}"));
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|&port| port != 0)
        .ok_or(format!("destination '{text}': '{port}' is not a valid port (1-65535)"))?;
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|err| format!("cannot resolve '{host}': {err}"))?
        .collect();
    // Prefer IPv4: it's what LANs and most VPNs use.
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or(addresses.first())
        .copied()
        .ok_or(format!("cannot resolve '{host}': no addresses"))
}

#[cfg(test)]
mod tests {
    use super::{Command, parse, parse_destination};

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn destinations() {
        assert_eq!(parse_destination("192.168.1.20:46000").unwrap().to_string(), "192.168.1.20:46000");
        assert_eq!(parse_destination("[::1]:46000").unwrap().to_string(), "[::1]:46000");
        assert_eq!(parse_destination("localhost:5").unwrap().port(), 5);
        for bad in ["192.168.1.20", "192.168.1.20:0", "192.168.1.20:70000", "192.168.1.20:x", ":46000", "::1:46000"] {
            assert!(parse_destination(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commands() {
        assert!(matches!(parse(args("")), Ok(Command::Receive(o)) if o.port == 46000));
        assert!(matches!(parse(args("--port 5000")), Ok(Command::Receive(o)) if o.port == 5000));
        assert!(matches!(parse(args("sources")), Ok(Command::Sources { json: false })));
        assert!(matches!(parse(args("send 10.0.0.2:46000 --help")), Ok(Command::Help)));
        let Ok(Command::Send(send)) = parse(args("send 10.0.0.2:46000 --source 48")) else { panic!() };
        assert_eq!((send.source.as_deref(), send.stdin), (Some("48"), false));
        let Ok(Command::Send(send)) = parse(args("send --web 46080")) else { panic!() };
        assert_eq!((send.destinations.len(), send.web.map(|a| a.to_string())), (0, Some("127.0.0.1:46080".into())));
        let Ok(Command::Send(send)) = parse(args("send 10.0.0.2:46000 --web 127.0.0.1:46080")) else { panic!() };
        assert!(send.destinations.len() == 1 && send.web.is_some());
        let Ok(Command::Send(send)) = parse(args("send 10.0.0.2:46000 239.255.46.1:46000 auto:46002")) else { panic!() };
        assert_eq!((send.destinations.len(), send.auto), (2, Some(46002)));
        assert!(matches!(parse(args("find --port 46001")), Ok(Command::Find(46001))));
        assert!(matches!(parse(args("--group 239.255.46.1 --no-discovery")), Ok(Command::Receive(o)) if o.group.is_some() && !o.discovery));
        assert!(matches!(parse(args("send --web 46080 --mic")), Ok(Command::Send(s)) if s.mic));
        assert!(matches!(parse(args("service install --web 46080")), Ok(Command::Service { action, send_args }) if action == "install" && send_args.len() == 2));
        assert!(matches!(parse(args("service status")), Ok(Command::Service { .. })));
        assert!(matches!(parse(args("devices")), Ok(Command::Devices { json: false })));
        assert!(matches!(parse(args("keygen")), Ok(Command::Keygen)));
        assert!(matches!(parse(args("version")), Ok(Command::Version)));
        assert!(matches!(parse(args("--version")), Ok(Command::Version)));
        assert!(matches!(parse(args("send 10.0.0.2:46000 -V")), Ok(Command::Version)));
        let key = crate::net::secure::generate().unwrap();
        let Ok(Command::Send(send)) = parse(args(&format!("send 10.0.0.2:46000 --opus --key {key}"))) else { panic!() };
        assert!(send.encoding.opus && send.encoding.key.is_some());
        assert!(matches!(parse(args(&format!("--key {key}"))), Ok(Command::Receive(o)) if o.key.is_some()));
        assert!(matches!(parse(args("--device Speakers --volume 50")), Ok(Command::Receive(o)) if o.volume == 0.5 && o.devices == ["Speakers"]));
        assert!(parse(args("--device HDMI --device Headphones")).is_err());
        assert!(matches!(parse(args("ws://localhost:46080")), Ok(Command::Receive(o)) if o.url.as_deref() == Some("ws://localhost:46080")));
        assert!(matches!(parse(args("receive ws://10.0.0.2:46080 --latency 150")), Ok(Command::Receive(o)) if o.latency_ms == 150 && o.url.is_some()));
        assert!(parse(args("ws://localhost:46080 --port 5000")).is_err());
        assert!(parse(args("ws://localhost:46080 --group 239.255.46.1")).is_err());
        assert!(parse(args("ws://a ws://b")).is_err());
        assert!(matches!(parse(args("send 10.0.0.2:46000 --stdin --rate 44100")), Ok(Command::Send(s)) if s.rate == 44100));
        // Rates and buffers outside what sound cards and packets allow are refused.
        assert!(parse(args("send 10.0.0.2:46000 --stdin --rate 1")).is_err());
        assert!(parse(args("send 10.0.0.2:46000 --stdin --rate 4000000000")).is_err());
        assert!(parse(args("--rate 1")).is_err());
        assert!(parse(args("--latency 1")).is_err());
        assert!(parse(args("--latency 100000")).is_err());
        assert!(matches!(parse(args("--latency 20 --rate 8000")), Ok(Command::Receive(o)) if o.latency_ms == 20 && o.rate == 8000));
    }

    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            "send",
            "send 10.0.0.2",
            "send 10.0.0.2:46000 extra",
            "send 10.0.0.2:46000 --rate 44100",
            "send 10.0.0.2:46000 --stdin --source 1",
            "send 10.0.0.2:46000 --source",
            "send --web",
            "send --web 0",
            "send --stdin --web 46080",
            "send 10.0.0.2:46000 --stdin --web 46080",
            "service",
            "service install",
            "service install 10.0.0.2:46000 --stdin",
            "service status --web 1",
            "service frobnicate",
            "--volume 500",
            "--volume -1",
            "devices --port 1",
            "send 10.0.0.2:46000 --volume 50",
            "send --web 46080 --opus",
            "--group 10.0.0.1",
            "send auto:0",
            "send 10.0.0.2:46000 10.0.0.3:46000 --stdin",
            "find --volume 3",
            "send 10.0.0.2:46000 --mic",
            "--mic",
            "send 10.0.0.2:46000 --key short",
            "send 10.0.0.2:46000 --key-file /nonexistent",
            "keygen extra",
            "--opus",
            "sources --port 1",
            "--port 0",
            "--port 99999",
            "--channels 3",
            "--frobnicate",
        ] {
            assert!(parse(args(bad)).is_err(), "{bad}");
        }
    }
}
