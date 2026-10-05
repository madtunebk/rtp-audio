//! `rtp-audio service`: run the sender as a systemd user service, so the sound starts with the
//! desktop instead of from an SSH session. Only this part of rtp-audio runs `systemctl`.

use std::error::Error;
use std::path::PathBuf;
use std::process::Command;

pub const UNIT: &str = "rtp-audio.service";

pub fn run(action: &str, send_args: &[String]) -> Result<(), Box<dyn Error>> {
    match action {
        "install" => install(send_args),
        "uninstall" => uninstall(),
        "status" => systemctl(&["status", "--no-pager", UNIT]).map(|_| ()),
        "start" | "stop" | "restart" => {
            systemctl(&[action, UNIT])?;
            eprintln!("rtp-audio service: {action} done");
            Ok(())
        }
        _ => Err(format!("unknown service action '{action}': use install, uninstall, status, start, stop or restart").into()),
    }
}

fn unit_path() -> Result<PathBuf, Box<dyn Error>> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("cannot find your home directory (HOME is not set)")?;
    Ok(config.join("systemd/user").join(UNIT))
}

/// Quote one argument for an ExecStart= line.
fn quote(arg: &str) -> String {
    let escaped = arg.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    format!("\"{escaped}\"")
}

fn unit_file(exe: &str, send_args: &[String]) -> String {
    let command = std::iter::once(quote(exe))
        .chain(std::iter::once(quote("send")))
        .chain(send_args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# Written by `rtp-audio service install`; remove with `rtp-audio service uninstall`.\n\
         [Unit]\n\
         Description=rtp-audio: send this desktop's sound\n\
         After=pulseaudio.service pipewire-pulse.service\n\
         \n\
         [Service]\n\
         ExecStart={command}\n\
         # The sound server may not be ready yet right after boot: try again.\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn install(send_args: &[String]) -> Result<(), Box<dyn Error>> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().ok_or("the path to rtp-audio is not valid UTF-8")?;
    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit_file(exe, send_args)).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    eprintln!("Wrote {}", path.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])?;
    eprintln!("rtp-audio now runs as a service: `rtp-audio service status` shows how it is doing.");
    eprintln!("It runs {exe}: keep that file where it is, or install again after moving it.");
    if !lingers() {
        eprintln!(
            "To start it at boot, without anyone logged in: sudo loginctl enable-linger {}",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        );
    }
    Ok(())
}

fn uninstall() -> Result<(), Box<dyn Error>> {
    let path = unit_path()?;
    if !path.exists() {
        return Err(format!("no rtp-audio service installed ({} does not exist)", path.display()).into());
    }
    // Stopping lets the sender switch the sound back before it exits.
    systemctl(&["disable", "--now", UNIT])?;
    std::fs::remove_file(&path)?;
    systemctl(&["daemon-reload"])?;
    eprintln!("Removed the rtp-audio service");
    Ok(())
}

/// Is the user's service manager kept running when they are not logged in?
fn lingers() -> bool {
    std::env::var("USER").is_ok_and(|user| std::path::Path::new("/var/lib/systemd/linger").join(user).exists())
}

/// The process running the service, if the given PID belongs to it.
pub fn is_service_process(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|cgroup| cgroup.contains(UNIT))
}

fn systemctl(args: &[&str]) -> Result<std::process::ExitStatus, Box<dyn Error>> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .map_err(|err| format!("cannot run systemctl ({err}): this needs systemd"))?;
    // `status` reports a stopped service with a non-zero code; that's an answer, not an error.
    if !status.success() && args[0] != "status" {
        return Err(format!("systemctl --user {} failed", args.join(" ")).into());
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::{quote, unit_file};

    #[test]
    fn quotes_arguments_for_systemd() {
        assert_eq!(quote("--web"), "\"--web\"");
        assert_eq!(quote(r#"a "b" 100% $HOME\x"#), r#""a \"b\" 100%% $$HOME\\x""#);
        let unit = unit_file("/opt/rtp audio/rtp-audio", &["--web".into(), "46080".into()]);
        assert!(unit.contains(r#"ExecStart="/opt/rtp audio/rtp-audio" "send" "--web" "46080""#));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
