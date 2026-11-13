//! Diagnostic: asks Spotify's Jam service about the account, with the stored
//! playback credential.
//!
//!   cargo run --example jam_probe -- current
//!   cargo run --example jam_probe -- host 120
//!   cargo run --example jam_probe -- GET /social-connect/v2/sessions/current
//!   cargo run --example jam_probe -- POST /social-connect/v3/sessions/join '{"join_token":"..."}'
//!
//! `host` brings up a silent Connect device, starts a Jam on it, prints
//! every Jam update and every command the device receives for the given
//! number of seconds, then ends the Jam.

use std::sync::Arc;

use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{Session, SessionConfig, cache::Cache, dealer::protocol::PayloadValue};
use librespot_playback::{
    audio_backend::{Sink, SinkResult},
    config::PlayerConfig,
    convert::Converter,
    decoder::AudioPacket,
    mixer::{Mixer, MixerConfig, softmixer::SoftMixer},
    player::Player,
};

struct Silent;

impl Sink for Silent {
    fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    fastframe_log::Logging::new("swiftspot", env!("CARGO_PKG_VERSION"))
        .filter("warn,librespot_connect=debug")
        .init()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dirs = swiftspot::paths::AppDirs::discover();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let store = swiftspot::credentials::Store::new(dirs);
        let loaded = store
            .lease(swiftspot::credentials::Slot::Playback)
            .load()
            .await?;
        let Some(swiftspot::credentials::Grant::Playback(credentials)) = loaded.grant else {
            anyhow::bail!("Enable playback in SwiftSpot first");
        };
        let cache =
            Cache::new::<&std::path::Path>(None, None, None, None)?.with_memory_credentials();
        let session = Session::new(SessionConfig::default(), Some(cache));
        match args.as_slice() {
            [verb, seconds] if verb == "host" => host(session, credentials, seconds.parse()?).await,
            _ => {
                session.connect(credentials, false).await?;
                eprintln!("connected, device {}", session.device_id());
                let (method, path, body) = match args.as_slice() {
                    [verb] if verb == "current" => {
                        ("GET", "/social-connect/v2/sessions/current", None)
                    }
                    [method, path] => (method.as_str(), path.as_str(), None),
                    [method, path, body] => (method.as_str(), path.as_str(), Some(body.as_str())),
                    _ => anyhow::bail!(
                        "usage: jam_probe current | host SECONDS | METHOD PATH [JSON]"
                    ),
                };
                println!("{}", ask(&session, method, path, body).await);
                Ok(())
            }
        }
    })
}

async fn ask(session: &Session, method: &str, path: &str, body: Option<&str>) -> String {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    let Ok(method) = http::Method::from_bytes(method.as_bytes()) else {
        return "error: bad method".into();
    };
    match session
        .spclient()
        .request(&method, path, Some(headers), body.map(str::as_bytes))
        .await
    {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("error: {error:?}"),
    }
}

async fn host(
    session: Session,
    credentials: librespot_core::authentication::Credentials,
    seconds: u64,
) -> anyhow::Result<()> {
    let mixer = Arc::new(SoftMixer::open(MixerConfig::default())?);
    let player = Player::new(
        PlayerConfig::default(),
        session.clone(),
        mixer.get_soft_volume(),
        || Box::new(Silent),
    );
    let config = ConnectConfig {
        name: "Jam probe".into(),
        ..ConnectConfig::default()
    };
    let (spirc, task) = Spirc::new(config, session.clone(), credentials, player, mixer).await?;
    tokio::spawn(task);
    let device = session.device_id().to_string();
    eprintln!("device {device} is up");

    let mut updates =
        session
            .dealer()
            .listen_for("social-connect/v2/session_update", |message| {
                Ok(match message.payload {
                    PayloadValue::Json(json) => json,
                    other => format!("{other:?}"),
                })
            })?;
    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(update) = updates.next().await {
            println!(
                "UPDATE {}",
                update.unwrap_or_else(|error| error.to_string())
            );
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let started = ask(
        &session,
        "GET",
        &format!(
            "/social-connect/v2/sessions/current_or_new?activate=true&local_device_id={device}"
        ),
        None,
    )
    .await;
    println!("STARTED {started}");
    let id = serde_json::from_str::<serde_json::Value>(&started)
        .ok()
        .and_then(|json| json["session_id"].as_str().map(str::to_string));
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
    if let Some(id) = id {
        println!(
            "ENDED {}",
            ask(
                &session,
                "DELETE",
                &format!("/social-connect/v3/sessions/{id}"),
                None
            )
            .await
        );
    }
    spirc.shutdown()?;
    Ok(())
}
