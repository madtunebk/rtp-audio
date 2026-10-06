//! Connection to the user's sound server through libpulse: PulseAudio itself, or PipeWire's
//! PulseAudio compatibility server (pipewire-pulse). Wraps the few requests we need as blocking
//! calls on top of libpulse's threaded main loop.

use std::cell::RefCell;
use std::error::Error;
use std::sync::{Arc, Mutex};

use libpulse_binding as pa;
use pa::callbacks::ListResult;
use pa::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pa::mainloop::threaded::Mainloop;
use pa::operation::{Operation, State as OperationState};
use pa::proplist::{Proplist, properties};

pub struct Pulse {
    // Dropped before the main loop it was created on.
    context: RefCell<Context>,
    mainloop: Box<RefCell<Mainloop>>,
}

pub struct ServerInfo {
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
}

pub struct Source {
    pub index: u32,
    pub name: String,
    pub description: String,
}

pub struct Sink {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub monitor_source: String,
    /// A sink property, see `routing::INSTANCE_PROPERTY`.
    pub instance: Option<String>,
}

pub struct SinkInput {
    pub index: u32,
    pub sink: u32,
}

pub struct Module {
    pub index: u32,
    pub name: String,
    pub argument: String,
}

/// Wakes the thread blocked in `Mainloop::wait`, from a libpulse callback.
#[derive(Clone, Copy)]
pub struct Wake(*mut Mainloop);

impl Wake {
    pub fn wake(self) {
        // SAFETY: callbacks run on the main loop thread while `Pulse` (which owns the main loop)
        // is alive; signalling is what libpulse's threaded main loop is designed for.
        unsafe { (*self.0).signal(false) }
    }
}

/// Where a request's callback leaves its result.
struct Reply<T> {
    slot: Arc<Mutex<Option<T>>>,
    wake: Wake,
}

impl<T> Reply<T> {
    fn send(&self, value: T) {
        *self.slot.lock().unwrap() = Some(value);
        self.wake.wake();
    }
}

impl Pulse {
    /// Connect to the current user's sound server. Never starts one.
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let mainloop = Box::new(RefCell::new(Mainloop::new().ok_or("could not create the libpulse main loop")?));
        let mut props = Proplist::new().ok_or("could not create a libpulse property list")?;
        let _ = props.set_str(properties::APPLICATION_NAME, "RTP Audio");
        let _ = props.set_str(properties::APPLICATION_ID, "rtp-audio");
        let context = Context::new_with_proplist(&*mainloop.borrow(), "RTP Audio", &props)
            .ok_or("could not create a libpulse context")?;
        let pulse = Self { context: RefCell::new(context), mainloop };
        let wake = pulse.wake();
        pulse.context.borrow_mut().set_state_callback(Some(Box::new(move || wake.wake())));

