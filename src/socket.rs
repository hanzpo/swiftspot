//! The API on a local socket, for front ends in other processes such as the
//! `swiftspot` terminal client (`tui/`).
//!
//! Each connection is one more front end of [`crate::server::Server`]: it
//! writes requests and reads replies and events, one JSON object per line,
//! exactly as the app does through `crate::ffi`. The socket sits in a folder
//! only its user can open, so it reaches no one the app does not already
//! answer to.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::backend::Waker;
use crate::server::Server;

/// Where the socket is: inside the data folder for a run with
/// `SWIFTSPOT_DATA`, otherwise in the user's private temporary folder, which
/// keeps the path under the 104 bytes macOS allows.
pub fn path(isolated: Option<&Path>) -> PathBuf {
    match isolated {
        Some(base) => base.join("api.sock"),
        None => std::env::temp_dir().join("swiftspot-api").join("api.sock"),
    }
}

/// Accepts front ends until dropped.
pub struct Listener {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Serves `server` at `path`. Fails when another SwiftSpot already
    /// answers there.
    pub fn start(path: PathBuf, server: Server, waker: Waker) -> std::io::Result<Self> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
            std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))?;
        }
        if UnixStream::connect(&path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "another SwiftSpot already serves the API",
            ));
        }
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("swiftspot-api".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        match stream {
                            Ok(stream) => {
                                if let Err(error) = serve(stream, &server, &waker) {
                                    log::warn!("API socket: a front end failed: {error}");
                                }
                            }
                            Err(error) => log::warn!("API socket: {error}"),
                        }
                    }
                })?
        };
        Ok(Self {
            path,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Connects one front end: its lines become requests, and its replies and
/// events go back as lines. It leaves when either side closes.
fn serve(stream: UnixStream, server: &Server, waker: &Waker) -> std::io::Result<()> {
    let (inbox, messages) = server.connect(waker.clone());
    let mut writer = stream.try_clone()?;
    std::thread::Builder::new()
        .name("swiftspot-api-out".into())
        .spawn(move || {
            for message in messages {
                let mut line = message.into_bytes();
                line.push(b'\n');
                if writer.write_all(&line).is_err() {
                    break;
                }
            }
            let _ = writer.shutdown(std::net::Shutdown::Both);
        })?;
    std::thread::Builder::new()
        .name("swiftspot-api-in".into())
        .spawn(move || {
            for line in BufReader::new(stream).split(b'\n') {
                let Ok(line) = line else { break };
                if !line.is_empty() {
                    inbox.submit(&line);
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn temporary() -> PathBuf {
        std::env::temp_dir().join(format!("ss-api-{:08x}", rand::random::<u32>()))
    }

    #[test]
    fn a_front_end_on_the_socket_gets_its_reply_and_the_events() {
        let folder = temporary();
        let path = path(Some(&folder));
        let server = Server::default();
        let listener = Listener::start(path.clone(), server.clone(), Waker::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"id\":3,\"method\":\"player.state\"}\n")
            .unwrap();
        let request = loop {
            if let Some(request) = server.take_requests().pop() {
                break request;
            }
            std::thread::yield_now();
        };
        assert_eq!(request.method, "player.state");
        server.reply(&request, Ok(serde_json::json!({ "playing": true })));
        server.broadcast("player", serde_json::json!({ "title": "Rosewood" }));

        let mut lines = BufReader::new(client.try_clone().unwrap()).lines();
        let reply: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(reply["id"], 3);
        assert_eq!(reply["result"]["playing"], true);
        let event: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(event["event"], "player");

        drop(client);
        drop(lines);
        drop(listener);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_second_listener_leaves_the_first_one_serving() {
        let folder = temporary();
        let path = path(Some(&folder));
        let first = Listener::start(path.clone(), Server::default(), Waker::default()).unwrap();
        let second = Listener::start(path.clone(), Server::default(), Waker::default());
        assert!(second.is_err());
        assert!(UnixStream::connect(&path).is_ok());
        drop(first);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
