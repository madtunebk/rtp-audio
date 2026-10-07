//! `rtp-audio --gui`: opens the window (src/gui), built with `--features gui`.
//!
//! On Windows and macOS the window is part of rtp-audio: their web views (WebView2, WKWebView) come
//! with the system. On Linux it is a program of its own, so that rtp-audio itself never needs
//! WebKitGTK (a server running `find` has none): rtp-audio carries it and writes it to the user's
//! cache the first time. Without the feature, rtp-audio looks for `rtp-audio-gui` next to itself.

/// Opens the window; returns when it closes.
#[cfg(all(feature = "gui", not(target_os = "linux")))]
pub fn gui() -> Result<(), Box<dyn std::error::Error>> {
    let me = std::env::current_exe()?;
    #[cfg(windows)]
    hide_own_console();
    // The window runs this very rtp-audio for its commands: the same version, whatever is in PATH.
    rtp_audio_gui::run(Some(me));
    Ok(())
}

/// Started from Explorer for the window, rtp-audio.exe got a console of its own: let it go, so
/// only the window shows. A console it shares with others (cmd, a terminal) stays.
#[cfg(all(feature = "gui", windows))]
fn hide_own_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut processes = [0u32; 2];
    if unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 2) } == 1 {
        unsafe { FreeConsole() };
    }
}

#[cfg(not(all(feature = "gui", not(target_os = "linux"))))]
pub use helper::gui;

/// The window as a program of its own (Linux, or any system built without the feature).
#[cfg(not(all(feature = "gui", not(target_os = "linux"))))]
mod helper {
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const NAME: &str = if cfg!(windows) { "rtp-audio-gui.exe" } else { "rtp-audio-gui" };

    #[cfg(all(feature = "gui", target_os = "linux"))]
    static EMBEDDED: &[u8] = include_bytes!(env!("RTP_AUDIO_GUI_BIN", "build.rs sets RTP_AUDIO_GUI_BIN: the window program it built"));

    /// Opens the window; returns when it closes.
    pub fn gui() -> Result<(), Box<dyn Error>> {
        let me = std::env::current_exe()?;
        let window = window_program(&me)?;
        check_runtime(&window)?;
        let mut command = Command::new(&window);
        // The window runs this very rtp-audio for its commands: the same version, whatever is in PATH.
        command.env("RTP_AUDIO_BIN", &me);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = command.exec();
            Err(format!("can't start {}: {err}", window.display()).into())
        }
        #[cfg(not(unix))]
        {
            // Closing this console also closes the window (and so what it runs).
            let mut child = command.spawn()?;
            #[cfg(windows)]
            end_with_this_process(&child);
            let status = child.wait()?;
            if status.success() { Ok(()) } else { Err(format!("the window ended with {status}").into()) }
        }
    }

    /// Ties `child` to this process: Windows ends it when this process ends, however that happens
    /// (closed, killed, crashed), through a job object that kills its processes when its last handle
    /// closes. The handle is kept for the life of the process on purpose.
    #[cfg(windows)]
    fn end_with_this_process(child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        use std::sync::OnceLock;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        static JOB: OnceLock<usize> = OnceLock::new();
        let job = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size);
            job as usize
        });
        if job != 0 {
            unsafe { AssignProcessToJobObject(job as _, child.as_raw_handle() as _) };
        }
    }

    /// Starts the window program only far enough to load its libraries: a missing WebKitGTK makes the
    /// system's loader fail before it runs, which is told here clearly instead of after the exec.
    fn check_runtime(window: &Path) -> Result<(), Box<dyn Error>> {
        let out = Command::new(window).arg("--check-runtime").output().map_err(|err| format!("can't start {}: {err}", window.display()))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim().to_string();
        if cfg!(target_os = "linux") && stderr.contains("error while loading shared libraries") {
            return Err(format!(
                "the window needs WebKitGTK 4.1, which isn't installed ({detail}).\n\
                 Debian/Ubuntu: sudo apt install libwebkit2gtk-4.1-0\n\
                 Fedora: sudo dnf install webkit2gtk4.1\n\
                 Arch: sudo pacman -S webkit2gtk-4.1\n\
                 Everything else in rtp-audio works without it."
            )
            .into());
        }
        Err(format!("the window can't start ({}): {detail}", out.status).into())
    }

    /// The window program: the embedded one, written to the cache once per build; else the one next to
    /// this rtp-audio.
    fn window_program(me: &Path) -> Result<PathBuf, Box<dyn Error>> {
        #[cfg(all(feature = "gui", target_os = "linux"))]
        {
            let _ = me;
            unpack()
        }
        #[cfg(not(all(feature = "gui", target_os = "linux")))]
        {
            let beside = me.parent().map(|dir| dir.join(NAME)).filter(|path| path.is_file());
            beside.ok_or_else(|| {
                format!(
                    "this rtp-audio was built without the window (--no-default-features), and {NAME} \
                     isn't next to it ({})",
                    me.display()
                )
                .into()
            })
        }
    }

    /// Writes the embedded window program to the cache, under a folder named after its contents (not
    /// just the version: two builds of one version differ), private to the user, atomically.
    #[cfg(all(feature = "gui", target_os = "linux"))]
    fn unpack() -> Result<PathBuf, Box<dyn Error>> {
        let dir = cache_dir()?.join(format!("gui-{}-{:016x}", env!("CARGO_PKG_VERSION"), fnv1a(EMBEDDED)));
        let path = dir.join(NAME);
        if std::fs::read(&path).is_ok_and(|on_disk| on_disk == EMBEDDED) {
            return Ok(path);
        }
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let temporary = dir.join(format!(".{NAME}.{}", std::process::id()));
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o700);
            }
            use std::io::Write;
            let mut file = options.open(&temporary)?;
            file.write_all(EMBEDDED)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })?;
        Ok(path)
    }

    /// The user's cache folder for rtp-audio.
    #[cfg(all(feature = "gui", target_os = "linux"))]
    fn cache_dir() -> Result<PathBuf, Box<dyn Error>> {
        let var = |name| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        let base = if cfg!(windows) {
            var("LOCALAPPDATA")
        } else if cfg!(target_os = "macos") {
            var("HOME").map(|home| home.join("Library/Caches"))
        } else {
            var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))
        };
        Ok(base.ok_or("no cache folder (HOME isn't set)")?.join("rtp-audio"))
    }

    /// A 64-bit FNV-1a hash: tells two builds of the window apart, not a security check (the bytes are
    /// compared in full before the cached copy is used).
    #[cfg(all(feature = "gui", target_os = "linux"))]
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| (hash ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
    }
}
