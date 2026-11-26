use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::client::Client;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Search(String),
    Liked,
    Albums,
    Queue,
    Playlist(String),
    Album(String),
}

impl Target {
    fn page(&self) -> String {
        match self {
            Self::Search(_) => "search".into(),
            Self::Liked => "liked".into(),
            Self::Albums => "albums".into(),
            Self::Queue => "queue".into(),
            Self::Playlist(id) => format!("playlist:{id}"),
            Self::Album(id) => format!("album:{id}"),
        }
    }

    fn request(&self) -> (&'static str, Value) {
        match self {
            Self::Search(query) => ("search", json!({ "query": query })),
            Self::Liked => ("library.liked", json!({})),
            Self::Albums => ("library.albums", json!({})),
            Self::Queue => ("queue.get", json!({})),
            Self::Playlist(id) => ("playlist.get", json!({ "id": id })),
            Self::Album(id) => ("album.get", json!({ "id": id })),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Play(Value),
    FromQueue { index: usize, queued: bool },
    Open(Target),
    Nothing,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: Option<u64>,
    pub uri: Option<String>,
    pub action: Action,
    pub header: bool,
    pub unplayable: bool,
}

impl Row {
    fn header(title: &str) -> Self {
        Self {
            title: title.to_string(),
            artists: String::new(),
            album: String::new(),
            duration_ms: None,
            uri: None,
            action: Action::Nothing,
            header: true,
            unplayable: false,
        }
    }

    fn song(value: &Value, context: Option<&str>) -> Self {
        let uri = text(value, "uri");
        let mut params = json!({ "uri": uri });
        if let Some(context) = context {
            params["context"] = json!(context);
            if let Some(position) = value.get("spotify_position") {
                params["spotify_position"] = position.clone();
            }
        }
        let artists = names(&value["artists"]);
        Self {
            title: text(value, "name"),
            artists: if artists.is_empty() {
                text(value, "subtitle")
            } else {
                artists
            },
            album: text(&value["album"], "name"),
            duration_ms: value["duration_ms"].as_u64(),
            uri: Some(uri),
            action: Action::Play(params),
            header: false,
            unplayable: value["playable"] == false,
        }
    }

    fn card(value: &Value) -> Self {
        let id = text(value, "id");
        let action = match value["kind"].as_str() {
            Some("album") => Action::Open(Target::Album(id)),
            Some("playlist") => Action::Open(Target::Playlist(id)),
            _ => Action::Nothing,
        };
        Self {
            title: text(value, "name"),
            artists: text(value, "subtitle"),
            album: String::new(),
            duration_ms: None,
            uri: None,
            action,
            header: false,
            unplayable: false,
        }
    }

    fn queued(value: &Value, action: Action) -> Self {
        Self {
            action,
            ..Self::song(value, None)
        }
    }
}

pub struct View {
    pub target: Target,
    pub title: String,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    more: bool,
    page: Option<String>,
    asked_more: bool,
    asked: bool,
}

impl View {
    fn new(target: Target) -> Self {
        let title = match &target {
            Target::Search(query) => format!("Search: {query}"),
            Target::Liked => "Liked Songs".into(),
            Target::Albums => "Albums".into(),
            Target::Queue => "Queue".into(),
            Target::Playlist(_) | Target::Album(_) => String::new(),
        };
        Self {
            target,
            title,
            rows: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            more: false,
            page: None,
            asked_more: false,
            asked: false,
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected).filter(|row| !row.header)
    }

    fn fill(&mut self, reply: &Value) {
        self.loading = reply["loading"] == true;
        self.error = reply["error"].as_str().map(str::to_string);
        self.more = reply["more"] == true;
        self.page = reply["page"].as_str().map(str::to_string);
        self.asked_more = false;
        if let Some(title) = reply["title"].as_str() {
            self.title = title.to_string();
        }
        let context = reply["context"].as_str();
        let mut rows = Vec::new();
        match &self.target {
            Target::Liked | Target::Playlist(_) | Target::Album(_) => {
                rows.extend(list(&reply["tracks"]).map(|each| Row::song(each, context)));
            }
            Target::Albums => rows.extend(list(&reply["cards"]).map(Row::card)),
            Target::Queue => {
                if reply["current"].is_object() {
                    rows.push(Row::header("Now playing"));
                    rows.push(Row::queued(&reply["current"], Action::Nothing));
                }
                let queued: Vec<&Value> = list(&reply["queued"]).collect();
                if !queued.is_empty() {
                    rows.push(Row::header("Next in queue"));
                    for (index, each) in queued.into_iter().enumerate() {
                        let action = Action::FromQueue {
                            index,
                            queued: true,
                        };
                        rows.push(Row::queued(each, action));
                    }
                }
                let next: Vec<&Value> = list(&reply["next"]).collect();
                if !next.is_empty() {
                    rows.push(Row::header(&match reply["context"].as_str() {
                        Some(name) => format!("Next from {name}"),
                        None => "Next up".to_string(),
                    }));
                    for (index, each) in next.into_iter().enumerate() {
                        let action = Action::FromQueue {
                            index,
                            queued: false,
                        };
                        rows.push(Row::queued(each, action));
                    }
                }
            }
            Target::Search(_) => {
                let sections = [
                    ("Songs", &reply["tracks"]),
                    ("Albums", &reply["albums"]),
                    ("Playlists", &reply["playlists"]),
                ];
                for (name, items) in sections {
                    let items: Vec<&Value> = list(items).collect();
                    if items.is_empty() {
                        continue;
                    }
                    rows.push(Row::header(name));
                    for each in items {
                        rows.push(if name == "Songs" {
                            Row::song(each, None)
                        } else {
                            Row::card(each)
                        });
                    }
                }
            }
        }
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        if self.rows.get(self.selected).is_some_and(|row| row.header) {
            self.step(1);
        }
    }

    fn step(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let mut index = (self.selected as isize + by).clamp(0, last);
        let direction = if by < 0 { -1 } else { 1 };
        while self.rows[index as usize].header {
            let next = index + direction;
            if !(0..=last).contains(&next) {
                if (0..=last).contains(&(index - direction))
                    && !self.rows[(index - direction) as usize].header
                {
                    index -= direction;
                }
                break;
            }
            index = next;
        }
        self.selected = index as usize;
    }
}

pub struct SideRow {
    pub label: String,
    pub depth: usize,
    pub target: Option<Target>,
}

pub struct Player {
    pub has_song: bool,
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub cover: Option<String>,
    pub duration_ms: u64,
    position_ms: u64,
    at: Instant,
    pub playing: bool,
    pub shuffle: bool,
    pub repeat: String,
    pub volume: u64,
    pub saved: Option<bool>,
    pub device: Option<String>,
    pub local: bool,
    pub playing_from: Option<String>,
}

impl Player {
    fn empty() -> Self {
        Self::from(&json!({}))
    }

    fn from(value: &Value) -> Self {
        let artists = names(&value["artists"]);
        Self {
            has_song: value["has_song"] == true,
            uri: text(value, "uri"),
            title: text(value, "title"),
            artists: if artists.is_empty() {
                text(value, "subtitle")
            } else {
                artists
            },
            album: text(&value["album"], "name"),
            cover: value["cover"]
                .as_str()
                .or(value["image"].as_str())
                .map(str::to_string),
            duration_ms: value["duration_ms"].as_u64().unwrap_or(0),
            position_ms: value["position_ms"].as_u64().unwrap_or(0),
            at: Instant::now(),
            playing: value["playing"] == true,
            shuffle: value["shuffle"] == true,
            repeat: value["repeat"].as_str().unwrap_or("off").to_string(),
            volume: value["volume"].as_u64().unwrap_or(0),
            saved: value["saved"].as_bool(),
            device: value["device"].as_str().map(str::to_string),
            local: value["local"] != false,
            playing_from: value["playing_from"].as_str().map(str::to_string),
        }
    }

    pub fn position_ms(&self) -> u64 {
        let moved = if self.playing {
            self.at.elapsed().as_millis() as u64
        } else {
            0
        };
        (self.position_ms + moved).min(self.duration_ms.max(self.position_ms))
    }

    fn seek(&mut self, position_ms: u64) {
        self.position_ms = position_ms.min(self.duration_ms);
        self.at = Instant::now();
    }
}

pub struct Lyrics {
    pub uri: Option<String>,
    pub lines: Vec<(Option<u64>, String)>,
    pub synced: bool,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Side,
    Main,
    Input,
}

enum Pending {
    Hello,
    Player,
    Playlists,
    View(Target),
    Lyrics,
    Image(String),
    Action,
}

pub struct State {
    pub focus: Focus,
    pub side: Vec<SideRow>,
    pub side_selected: usize,
    pub view: View,
    back: Vec<Target>,
    pub player: Player,
    pub lyrics: Option<Lyrics>,
    pub show_lyrics: bool,
    pub help: bool,
    pub query: String,
    pub auth: String,
    pub notice: Option<(String, Instant)>,
    pub closed: bool,
    pub cover: Option<(String, Vec<u8>)>,
    pub want_art: bool,
    pending: HashMap<u64, Pending>,
    retry_at: Option<Instant>,
    retry_lyrics_at: Option<Instant>,
}

pub enum Flow {
    Continue,
    Quit,
}

impl State {
    pub fn new(want_art: bool) -> Self {
        Self {
            focus: Focus::Main,
            side: fixed_side(),
            side_selected: 1,
            view: View::new(Target::Liked),
            back: Vec::new(),
            player: Player::empty(),
            lyrics: None,
            show_lyrics: false,
            help: false,
            query: String::new(),
            auth: "starting".into(),
            notice: None,
            closed: false,
            cover: None,
            want_art,
            pending: HashMap::new(),
            retry_at: None,
            retry_lyrics_at: None,
        }
    }

    pub fn start(&mut self, client: &mut Client) {
        self.ask(client, "hello", json!({}), Pending::Hello);
        self.ask(client, "player.state", json!({}), Pending::Player);
        self.ask(client, "library.playlists", json!({}), Pending::Playlists);
        self.load(client);
    }

    pub fn signed_in(&self) -> bool {
        self.auth == "connected"
    }

    pub fn wait(&self) -> Duration {
        if self.player.playing {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(1)
        }
    }

    fn ask(&mut self, client: &mut Client, method: &str, params: Value, pending: Pending) {
        let id = client.send(method, params);
        self.pending.insert(id, pending);
    }

    fn act(&mut self, client: &mut Client, method: &str, params: Value) {
        self.ask(client, method, params, Pending::Action);
    }

    fn load(&mut self, client: &mut Client) {
        let target = self.view.target.clone();
        let (method, params) = target.request();
        self.view.asked = true;
        self.ask(client, method, params, Pending::View(target));
    }

    fn open(&mut self, client: &mut Client, target: Target) {
        if self.view.target != target {
            let previous = std::mem::replace(&mut self.view, View::new(target));
            self.back.push(previous.target);
        }
        self.show_lyrics = false;
        self.focus = Focus::Main;
        self.load(client);
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(text, _)| text.as_str())
    }

    pub fn tick(&mut self, client: &mut Client) {
        let now = Instant::now();
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            self.load(client);
        }
        if self.retry_lyrics_at.is_some_and(|at| at <= now) {
            self.retry_lyrics_at = None;
            self.ask(client, "lyrics.get", json!({}), Pending::Lyrics);
        }
    }

