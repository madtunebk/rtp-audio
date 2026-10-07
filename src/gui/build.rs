use std::path::Path;
use std::process::Command;

fn main() {
    // The page (web/, Svelte) is built here by npm and embedded into the window, again whenever its
    // sources change: no step to remember before cargo.
    for path in ["web/src", "web/index.html", "web/package.json", "web/package-lock.json", "web/vite.config.js", "web/svelte.config.js"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let web = Path::new("web");
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    if !web.join("node_modules").exists() {
        run(Command::new(npm).arg("ci").current_dir(web));
    }
    run(Command::new(npm).args(["run", "build"]).current_dir(web));
    tauri_build::build();
}

fn run(command: &mut Command) {
    // npm's output goes to stderr: a build script's stdout is read by cargo.
    match command.stdout(std::io::stderr()).status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{command:?} failed ({status})"),
        Err(err) => panic!(
            "can't run {command:?} ({err}): building the window's page needs Node.js 20 or newer with npm \
             (https://nodejs.org). For rtp-audio without the window: cargo build --release --no-default-features"
        ),
    }
}