        let connected = pulse.locked(|mainloop, context| {
            mainloop.start()?;
            context.connect(None, ContextFlags::NOAUTOSPAWN, None)?;
            loop {
                match context.get_state() {
                    ContextState::Ready => return Ok(()),
                    ContextState::Failed | ContextState::Terminated => return Err(context.errno()),
                    _ => mainloop.wait(),
                }
            }
        });
        connected.map_err(|err| -> Box<dyn Error> { no_server_help(&format!("{err}")).into() })?;
        Ok(pulse)
    }

    pub fn wake(&self) -> Wake {
        Wake(self.mainloop.as_ptr())
    }

    /// Run `f` holding the main loop lock, as every use of the context and its streams must.
    pub fn locked<R>(&self, f: impl FnOnce(&mut Mainloop, &mut Context) -> R) -> R {
        let mut mainloop = self.mainloop.borrow_mut();
        mainloop.lock();
        let result = f(&mut mainloop, &mut self.context.borrow_mut());
        mainloop.unlock();
        result
    }

    pub fn is_connected(&self) -> bool {
        self.locked(|_, context| context.get_state() == ContextState::Ready)
    }

    /// The server's address, which tells servers apart (e.g. `/run/user/1000/pulse/native`).
    pub fn server(&self) -> String {
        self.locked(|_, context| context.get_server()).unwrap_or_default()
    }

    /// Start a request and wait for its callback to `send` a result.
    fn request<T, C: ?Sized>(
        &self,
        what: &str,
        start: impl FnOnce(&mut Context, Reply<T>) -> Operation<C>,
    ) -> Result<T, Box<dyn Error>> {
        let slot = Arc::new(Mutex::new(None));
        let reply = Reply { slot: Arc::clone(&slot), wake: self.wake() };
        let error = self.locked(|mainloop, context| {
            if context.get_state() != ContextState::Ready {
                return Some("the connection to the sound server was lost".to_string());
            }
            let operation = start(context, reply);
            while operation.get_state() == OperationState::Running {
                mainloop.wait();
            }
            match context.get_state() {
                ContextState::Ready => Some(format!("{}", context.errno())),
                _ => Some("the connection to the sound server was lost".to_string()),
            }
        });
        let value = slot.lock().unwrap().take();
        value.ok_or_else(|| format!("{what} failed: {}", error.unwrap_or_default()).into())
    }

    /// A request whose callback reports success.
    fn act<C: ?Sized>(
        &self,
        what: &str,
        start: impl FnOnce(&mut Context, Reply<bool>) -> Operation<C>,
    ) -> Result<(), Box<dyn Error>> {
        if self.request(what, start)? {
            Ok(())
        } else {
            Err(format!("{what} failed: {}", self.locked(|_, context| format!("{}", context.errno()))).into())
        }
    }

    pub fn server_info(&self) -> Result<ServerInfo, Box<dyn Error>> {
        self.request("reading the sound server's settings", |context, reply| {
            context.introspect().get_server_info(move |info| {
                reply.send(ServerInfo {
                    default_sink: info.default_sink_name.as_ref().map(|name| name.to_string()),
                    default_source: info.default_source_name.as_ref().map(|name| name.to_string()),
                })
            })
        })
    }

    pub fn sources(&self) -> Result<Vec<Source>, Box<dyn Error>> {
        self.request("listing sources", |context, reply| {
            let mut items = Vec::new();
            context.introspect().get_source_info_list(move |result| match result {
                ListResult::Item(info) => items.push(Source {
                    index: info.index,
                    name: text(&info.name),
                    description: text(&info.description),
                }),
                ListResult::End => reply.send(std::mem::take(&mut items)),
                ListResult::Error => reply.wake.wake(),
            })
        })
    }

    pub fn sinks(&self) -> Result<Vec<Sink>, Box<dyn Error>> {
        self.request("listing outputs", |context, reply| {
            let mut items = Vec::new();
            context.introspect().get_sink_info_list(move |result| match result {
                ListResult::Item(info) => items.push(Sink {
                    index: info.index,
                    name: text(&info.name),
                    description: text(&info.description),
                    monitor_source: text(&info.monitor_source_name),
                    instance: info.proplist.get_str(super::routing::INSTANCE_PROPERTY),
                }),
                ListResult::End => reply.send(std::mem::take(&mut items)),
                ListResult::Error => reply.wake.wake(),
            })
        })
    }

    pub fn sink_inputs(&self) -> Result<Vec<SinkInput>, Box<dyn Error>> {
        self.request("listing playing streams", |context, reply| {
            let mut items = Vec::new();
            context.introspect().get_sink_input_info_list(move |result| match result {
                ListResult::Item(info) => items.push(SinkInput { index: info.index, sink: info.sink }),
                ListResult::End => reply.send(std::mem::take(&mut items)),
                ListResult::Error => reply.wake.wake(),
            })
        })
    }

    pub fn modules(&self) -> Result<Vec<Module>, Box<dyn Error>> {
        self.request("listing modules", |context, reply| {
            let mut items = Vec::new();
            context.introspect().get_module_info_list(move |result| match result {
                ListResult::Item(info) => items.push(Module {
                    index: info.index,
                    name: text(&info.name),
                    argument: text(&info.argument),
                }),
                ListResult::End => reply.send(std::mem::take(&mut items)),
                ListResult::Error => reply.wake.wake(),
            })
        })
    }

    /// Load a module and return its index.
    pub fn load_module(&self, name: &str, argument: &str) -> Result<u32, Box<dyn Error>> {
        let index = self.request(&format!("loading {name}"), |context, reply| {
            context.introspect().load_module(name, argument, move |index| reply.send(index))
        })?;
        if index == pa::def::INVALID_INDEX {
            let reason = self.locked(|_, context| format!("{}", context.errno()));
            return Err(format!("loading {name} failed: {reason}").into());
        }
        Ok(index)
    }

    pub fn unload_module(&self, index: u32) -> Result<(), Box<dyn Error>> {
        self.act(&format!("unloading module {index}"), |context, reply| {
            context.introspect().unload_module(index, move |ok| reply.send(ok))
        })
    }

    pub fn set_default_sink(&self, name: &str) -> Result<(), Box<dyn Error>> {
        self.act_when_ready(&format!("making {name} the default output"), |context, reply| {
            context.set_default_sink(name, move |ok| reply.send(ok))
        })
    }

    pub fn set_default_source(&self, name: &str) -> Result<(), Box<dyn Error>> {
        self.act_when_ready(&format!("making {name} the default input"), |context, reply| {
            context.set_default_source(name, move |ok| reply.send(ok))
        })
    }

    /// Like `act`, for changes that need PipeWire's session manager (WirePlumber), which keeps the
    /// defaults. When the sound server was only just started for us (no one logged in to the
    /// desktop yet), the session manager takes a moment to come up, and until then the server
    /// answers "Not supported": wait for it a few seconds before giving up.
    fn act_when_ready<C: ?Sized>(
        &self,
        what: &str,
        start: impl Fn(&mut Context, Reply<bool>) -> Operation<C>,
    ) -> Result<(), Box<dyn Error>> {
        let not_supported = pa::error::PAErr::from(pa::error::Code::NotSupported).0.abs();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if self.request(what, &start)? {
                return Ok(());
            }
            let errno = self.locked(|_, context| context.errno());
            if errno.0.abs() != not_supported || std::time::Instant::now() >= deadline {
                return Err(format!("{what} failed: {errno}").into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    pub fn move_sink_input(&self, input: u32, sink: u32) -> Result<(), Box<dyn Error>> {
        self.act(&format!("moving stream {input}"), |context, reply| {
            context.introspect().move_sink_input_by_index(input, sink, Some(Box::new(move |ok| reply.send(ok))))
        })
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        // Stop the main loop thread first so no callback runs while we tear down.
        self.mainloop.borrow_mut().stop();
        let mut context = self.context.borrow_mut();
        context.set_state_callback(None);
        context.disconnect();
    }
}

fn text(value: &Option<std::borrow::Cow<'_, str>>) -> String {
    value.as_deref().unwrap_or_default().to_string()
}

/// What to do when there's no sound server to connect to.
fn no_server_help(reason: &str) -> String {
    let mut message = format!("cannot connect to the sound server ({reason}).\n");
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        message += "  XDG_RUNTIME_DIR is not set: run rtp-audio as the logged-in desktop user, not with sudo or su.\n";
    }
    message += "  Check that PulseAudio or PipeWire runs for this user:\n\
        \x20   PipeWire (most current distributions) needs its PulseAudio server, package\n\
        \x20   pipewire-pulse: systemctl --user enable --now pipewire-pulse.socket\n\
        \x20   PulseAudio: pulseaudio --start";
    message
}