    pub fn message(&mut self, message: Value, client: &mut Client) {
        if let Some(event) = message["event"].as_str() {
            self.event(event, &message["data"], client);
            return;
        }
        let Some(id) = message["id"].as_u64() else {
            return;
        };
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        if let Some(error) = message["error"].as_str() {
            match pending {
                Pending::View(target) if target == self.view.target => {
                    self.view.loading = false;
                    self.view.error = Some(error.to_string());
                }
                Pending::Action => self.notify(error),
                _ => {}
            }
            return;
        }
        let result = &message["result"];
        match pending {
            Pending::Hello => self.hello(result),
            Pending::Player => self.set_player(result, client),
            Pending::Playlists => self.set_playlists(result),
            Pending::View(target) => {
                if target == self.view.target {
                    self.view.fill(result);
                    if self.view.loading && self.signed_in() {
                        self.retry_at = Some(Instant::now() + Duration::from_millis(800));
                    }
                }
            }
            Pending::Lyrics => self.set_lyrics(result),
            Pending::Image(url) => {
                use base64::Engine as _;
                if let Some(data) = result["data"].as_str()
                    && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
                {
                    self.cover = Some((url, bytes));
                }
            }
            Pending::Action => {}
        }
    }

    fn event(&mut self, event: &str, data: &Value, client: &mut Client) {
        match event {
            "player" => self.set_player(data, client),
            "auth" => {
                let was = self.signed_in();
                self.hello(data);
                if !was && self.signed_in() {
                    self.ask(client, "library.playlists", json!({}), Pending::Playlists);
                    self.load(client);
                }
            }
            "library" => {
                self.ask(client, "library.playlists", json!({}), Pending::Playlists);
                if matches!(self.view.target, Target::Liked | Target::Albums) {
                    self.load(client);
                }
            }
            "queue" if self.view.target == Target::Queue => self.load(client),
            "lyrics" if self.show_lyrics => {
                self.ask(client, "lyrics.get", json!({}), Pending::Lyrics);
            }
            "page" => {
                let page = data["page"].as_str().unwrap_or_default();
                if page == "playlists" {
                    self.ask(client, "library.playlists", json!({}), Pending::Playlists);
                } else if page == self.view.target.page() || self.view.page.as_deref() == Some(page)
                {
                    self.load(client);
                }
            }
            "saved" => {
                if let Some(saved) = data[self.player.uri.as_str()].as_bool() {
                    self.player.saved = Some(saved);
                }
            }
            _ => {}
        }
    }

