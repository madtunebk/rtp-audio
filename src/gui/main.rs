#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The window of `rtp-audio --gui` as a program of its own, for Linux: rtp-audio starts it with
//! RTP_AUDIO_BIN set to its own path, so the window runs that very rtp-audio for its commands.

fn main() {
    // Asked by rtp-audio before opening the window: getting here means the system libraries the
    // window needs (WebKitGTK on Linux) were found and loaded.
    if std::env::args().nth(1).as_deref() == Some("--check-runtime") {
        return;
    }
    rtp_audio_gui::run(None);
}
