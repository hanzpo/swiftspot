use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Input;

pub fn socket_path() -> PathBuf {
    match std::env::var_os("SWIFTSPOT_DATA") {
        Some(base) => PathBuf::from(base).join("api.sock"),
        None => std::env::temp_dir().join("swiftspot-api").join("api.sock"),
    }
}

pub struct Client {
    stream: UnixStream,
    next_id: u64,
}

impl Client {
    pub fn connect() -> Result<Self, String> {
        let path = socket_path();
        let open = |stream| Self { stream, next_id: 1 };
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(open(stream));
        }
        if std::env::var_os("SWIFTSPOT_DATA").is_some() {
            return Err(format!("SwiftSpot is not running at {}", path.display()));
        }
        let launched = std::process::Command::new("open")
            .args(["-g", "-j", "-b", "com.hanzpo.swiftspot"])
            .status()
            .map_err(|error| format!("could not start SwiftSpot: {error}"))?;
        if !launched.success() {
            return Err("could not start SwiftSpot. Is it installed?".into());
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(stream) = UnixStream::connect(&path) {
                return Ok(open(stream));
            }
            if Instant::now() > deadline {
                return Err("SwiftSpot started but does not answer. Update the app.".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn listen(&self, to: Sender<Input>) -> std::io::Result<()> {
        let reader = self.stream.try_clone()?;
        std::thread::Builder::new()
            .name("swiftspot-socket".into())
            .spawn(move || {
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(message) = serde_json::from_str::<Value>(&line)
                        && to.send(Input::Server(message)).is_err()
                    {
                        return;
                    }
                }
                let _ = to.send(Input::Closed);
            })?;
        Ok(())
    }

    pub fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({ "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        let _ = self.stream.write_all(line.as_bytes());
        id
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send(method, params);
        let reader = self.stream.try_clone().map_err(|error| error.to_string())?;
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        for line in BufReader::new(reader).lines() {
            let line = line.map_err(|_| "SwiftSpot did not answer".to_string())?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] == id {
                if let Some(error) = message["error"].as_str() {
                    return Err(error.to_string());
                }
                return Ok(message["result"].clone());
            }
        }
        Err("SwiftSpot closed the connection".into())
    }
}
