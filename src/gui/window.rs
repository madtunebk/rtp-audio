//! A window over the `rtp-audio` command (`rtp-audio --gui`). It lists outputs and sources with
//! `--json`, runs one receiver or sender at a time, and passes its lines to the page as events.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

/// The running rtp-audio, if any, and a number that tells its events from an earlier one's.
#[derive(Default)]
struct Running {
    child: Mutex<Option<(u64, Child)>>,
    next: Mutex<u64>,
}

#[derive(Clone, Serialize)]
struct Line {
    run: u64,
    /// "out" or "err".
    stream: &'static str,
    line: String,
}

#[derive(Clone, Serialize)]
struct Exit {
    run: u64,
    code: Option<i32>,
}

/// The rtp-audio to run: the one this window is part of, else the one that started it
/// ($RTP_AUDIO_BIN), else the one next to this program; never another one found in PATH, which
/// could be another version.
fn binary() -> PathBuf {
    if let Some(path) = CLI.get() {
        return path.clone();
    }
    if let Some(path) = std::env::var_os("RTP_AUDIO_BIN") {
        return path.into();
    }
    let name = if cfg!(windows) { "rtp-audio.exe" } else { "rtp-audio" };
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent().map_or_else(|| name.into(), |dir| dir.join(name))
}

fn rtp_audio() -> Command {
    let mut command = Command::new(binary());
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up for each call.
        command.creation_flags(0x0800_0000);
    }
    command
}

/// Runs a short rtp-audio command and returns what it printed, or its error.
fn output(args: &[&str]) -> Result<String, String> {
    let out = rtp_audio()
        .args(args)
        .output()
        .map_err(|err| format!("can't run {}: {err}", binary().display()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.trim().trim_start_matches("rtp-audio: ").to_string())
    }
}

/// "rtp-audio 0.4.0", or why it can't be run.
#[tauri::command]
fn backend_version() -> Result<String, String> {
    output(&["version"]).map(|v| v.trim().to_string())
}

/// `rtp-audio devices --json`, as JSON text.
#[tauri::command]
fn list_outputs() -> Result<String, String> {
    output(&["devices", "--json"])
}

/// `rtp-audio sources --json`, as JSON text (Linux only).
#[tauri::command]
fn list_sources() -> Result<String, String> {
    output(&["sources", "--json"])
}

/// Starts `rtp-audio ARGS` (a receiver or a sender); its lines arrive as "rtp-line" events and
/// its end as "rtp-exit". Returns the run's number.
#[tauri::command]
fn start(app: AppHandle, running: State<Running>, args: Vec<String>) -> Result<u64, String> {
    // Only what the page offers: a receiver or a sender, never `service` or other commands.
    if !matches!(args.first().map(String::as_str), Some("receive" | "send")) || args.iter().any(|a| a == "--stdin") {
        return Err("only receive and send can be started".into());
    }
    let mut slot = running.child.lock().unwrap();
    if let Some((_, child)) = slot.as_mut()
        && child.try_wait().ok().flatten().is_none()
    {
        return Err("rtp-audio is already running; stop it first".into());
    }
    let mut command = rtp_audio();
    command.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
    // If this window goes away without stopping it (killed, crashed), rtp-audio still stops: on
    // Linux it gets the Ctrl+C a Stop would send (so a sender switches the sound back), on Windows
    // the system ends it with this process.
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        let window = libc::getpid();
        command.pre_exec(move || {
            // Ctrl+C must stop it even when this window was started with SIGINT ignored (as a
            // background job is), since ignored signals are inherited.
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT);
            // The window ended before that took effect: stop now.
            if libc::getppid() != window {
                libc::raise(libc::SIGINT);
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|err| format!("can't run {}: {err}", binary().display()))?;
    #[cfg(windows)]
    end_with_this_process(&child);
    let run = {
        let mut next = running.next.lock().unwrap();
        *next += 1;
        *next
    };
    let stderr = child.stderr.take().map(|err| forward(app.clone(), run, "err", err));
    let stdout = child.stdout.take().unwrap();
    *slot = Some((run, child));
    std::thread::spawn(move || {
        forward(app.clone(), run, "out", stdout).join().ok();
        stderr.map(|thread| thread.join().ok());
        // Both pipes closed: the process ended (or is about to).
        let state = app.state::<Running>();
        let code = match state.child.lock().unwrap().take_if(|(id, _)| *id == run) {
            Some((_, mut child)) => child.wait().ok().and_then(|status| status.code()),
            None => None,
        };
        app.emit("rtp-exit", Exit { run, code }).ok();
    });
    Ok(run)
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

/// Sends each line from `pipe` to the page. Status lines rewrite themselves with \r at a
/// terminal; here each piece between \r and \n is a line of its own.
fn forward(app: AppHandle, run: u64, stream: &'static str, pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buffer = Vec::new();
        while reader.read_until(b'\n', &mut buffer).is_ok_and(|n| n > 0) {
            for piece in String::from_utf8_lossy(&buffer).split('\r') {
                let line = piece.trim_end();
                if !line.trim().is_empty() {
                    app.emit("rtp-line", Line { run, stream, line: line.to_string() }).ok();
                }
            }
            buffer.clear();
        }
    })
}

/// Asks the running rtp-audio to stop the way Ctrl+C does, so a sender switches the sound back;
/// kills it if it hasn't ended after 5 s.
#[tauri::command]
fn stop(app: AppHandle, running: State<Running>) {
    let Some(run) = interrupt(&running) else { return };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        if let Some((id, child)) = app.state::<Running>().child.lock().unwrap().as_mut()
            && *id == run
        {
            child.kill().ok();
        }
    });
}

/// Ctrl+C for the running rtp-audio (on Windows, which can't send one to a windowless process,
/// a kill: only the receiver runs there, and it has nothing to undo). Returns its run number.
fn interrupt(running: &Running) -> Option<u64> {
    let mut slot = running.child.lock().unwrap();
    let (run, child) = slot.as_mut()?;
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    #[cfg(not(unix))]
    child.kill().ok();
    Some(*run)
}

/// The rtp-audio this window is part of (Windows, macOS), given to `run`.
static CLI: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Opens the window; `cli` is the rtp-audio it runs for its commands, when the window is part of
/// it. Returns when the window closes.
pub fn run(cli: Option<PathBuf>) {
    if let Some(path) = cli {
        CLI.set(path).ok();
    }
    let app = tauri::Builder::default()
        .manage(Running::default())
        .invoke_handler(tauri::generate_handler![backend_version, list_outputs, list_sources, start, stop])
        .build(tauri::generate_context!())
        .expect("could not start RTP Audio Studio");
    app.run(|app, event| {
        // Closing the window stops rtp-audio first, and waits (a little) for a sender to switch
        // the sound back.
        if let RunEvent::Exit = event {
            let running = app.state::<Running>();
            if interrupt(&running).is_some() {
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    let mut slot = running.child.lock().unwrap();
                    match slot.as_mut().map(|(_, child)| child.try_wait()) {
                        Some(Ok(None)) => {}
                        _ => return,
                    }
                    drop(slot);
                    std::thread::sleep(Duration::from_millis(50));
                }
                if let Some((_, child)) = running.child.lock().unwrap().as_mut() {
                    child.kill().ok();
                }
            }
        }
    });
}
