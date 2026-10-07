use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Windows: rtp-audio.exe's icon and manifest. The manifest asks for Common Controls 6, which the
    // window imports from: without it Windows won't start the exe at all.
    println!("cargo:rerun-if-changed=windows");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        embed_resource::compile("windows/rtp-audio.rc", embed_resource::NONE).manifest_required().expect("Windows resources");
    }
    // Linux with the window (the default): the window is a program of its own, so that rtp-audio
    // itself never links WebKitGTK. Build it here and carry it (src/launch.rs).
    if target_os == "linux" && env::var_os("CARGO_FEATURE_GUI").is_some() {
        let window = window_program();
        println!("cargo:rustc-env=RTP_AUDIO_GUI_BIN={}", window.display());
    }
}

/// Builds src/gui (the window program) for this target and profile, in a target folder of its own,
/// and returns its path; or the one given in RTP_AUDIO_GUI_BIN.
fn window_program() -> PathBuf {
    println!("cargo:rerun-if-env-changed=RTP_AUDIO_GUI_BIN");
    if let Some(path) = env::var_os("RTP_AUDIO_GUI_BIN") {
        return path.into();
    }
    for path in ["src/gui/window.rs", "src/gui/main.rs", "src/gui/build.rs", "src/gui/Cargo.toml", "src/gui/Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in ["src/gui/tauri.conf.json", "src/gui/capabilities", "src/gui/icons", "src/gui/web/src", "src/gui/web/index.html"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let var = |name: &str| env::var(name).unwrap_or_default();
    // OUT_DIR is <target>[/<triple>]/<profile>/build/rtp-audio-<hash>/out.
    let out = PathBuf::from(var("OUT_DIR"));
    let target_dir = out.ancestors().nth(4).expect("OUT_DIR inside a target folder").join("gui-program");
    let (profile, target) = (var("PROFILE"), var("TARGET"));
    let mut cargo = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo.args(["build", "--manifest-path", "src/gui/Cargo.toml", "--target", &target, "--target-dir"]).arg(&target_dir);
    if profile == "release" {
        cargo.arg("--release");
    }
    // What cargo told this build script is about rtp-audio, not about the window's own build.
    for (name, _) in env::vars() {
        let ours = name.starts_with("CARGO_FEATURE_") || name.starts_with("CARGO_CFG_") || name.starts_with("CARGO_PKG_") || name.starts_with("CARGO_MANIFEST_");
        if ours || ["OUT_DIR", "TARGET", "HOST", "PROFILE", "OPT_LEVEL", "DEBUG", "NUM_JOBS", "CARGO_TARGET_DIR"].contains(&name.as_str()) {
            cargo.env_remove(name);
        }
    }
    // Its progress goes to stderr: this script's stdout is read by cargo.
    let status = cargo.stdout(std::io::stderr()).status().expect("cargo, to build the window program");
    assert!(status.success(), "building the window program (src/gui) failed; for rtp-audio without the window: --no-default-features");
    let name = if target.contains("windows") { "rtp-audio-gui.exe" } else { "rtp-audio-gui" };
    target_dir.join(&target).join(&profile).join(name)
}