    fn hello(&mut self, hello: &Value) {
        self.auth = hello["auth"].as_str().unwrap_or("starting").to_string();
    }

    fn set_player(&mut self, value: &Value, client: &mut Client) {
        let previous = std::mem::replace(&mut self.player, Player::from(value));
        if previous.uri != self.player.uri && self.show_lyrics {
            self.lyrics = None;
            self.ask(client, "lyrics.get", json!({}), Pending::Lyrics);
        }
        if self.want_art
            && let Some(url) = self.player.cover.clone()
            && previous.cover.as_deref() != Some(url.as_str())
            && self.cover.as_ref().is_none_or(|(have, _)| *have != url)
        {
            self.ask(
                client,
                "image.get",
                json!({ "url": url }),
                Pending::Image(url),
            );
        }
    }

    fn set_playlists(&mut self, reply: &Value) {
        if reply["loading"] == true && reply["rows"].as_array().is_none_or(Vec::is_empty) {
            return;
        }
        let mut side = fixed_side();
        side.push(SideRow {
            label: String::new(),
            depth: 0,
            target: None,
        });
        fn walk(rows: &Value, side: &mut Vec<SideRow>) {
            for row in list(rows) {
                let depth = row["depth"].as_u64().unwrap_or(0) as usize;
                if row["kind"] == "folder" {
                    side.push(SideRow {
                        label: text(row, "name"),
                        depth,
                        target: None,
                    });
                    walk(&row["children"], side);
                } else {
                    side.push(SideRow {
                        label: text(row, "name"),
                        depth,
                        target: Some(Target::Playlist(text(row, "id"))),
                    });
                }
            }
        }
        walk(&reply["rows"], &mut side);
        let chosen = self
            .side
            .get(self.side_selected)
            .and_then(|row| row.target.clone());
        self.side = side;
        if let Some(chosen) = chosen
            && let Some(index) = self
                .side
                .iter()
                .position(|row| row.target.as_ref() == Some(&chosen))
        {
            self.side_selected = index;
        }
        self.side_selected = self.side_selected.min(self.side.len() - 1);
    }

