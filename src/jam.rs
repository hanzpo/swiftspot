//! Spotify Jam: a shared listening session that friends join from a link and
//! add songs to. Spotify's own apps run it through the `social-connect`
//! service on the streaming session's client endpoint; the Web API has
//! nothing for it. The requests are the ones the desktop app sends.
//!
//! A Jam belongs to the account, not to a device: one started on a phone
//! with this computer as its speaker is the same Jam, and updates about it
//! reach every device the account has connected.

use librespot_core::{Session, error::ErrorKind};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub image: Option<String>,
    pub you: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jam {
    pub id: String,
    pub token: String,
    pub owner_id: String,
    pub hosting: bool,
    pub members: Vec<Member>,
    pub host_device: Option<String>,
    pub host_device_name: Option<String>,
    /// Spotify asks this member's player commands to go through the Jam,
    /// which passes them to the host's device.
    pub redirects_commands: bool,
    pub timestamp: u64,
    pub can_remove: bool,
}

impl Jam {
    pub fn host(&self) -> Option<&Member> {
        self.members
            .iter()
            .find(|member| member.id == self.owner_id)
    }

    /// The `spotify:` URI an invitation names; its web address is the link
    /// people open.
    pub fn invite_uri(&self) -> String {
        format!("spotify:socialsession:{}", self.token)
    }
}

/// What someone else's invitation leads to, before joining.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub token: String,
    pub host: String,
    pub people: usize,
}

/// A pushed change to the account's Jam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    Changed(Jam),
    Ended,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn member(value: &Value) -> Option<Member> {
    let id = text(value, "id")?;
    let name = text(value, "display_name")
        .or_else(|| text(value, "username"))
        .unwrap_or_default();
    Some(Member {
        id,
        name,
        image: text(value, "image_url"),
        you: value["is_current_user"].as_bool().unwrap_or(false),
    })
}

/// The Jam a session answer describes, or `None` when it is not a live one.
pub fn parse(value: &Value) -> Option<Jam> {
    if value.get("active").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let id = text(value, "session_id")?;
    let host_info = value.get("host_device_info");
    Some(Jam {
        id,
        token: text(value, "join_session_token").unwrap_or_default(),
        owner_id: text(value, "session_owner_id").unwrap_or_default(),
        hosting: value["is_session_owner"].as_bool().unwrap_or(false),
        members: value["session_members"]
            .as_array()
            .map(|members| members.iter().filter_map(member).collect())
            .unwrap_or_default(),
        host_device: text(value, "hostActiveDeviceId")
            .or_else(|| host_info.and_then(|info| text(info, "device_id"))),
        host_device_name: host_info.and_then(|info| text(info, "device_name")),
        redirects_commands: ["redirect_commands", "redirectCommands"]
            .iter()
            .any(|key| value[*key].as_bool() == Some(true)),
        timestamp: value["timestamp"]
            .as_str()
            .and_then(|stamp| stamp.parse().ok())
            .or_else(|| value["timestamp"].as_u64())
            .unwrap_or(0),
        can_remove: value["user_capabilities"]["remove_participants"]
            .as_bool()
            .unwrap_or(false),
    })
}

/// A pushed `social-connect/v2/session_update`, or `None` when it says
/// nothing this app shows.
pub fn update(json: &str) -> Option<Update> {
    let value: Value = serde_json::from_str(json).ok()?;
    match value["reason"].as_str() {
        Some("SESSION_DELETED" | "YOU_LEFT" | "YOU_WERE_KICKED") => Some(Update::Ended),
        _ => value.get("session").map(|session| match parse(session) {
            Some(jam) => Update::Changed(jam),
            None => Update::Ended,
        }),
    }
}

