//! The C interface the SwiftUI app links the engine through.
//!
//! The engine runs on a thread of its own inside the app's process. The app
//! hands it JSON requests with [`swiftspot_send`]; replies and events come
//! back, one JSON object per call, through the callback given to
//! [`swiftspot_start`], on a delivery thread. A call with a null pointer
//! means the engine has stopped and nothing more will arrive.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::app::{App, AppOptions};
use crate::backend::Waker;
use crate::paths::AppDirs;
use crate::server::Inbox;
use crate::settings::Settings;

/// Receives one reply or event: `len` bytes of UTF-8 JSON at `data`, valid
/// only for the call.
pub type Deliver = extern "C" fn(context: *mut c_void, data: *const u8, len: usize);

struct Context(*mut c_void);

// SAFETY: the app's context is only passed back to its own callback, which
// the app promises can run on any thread.
unsafe impl Send for Context {}

/// The running engine, as the app holds it.
pub struct Engine {
    inbox: Option<Inbox>,
    waker: Waker,
    stop: Arc<AtomicBool>,
    engine: Option<JoinHandle<()>>,
    delivery: Option<JoinHandle<()>>,
}

fn start_logging(dirs: &AppDirs) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if let Err(error) = fastframe_log::Logging::new("swiftspot", env!("CARGO_PKG_VERSION"))
            .filter("warn,swiftspot=info")
            .file(dirs.log_file())
            .panic_log(dirs.panic_log())
            .panic_message(fastframe_log::PanicMessage::Redacted(
                fastframe_log::redact::links,
            ))
            .init()
        {
            eprintln!("not logging: {error}");
        }
    });
}

impl Engine {
    fn start(deliver: Deliver, context: Context) -> std::io::Result<Self> {
        // SWIFTSPOT_DATA keeps a test run's files, and its sign-in, apart
        // from the person's own, and leaves the media keys to their app.
        let isolated = std::env::var_os("SWIFTSPOT_DATA").map(std::path::PathBuf::from);
        let options = match isolated {
            Some(_) => AppOptions {
                restore_sign_in: false,
                media_controls: false,
            },
            None => AppOptions::default(),
        };
        let dirs = isolated.map_or_else(AppDirs::discover, |base| AppDirs {
            config: base.join("config"),
            state: base.join("state"),
            cache: base.join("cache"),
        });
        let dirs_ready = dirs.ensure();
        start_logging(&dirs);
        if let Err(error) = dirs_ready {
            log::warn!("unable to create the application directories: {error}");
        }
        let waker = Waker::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let engine = {
            let waker = waker.clone();
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("swiftspot-engine".into())
                .spawn(move || {
                    crate::file_writer::mark_ui_thread();
                    let mut settings = Settings::load(&dirs.settings_file());
                    if matches!(settings.device_name.as_str(), "Spotifast" | "Spotlite") {
                        settings.device_name = "SwiftSpot".to_string();
                    }
                    let mut app = App::new(&waker, dirs, settings, options);
                    let _ = ready_tx.send(app.connect_front_end(&waker));
                    while !stop.load(Ordering::Acquire) && !app.quit_requested {
                        let wait = app.engine_step(&waker);
                        if stop.load(Ordering::Acquire) || app.quit_requested {
                            break;
                        }
                        waker.wait(wait);
                    }
                    app.shutdown();
                })?
        };
        let (inbox, messages) = ready_rx
            .recv()
            .map_err(|_| std::io::Error::other("the engine stopped while starting"))?;
        let delivery = std::thread::Builder::new()
            .name("swiftspot-delivery".into())
            .spawn(move || {
                let context = context;
                for message in messages {
                    deliver(context.0, message.as_ptr(), message.len());
                }
                deliver(context.0, std::ptr::null(), 0);
            })?;
        Ok(Self {
            inbox: Some(inbox),
            waker,
            stop,
            engine: Some(engine),
            delivery: Some(delivery),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.waker.wake();
        if let Some(engine) = self.engine.take() {
            let _ = engine.join();
        }
        // The front end's channel closes with its inbox, which ends delivery
        // even while a network answer still holds the server.
        self.inbox = None;
        if let Some(delivery) = self.delivery.take() {
            let _ = delivery.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the engine. Returns null when it cannot start. `deliver` is
/// called with `context` from the engine's delivery thread until
/// [`swiftspot_stop`] returns.
#[unsafe(no_mangle)]
pub extern "C" fn swiftspot_start(deliver: Deliver, context: *mut c_void) -> *mut Engine {
    match Engine::start(deliver, Context(context)) {
        Ok(engine) => Box::into_raw(Box::new(engine)),
        Err(error) => {
            log::error!("the engine didn't start: {error}");
            std::ptr::null_mut()
        }
    }
}

/// Queues one JSON request: `len` bytes of UTF-8 at `data`, copied before
/// this returns.
///
/// # Safety
///
/// `engine` comes from [`swiftspot_start`] and has not been stopped;
/// `data` points at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn swiftspot_send(engine: *const Engine, data: *const u8, len: usize) {
    if engine.is_null() || data.is_null() {
        return;
    }
    // SAFETY: the caller keeps both pointers valid for this call.
    let (engine, line) = unsafe { (&*engine, std::slice::from_raw_parts(data, len)) };
    if let Some(inbox) = &engine.inbox {
        inbox.submit(line);
    }
}

/// Saves the state, stops playback and the session, and frees the engine.
/// No callback runs after this returns.
///
/// # Safety
///
/// `engine` comes from [`swiftspot_start`] and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn swiftspot_stop(engine: *mut Engine) {
    if engine.is_null() {
        return;
    }
    // SAFETY: the caller hands back the pointer swiftspot_start made.
    drop(unsafe { Box::from_raw(engine) });
}