    fn set_lyrics(&mut self, reply: &Value) {
        let lyrics = Lyrics {
            uri: reply["uri"].as_str().map(str::to_string),
            lines: list(&reply["lines"])
                .map(|line| (line["time_ms"].as_u64(), text(line, "text")))
                .collect(),
            synced: reply["synced"] == true,
            loading: reply["loading"] == true,
            error: reply["error"].as_str().map(str::to_string),
        };
        if lyrics.loading {
            self.retry_lyrics_at = Some(Instant::now() + Duration::from_secs(1));
        }
        self.lyrics = Some(lyrics);
    }

    pub fn key(&mut self, key: KeyEvent, client: &mut Client) -> Flow {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            return Flow::Quit;
        }
        if self.focus == Focus::Input {
            self.type_query(key, client);
            return Flow::Continue;
        }
        if self.help {
            self.help = false;
            return Flow::Continue;
        }
        let page = 10;
        match key.code {
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(' ') => {
                self.player.seek(self.player.position_ms());
                self.player.playing = !self.player.playing;
                self.act(client, "player.toggle", json!({}));
            }
            KeyCode::Char('n') => self.act(client, "player.next", json!({})),
            KeyCode::Char('p') => self.act(client, "player.previous", json!({})),
            KeyCode::Left | KeyCode::Right if self.player.has_song => {
                let now = self.player.position_ms();
                let to = if key.code == KeyCode::Left {
                    now.saturating_sub(10_000)
                } else {
                    now + 10_000
                };
                self.player.seek(to);
                let position = self.player.position_ms;
                self.act(client, "player.seek", json!({ "position_ms": position }));
            }
            KeyCode::Char('s') => {
                self.player.shuffle = !self.player.shuffle;
                let on = self.player.shuffle;
                self.act(client, "player.shuffle", json!({ "on": on }));
            }
            KeyCode::Char('r') => {
                let mode = match self.player.repeat.as_str() {
                    "off" => "context",
                    "context" => "track",
                    _ => "off",
                };
                self.player.repeat = mode.to_string();
                self.act(client, "player.repeat", json!({ "mode": mode }));
            }
            KeyCode::Char('+' | '=') | KeyCode::Char('-') => {
                let up = key.code != KeyCode::Char('-');
                let volume = if up {
                    (self.player.volume + 5).min(100)
                } else {
                    self.player.volume.saturating_sub(5)
                };
                self.player.volume = volume;
                self.act(client, "player.volume", json!({ "percent": volume }));
            }
            KeyCode::Char('f') if self.player.has_song => {
                self.player.saved = Some(!self.player.saved.unwrap_or(false));
                self.act(client, "player.like", json!({}));
            }
            KeyCode::Char('L') => {
                self.show_lyrics = !self.show_lyrics;
                if self.show_lyrics {
                    self.ask(client, "lyrics.get", json!({}), Pending::Lyrics);
                }
            }
            KeyCode::Char('/') => {
                self.query.clear();
                self.focus = Focus::Input;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Side => Focus::Main,
                    _ => Focus::Side,
                };
            }
            KeyCode::Char('h') => self.focus = Focus::Side,
            KeyCode::Char('l') => self.focus = Focus::Main,
            KeyCode::Down | KeyCode::Char('j') => self.step(1, client),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1, client),
            KeyCode::PageDown => self.step(page, client),
            KeyCode::PageUp => self.step(-page, client),
            KeyCode::Char('d') if control => self.step(page, client),
            KeyCode::Char('u') if control => self.step(-page, client),
            KeyCode::Char('g') | KeyCode::Home => self.step(-100_000, client),
            KeyCode::Char('G') | KeyCode::End => self.step(100_000, client),
            KeyCode::Enter => self.enter(client),
            KeyCode::Char('a') if self.focus == Focus::Main => self.add_to_queue(client),
            KeyCode::Esc | KeyCode::Backspace => {
                if self.show_lyrics {
                    self.show_lyrics = false;
                } else if let Some(target) = self.back.pop() {
                    self.view = View::new(target);
                    self.load(client);
                }
            }
            _ => {}
        }
        Flow::Continue
    }

    fn type_query(&mut self, key: KeyEvent, client: &mut Client) {
        match key.code {
            KeyCode::Esc => self.focus = Focus::Main,
            KeyCode::Enter => {
                let query = self.query.trim().to_string();
                if query.is_empty() {
                    self.focus = Focus::Main;
                } else {
                    self.open(client, Target::Search(query));
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(character) => self.query.push(character),
            _ => {}
        }
    }

    fn step(&mut self, by: isize, client: &mut Client) {
        if self.focus == Focus::Side {
            let last = self.side.len() as isize - 1;
            let mut index = (self.side_selected as isize + by).clamp(0, last);
            while self.side[index as usize].label.is_empty() {
                let next = index + by.signum();
                if !(0..=last).contains(&next) {
                    break;
                }
                index = next;
            }
            self.side_selected = index as usize;
            return;
        }
        if self.show_lyrics {
            return;
        }
        self.view.step(by);
        if self.view.more
            && !self.view.asked_more
            && self.view.selected + 15 >= self.view.rows.len()
            && let Some(page) = self.view.page.clone()
        {
            self.view.asked_more = true;
            self.act(client, "page.more", json!({ "page": page }));
        }
    }

    fn enter(&mut self, client: &mut Client) {
        if self.focus == Focus::Side {
            if let Some(target) = self.side[self.side_selected].target.clone() {
                if let Target::Search(_) = target {
                    self.query.clear();
                    self.focus = Focus::Input;
                } else {
                    self.back.clear();
                    self.open(client, target);
                }
            }
            return;
        }
        let Some(row) = self.view.selected_row().cloned() else {
            return;
        };
        if row.unplayable {
            self.notify("Spotify can't play this song here");
            return;
        }
        match row.action {
            Action::Play(params) => self.act(client, "player.play", params),
            Action::FromQueue { index, queued } => {
                self.act(
                    client,
                    "queue.play",
                    json!({ "index": index, "queued": queued }),
                );
            }
            Action::Open(target) => self.open(client, target),
            Action::Nothing => {}
        }
    }

    fn add_to_queue(&mut self, client: &mut Client) {
        let Some(row) = self.view.selected_row() else {
            return;
        };
        let Some(uri) = row.uri.clone() else {
            return;
        };
        let name = row.title.clone();
        self.act(client, "queue.add", json!({ "uri": uri, "name": name }));
        self.notify(format!("Added {name} to the queue"));
    }
}

fn fixed_side() -> Vec<SideRow> {
    [
        ("Search", Target::Search(String::new())),
        ("Liked Songs", Target::Liked),
        ("Albums", Target::Albums),
        ("Queue", Target::Queue),
    ]
    .into_iter()
    .map(|(label, target)| SideRow {
        label: label.to_string(),
        depth: 0,
        target: Some(target),
    })
    .collect()
}

fn list(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn names(artists: &Value) -> String {
    list(artists)
        .filter_map(|artist| artist["name"].as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(name: &str) -> Value {
        json!({ "uri": format!("spotify:track:{name}"), "name": name, "artists": [{ "name": "A" }] })
    }

    #[test]
    fn a_queue_reply_becomes_sections_and_skips_headers() {
        let mut view = View::new(Target::Queue);
        view.fill(&json!({
            "current": song("now"),
            "queued": [song("mine")],
            "next": [song("x"), song("y")],
            "context": "Mix",
        }));
        let titles: Vec<&str> = view.rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Now playing",
                "now",
                "Next in queue",
                "mine",
                "Next from Mix",
                "x",
                "y"
            ]
        );
        assert_eq!(view.selected, 1);
        view.step(1);
        assert_eq!(view.selected_row().unwrap().title, "mine");
        view.step(1);
        assert_eq!(view.selected_row().unwrap().title, "x");
        assert_eq!(
            view.selected_row().unwrap().action,
            Action::FromQueue {
                index: 0,
                queued: false
            }
        );
    }

    #[test]
    fn a_playlist_song_plays_in_its_context() {
        let mut view = View::new(Target::Playlist("p".into()));
        let mut row = song("a");
        row["spotify_position"] = json!(4);
        view.fill(&json!({ "title": "Mix", "context": "spotify:playlist:p", "tracks": [row] }));
        assert_eq!(view.title, "Mix");
        assert_eq!(
            view.rows[0].action,
            Action::Play(json!({
                "uri": "spotify:track:a",
                "context": "spotify:playlist:p",
                "spotify_position": 4,
            }))
        );
    }

    #[test]
    fn search_results_open_albums_and_playlists() {
        let mut view = View::new(Target::Search("q".into()));
        view.fill(&json!({
            "tracks": [song("a")],
            "albums": [{ "kind": "album", "id": "b", "name": "B" }],
            "playlists": [],
        }));
        let titles: Vec<&str> = view.rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["Songs", "a", "Albums", "B"]);
        assert_eq!(view.rows[3].action, Action::Open(Target::Album("b".into())));
    }

    #[test]
    fn the_position_holds_while_paused() {
        let mut player = Player::from(&json!({
            "has_song": true, "playing": false, "position_ms": 5000, "duration_ms": 9000,
        }));
        assert_eq!(player.position_ms(), 5000);
        player.seek(20_000);
        assert_eq!(player.position_ms(), 9000);
    }
}