fn is_token(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The join token in an invitation: `spotify:socialsession:TOKEN`,
/// `spotify://socialsession/TOKEN`, or the web address
/// `https://open.spotify.com/socialsession/TOKEN`, with or without a locale
/// segment or a query string.
pub fn token(link: &str) -> Option<String> {
    let link = link.trim();
    if let Some(token) = link.strip_prefix("spotify:socialsession:") {
        return is_token(token).then(|| token.to_string());
    }
    let rest = link
        .strip_prefix("spotify://")
        .or_else(|| link.strip_prefix("https://"))
        .or_else(|| link.strip_prefix("http://"))?;
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    let mut segments = rest.split('/').filter(|segment| !segment.is_empty());
    let mut first = segments.next()?;
    if first == "open.spotify.com" {
        first = segments.next()?;
    }
    if first.starts_with("intl-") {
        first = segments.next()?;
    }
    if first != "socialsession" {
        return None;
    }
    let token = segments.next()?;
    is_token(token).then(|| token.to_string())
}

/// A short link Spotify's phone apps share, `https://spotify.link/CODE`,
/// which redirects to the full address.
pub fn is_short_link(link: &str) -> bool {
    let link = link.trim();
    let Some(rest) = link
        .strip_prefix("https://")
        .or_else(|| link.strip_prefix("http://"))
    else {
        return false;
    };
    rest.strip_prefix("spotify.link/")
        .is_some_and(|code| is_token(&code[..code.find(['?', '#', '/']).unwrap_or(code.len())]))
}

/// Whether `text` is a Jam invitation in any shape, short or full.
pub fn is_invitation(text: &str) -> bool {
    token(text).is_some() || is_short_link(text)
}

async fn ask(
    session: &Session,
    method: http::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Option<Value>, librespot_core::Error> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    let body = body.map(Value::to_string);
    let bytes = session
        .spclient()
        .request(
            &method,
            path,
            Some(headers),
            body.as_deref().map(str::as_bytes),
        )
        .await?;
    Ok(serde_json::from_slice(&bytes).ok())
}

fn gone<T>(result: Result<T, librespot_core::Error>) -> Result<Option<T>, librespot_core::Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The account's Jam, or `None` when it is in none.
pub async fn current(session: &Session) -> anyhow::Result<Option<Jam>> {
    let answer = gone(
        ask(
            session,
            http::Method::GET,
            "/social-connect/v2/sessions/current",
            None,
        )
        .await,
    )?;
    Ok(answer.flatten().as_ref().and_then(parse))
}

/// Starts a Jam played on `device`, or gives the one already running.
pub async fn start(session: &Session, device: &str) -> anyhow::Result<Jam> {
    let path = format!(
        "/social-connect/v2/sessions/current_or_new?activate=true&local_device_id={}",
        percent_encoding::utf8_percent_encode(device, percent_encoding::NON_ALPHANUMERIC)
    );
    let answer = ask(session, http::Method::GET, &path, None).await?;
    answer
        .as_ref()
        .and_then(parse)
        .ok_or_else(|| anyhow::anyhow!("Spotify started no Jam"))
}

/// Ends the Jam for everyone in it. One already gone counts as ended.
pub async fn end(session: &Session, id: &str) -> anyhow::Result<()> {
    gone(
        ask(
            session,
            http::Method::DELETE,
            &format!("/social-connect/v3/sessions/{id}"),
            None,
        )
        .await,
    )?;
    Ok(())
}

/// Leaves someone else's Jam. One already gone counts as left.
pub async fn leave(session: &Session, id: &str) -> anyhow::Result<()> {
    gone(
        ask(
            session,
            http::Method::POST,
            &format!("/social-connect/v3/sessions/{id}/leave"),
            None,
        )
        .await,
    )?;
    Ok(())
}

/// Who hosts the Jam an invitation leads to, or `None` when it has ended.
pub async fn invite(session: &Session, token: &str) -> anyhow::Result<Option<Invite>> {
    let answer = gone(
        ask(
            session,
            http::Method::GET,
            &format!("/social-connect/v2/sessions/info/{token}"),
            None,
        )
        .await,
    )?;
    let Some(value) = answer.flatten() else {
        return Ok(None);
    };
    if value.get("active").and_then(Value::as_bool) == Some(false) {
        return Ok(None);
    }
    let owner = text(&value, "session_owner_id").unwrap_or_default();
    let members: Vec<Member> = value["session_members"]
        .as_array()
        .map(|members| members.iter().filter_map(member).collect())
        .unwrap_or_default();
    let host = members
        .iter()
        .find(|member| member.id == owner)
        .map(|member| member.name.clone())
        .unwrap_or_default();
    Ok(Some(Invite {
        token: token.to_string(),
        host,
        people: members.len(),
    }))
}

/// The body that joins a Jam from a link, listening on `device` and able to
/// add songs and control playback, as the desktop app joins from a link.
fn join_body(token: &str, device: &str) -> Value {
    serde_json::json!({
        "join_token": token,
        "join_type": "deeplinking",
        "playback_control": "LISTEN_AND_CONTROL",
        "local_device_id": device,
        "join_attempt_id": attempt_id(),
        "permission_mode": "FULL",
    })
}

fn attempt_id() -> String {
    let bytes: [u8; 16] = rand::random();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Joins the Jam behind `token`, listening on `device`.
pub async fn join(session: &Session, token: &str, device: &str) -> anyhow::Result<Jam> {
    let answer = ask(
        session,
        http::Method::POST,
        "/social-connect/v3/sessions/join",
        Some(&join_body(token, device)),
    )
    .await?;
    answer
        .as_ref()
        .and_then(parse)
        .ok_or_else(|| anyhow::anyhow!("Spotify joined no Jam"))
}

/// Removes someone from the Jam this account hosts.
pub async fn remove(session: &Session, id: &str, member: &str) -> anyhow::Result<Option<Jam>> {
    let answer = ask(
        session,
        http::Method::POST,
        &format!("/social-connect/v3/sessions/{id}/member/{member}/kick"),
        None,
    )
    .await?;
    Ok(answer.as_ref().and_then(parse))
}

/// A player command a guest sends through the Jam to the host's device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    AddToQueue(String),
    Next,
    Previous,
    Pause,
    Resume,
}

fn command_body(command: &Command) -> Value {
    let command = match command {
        Command::AddToQueue(uri) => serde_json::json!({
            "endpoint": "add_to_queue",
            "track": { "uri": uri },
        }),
        Command::Next => serde_json::json!({ "endpoint": "skip_next" }),
        Command::Previous => serde_json::json!({ "endpoint": "skip_prev" }),
        Command::Pause => serde_json::json!({ "endpoint": "pause" }),
        Command::Resume => serde_json::json!({ "endpoint": "resume" }),
    };
    serde_json::json!({ "command": command })
}

/// Sends `command` through the Jam `id` from `device`. `false` when the
/// Jam is gone.
pub async fn send(
    session: &Session,
    id: &str,
    device: &str,
    command: &Command,
) -> anyhow::Result<bool> {
    let path = format!(
        "/social-connect/v2/sessions/{id}/commands/from/{}",
        percent_encoding::utf8_percent_encode(device, percent_encoding::NON_ALPHANUMERIC)
    );
    let sent = gone(
        ask(
            session,
            http::Method::POST,
            &path,
            Some(&command_body(command)),
        )
        .await,
    )?;
    Ok(sent.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Value {
        serde_json::json!({
            "timestamp": "1794551163835",
            "session_id": "9010cbf062c0087df476840f44802ba9",
            "join_session_token": "4nQzpzVCLVuRdOCKpGK649",
            "session_owner_id": "owner",
            "session_members": [
                {"id": "owner", "display_name": "Ana", "username": "ana1", "image_url": "https://i.scdn.co/image/a", "is_current_user": true},
                {"id": "guest", "display_name": "", "username": "bo2"},
            ],
            "is_session_owner": true,
            "hostActiveDeviceId": "device",
            "host_device_info": {"device_id": "device", "device_name": "SwiftSpot"},
            "active": true,
            "user_capabilities": {"invite": true, "remove_participants": true},
        })
    }

    #[test]
    fn a_session_answer_reads_as_its_jam() {
        let jam = parse(&session()).unwrap();
        assert_eq!(jam.id, "9010cbf062c0087df476840f44802ba9");
        assert_eq!(jam.token, "4nQzpzVCLVuRdOCKpGK649");
        assert!(jam.hosting);
        assert!(jam.can_remove);
        assert!(!jam.redirects_commands);
        assert_eq!(jam.timestamp, 1_794_551_163_835);
        assert_eq!(jam.host_device.as_deref(), Some("device"));
        assert_eq!(jam.host_device_name.as_deref(), Some("SwiftSpot"));
        assert_eq!(jam.host().map(|host| host.name.as_str()), Some("Ana"));
        assert!(jam.members[0].you);
        assert_eq!(
            jam.members[1].name, "bo2",
            "a blank display name falls back to the username"
        );
        assert_eq!(jam.members[1].image, None);
        assert_eq!(
            jam.invite_uri(),
            "spotify:socialsession:4nQzpzVCLVuRdOCKpGK649"
        );
    }

    #[test]
    fn an_inactive_session_is_no_jam() {
        let mut value = session();
        value["active"] = Value::Bool(false);
        assert_eq!(parse(&value), None);
        assert_eq!(parse(&serde_json::json!({})), None);
    }

    #[test]
    fn a_guest_told_to_redirect_commands_does() {
        let mut value = session();
        value["is_session_owner"] = Value::Bool(false);
        value["redirect_commands"] = Value::Bool(true);
        let jam = parse(&value).unwrap();
        assert!(!jam.hosting);
        assert!(jam.redirects_commands);
    }

    #[test]
    fn pushed_updates_change_or_end_the_jam() {
        let changed =
            serde_json::json!({"reason": "USER_JOINED", "session": session()}).to_string();
        assert!(matches!(update(&changed), Some(Update::Changed(jam)) if jam.members.len() == 2));
        for reason in ["SESSION_DELETED", "YOU_LEFT", "YOU_WERE_KICKED"] {
            let ended = serde_json::json!({"reason": reason, "session": session()}).to_string();
            assert_eq!(update(&ended), Some(Update::Ended), "{reason}");
        }
        let unknown =
            serde_json::json!({"reason": "SESSION_MEMBER_PAUSE_CHANGED", "session": session()})
                .to_string();
        assert!(matches!(update(&unknown), Some(Update::Changed(_))));
        assert_eq!(update(r#"{"reason":"WIFI_BROADCAST_CHANGED"}"#), None);
        assert_eq!(update("not json"), None);
    }

    #[test]
    fn invitations_give_their_token_in_every_shape() {
        for link in [
            "spotify:socialsession:4nQzpzVCLVuRdOCKpGK649",
            "spotify://socialsession/4nQzpzVCLVuRdOCKpGK649",
            "https://open.spotify.com/socialsession/4nQzpzVCLVuRdOCKpGK649",
            " https://open.spotify.com/intl-de/socialsession/4nQzpzVCLVuRdOCKpGK649?si=abc ",
            "http://open.spotify.com/socialsession/4nQzpzVCLVuRdOCKpGK649#x",
        ] {
            assert_eq!(
                token(link).as_deref(),
                Some("4nQzpzVCLVuRdOCKpGK649"),
                "{link}"
            );
            assert!(is_invitation(link));
        }
        for link in [
            "https://open.spotify.com/track/4nQzpzVCLVuRdOCKpGK649",
            "https://example.com/socialsession/abc",
            "spotify:socialsession:",
            "spotify:socialsession:a/b",
            "Jam",
        ] {
            assert_eq!(token(link), None, "{link}");
        }
    }

    #[test]
    fn short_links_are_told_apart() {
        assert!(is_short_link("https://spotify.link/AbC123xyz"));
        assert!(is_short_link("https://spotify.link/AbC123xyz?si=1"));
        assert!(!is_short_link("https://spotify.link/"));
        assert!(!is_short_link("https://example.link/AbC"));
        assert!(!is_short_link("spotify.link/AbC"));
    }

    #[test]
    fn a_join_listens_here_and_controls() {
        let body = join_body("tok", "dev");
        assert_eq!(body["join_token"], "tok");
        assert_eq!(body["join_type"], "deeplinking");
        assert_eq!(body["playback_control"], "LISTEN_AND_CONTROL");
        assert_eq!(body["local_device_id"], "dev");
        assert_eq!(body["permission_mode"], "FULL");
        let attempt = body["join_attempt_id"].as_str().unwrap();
        assert_eq!(attempt.len(), 36);
        assert_ne!(
            attempt,
            join_body("tok", "dev")["join_attempt_id"].as_str().unwrap()
        );
    }

    #[test]
    fn commands_name_the_endpoint_the_host_runs() {
        let add = command_body(&Command::AddToQueue("spotify:track:1".into()));
        assert_eq!(add["command"]["endpoint"], "add_to_queue");
        assert_eq!(add["command"]["track"]["uri"], "spotify:track:1");
        assert_eq!(
            command_body(&Command::Next)["command"]["endpoint"],
            "skip_next"
        );
        assert_eq!(
            command_body(&Command::Previous)["command"]["endpoint"],
            "skip_prev"
        );
        assert_eq!(
            command_body(&Command::Pause)["command"]["endpoint"],
            "pause"
        );
        assert_eq!(
            command_body(&Command::Resume)["command"]["endpoint"],
            "resume"
        );
    }
}
