mod art;
mod client;
mod state;
mod ui;

use std::process::ExitCode;
use std::sync::mpsc::{RecvTimeoutError, channel};

use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use serde_json::json;

use client::Client;
use state::{Flow, State};

pub enum Input {
    Key(KeyEvent),
    Resize,
    Server(serde_json::Value),
    Closed,
}

const USAGE: &str = "\
swiftspot: SwiftSpot in the terminal

Usage:
  swiftspot             Open the player
  swiftspot toggle      Play or pause
  swiftspot next        Next song
  swiftspot previous    Previous song
  swiftspot like        Like or unlike the playing song
  swiftspot now         Print the playing song

The SwiftSpot app must be installed. It starts in the background when needed.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => run(),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("swiftspot {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(command @ ("toggle" | "next" | "previous" | "like")) => {
            Client::connect().and_then(|mut client| {
                client
                    .call(&format!("player.{command}"), json!({}))
                    .map(|_| ())
            })
        }
        Some("now") => now(),
        Some(other) => {
            eprintln!("swiftspot: unknown command {other:?}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("swiftspot: {error}");
            ExitCode::FAILURE
        }
    }
}

fn now() -> Result<(), String> {
    let mut client = Client::connect()?;
    let player = client.call("player.state", json!({}))?;
    if player["has_song"] != true {
        println!("Nothing playing");
        return Ok(());
    }
    let artists: Vec<&str> = player["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| artist["name"].as_str())
        .collect();
    let title = player["title"].as_str().unwrap_or_default();
    if artists.is_empty() {
        println!("{title}");
    } else {
        println!("{title} · {}", artists.join(", "));
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut client = Client::connect()?;
    let (sender, inputs) = channel();
    client
        .listen(sender.clone())
        .map_err(|error| error.to_string())?;
    std::thread::Builder::new()
        .name("swiftspot-keys".into())
        .spawn(move || {
            loop {
                let input = match event::read() {
                    Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => Input::Key(key),
                    Ok(Event::Resize(..)) => Input::Resize,
                    Ok(_) => continue,
                    Err(_) => return,
                };
                if sender.send(input).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| error.to_string())?;

    let mut art = art::Art::new();
    let mut state = State::new(art.enabled());
    state.start(&mut client);
    let mut terminal = ratatui::init();
    let mut stdout = std::io::stdout();
    let outcome = loop {
        let mut area = None;
        if let Err(error) = terminal.draw(|frame| area = ui::draw(frame, &state)) {
            break Err(error.to_string());
        }
        if let Some((url, bytes)) = state.cover.take()
            && !art.has(&url)
        {
            art.set(url, &bytes);
        }
        let cover = state
            .player
            .cover
            .as_deref()
            .filter(|_| state.player.has_song);
        art.sync(&mut stdout, if state.help { None } else { cover }, area);

        let input = match inputs.recv_timeout(state.wait()) {
            Ok(input) => input,
            Err(RecvTimeoutError::Timeout) => {
                state.tick(&mut client);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        };
        let mut flow = Flow::Continue;
        for input in std::iter::once(input).chain(inputs.try_iter()) {
            match input {
                Input::Key(key) => flow = state.key(key, &mut client),
                Input::Resize => art.invalidate(),
                Input::Server(message) => state.message(message, &mut client),
                Input::Closed => {
                    state.closed = true;
                    flow = Flow::Quit;
                }
            }
            if matches!(flow, Flow::Quit) {
                break;
            }
        }
        state.tick(&mut client);
        if matches!(flow, Flow::Quit) {
            break Ok(());
        }
    };
    art.forget(&mut stdout);
    ratatui::restore();
    if state.closed {
        return Err("SwiftSpot quit".into());
    }
    outcome
}
