//! The application: state, event handling, and the actions views ask for.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::api::PlayRequest;
use crate::api::models::{
    Album, Artist, ArtistRef, Device, PlayableItem, PlaybackState, Playlist, PlaylistItem, Queue,
    Show, Track, TrackCount, User, UserRef,
};
use crate::backend::{
    ApiRequest, ApiResponse, AuthStatus, Backend, Command, Event, JamRequest, LocalPlayback,
    LyricsRequest, PLAYLIST_PAGE_SIZE, PlaylistCacheRows, RecentsFor, RemoteAction, Waker,
};
use crate::i18n::{Locale, gettext, ngettext};
use crate::model::QueueTab;
use crate::model::*;
use crate::paths::AppDirs;
use crate::player::{EngineConfig, LoadSpec, LocalState, Playback, PlayerCommand, RepeatMode};
use crate::settings::{CachedRootlist, SessionState, Settings};
use crate::util;

type UiCtx<'a> = Option<()>;
use fastframe_now_playing::{
    Command as MediaCommand, NowPlaying as MediaControls, Playback as MediaPlayback,
    Repeat as MediaRepeat, State as MediaState, Track as MediaTrack,
};

const REMOTE_POLL_ACTIVE: Duration = Duration::from_secs(4);
const REMOTE_POLL_IDLE: Duration = Duration::from_secs(20);
/// The shortest regular gap between playback polls, which spend the Web API
/// quota.
const REMOTE_POLL_MIN: Duration = Duration::from_secs(15);
/// How long after a command to another device, or a switch to one, polls are
/// not held to `REMOTE_POLL_MIN`, so the result is confirmed before the
/// optimistic holds run out.
const REMOTE_COMMAND_WINDOW: Duration = Duration::from_secs(10);
const REMOTE_FRESH: Duration = Duration::from_secs(60);
const REMOTE_REPEAT_HOLD: Duration = Duration::from_secs(5);
const DEVICES_FRESH: Duration = Duration::from_secs(12);
const QUEUE_FRESH: Duration = Duration::from_secs(20);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(280);
/// How old the saved resume point may grow while music plays.
const SESSION_REFRESH: Duration = Duration::from_secs(30);
/// How far into a song Previous restarts it rather than stepping back,
/// matching what librespot does during playback.
const RESTART_BEFORE_PREVIOUS: u32 = 3_000;

const TOAST_LIFETIME: Duration = Duration::from_millis(3200);
/// Match other interface animations. egui subtracts the predicted frame time
/// from delayed repaints, so 33 ms drives roughly one frame per 16 ms.
const OPTIMISTIC_HOLD: Duration = Duration::from_millis(2500);

/// Where a seek the listener made `elapsed` ago has got to: on from the
/// seek while the song plays, never past its end.
fn held_position(position: u32, elapsed: Duration, playing: bool, duration_ms: u32) -> u32 {
    if playing {
        let elapsed = u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX);
        position
            .saturating_add(elapsed)
            .min(duration_ms.max(position))
    } else {
        position
    }
}

/// How long a newly started context remains visible while Spotify catches up.
/// During local takeover, Spotify may briefly alternate between old and new
/// context state.
const ASSUMED_CONTEXT_HOLD: Duration = Duration::from_secs(8);
/// How long the interface trusts its own play/pause over a polled state that
/// has not caught up yet. Spotify can take a moment to report a command it
/// has already carried out.
const PLAYBACK_HOLD: Duration = Duration::from_secs(6);
/// Delay before checking playback again after a command.
const REMOTE_RECHECK: Duration = Duration::from_millis(1200);
/// Delay before checking the queue after a local change.
const QUEUE_RECHECK: Duration = Duration::from_millis(700);
/// Number of stale queue responses accepted before trusting Spotify's state.
const QUEUE_STALE_RETRIES: u8 = 6;
/// Duplicate queue requests within this window count as one click.
const QUEUE_ADD_DEBOUNCE: Duration = Duration::from_millis(1500);
/// How many played contexts the sidebar's Recently played order keeps.
const RECENT_CONTEXTS_KEPT: usize = 60;
const CONTAINS_BATCH: usize = 40;

pub struct RemoteSnapshot {
    pub state: PlaybackState,
    pub received_at: Instant,
}

/// Keep the chosen volume until a poll sent after its write completes can
/// replace it. A cached snapshot or an in-flight poll cannot confirm a write.
struct PendingRemoteVolume {
    percent: u8,
    device_id: Option<String>,
    request: u64,
    /// Polls already sent when the write completed still describe the past.
    /// None while the write is waiting for Spotify.
    after_poll: Option<u64>,
    mismatches: u8,
}

/// A context shown as playing before Spotify confirms it.
struct AssumedContext {
    uri: String,
    /// Shuffle state included in the play request, if any.
    shuffle: Option<bool>,
    at: Instant,
}

/// A track the interface shows before playback has reported its new state.
///
/// Local engine events are ordered, so its next track report settles the
/// intent. Remote playback is polled and may lag; one contradictory poll is
/// ignored and a second one settles on what Spotify actually reports.
struct TrackIntent {
    uri: String,
    position_ms: u32,
    at: Instant,
    confirmation: TrackConfirmation,
}

/// What saving the edit dialog sends: only the details that changed.
/// See [`App::playlists_check`].
struct PlaylistsCheck {
    started: Instant,
    /// The account's playlists as the session lists them, once it has.
    rootlist: Option<Vec<String>>,
    /// The saved list has been read from disk, or found missing.
    cache_settled: bool,
}

struct PlaylistDetailChanges {
    name: Option<String>,
    description: Option<String>,
    public: Option<bool>,
    /// The description was cleared, which Spotify doesn't allow.
    kept_description: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackConfirmation {
    Local,
    Remote { after_poll: u64, mismatches: u8 },
}

/// Upcoming context order before shuffle was toggled, used to detect lagging responses.
#[derive(Clone, Debug)]
struct QueueShufflePending {
    current_uri: Option<String>,
    context_uris: Vec<String>,
    at: Instant,
}

/// The playing item as the interface sees it, whichever device plays it.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    pub local: bool,
    pub device_name: Option<String>,
    pub uri: String,
    pub id: Option<String>,
    pub title: String,
    pub artists: Vec<ArtistRef>,
    pub subtitle: String,
    pub album_name: String,
    pub album_id: Option<String>,
    pub show_id: Option<String>,
    pub art_url: Option<String>,
    pub art_small: Option<String>,
    pub duration_ms: u32,
    pub position_ms: u32,
    pub playing: bool,
    pub loading: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub volume_percent: u8,
    pub can_control: bool,
    /// Whether Spotify lets this app change the device's volume. A phone
    /// playing over Bluetooth, for one, reports that it does not.
    pub can_set_volume: bool,
    pub is_episode: bool,
    /// The remembered song from the last session, shown paused before a
    /// first press. Nothing is playing yet.
    pub resuming: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Local,
    Remote(Option<String>),
}

struct PendingAlbumQueue {
    id: String,
    label: String,
    target: Target,
    offset: u32,
    tracks: Vec<Track>,
}

struct PendingQueueAdd {
    item: PlayableItem,
    at: Instant,
    /// Position in `manual_queue`, adjusted whenever an earlier row leaves.
    manual_index: usize,
    /// Remote write and position within it, until Spotify accepts this copy.
    write: Option<(u64, usize)>,
}

/// Song links pasted into a playlist, in clipboard order, while some of
/// the songs are still being looked up.
struct PendingPaste {
    playlist_id: String,
    playlist_name: String,
    uris: Vec<String>,
    /// The songs known so far, by the pasted URI.
    found: HashMap<String, PlayableItem>,
    /// Links Spotify had no song for, or that name an unknown episode.
    missing: HashSet<String>,
}

impl PendingPaste {
    fn settled(&self) -> bool {
        self.uris
            .iter()
            .all(|uri| self.found.contains_key(uri) || self.missing.contains(uri))
    }
}

/// How the application is being started.
#[derive(Clone, Copy, Debug)]
pub struct AppOptions {
    /// Demo and isolated tests must not read or migrate real Spotify grants.
    pub restore_sign_in: bool,
    /// Register with macOS Now Playing and the media keys.
    pub media_controls: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            restore_sign_in: true,
            media_controls: true,
        }
    }
}

/// Listening time for the current track.
///
/// `listened` stores completed intervals. `playing_since` starts the current
/// interval and is `None` while paused.
struct Listening {
    uri: String,
    listened: std::time::Duration,
    playing_since: Option<Instant>,
    recorded: bool,
}

/// Explicit fetch/error state for karaoke's CoreML vocal-separation model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum KaraokeModelState {
    #[default]
    Idle,
    Downloading,
    Failed(String),
}

/// Explicit state for Spotify Jam sessions and incoming invitations.
#[derive(Clone, Debug, Default)]
pub(crate) struct JamState {
    pub active: Option<crate::jam::Jam>,
    pub busy: bool,
    pub pending_invite: Option<crate::jam::Invite>,
}

/// A song's lyrics' words, placed in time.
pub struct LyricWords {
    pub uri: String,
    pub lines: crate::words::Lines,
}

pub struct App {
    pub dirs: AppDirs,
    pub settings: Settings,
    /// Proxy policy actually handed to network workers. Manual form edits are
    /// drafts until Apply (or Sign in), so unrelated downloads and engine
    /// restarts must not observe them early.
    applied_proxy: crate::settings::ProxyConfig,
    applied_proxy_preferences: crate::settings::ProxyPreferences,
    pending_proxy_preferences: HashMap<u64, crate::settings::ProxyPreferences>,
    proxy_request: u64,
    last_proxy_applied: u64,
    proxy_form_edited: bool,
    settings_dirty: bool,
    last_settings_save: Instant,
    pub backend: Backend,
    files: crate::file_writer::FileWriter,
    media_controls: Option<MediaControls>,
    /// The desktop's light or dark preference, for "Follow system".
    /// The artwork the media controls were last given, and the URL it came
    /// from. Finding the file touches the disk and the controls are synced
    /// every frame, so the answer is kept until the artwork changes.
    media_art: Option<(String, PathBuf)>,
    /// Whether the tray menu last offered Pause rather than Play.
    pub window_hidden: bool,
    /// The window should close but the process should stay in the tray.
    pub hide_intent: bool,
    /// The outer loop should recreate the hidden window.
    pub wants_show: bool,
    /// Started with `--background` for a front end: no window at first, and
    /// closing the window keeps the app running.
    pub serve_in_background: bool,
    /// The local API front ends such as external clients drive (src/server.rs).
    api: Option<crate::server::Server>,
    /// A start-up check of the saved playlists against the account's
    /// playlist list over the streaming session, before reading them all
    /// again over the shared app's quota.
    playlists_check: Option<PlaylistsCheck>,
    /// Changes a playlist rename is waiting on Spotify to confirm, to apply
    /// to the library's list once it does, instead of reading it again.
    pending_playlist_details: HashMap<String, PlaylistDetailChanges>,
    /// What the API's clients were last told.
    api_pushed: api::Pushed,
    /// Sample data is loaded; Spotify requests are disabled.
    pub offline: bool,
    /// The interface language, always English.
    pub locale: crate::i18n::Locale,

    pub auth: AuthStatus,
    pub user: Option<User>,
    pub local_device_id: Option<String>,
    /// Ignore the previous local song until a Connect transfer reports its track.
    local_transfer_sequence: Option<u64>,
    /// Local playback is authorized and the engine is connected.
    pub local_ready: bool,
    pub local_playback: LocalPlayback,
    pub local: LocalState,
    pub remote: Option<RemoteSnapshot>,
    remote_polled_at: Instant,
    remote_poll_pending: bool,
    /// When a poll last went out early for a song ending on another device.
    remote_end_polled_at: Option<Instant>,
    /// When a command to another device, or a switch to one, last succeeded.
    remote_command_at: Option<Instant>,
    /// Serial of the newest playback poll sent; older answers are stale.
    remote_poll_seq: u64,
    /// The restorable session (sorts, recents, resume point) changed and
    /// should be written shortly, not only at exit.
    pub session_dirty: bool,
    last_session_save: Instant,
    /// The saved zoom has been applied to the context once.
    pub devices: Vec<Device>,
    /// Receivers seen on the local network. Spotify lists a receiver only
    /// once it has an account, so these are the ones it cannot see yet.
    pub receivers: Vec<crate::zeroconf::Receiver>,
    /// The receiver currently being handed the account, by name.
    pub activating_receiver: Option<String>,
    pub devices_loading: bool,
    devices_fetched_at: Option<Instant>,
    pub selected_device: Option<String>,
    pub queue: Loadable<Queue>,
    queue_fetched_at: Option<Instant>,
    /// Latest queue request sequence. Older responses are discarded.
    queue_seq: u64,
    /// When to retry after a stale queue response.
    queue_recheck_at: Option<Instant>,
    queue_stale_retries: u8,
    /// Rows removed by Clear queue, used to detect stale responses.
    queue_cleared: Option<(std::collections::HashSet<String>, Instant)>,
    /// Upcoming context order before shuffle was toggled, used to detect lagging responses.
    queue_shuffle_pending: Option<QueueShufflePending>,
    /// When a local reorder or positional insert last landed, used to
    /// reject a fetch whose queued rows still show the pre-move order.
    queue_reorder_pending: Option<Instant>,

    pub library: Library,
    liked_songs: crate::liked::LikedSongs,
    liked_recheck_at: Option<Instant>,
    pub home: HomeData,
    /// Local play history. See [`crate::history`].
    pub plays: crate::history::History,
    /// Current track timing used to decide when a play counts.
    listening: Option<Listening>,
    pub recents: crate::model::CursorList<crate::api::models::PlayHistory>,
    /// The Recent tab's rows: what was played here and what
    /// Spotify knows of the other devices, as one list. Rebuilt when
    /// either side changes rather than every frame, and shared so that
    /// drawing it never copies the list.
    pub recents_view: std::sync::Arc<[crate::api::models::PlayHistory]>,
    pub recents_generation: u64,
    pub queue_tab: QueueTab,
    pub search: SearchState,
    pub playlist_pages: HashMap<String, PlaylistPage>,
    /// One checkpoint snapshot at a time, including pages evicted while it writes.
    playlist_cache_write_in_flight: bool,
    load_generation: u64,
    pub album_pages: HashMap<String, AlbumPage>,
    pub artist_pages: HashMap<String, ArtistPage>,
    pub show_pages: HashMap<String, ShowPage>,
    /// Profile pages by user id.
    pub user_pages: HashMap<String, UserPage>,
    /// Radio pages by the seed's URI.
    pub radio_pages: HashMap<String, RadioPage>,
    pub track_cache: HashMap<String, Track>,
    track_requests: HashSet<String>,
    /// Album URIs already resolved or attempted through librespot this session.
    album_types_requested: HashSet<String>,
    /// Saved shows Spotify marks as audiobooks, which librespot cannot play;
    /// the Podcasts shelf leaves them out.
    pub audiobook_shows: HashSet<String>,
    /// Saved shows already asked about.
    audiobooks_requested: HashSet<String>,
    /// Headers the streaming session read for library playlists the Web API
    /// listed blank, by playlist id, so a reloaded list fills at once.
    playlist_headers: HashMap<String, Playlist>,
    /// Blank library playlists already asked about.
    playlist_headers_requested: HashSet<String>,
    /// Album URIs positively identified as EPs by librespot.
    confirmed_ep_albums: HashSet<String>,
    page_used: HashMap<Page, Instant>,
    track_used: HashMap<String, Instant>,

    pub history: Vec<Page>,
    pub history_index: usize,

    pub saved: HashMap<String, bool>,
    saved_pending: HashSet<String>,
    /// Spotify may expose one recording under several market-specific track
    /// URIs. Map those URIs to the recording identity returned by the API.
    track_recordings: HashMap<String, String>,
    /// `track_recordings` the other way round: each recording's URIs, so
    /// whether a recording is saved is answered from its own few URIs
    /// rather than by scanning every saved URI, once per track of a page.
    recording_uris: HashMap<String, HashSet<String>>,
    /// Known playlist-track availability for this account. Keep positive
    /// answers too, so an older disk cache cannot make a song unavailable
    /// again after the Web API has confirmed it can play.
    playlist_availability: HashMap<String, bool>,
    /// Recording identities for which at least one known URI is saved.
    saved_recordings: HashSet<String>,
    /// Optimistic library writes that a stale contains response must not undo.
    saved_writes: HashMap<String, bool>,
    /// Covers whose download failed, and when, so their colour is asked for
    /// again after a pause rather than never or on every frame.
    /// Home has shelves from Spotify that the disk copy does not hold yet.
    home_cache_dirty: bool,

    pub dialog: Option<Dialog>,
    cover_request: u64,
    cover_uploads: HashMap<String, u64>,
    /// Successful uploads stay visible while Spotify propagates the new image.
    uploaded_covers: std::collections::HashMap<String, crate::playlist_cover::PendingCover>,
    pub show_queue_panel: bool,
    pub show_lyrics_panel: bool,
    pub lyrics_fullscreen: Option<bool>,
    pub lyrics_fullscreen_seen: bool,
    lyrics_fullscreen_restoring: Option<bool>,
    lyrics_restore_maximized: bool,
    /// The track the lyrics below are for.
    pub lyrics_uri: Option<String>,
    /// How many of `LYRICS_RETRIES` this song has used.
    lyrics_retries: usize,
    /// When to ask again for lyrics a busy service didn't answer for.
    lyrics_retry_at: Option<Instant>,
    /// `Loaded(None)` when no lyrics are available.
    pub lyrics: Loadable<Option<crate::lyrics::Lyrics>>,
    /// The playing song's words, placed in time, once they have been.
    pub lyric_words: Option<LyricWords>,
    /// The song after the playing one whose words were last made ready.
    words_prepared: Option<String>,
    /// Whether the panel follows the current line. Manual scrolling disables
    /// it until Follow is used or the track changes.
    pub lyrics_following: bool,
    /// The line the panel last positioned itself for (`Some(None)` before
    /// the first line), so it moves once per change; `None` until it has
    /// positioned itself at all for this track.
    pub lyrics_line_shown: Option<Option<usize>>,
    pub show_devices: bool,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    volume_before_mute: Option<u8>,
    /// Context and track URIs whose pending play buttons show a spinner.
    pending_play_keys: Vec<String>,
    pending_play_at: Option<Instant>,
    /// A play request made while the local engine was still connecting; it
    /// starts the moment the engine reports ready.
    queued_play: Option<PlayRequest>,
    /// Last list sent to local playback. Used for autoplay because librespot
    /// cannot continue a list without a context URI.
    local_list: Option<Vec<String>>,
    /// Activated receiver waiting to appear in Spotify's device list.
    pending_transfer_to: Option<(String, Instant)>,
    /// When to take a confirming look at remote playback after a command.
    remote_recheck_at: Option<Instant>,
    pub seek_preview: Option<f32>,
    pub volume_preview: Option<f32>,
    /// Window geometry to restore on next attach, from the session file.
    session_window_size: Option<[f32; 2]>,
    /// The mode to return the window to when the app closed in
    /// fullscreen lyrics.
    session_window_pos: Option<[f32; 2]>,
    /// Last observed window geometry, updated each frame for saving.
    last_window_size: Option<[f32; 2]>,
    /// Where the open dialog drew itself, so its own layout can be held
    /// to the window it has to fit inside.
    last_window_pos: Option<[f32; 2]>,
    last_eviction: Instant,
    /// Playback snapshot for the current frame, built once per redraw.
    frame_now: Option<NowPlaying>,
    pub sign_in_url: Option<String>,
    /// The verified personal Web API application, when acceleration is ready.
    pub web_app: Option<String>,
    /// A shared grant from before 0.14.2 is signed in: Spotify can stop
    /// accepting it at any time, so Settings offers the personal app.
    pub shared_grant: bool,
    /// The proxy request that carries a sign-in, until the proxy is applied.
    sign_in_request: Option<u64>,
    pending_remote_position: Option<(u32, Instant)>,
    pending_remote_volume: Option<PendingRemoteVolume>,
    remote_volume_request: u64,
    /// A local volume set here that the engine has not echoed back yet. It
    /// reports `VolumeChanged` asynchronously while position snapshots land
    /// every second, so a snapshot must not undo the change on its way past.
    pending_local_volume: Option<(u16, Instant)>,
    /// A local seek position set here that the engine has not confirmed yet.
    pending_local_position: Option<(u32, Instant)>,
    optimistic_playing: Option<(bool, Instant)>,
    /// Track shown immediately after a play or skip, until playback reports.
    intent_track: Option<TrackIntent>,
    /// Requested shuffle mode, applied to every context until changed.
    shuffle_wanted: bool,
    /// The repeat mode saved by the last run, which the first local load
    /// starts with: the engine starts at off.
    repeat_wanted: RepeatMode,
    /// Last local shuffle change, used to ignore its echo from the engine.
    shuffle_set_at: Option<Instant>,
    repeat_hold: Option<(String, Instant)>,
    remote_read_failed: bool,
    /// When tracks recently came up unavailable, to spot a key-service
    /// cascade and reconnect once instead of skipping through an album.
    unavailable_at: Vec<Instant>,
    last_unavailable_reconnect: Option<Instant>,
    /// The Premium notice has been shown for this sign-in.
    premium_notice_shown: bool,
    /// Context shown immediately after play, until Spotify confirms it. An
    /// empty URI means a plain track list, whose lack of a context must also
    /// hide stale state.
    assumed_context: Option<AssumedContext>,
    last_now_playing_uri: Option<String>,
    last_now_playing_sequence: u64,
    /// A requested start whose effect on the manual queue is already applied.
    /// Context loads keep it; explicit skips consume their rows immediately.
    queue_start_pending: Option<Target>,
    pub playlist_busy: bool,
    pub quit_requested: bool,
    /// How each table is sorted, per page, for as long as the app runs.
    pub table_sorts: HashMap<Page, TableSort>,
    /// User ids resolved to display names; `None` while unknown, so an id
    /// is asked about only once per run.
    pub user_names: HashMap<String, Option<String>>,
    pub user_names_revision: u64,
    /// Context URIs most recently played, newest first: the sidebar's
    /// order. Kept with the session, so it survives a restart.
    pub recent_contexts: Vec<String>,
    /// What was playing when the app last closed, to resume from cold.
    pub resume_context: Option<String>,
    pub resume_track: Option<String>,
    pub resume_position_ms: u32,
    /// The song and position a resume asked its context to start at, until
    /// the player reports what it started.
    resume_load: Option<(String, u32)>,
    /// Manually queued songs restored with the remembered track.
    pub resume_queue: Vec<String>,
    /// Manually queued songs from this session, oldest first.
    pub manual_queue: Vec<String>,
    /// Queue additions shown before Spotify confirms them, with request time.
    pending_queue_adds: Vec<PendingQueueAdd>,
    pending_album_queues: HashMap<u64, PendingAlbumQueue>,
    pending_queue_batches: HashMap<u64, Target>,
    album_queue_serial: u64,
    last_album_queue: Option<(String, Instant)>,
    /// The account's playlist tree from Spotify, folders and all; empty
    /// until the session answers.
    pub rootlist: Vec<crate::player::RootlistEntry>,
    /// Last good tree and the account it belongs to, kept across restarts.
    rootlist_cache: Option<CachedRootlist>,
    /// Playlists the account may add songs to by Spotify's own word, by
    /// URI: the ones shared with it by invitation, which the Web API's
    /// collaborative flag does not show. Empty until the session answers.
    pub editable_by_grant: std::collections::BTreeSet<String>,
    /// A Spotify link handed over from outside, waiting for the account
    /// to be signed in and, for a song, for its album to be known.
    pending_link: Option<String>,
    /// The songs behind the links last copied from a track table, so a
    /// paste of those links adds rows with their names straight away.
    copied_songs: Vec<PlayableItem>,
    /// The songs a client described for the playlist it is creating, so
    /// the new playlist's rows are whole at once.
    created_with: Vec<PlayableItem>,
    /// Pasted song links waiting for Spotify to describe the songs this
    /// app has not seen, before they are added to their playlist.
    pending_pastes: Vec<PendingPaste>,
    /// Sidebar folders rolled up, by their rootlist ids.
    pub collapsed_folders: Vec<String>,
    /// The equalizer's settings, shared with the player.
    eq: crate::eq::SharedEq,
    /// Karaoke, shared with the player.
    pub(crate) karaoke: crate::karaoke::SharedKaraoke,
    /// Explicit download/error state for the karaoke vocal model.
    pub(crate) karaoke_model: KaraokeModelState,
    /// Explicit Jam state (active session, busy flag, pending invitation).
    pub(crate) jam_state: JamState,
    /// URI -> saved state deltas accumulated since the last API push.
    pub(crate) saved_deltas: HashMap<String, bool>,
}

/// How many plays the Home shelf asks for: it shows sixteen cards.
const HOME_RECENTS: u32 = 50;
/// How many saved podcasts Home reads new episodes from, most recently
/// saved first. Each costs one request per Home refresh.
const HOME_PODCAST_SHOWS: usize = 8;
/// How many plays the Recents tab asks for at a time. Spotify's own
/// endpoint limit is fifty. A shorter page marks the end.
const RECENTS_PAGE: u32 = 50;

/// Who the desktop's media controls belong to. Links to Spotify, as
/// `spotify:` URIs or web addresses, are what they may ask SwiftSpot to open.
fn media_app() -> fastframe_now_playing::App {
    let mut app = fastframe_now_playing::App::new("swiftspot", "SwiftSpot");
    app.uri_schemes = vec!["spotify".into(), "https".into(), "http".into()];
    app
}

/// How long to wait before each new try at lyrics that failed.
const LYRICS_RETRIES: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
    Duration::from_secs(120),
];

/// Files `uri` under the recording `key` in `track_recordings` and in its
/// reverse index, `recording_uris`, so the two always agree.
fn link_recording(
    track_recordings: &mut HashMap<String, String>,
    recording_uris: &mut HashMap<String, HashSet<String>>,
    uri: String,
    key: String,
) {
    if let Some(previous) = track_recordings.insert(uri.clone(), key.clone())
        && previous != key
        && let Some(uris) = recording_uris.get_mut(&previous)
    {
        uris.remove(&uri);
        if uris.is_empty() {
            recording_uris.remove(&previous);
        }
    }
    recording_uris.entry(key).or_default().insert(uri);
}

impl App {
    pub fn new(waker: &Waker, dirs: AppDirs, mut settings: Settings, options: AppOptions) -> Self {
        // The legacy password file has no endpoint of its own. Keep the old
        // settings beside it until migration binds that password in the store.
        settings.proxy_password_legacy |= dirs.proxy_secret_file().try_exists().unwrap_or(true);
        let plays = crate::history::History::load(&dirs.history_file());
        let eq = crate::eq::shared();
        if let Ok(mut shared) = eq.lock() {
            *shared = eq_settings(&settings);
        }
        let karaoke = crate::karaoke::Karaoke::shared();
        karaoke.set_vocal_level(settings.karaoke_vocal_level);
        if crate::karaoke::model::installed(&dirs.karaoke_dir()) {
            karaoke.set_loader(Some(karaoke_loader(&dirs)));
        }
        let applied_proxy_preferences = settings.proxy_preferences();
        let applied_proxy = match settings.proxy_config() {
            Ok(proxy) => proxy,
            Err(error) => crate::settings::ProxyConfig::Invalid(error),
        };
        let engine_config = engine_config(
            &dirs,
            &settings,
            applied_proxy.clone(),
            std::sync::Arc::clone(&eq),
            std::sync::Arc::clone(&karaoke),
        );
        let backend = Backend::spawn(
            dirs.clone(),
            engine_config,
            settings.web_client_id.clone(),
            waker.clone(),
            options.restore_sign_in,
        );
        let locale = crate::i18n::Locale::English;
        let applied_proxy = if options.restore_sign_in {
            crate::settings::ProxyConfig::Invalid(
                gettext(locale, "Restoring proxy settings").into_owned(),
            )
        } else {
            applied_proxy
        };
        let session = SessionState::load(&dirs.session_file());
        let wake = waker.clone();
        let media_controls = options
            .media_controls
            .then(|| MediaControls::start(media_app(), move || wake.wake()));
        let media_controls = {
            let mut media_controls = media_controls;
            if let (Some(controls), Some(track)) =
                (&mut media_controls, session.last_track.as_deref())
            {
                controls.claim(
                    track,
                    Duration::from_millis(u64::from(session.last_position_ms)),
                );
            }
            media_controls
        };

        let first_page = session
            .last_page
            .as_deref()
            .and_then(Page::decode)
            .filter(|page| !matches!(page, Page::Settings | Page::Queue))
            .unwrap_or(Page::Home);

        let mut app = Self {
            dirs,
            settings,
            applied_proxy,
            applied_proxy_preferences,
            pending_proxy_preferences: HashMap::new(),
            proxy_request: 0,
            last_proxy_applied: 0,
            proxy_form_edited: false,
            settings_dirty: false,
            last_settings_save: Instant::now(),
            backend,
            files: crate::file_writer::FileWriter::new(),
            media_controls,
            media_art: None,
            window_hidden: false,
            hide_intent: false,
            wants_show: false,
            serve_in_background: false,
            playlists_check: None,
            pending_playlist_details: HashMap::new(),
            api: None,
            api_pushed: Default::default(),
            offline: false,
            locale,
            auth: AuthStatus::Starting,
            user: None,
            local_device_id: None,
            local_transfer_sequence: None,
            local_ready: false,
            local_playback: LocalPlayback::Unavailable,
            local: LocalState::default(),
            remote: None,
            remote_polled_at: Instant::now() - REMOTE_POLL_IDLE,
            remote_poll_pending: false,
            remote_end_polled_at: None,
            remote_command_at: None,
            remote_poll_seq: 0,
            session_dirty: false,
            last_session_save: Instant::now(),
            devices: Vec::new(),
            receivers: Vec::new(),
            activating_receiver: None,
            devices_loading: false,
            devices_fetched_at: None,
            selected_device: None,
            queue: match &session.last_track {
                Some(current) if !session.last_queue_rows.is_empty() => {
                    // The queue as it was at close, shown until something
                    // plays; then the live queue takes over. One saved by
                    // an earlier version may still hold Spotify's padding.
                    let mut rows = session.last_queue_rows.clone();
                    if RepeatMode::from_api(&session.repeat) == RepeatMode::Off {
                        let queued = Self::end_of_queued_rows(&rows, &session.last_added_queue);
                        Self::drop_wraparound(&mut rows, queued, current);
                    }
                    Loadable::Loaded(Queue {
                        currently_playing: None,
                        queue: rows,
                    })
                }
                _ => Loadable::NotLoaded,
            },
            queue_fetched_at: None,
            queue_seq: 0,
            queue_recheck_at: None,
            queue_stale_retries: 0,
            queue_cleared: None,
            queue_shuffle_pending: None,
            queue_reorder_pending: None,
            library: Library::default(),
            liked_songs: crate::liked::LikedSongs::default(),
            liked_recheck_at: None,
            home: HomeData::default(),
            plays,
            listening: None,
            recents: crate::model::CursorList::default(),
            recents_view: std::sync::Arc::from([]),
            recents_generation: 0,
            queue_tab: session
                .queue_tab
                .as_deref()
                .and_then(QueueTab::decode)
                .unwrap_or_default(),
            search: SearchState::default(),
            playlist_pages: HashMap::new(),
            playlist_cache_write_in_flight: false,
            load_generation: 0,
            album_pages: HashMap::new(),
            artist_pages: HashMap::new(),
            show_pages: HashMap::new(),
            user_pages: HashMap::new(),
            radio_pages: HashMap::new(),
            track_cache: HashMap::new(),
            track_requests: HashSet::new(),
            album_types_requested: HashSet::new(),
            audiobook_shows: HashSet::new(),
            audiobooks_requested: HashSet::new(),
            playlist_headers: HashMap::new(),
            playlist_headers_requested: HashSet::new(),
            confirmed_ep_albums: HashSet::new(),
            page_used: HashMap::new(),
            track_used: HashMap::new(),
            history: vec![first_page],
            history_index: 0,
            saved: HashMap::new(),
            saved_pending: HashSet::new(),
            track_recordings: HashMap::new(),
            recording_uris: HashMap::new(),
            playlist_availability: HashMap::new(),
            saved_recordings: HashSet::new(),
            saved_writes: HashMap::new(),
            home_cache_dirty: false,
            dialog: None,
            cover_request: 0,
            cover_uploads: HashMap::new(),
            uploaded_covers: Default::default(),
            show_queue_panel: false,
            show_lyrics_panel: false,
            lyrics_fullscreen: None,
            lyrics_fullscreen_seen: false,
            lyrics_fullscreen_restoring: None,
            lyrics_restore_maximized: false,
            lyrics_uri: None,
            lyrics_retries: 0,
            lyrics_retry_at: None,
            lyrics: Loadable::NotLoaded,
            lyric_words: None,
            words_prepared: None,
            lyrics_following: true,
            lyrics_line_shown: None,
            show_devices: false,
            toasts: Vec::new(),
            actions: Vec::new(),
            volume_before_mute: None,
            pending_play_keys: Vec::new(),
            pending_play_at: None,
            queued_play: None,
            local_list: None,
            pending_transfer_to: None,
            remote_recheck_at: None,
            seek_preview: None,
            volume_preview: None,
            session_window_size: session.window_size,
            session_window_pos: session.window_pos,
            last_window_size: None,
            last_window_pos: None,
            last_eviction: Instant::now(),
            frame_now: None,
            sign_in_url: None,
            web_app: None,
            shared_grant: false,
            sign_in_request: None,
            pending_remote_position: None,
            pending_remote_volume: None,
            remote_volume_request: 0,
            pending_local_volume: None,
            pending_local_position: None,
            optimistic_playing: None,
            intent_track: None,
            shuffle_wanted: session.shuffle_on,
            repeat_wanted: RepeatMode::from_api(&session.repeat),
            shuffle_set_at: None,
            repeat_hold: None,
            remote_read_failed: false,
            unavailable_at: Vec::new(),
            last_unavailable_reconnect: None,
            premium_notice_shown: false,
            assumed_context: None,
            last_now_playing_uri: None,
            last_now_playing_sequence: 0,
            queue_start_pending: None,
            playlist_busy: false,
            quit_requested: false,
            table_sorts: session
                .sorts
                .iter()
                .filter_map(|(page, sort)| Some((Page::decode(page)?, *sort)))
                .collect(),
            user_names: HashMap::new(),
            user_names_revision: 0,
            recent_contexts: session.recent_contexts.clone(),
            resume_context: session.last_context.clone(),
            resume_track: session.last_track.clone(),
            resume_position_ms: session.last_position_ms,
            resume_load: None,
            resume_queue: session.last_added_queue.clone(),
            manual_queue: Vec::new(),
            pending_queue_adds: Vec::new(),
            pending_album_queues: HashMap::new(),
            pending_queue_batches: HashMap::new(),
            album_queue_serial: 0,
            last_album_queue: None,
            rootlist: Vec::new(),
            rootlist_cache: session.rootlist.clone(),
            editable_by_grant: std::collections::BTreeSet::new(),
            pending_link: None,
            copied_songs: Vec::new(),
            created_with: Vec::new(),
            pending_pastes: Vec::new(),
            collapsed_folders: session.collapsed_folders.clone(),
            eq,
            karaoke,
            karaoke_model: KaraokeModelState::Idle,
            jam_state: JamState::default(),
            saved_deltas: HashMap::new(),
        };
        app.local.volume = app.settings.volume;
        // What was played here is on disk and needs nothing from the
        // network, so the tab has rows before Spotify has answered.
        app.rebuild_recents();
        app
    }

    /// Connects a front end to the API (src/server.rs). Its replies and
    /// events arrive on the receiver.
    pub fn connect_front_end(
        &mut self,
        waker: &crate::backend::Waker,
    ) -> (crate::server::Inbox, std::sync::mpsc::Receiver<String>) {
        self.api
            .get_or_insert_with(crate::server::Server::default)
            .connect(waker.clone())
    }

    // ---- derived state -----------------------------------------------------

    pub fn page(&self) -> &Page {
        &self.history[self.history_index]
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.auth, AuthStatus::Connected { .. })
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user.as_ref().map(|user| user.id.as_str())
    }

    /// The library list's entry for a playlist, when it holds one.
    /// Fills library playlists the Web API listed blank from the headers
    /// the streaming session read, and asks it once about the rest.
    fn fill_blank_playlists(&mut self) {
        let Some(playlists) = self.library.playlists.get_mut() else {
            return;
        };
        let mut unread = Vec::new();
        for playlist in playlists
            .iter_mut()
            .filter(|playlist| playlist.name.is_empty() && !playlist.id.is_empty())
        {
            match self.playlist_headers.get(&playlist.id) {
                Some(header) => playlist.fill_blank_from(header),
                None => {
                    if self.playlist_headers_requested.insert(playlist.id.clone()) {
                        unread.push(playlist.id.clone());
                    }
                }
            }
        }
        if !unread.is_empty() {
            self.backend.send(Command::PlaylistHeaders(unread));
        }
    }

    fn library_entry(&self, id: &str) -> Option<&Playlist> {
        self.library
            .playlists
            .get()?
            .iter()
            .find(|playlist| playlist.id == id)
    }

    /// The signed-in account's display name, when `owner` is that account.
    fn own_name(&self, owner: Option<&str>) -> Option<String> {
        self.user
            .as_ref()
            .filter(|user| Some(user.id.as_str()) == owner)?
            .display_name
            .clone()
    }

    pub fn is_saved(&self, uri: &str) -> Option<bool> {
        let exact = self.saved.get(uri).copied();
        if exact == Some(true)
            || self
                .track_recordings
                .get(uri)
                .is_some_and(|key| self.saved_recordings.contains(key))
        {
            Some(true)
        } else {
            exact
        }
    }

    /// Album, Single, EP, Compilation or Appears On, in the interface language.
    pub(crate) fn album_kind_label(&self, album: &Album) -> Cow<'static, str> {
        if album.is_single_release() && self.confirmed_ep_albums.contains(&album.uri) {
            Cow::Borrowed("EP")
        } else {
            album.kind_label(self.locale)
        }
    }

    /// Whether any URI of the recording is saved.
    fn recording_saved(&self, key: &str) -> bool {
        self.recording_uris
            .get(key)
            .is_some_and(|uris| uris.iter().any(|uri| self.saved.get(uri) == Some(&true)))
    }

    fn remember_track_recording(&mut self, track: &Track) {
        let Some(key) = track.recording_key() else {
            return;
        };
        link_recording(
            &mut self.track_recordings,
            &mut self.recording_uris,
            track.uri.clone(),
            key.clone(),
        );
        if let Some(linked) = &track.linked_from
            && !linked.uri.is_empty()
        {
            link_recording(
                &mut self.track_recordings,
                &mut self.recording_uris,
                linked.uri.clone(),
                key.clone(),
            );
        }
        if self.recording_saved(&key) {
            self.saved_recordings.insert(key);
        }
    }

    fn record_saved(&mut self, uri: String, saved: bool) {
        let changed = self.saved.insert(uri.clone(), saved) != Some(saved);
        if changed {
            self.saved_deltas.insert(uri, saved);
        }
    }

    fn set_saved_state(&mut self, uri: String, saved: bool) {
        self.record_saved(uri.clone(), saved);
        let Some(key) = self.track_recordings.get(&uri).cloned() else {
            return;
        };
        if saved {
            self.saved_recordings.insert(key);
        } else if !self.recording_saved(&key) {
            self.saved_recordings.remove(&key);
        }
    }

    fn saved_toggle_targets(&self, uri: &str) -> Vec<String> {
        let Some(key) = self.track_recordings.get(uri) else {
            return vec![uri.to_string()];
        };
        let equivalents: Vec<String> = self
            .recording_uris
            .get(key)
            .into_iter()
            .flatten()
            .filter(|candidate| self.saved.get(*candidate) == Some(&true))
            .cloned()
            .collect();
        if equivalents.is_empty() {
            vec![uri.to_string()]
        } else {
            equivalents
        }
    }

    fn remote_fresh(&self) -> Option<&RemoteSnapshot> {
        self.remote
            .as_ref()
            .filter(|remote| self.remote_read_failed || remote.received_at.elapsed() < REMOTE_FRESH)
    }

    /// Where playback commands go: this computer's player or a remote device.
    pub fn target(&self) -> Target {
        if self.local_ready && self.local.is_active() {
            return Target::Local;
        }
        if let Some(selected) = &self.selected_device
            && Some(selected.as_str()) != self.local_device_id.as_deref()
        {
            return Target::Remote(Some(selected.clone()));
        }
        if let Some(remote) = self.remote_fresh() {
            let device = remote.state.device.as_ref();
            let is_local = device
                .and_then(|device| device.id.as_deref())
                .is_some_and(|id| Some(id) == self.local_device_id.as_deref());
            if !is_local && (remote.state.is_playing || remote.state.item.is_some()) {
                return Target::Remote(device.and_then(|device| device.id.clone()));
            }
        }
        if self.local_ready {
            Target::Local
        } else {
            Target::Remote(None)
        }
    }

    /// Context shown as playing, including pending local play requests.
    pub fn playing_context_uri(&self) -> Option<String> {
        let remote = self
            .remote
            .as_ref()
            .and_then(|remote| remote.state.context.as_ref())
            .map(|context| crate::link::canonical_context_uri(&context.uri));
        if let Some(assumed) = &self.assumed_context {
            let held = assumed.at.elapsed() < ASSUMED_CONTEXT_HOLD;
            // A filtered or sorted context plays as plain tracks and will not
            // report a context URI. Keep the assumed URI unless contradicted
            // by a poll taken since the request: one from before it still
            // tells the story from before, whatever device it describes.
            let contradicted = self.remote.as_ref().is_some_and(|snapshot| {
                snapshot.received_at > assumed.at
                    && snapshot.state.context.as_ref().is_some_and(|context| {
                        crate::link::canonical_context_uri(&context.uri) != assumed.uri
                    })
            });
            if held || self.remote_lags_local() || (!contradicted && self.believed_playing()) {
                return (!assumed.uri.is_empty()).then(|| assumed.uri.clone());
            }
        }
        remote
    }

    /// Whether the last poll names another song than this computer plays. It
    /// describes the past, however late it arrived, and its context with it:
    /// a playlist left for an album must not light up again (#1121).
    fn remote_lags_local(&self) -> bool {
        let Some(track) = self.local.track.as_ref().filter(|_| self.local.is_active()) else {
            return false;
        };
        self.remote
            .as_ref()
            .and_then(|snapshot| snapshot.state.item.as_ref())
            .is_some_and(|item| item.uri() != track.uri)
    }

    /// Track shown as current, including a recent unconfirmed play request.
    pub fn current_track_uri(&self) -> Option<String> {
        if let Some(intent) = &self.intent_track
            && (intent.at.elapsed() < PLAYBACK_HOLD || self.queued_play.is_some())
        {
            return Some(intent.uri.clone());
        }
        self.now_playing().map(|now| now.uri)
    }

    /// Shows a user-requested track until the responsible playback surface
    /// has had a chance to report what really started.
    fn expect_track(&mut self, uri: String, position_ms: u32) {
        let confirmation = match self.target() {
            Target::Local => TrackConfirmation::Local,
            Target::Remote(_) => TrackConfirmation::Remote {
                after_poll: self.remote_poll_seq,
                mismatches: 0,
            },
        };
        self.intent_track = Some(TrackIntent {
            uri,
            position_ms,
            at: Instant::now(),
            confirmation,
        });
    }

    /// The local engine is the playback authority. A real track change wins
    /// even when the queue's optimistic guess named a different track.
    fn reconcile_local_track_intent(&mut self, track_changed: bool, reported: Option<&str>) {
        let settled = self.intent_track.as_ref().is_some_and(|intent| {
            matches!(intent.confirmation, TrackConfirmation::Local)
                && (track_changed || reported == Some(intent.uri.as_str()))
        });
        if settled {
            self.intent_track = None;
        }
    }

    /// A poll issued before the command cannot settle its intent. Spotify's
    /// first later answer may still describe the old track, so a mismatch is
    /// held and checked once more. Agreement, or two later mismatches, settles
    /// on the reported state.
    fn reconcile_remote_track_intent(&mut self, poll: u64, reported: Option<&str>) {
        let mut settled = false;
        let mut recheck = false;
        if let Some(intent) = &mut self.intent_track
            && let TrackConfirmation::Remote {
                after_poll,
                mismatches,
            } = &mut intent.confirmation
            && poll > *after_poll
        {
            if reported == Some(intent.uri.as_str()) || *mismatches > 0 {
                settled = true;
            } else {
                *after_poll = poll;
                *mismatches += 1;
                recheck = true;
            }
        }
        if settled {
            self.intent_track = None;
        } else if recheck {
            self.remote_recheck_at = Some(Instant::now() + REMOTE_RECHECK);
        }
    }

    /// Playback state shown by the UI, including a recent local request.
    pub fn believed_playing(&self) -> bool {
        if let Some((playing, at)) = self.optimistic_playing
            && at.elapsed() < PLAYBACK_HOLD
        {
            return playing;
        }
        self.now_playing().is_some_and(|now| now.playing)
    }

    pub fn playing_context_shuffle(&self) -> bool {
        self.shuffle_wanted
    }

    /// Current item for menus, using cached track details when available.
    pub fn now_playing_item(&self) -> Option<PlayableItem> {
        let now = self.now_playing()?;
        if now.is_episode {
            return None;
        }
        if let Some(track) = now.id.as_deref().and_then(|id| self.track_cache.get(id)) {
            return Some(PlayableItem::Track(track.clone()));
        }
        // The album and art go along, so a playlist row added from the
        // player bar shows them before Spotify returns the song.
        let images = [(&now.art_small, 64), (&now.art_url, 640)]
            .into_iter()
            .filter_map(|(url, width)| {
                Some(crate::api::models::Image {
                    url: url.clone()?,
                    width: Some(width),
                    height: Some(width),
                })
            })
            .collect();
        Some(PlayableItem::Track(Track {
            id: now.id.clone(),
            uri: now.uri.clone(),
            name: now.title.clone(),
            artists: now.artists.clone(),
            album: Some(Album {
                uri: now
                    .album_id
                    .as_ref()
                    .map(|id| format!("spotify:album:{id}"))
                    .unwrap_or_default(),
                id: now.album_id.clone().unwrap_or_default(),
                name: now.album_name.clone(),
                images,
                ..Album::default()
            }),
            duration_ms: now.duration_ms,
            ..Track::default()
        }))
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        if let Some(now) = &self.frame_now {
            return Some(now.clone());
        }
        self.requested_track_preview()
            .or_else(|| self.now_playing_live())
            .or_else(|| self.resume_preview())
    }

    /// Keep the player bar on the same requested song as the playlist marker
    /// while the responsible playback surface catches up.
    fn requested_track_preview(&self) -> Option<NowPlaying> {
        let intent = self.intent_track.as_ref()?;
        if intent.at.elapsed() >= PLAYBACK_HOLD && self.queued_play.is_none() {
            return None;
        }
        let mut now = self.cached_track_preview(&intent.uri, intent.position_ms)?;
        let actual = self.now_playing_live();
        now.local = matches!(intent.confirmation, TrackConfirmation::Local);
        now.playing = match self.optimistic_playing {
            Some((playing, at)) if at.elapsed() < PLAYBACK_HOLD => playing,
            _ => self.queued_play.is_some() || actual.as_ref().is_some_and(|now| now.playing),
        };
        now.loading = true;
        now.resuming = false;
        if let Some(actual) = actual {
            now.device_name = actual.device_name;
            now.repeat = actual.repeat;
            now.volume_percent = actual.volume_percent;
            now.can_control = actual.can_control;
            now.can_set_volume = actual.can_set_volume;
        }
        Some(now)
    }

    /// What a device is actually playing, here or elsewhere.
    fn now_playing_live(&self) -> Option<NowPlaying> {
        if self.local.is_active() && self.local_transfer_sequence != Some(self.local.track_sequence)
        {
            let track = self.local.track.as_ref()?;
            let cached = track
                .uri
                .rsplit(':')
                .next()
                .and_then(|id| self.track_cache.get(id));
            // Playback already carries artist IDs. Keep those links available
            // while the Web API is loading (or its cached credits have no IDs).
            let artists = if track.artists.iter().any(|artist| artist.id.is_some()) {
                track.artists.clone()
            } else {
                cached
                    .map(|cached| cached.artists.clone())
                    .unwrap_or_else(|| track.artists.clone())
            };
            let playing = match self.optimistic_playing {
                Some((playing, at)) if at.elapsed() < PLAYBACK_HOLD => playing,
                _ => self.local.playback == Playback::Playing,
            };
            return Some(NowPlaying {
                local: true,
                device_name: None,
                uri: track.uri.clone(),
                id: util::uri_id(&track.uri).map(str::to_string),
                title: track.title.clone(),
                subtitle: track.artist_names(),
                artists,
                album_name: track.album.clone(),
                album_id: cached
                    .and_then(|cached| cached.album.as_ref())
                    .map(|album| album.id.clone()),
                show_id: None,
                art_url: track.art_url.clone(),
                art_small: track
                    .art_small_url
                    .clone()
                    .or_else(|| track.art_url.clone()),
                duration_ms: track.duration_ms,
                position_ms: match self.pending_local_position {
                    Some((position, at)) if at.elapsed() < OPTIMISTIC_HOLD => held_position(
                        position,
                        at.elapsed(),
                        self.local.playback == Playback::Playing,
                        track.duration_ms,
                    ),
                    _ => self.local.position_now(),
                },
                playing,
                // `Playback` keeps reading `Playing` through a load so the
                // controls stay visible; the engine's own loading flag is
                // what the spinner follows.
                loading: self.local.loading,
                shuffle: self.shuffle_wanted,
                repeat: self.local.repeat,
                volume_percent: volume_to_percent(self.local.volume),
                can_control: true,
                can_set_volume: true,
                is_episode: track.is_episode,
                resuming: false,
            });
        }
        let remote = self.remote_fresh()?;
        // Ignore a remote snapshot for this device when the local engine has
        // no track. The snapshot may predate a local stop.
        if self.local_device_id.is_some()
            && remote
                .state
                .device
                .as_ref()
                .and_then(|device| device.id.as_deref())
                == self.local_device_id.as_deref()
        {
            return None;
        }
        let item = remote.state.item.as_ref()?;
        let device = remote.state.device.as_ref();
        let playing = match self.optimistic_playing {
            Some((playing, at)) if at.elapsed() < PLAYBACK_HOLD => playing,
            _ => remote.state.is_playing,
        };
        let position = match self.pending_remote_position {
            Some((position, at)) if at.elapsed() < OPTIMISTIC_HOLD => {
                held_position(position, at.elapsed(), playing, item.duration_ms())
            }
            _ => {
                let base = remote.state.progress_ms.unwrap_or(0);
                if remote.state.is_playing {
                    (base as u64 + remote.received_at.elapsed().as_millis() as u64)
                        .min(item.duration_ms() as u64) as u32
                } else {
                    base
                }
            }
        };
        let volume = match &self.pending_remote_volume {
            Some(pending)
                if pending.device_id.as_deref()
                    == device.and_then(|device| device.id.as_deref()) =>
            {
                pending.percent
            }
            _ => device
                .and_then(|device| device.volume_percent)
                .unwrap_or(50),
        };
        let (artists, album_name, album_id, show_id, is_episode) = match item {
            PlayableItem::Track(track) => (
                track.artists.clone(),
                track
                    .album
                    .as_ref()
                    .map(|album| album.name.clone())
                    .unwrap_or_default(),
                track.album.as_ref().map(|album| album.id.clone()),
                None,
                false,
            ),
            PlayableItem::Episode(episode) => (
                Vec::new(),
                episode
                    .show
                    .as_ref()
                    .map(|show| show.name.clone())
                    .unwrap_or_default(),
                None,
                episode.show.as_ref().map(|show| show.id.clone()),
                true,
            ),
        };
        Some(NowPlaying {
            local: false,
            device_name: device.map(|device| device.name.clone()),
            uri: item.uri().to_string(),
            id: item.id().map(str::to_string),
            title: item.name().to_string(),
            subtitle: item.subtitle(),
            artists,
            album_name,
            album_id,
            show_id,
            art_url: item.image(640).map(str::to_string),
            art_small: item.image(64).map(str::to_string),
            duration_ms: item.duration_ms(),
            position_ms: position,
            playing,
            loading: false,
            shuffle: self.shuffle_wanted,
            repeat: RepeatMode::from_api(&remote.state.repeat_state),
            volume_percent: volume,
            can_control: device.is_none_or(|device| !device.is_restricted),
            can_set_volume: device.is_none_or(|device| {
                !device.is_restricted && device.supports_volume != Some(false)
            }),
            is_episode,
            resuming: false,
        })
    }

    /// Last session's track, shown paused when no device is playing.
    fn resume_preview(&self) -> Option<NowPlaying> {
        let uri = self.resume_track.as_deref()?;
        self.cached_track_preview(uri, self.resume_position_ms)
    }

    fn cached_track_preview(&self, uri: &str, position_ms: u32) -> Option<NowPlaying> {
        let track = self.track_cache.get(util::uri_id(uri)?)?;
        Some(NowPlaying {
            local: true,
            device_name: None,
            uri: uri.to_string(),
            id: track.id.clone(),
            title: track.name.clone(),
            subtitle: track.artist_names(),
            artists: track.artists.clone(),
            album_name: track
                .album
                .as_ref()
                .map(|album| album.name.clone())
                .unwrap_or_default(),
            album_id: track.album.as_ref().map(|album| album.id.clone()),
            show_id: None,
            art_url: track.image(640).map(str::to_string),
            art_small: track.image(64).map(str::to_string),
            duration_ms: track.duration_ms,
            position_ms: position_ms.min(track.duration_ms),
            playing: false,
            loading: false,
            shuffle: self.shuffle_wanted,
            repeat: self.local_repeat(),
            volume_percent: volume_to_percent(self.local.volume),
            can_control: true,
            can_set_volume: true,
            is_episode: false,
            resuming: true,
        })
    }

    /// Records who is behind a user id. A lookup that found nothing never
    /// replaces a name already known: when the session connects, names
    /// still being looked up are asked for again, and the lookup that
    /// failed may answer after the one that found the name.
    pub fn set_user_name(&mut self, id: String, name: Option<String>) {
        if name.is_none() && self.user_names.get(&id).is_some_and(Option::is_some) {
            return;
        }
        if self.user_names.get(&id) != Some(&name) {
            {
                let shown: Vec<String> = self
                    .playlist_pages
                    .iter()
                    .filter(|(_, page)| page.contributors.contains(&id))
                    .map(|(playlist, _)| format!("playlist:{playlist}"))
                    .collect();
                for page in shown {
                    self.note_api_page(page);
                }
            }
            self.user_names.insert(id, name);
            self.user_names_revision = self.user_names_revision.wrapping_add(1);
        }
    }

    /// A profile arrived. Its name also names the user wherever their id
    /// shows, as the adder of a song or a playlist's owner.
    fn receive_user_profile(
        &mut self,
        id: String,
        result: Result<crate::api::models::UserProfile, String>,
    ) {
        let profile = match result {
            Ok(profile) => {
                if profile.name != id {
                    self.set_user_name(id.clone(), Some(profile.name.clone()));
                }
                Loadable::Loaded(profile)
            }
            Err(error) => Loadable::Failed(error),
        };
        if let Some(page) = self.user_pages.get_mut(&id) {
            page.profile = profile;
        }
    }

    pub fn any_play_pending(&self) -> bool {
        self.pending_fresh() && !self.pending_play_keys.is_empty()
    }

    fn pending_fresh(&self) -> bool {
        // A request queued behind a connecting engine stays pending for as
        // long as the engine may take; an ordinary request times out fast.
        self.queued_play.is_some()
            || self
                .pending_play_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(8))
    }

    fn set_play_pending(&mut self, keys: Vec<String>) {
        self.pending_play_keys = keys;
        self.pending_play_at = Some(Instant::now());
    }

    fn clear_play_pending(&mut self) {
        self.pending_play_keys.clear();
        self.pending_play_at = None;
    }

    fn drop_queued_play(&mut self) {
        if self.queued_play.take().is_some() {
            self.clear_play_pending();
            self.intent_track = None;
            self.queue_start_pending = None;
            self.assumed_context = None;
            self.optimistic_playing = None;
        }
    }

    pub fn known_playlist(&self, id: &str) -> Option<&Playlist> {
        self.library
            .playlists
            .get()
            .and_then(|playlists| playlists.iter().find(|playlist| playlist.id == id))
            .or_else(|| {
                self.search
                    .results
                    .get()
                    .and_then(|results| results.playlists.as_ref())
                    .and_then(|playlists| playlists.items.iter().find(|playlist| playlist.id == id))
            })
            .or_else(|| {
                self.home.discover.values().find_map(|playlists| {
                    playlists
                        .get()
                        .and_then(|playlists| playlists.iter().find(|playlist| playlist.id == id))
                })
            })
    }

    pub fn known_album(&self, id: &str) -> Option<&Album> {
        self.library
            .albums
            .items
            .iter()
            .find(|saved| saved.album.id == id)
            .map(|saved| &saved.album)
            .or_else(|| {
                let results = self.search.results.get()?;
                results
                    .albums
                    .as_ref()
                    .and_then(|albums| albums.items.iter().find(|album| album.id == id))
                    .or_else(|| {
                        results.tracks.as_ref().and_then(|tracks| {
                            tracks
                                .items
                                .iter()
                                .filter_map(|track| track.album.as_ref())
                                .find(|album| album.id == id)
                        })
                    })
            })
            .or_else(|| {
                self.library
                    .liked
                    .items
                    .iter()
                    .filter_map(|saved| saved.track.album.as_ref())
                    .find(|album| album.id == id)
            })
            .or_else(|| {
                self.playlist_pages.values().find_map(|page| {
                    page.items
                        .items
                        .iter()
                        .find_map(|item| match item.playable() {
                            Some(PlayableItem::Track(track)) => {
                                track.album.as_ref().filter(|album| album.id == id)
                            }
                            _ => None,
                        })
                })
            })
            .or_else(|| {
                self.artist_pages.values().find_map(|page| {
                    page.albums
                        .values()
                        .find_map(|albums| albums.items.iter().find(|album| album.id == id))
                })
            })
            .or_else(|| {
                [
                    &self.home.top_tracks,
                    &self.home.top_songs,
                    &self.home.recommendations,
                ]
                .into_iter()
                .find_map(|tracks| {
                    tracks.get().and_then(|tracks| {
                        tracks
                            .iter()
                            .filter_map(|track| track.album.as_ref())
                            .find(|album| album.id == id)
                    })
                })
            })
    }

    pub fn known_artist(&self, id: &str) -> Option<&Artist> {
        self.library
            .artists
            .items
            .iter()
            .find(|artist| artist.id == id)
            .or_else(|| {
                self.search
                    .results
                    .get()
                    .and_then(|results| results.artists.as_ref())
                    .and_then(|artists| artists.items.iter().find(|artist| artist.id == id))
            })
            .or_else(|| {
                self.home
                    .top_artists
                    .get()
                    .and_then(|artists| artists.iter().find(|artist| artist.id == id))
            })
            .or_else(|| {
                self.artist_pages.values().find_map(|page| {
                    page.related
                        .get()
                        .and_then(|artists| artists.iter().find(|artist| artist.id == id))
                })
            })
    }

    // ---- frame ---------------------------------------------------------------

    fn handle_events(&mut self) {
        let events = self.backend.poll();
        self.handle_backend_events(events);
    }

    fn handle_backend_events(&mut self, events: Vec<Event>) {
        for event in events {
            if self.offline {
                continue;
            }
            if let Some(page) = api::page_for_event(&event) {
                self.note_api_page(page);
            }
            match event {
                Event::PlaylistCoverChecked {
                    id,
                    request,
                    images,
                    result,
                } => {
                    self.cover_checked(&id, request, images, result);
                }
                Event::Auth(status) => self.handle_auth(status),
                Event::Playback(status) => self.handle_playback(status),
                Event::Jam(jam) => self.receive_jam(jam),
                Event::JamInvite(Some(invite)) => {
                    self.jam_state.pending_invite = Some(invite);
                }
                Event::JamInvite(None) => {
                    self.jam_state.pending_invite = None;
                    self.toast_error(gettext(self.locale, "This Jam has ended"));
                }
                Event::JamFailed(message) => {
                    self.jam_state.busy = false;
                    self.toast_error(message);
                }
                Event::OpenLink(uri) => self.actions.push(Action::OpenLink(uri)),
                Event::Receivers(receivers) => self.receivers = receivers,
                Event::ReceiverActivated { name, result } => {
                    self.activating_receiver = None;
                    match result {
                        Ok(()) => {
                            self.toast(
                                gettext(self.locale, "{name} is ready").replace("{name}", &name),
                            );
                            // It takes a moment to appear in the device list.
                            self.pending_transfer_to = Some((name, Instant::now()));
                            self.devices_fetched_at = None;
                            self.refresh_devices();
                        }
                        Err(error) => self.toast_error(format!("{name}: {error}")),
                    }
                }
                Event::Local(state) => self.handle_local(*state),
                Event::Api(response) => self.handle_api(*response),
                Event::ProxyRestored { config, password } => {
                    self.handle_proxy_restored(config, password)
                }
                Event::ProxyPasswordStored => self.handle_proxy_password_stored(),
                Event::ProxyStorageFailed(error) => {
                    let message = if self.settings.proxy_password_legacy {
                        gettext(
                            self.locale,
                            "{error} The original settings file is kept intact; changed preferences are not saved yet.",
                        )
                        .replace("{error}", error.proxy_message())
                    } else {
                        error.proxy_message().to_string()
                    };
                    self.toast_error(message);
                }
                Event::ProxyApplied {
                    request,
                    config,
                    result,
                } => self.handle_proxy_applied(request, config, result),
                Event::Error(message) => self.toast_error(message),
                Event::Rootlist { result } => match result {
                    Ok(rootlist) => {
                        let account_id = self.user_id().map(str::to_owned).or_else(|| {
                            if let AuthStatus::Connected { username } = &self.auth {
                                Some(username.clone())
                            } else {
                                None
                            }
                        });
                        self.rootlist = rootlist.entries;
                        self.editable_by_grant = rootlist.editable;
                        if let Some(check) = &mut self.playlists_check {
                            check.rootlist = Some(
                                self.rootlist
                                    .iter()
                                    .filter_map(|entry| match entry {
                                        crate::player::RootlistEntry::Playlist(uri) => {
                                            Some(uri.clone())
                                        }
                                        _ => None,
                                    })
                                    .collect(),
                            );
                            self.settle_playlists_check();
                        }
                        if let Some(account_id) = account_id {
                            self.rootlist_cache = Some(CachedRootlist {
                                account_id,
                                entries: self.rootlist.clone(),
                            });
                            self.session_dirty = true;
                        }
                    }
                    Err(error) => {
                        log::warn!("rootlist unavailable: {error}");
                        if self.playlists_check.take().is_some() {
                            self.load_playlists();
                        }
                    }
                },
                Event::KaraokeModel(result) => match result {
                    Ok(_) => {
                        self.karaoke_model = KaraokeModelState::Idle;
                        self.karaoke.set_loader(Some(karaoke_loader(&self.dirs)));
                    }
                    Err(error) => {
                        self.karaoke.set_enabled(false);
                        self.karaoke_model = KaraokeModelState::Failed(error);
                    }
                },
                Event::Lyrics { uri, result } => {
                    if self.lyrics_uri.as_deref() == Some(uri.as_str()) {
                        self.lyrics = match result {
                            Ok(found) => Loadable::Loaded(found),
                            Err(error) => self.lyrics_failed(error, Instant::now()),
                        };
                        self.request_words();
                    }
                }
                Event::Words { uri, key, result } => match result {
                    Ok(lines) if self.words_key().is_some_and(|wanted| wanted == key) => {
                        self.lyric_words = Some(LyricWords { uri, lines });
                    }
                    Ok(_) => {}
                    Err(error) => log::info!("words: {uri} not placed: {error}"),
                },
                Event::PlaylistCache {
                    account_id,
                    id,
                    generation,
                    cache,
                } => {
                    self.receive_playlist_cache(&account_id, &id, generation, cache);
                }
                Event::PlaylistCacheStored {
                    account_id,
                    id,
                    generation,
                    snapshot,
                    success,
                } => {
                    self.receive_playlist_cache_stored(
                        &account_id,
                        &id,
                        generation,
                        &snapshot,
                        success,
                    );
                }
                Event::LikedSongsCache {
                    account_id,
                    generation,
                    cache,
                } => {
                    self.receive_liked_cache(&account_id, generation, cache);
                }
                Event::HomeCache { account_id, cache } => {
                    if self.user_id() == Some(account_id.as_str())
                        && let Some(cache) = cache.filter(|cache| cache.valid_for(&account_id))
                    {
                        cache.restore(&mut self.home, &mut self.library);
                    }
                    if let Some(check) = &mut self.playlists_check {
                        check.cache_settled = true;
                        self.settle_playlists_check();
                    }
                }
                Event::UserName { id, name } => {
                    self.set_user_name(id, name);
                }
                Event::UserProfile { id, result } => self.receive_user_profile(id, result),
                Event::AudiobookShows(uris) => {
                    self.audiobook_shows.extend(uris);
                }
                Event::PlaylistHeader(header) => {
                    self.playlist_headers.insert(header.id.clone(), header);
                    self.fill_blank_playlists();
                }
                Event::Radio {
                    seed,
                    generation,
                    result,
                } => self.receive_radio(&seed, generation, result),
                Event::AlbumType { uri, result } => match result {
                    Ok(true) => {
                        self.confirmed_ep_albums.insert(uri);
                    }
                    Ok(false) => {}
                    Err(error) => log::debug!("album type unavailable for {uri}: {error}"),
                },
                Event::WebApp { client_id } => self.web_app = client_id,
                Event::SharedGrant(signed_in) => self.shared_grant = signed_in,
            }
        }
    }

    fn handle_auth(&mut self, status: AuthStatus) {
        match &status {
            AuthStatus::Connected { .. } => {
                self.sign_in_url = None;
                self.reset_data();
                // The saved list may need no reading again: the session's
                // playlist list, which has no quota, says first.
                self.playlists_check = Some(PlaylistsCheck {
                    started: Instant::now(),
                    rootlist: None,
                    cache_settled: false,
                });
                self.backend.send(Command::Rootlist);
                self.ensure_loaded(self.page().clone());
                self.poll_remote(true);
            }
            AuthStatus::WaitingForBrowser { url } => self.sign_in_url = Some(url.clone()),
            AuthStatus::SignedOut => {
                self.drop_queued_play();
                self.uploaded_covers.clear();
                self.sign_in_url = None;
                self.web_app = None;
                self.shared_grant = false;
                self.user = None;
                self.premium_notice_shown = false;
                self.local = LocalState::default();
                self.local_ready = false;
                self.local_device_id = None;
                self.local_playback = LocalPlayback::Unavailable;
                self.remote = None;
                self.rootlist.clear();
                self.rootlist_cache = None;
                self.editable_by_grant.clear();
                self.session_dirty = true;
                self.reset_data();
            }
            AuthStatus::Failed(message) => {
                self.sign_in_url = None;
                self.toast_error(message.clone());
            }
            // The sign-in screen explains it; a toast would say it twice.
            AuthStatus::Refused(_) => {
                self.sign_in_url = None;
                self.drop_queued_play();
            }
            _ => {}
        }
        self.auth = status;
    }

    fn handle_playback(&mut self, status: LocalPlayback) {
        match &status {
            LocalPlayback::Ready { device_id } => {
                self.local_device_id = Some(device_id.clone());
                self.local_ready = true;
                if !self.settings.playback_authorized {
                    self.settings.playback_authorized = true;
                    self.settings_dirty = true;
                }
                // Spotify's own Home is read over the streaming session,
                // which a Home load at start-up comes before.
                if self.home.requested && !self.home.spotify_fetched {
                    self.backend.api(ApiRequest::SpotifyHome {
                        generation: self.home.generation,
                    });
                }
                if let Some(request) = self.queued_play.take() {
                    self.play_request(request, false);
                }
            }
            LocalPlayback::Unavailable => {
                self.local_ready = false;
                self.local_device_id = None;
                if self.settings.playback_authorized {
                    self.settings.playback_authorized = false;
                    self.settings_dirty = true;
                }
                self.drop_queued_play();
            }
            LocalPlayback::Failed(message) => {
                self.local_ready = false;
                self.drop_queued_play();
                self.toast_error(
                    gettext(self.locale, "Local playback: {error}").replace("{error}", message),
                );
            }
            LocalPlayback::Authorizing | LocalPlayback::Connecting => {}
        }
        self.local_playback = status;
    }

    /// Clears all optimistic playback and queue holds so stale expectations
    /// never survive an account reset or sign-out.
    fn clear_all_holds(&mut self) {
        self.optimistic_playing = None;
        self.intent_track = None;
        self.pending_play_keys.clear();
        self.pending_play_at = None;
        self.queued_play = None;
        self.assumed_context = None;
        self.pending_remote_position = None;
        self.pending_remote_volume = None;
        self.pending_local_volume = None;
        self.pending_local_position = None;
        self.shuffle_set_at = None;
        self.repeat_hold = None;
        self.local_transfer_sequence = None;
        self.pending_transfer_to = None;
        self.remote_recheck_at = None;
        self.remote_end_polled_at = None;
        self.remote_command_at = None;
        self.remote_read_failed = false;
        self.resume_load = None;
        self.local_list = None;
        self.queue_recheck_at = None;
        self.queue_stale_retries = 0;
        self.queue_cleared = None;
        self.queue_shuffle_pending = None;
        self.queue_reorder_pending = None;
        self.queue_start_pending = None;
        self.pending_queue_adds.clear();
        self.pending_album_queues.clear();
        self.pending_queue_batches.clear();
        self.last_album_queue = None;
    }

    fn reset_data(&mut self) {
        self.clear_all_holds();
        self.jam_state = JamState::default();
        self.cover_uploads.clear();
        self.uploaded_covers.clear();
        // Pages still on their way belong to the signed-out account's load.
        self.library = Library {
            playlists_generation: self.library.playlists_generation,
            ..Library::default()
        };
        self.liked_songs = crate::liked::LikedSongs::default();
        self.liked_recheck_at = None;
        self.home = HomeData::default();
        self.home_cache_dirty = false;
        self.playlist_pages.clear();
        self.album_pages.clear();
        self.artist_pages.clear();
        self.show_pages.clear();
        self.album_types_requested.clear();
        self.audiobook_shows.clear();
        self.audiobooks_requested.clear();
        self.playlist_headers.clear();
        self.playlist_headers_requested.clear();
        self.confirmed_ep_albums.clear();
        self.saved.clear();
        self.saved_deltas.clear();
        self.saved_pending.clear();
        self.track_recordings.clear();
        self.recording_uris.clear();
        self.playlist_availability.clear();
        self.saved_recordings.clear();
        self.saved_writes.clear();
        self.queue = Loadable::NotLoaded;
        self.devices.clear();
        self.devices_fetched_at = None;
        self.search.results = Loadable::NotLoaded;
        self.search.committed.clear();
        self.search.playlists = None;
        self.search.serial += 1;
        self.search.catalogue_pending = false;
        self.search.playlists_pending = false;
        self.search.error = None;
        self.page_used.clear();
        self.track_used.clear();
        self.copied_songs.clear();
        self.pending_pastes.clear();
    }

    fn handle_local(&mut self, state: LocalState) {
        if self
            .local_transfer_sequence
            .is_some_and(|sequence| sequence != state.track_sequence)
        {
            self.local_transfer_sequence = None;
        }
        if state.track_sequence != self.local.track_sequence
            && matches!(self.target(), Target::Local)
        {
            self.listening = None;
        }
        let track_changed =
            state.track != self.local.track || state.track_sequence != self.local.track_sequence;
        let reconnected = state.connected && !self.local.connected;
        if state.shuffle != self.local.shuffle
            && self
                .shuffle_set_at
                .is_none_or(|at| at.elapsed() > Duration::from_secs(5))
        {
            // Accept shuffle changes made by another client.
            self.shuffle_wanted = state.shuffle;
        }
        if state.playback != self.local.playback {
            self.optimistic_playing = None;
            if matches!(state.playback, Playback::Playing | Playback::Loading) {
                self.clear_play_pending();
            }
        }
        if state.track != self.local.track {
            self.clear_play_pending();
        }
        self.reconcile_local_track_intent(
            track_changed,
            state.track.as_ref().map(|track| track.uri.as_str()),
        );
        let held_volume = self.held_local_volume(state.volume);
        if held_volume.is_none() && state.volume != self.settings.volume {
            self.settings.volume = state.volume;
            self.settings_dirty = true;
        }
        if state.seek_sequence != self.local.seek_sequence || track_changed {
            self.pending_local_position = None;
        }
        if state.seek_sequence != self.local.seek_sequence
            && let Some(controls) = &self.media_controls
        {
            controls.seeked(Duration::from_millis(u64::from(state.position_ms)));
        }
        if let Some(error) = &state.error
            && self.local.error.as_deref() != Some(error.as_str())
        {
            self.toast_error(engine_error_text(self.locale, error));
            // One unavailable track is Spotify's catalogue; several in a
            // row is the session's audio-key service gone bad, which
            // leaves librespot feeding the decoder encrypted bytes and
            // skipping through the whole album. A fresh session cures it.
            if error.starts_with("This item isn't available") {
                let now = Instant::now();
                self.unavailable_at
                    .retain(|at| now.duration_since(*at) < Duration::from_secs(20));
                self.unavailable_at.push(now);
                if self.unavailable_at.len() >= 3
                    && self
                        .last_unavailable_reconnect
                        .is_none_or(|at| at.elapsed() > Duration::from_secs(60))
                {
                    self.unavailable_at.clear();
                    self.last_unavailable_reconnect = Some(now);
                    self.backend.send(Command::Reconnect);
                    self.toast(gettext(
                        self.locale,
                        "Spotify audio disconnected. Reconnecting local playback",
                    ));
                }
            }
        }
        if let Some(seed) = autoplay_seed(
            self.local_list.as_deref(),
            self.settings.autoplay,
            &self.local,
            &state,
        ) {
            log::info!("the list ended; playing what Spotify follows {seed} with");
            self.local_list = None;
            self.backend.player(PlayerCommand::Load(LoadSpec {
                context_uri: Some(seed),
                play: true,
                autoplay: true,
                ..LoadSpec::default()
            }));
        }
        // Another device may have taken over while the last poll still
        // names this one.
        if self.local.is_active() && !state.is_active() {
            self.poll_remote_soon();
        }
        self.local = state;
        if let Some(volume) = held_volume {
            self.local.volume = volume;
        }
        if track_changed {
            self.on_now_playing_changed();
        }
        if reconnected {
            if let Some(request) = self.queued_play.take() {
                self.play_request(request, false);
            }
            // Retry names requested before the session connected.
            let unresolved: Vec<String> = self
                .user_names
                .iter()
                .filter(|(_, name)| name.is_none())
                .map(|(id, _)| id.clone())
                .collect();
            for id in &unresolved {
                self.user_names.remove(id);
            }
            self.request_user_names(unresolved);
        }
    }

    /// Loads details for the remembered track before playback starts.
    fn request_resume_track(&mut self) {
        if self.now_playing_live().is_some() {
            return;
        }
        let Some(uri) = self.resume_track.clone() else {
            return;
        };
        // Episodes are not in the track endpoint; the preview skips them.
        if !uri.starts_with("spotify:track:") {
            return;
        }
        let Some(id) = util::uri_id(&uri).map(str::to_string) else {
            return;
        };
        if self.track_cache.contains_key(&id) {
            self.track_used.insert(id, Instant::now());
            return;
        }
        if !self.track_requests.insert(id.clone()) {
            return;
        }
        self.backend.api(ApiRequest::Track { id });
    }

    /// Whether the player bar shows the remembered track without live playback.
    fn resume_only(&self) -> bool {
        self.resume_track.is_some() && self.now_playing_live().is_none()
    }

    /// Loads the remembered context so Previous and Next work before playback.
    fn ensure_resume_context_loaded(&mut self) {
        if !self.resume_only() {
            return;
        }
        let Some(context) = self.resume_context.clone() else {
            return;
        };
        if self.context_track_uris(&context).is_some() {
            return;
        }
        if let Some(page) = Page::decode(&Self::context_page(&context)) {
            self.ensure_loaded(page);
        }
    }

    /// Where the playing songs come from, for the queue's header: the
    /// playlist, album, artist, or podcast with its page, Liked Songs, or
    /// a song radio as plain text because a station has no page. A context
    /// whose name has not loaded is still named by kind so it can open.
    pub fn playing_from(&self) -> Option<PlayingFrom> {
        let context = self.playing_context_uri()?;
        if context.ends_with(":collection") {
            return Some(PlayingFrom {
                name: gettext(self.locale, "Liked Songs").into_owned(),
                page: Some(Page::LikedSongs),
            });
        }
        if context.starts_with("spotify:station:") {
            return Some(PlayingFrom {
                name: self
                    .station_name(&context)
                    .unwrap_or_else(|| gettext(self.locale, "Radio").into_owned()),
                page: util::station_seed(&context).map(Page::Radio),
            });
        }
        let kind = util::uri_kind(&context)?;
        let id = util::uri_id(&context)?.to_string();
        let (name, page) = match kind {
            "playlist" => {
                let name = self
                    .library_entry(&id)
                    .map(|playlist| playlist.name.clone())
                    .or_else(|| {
                        self.playlist_pages
                            .get(&id)
                            .and_then(|page| page.playlist.get())
                            .map(|playlist| playlist.name.clone())
                    });
                (
                    name.unwrap_or_else(|| gettext(self.locale, "Playlist").into_owned()),
                    Page::Playlist(id),
                )
            }
            "album" => {
                let name = self
                    .album_pages
                    .get(&id)
                    .and_then(|page| page.album.get())
                    .map(|album| album.name.clone())
                    .or_else(|| {
                        self.now_playing()
                            .filter(|now| now.album_id.as_deref() == Some(id.as_str()))
                            .map(|now| now.album_name)
                    });
                (
                    name.unwrap_or_else(|| gettext(self.locale, "Album").into_owned()),
                    Page::Album(id),
                )
            }
            "artist" => {
                let name = self
                    .artist_pages
                    .get(&id)
                    .and_then(|page| page.artist.get())
                    .map(|artist| artist.name.clone())
                    .or_else(|| {
                        self.now_playing().and_then(|now| {
                            now.artists
                                .into_iter()
                                .find(|artist| artist.id.as_deref() == Some(id.as_str()))
                                .map(|artist| artist.name)
                        })
                    });
                (
                    name.unwrap_or_else(|| gettext(self.locale, "Artist").into_owned()),
                    Page::Artist(id),
                )
            }
            "show" => {
                let name = self
                    .show_pages
                    .get(&id)
                    .and_then(|page| page.show.get())
                    .map(|show| show.name.clone())
                    .or_else(|| {
                        self.library
                            .shows
                            .items
                            .iter()
                            .find(|saved| saved.show.id == id)
                            .map(|saved| saved.show.name.clone())
                    });
                (
                    name.unwrap_or_else(|| gettext(self.locale, "Podcast").into_owned()),
                    Page::Show(id),
                )
            }
            _ => return None,
        };
        Some(PlayingFrom {
            name,
            page: Some(page),
        })
    }

    /// "<Name> Radio" for a station whose seed's name is known.
    fn station_name(&self, context: &str) -> Option<String> {
        self.radio_name(&util::station_seed(context)?)
    }

    /// Encodes a context as a value accepted by `Page::decode`.
    fn context_page(context_uri: &str) -> String {
        if context_uri.ends_with(":collection") {
            return "liked".to_owned();
        }
        match (util::uri_kind(context_uri), util::uri_id(context_uri)) {
            (Some(kind), Some(id)) => format!("{kind}:{id}"),
            _ => String::new(),
        }
    }

    /// Moves the paused remembered track within its context without playing.
    /// Returns `false` when the context is unavailable.
    fn step_resume(&mut self, forward: bool) -> bool {
        let Some(context) = self.resume_context.clone() else {
            return false;
        };
        let Some(uris) = self.context_track_uris(&context) else {
            return false;
        };
        let current = self.resume_track.clone().unwrap_or_default();
        let next = if self.shuffle_wanted && forward {
            // Preserve shuffle behavior before playback resumes.
            let choices: Vec<&String> = uris.iter().filter(|uri| **uri != current).collect();
            if choices.is_empty() {
                return false;
            }
            choices[rand::random_range(0..choices.len())].clone()
        } else {
            let Some(index) = uris.iter().position(|uri| *uri == current) else {
                return false;
            };
            let last = uris.len() - 1;
            let target = match (forward, index) {
                (true, i) if i == last => 0,
                (true, i) => i + 1,
                (false, 0) => last,
                (false, i) => i - 1,
            };
            uris[target].clone()
        };
        self.cache_track_from_context(&context, &next);
        self.resume_track = Some(next);
        self.resume_position_ms = 0;
        self.session_dirty = true;
        true
    }

    /// Caches track details from a loaded context for immediate display.
    fn cache_track_from_context(&mut self, context_uri: &str, uri: &str) {
        let Some(id) = util::uri_id(uri) else {
            return;
        };
        if self.track_cache.contains_key(id) {
            self.track_used.insert(id.to_owned(), Instant::now());
            return;
        }
        let found = if let Some(pid) = context_uri.strip_prefix("spotify:playlist:") {
            self.playlist_pages.get(pid).and_then(|page| {
                page.items
                    .items
                    .iter()
                    .find_map(|item| match item.playable() {
                        Some(PlayableItem::Track(track)) if track.uri == uri => Some(track.clone()),
                        _ => None,
                    })
            })
        } else if let Some(aid) = context_uri.strip_prefix("spotify:album:") {
            self.album_pages.get(aid).and_then(|page| {
                page.tracks
                    .items
                    .iter()
                    .find(|track| track.uri == uri)
                    .cloned()
            })
        } else if context_uri.ends_with(":collection") {
            self.library
                .liked
                .items
                .iter()
                .find(|item| item.track.uri == uri)
                .map(|item| item.track.clone())
        } else {
            None
        };
        if let Some(track) = found {
            self.remember_track_recording(&track);
            self.track_cache.insert(id.to_owned(), track);
            self.track_used.insert(id.to_owned(), Instant::now());
        }
    }

    /// Older playlist caches predate recording identities. Once the current
    /// track supplies an ISRC, refresh only plausible aliases so Spotify can
    /// confirm which market-specific URI is the same recording.
    fn request_recording_candidates(&mut self, track: &Track) {
        if track.recording_key().is_none() {
            return;
        }
        let mut candidates = HashSet::new();
        for page in self.playlist_pages.values() {
            for item in &page.items.items {
                let Some(PlayableItem::Track(candidate)) = item.playable() else {
                    continue;
                };
                if same_recording_hint(track, candidate)
                    && let Some(id) = candidate.id.as_ref()
                    && self
                        .track_cache
                        .get(id)
                        .is_none_or(|known| known.recording_key().is_none())
                {
                    candidates.insert(id.clone());
                }
            }
        }
        for id in candidates {
            if self.track_requests.insert(id.clone()) {
                self.backend.api(ApiRequest::Track { id });
            }
        }
    }

    fn on_now_playing_changed(&mut self) {
        // Queue consumption and saved session state follow confirmed playback,
        // not the song shown optimistically while a command is still pending.
        let Some(now) = self.now_playing_live() else {
            return;
        };
        // A context no longer holding the resumed song, as a playlist does
        // not hold the recommendation that followed it, starts at its first
        // song instead. Replace that with the song itself, before the saved
        // queue or the saved song learn of the detour (#856).
        if now.local
            && let Some((uri, position_ms)) = self.resume_load.take()
            && now.uri != uri
        {
            let mut request = PlayRequest::tracks(vec![uri]);
            request.position_ms = position_ms;
            self.play_request(request, false);
            return;
        }
        let same_uri = self.last_now_playing_uri.as_deref() == Some(now.uri.as_str());
        let new_occurrence =
            now.local && self.last_now_playing_sequence != self.local.track_sequence;
        if same_uri && !new_occurrence {
            return;
        }
        // A new track is a new resume point.
        self.session_dirty = true;
        let queue_already_updated =
            self.queue_start_pending.take().as_ref() == Some(&self.target());
        let repeating = same_uri && new_occurrence && now.repeat == RepeatMode::Track;
        // Restore the saved queue only when the remembered track resumes.
        if !self.resume_queue.is_empty() {
            let queued = std::mem::take(&mut self.resume_queue);
            self.session_dirty = true;
            if now.local && self.resume_track.as_deref() == Some(now.uri.as_str()) {
                for uri in queued {
                    self.manual_queue.push(uri.clone());
                    self.backend.api(ApiRequest::AddToQueue {
                        uri,
                        device_id: self.local_device_id.clone(),
                        label: String::new(),
                    });
                }
            }
        }
        // A context load can start the same song as the manual queue's head
        // without consuming that queued copy. Explicit skips have already
        // consumed their rows; only an unrequested advance consumes one here.
        if !queue_already_updated && !repeating {
            self.consume_manual_queue_head(&now.uri);
        }
        if !queue_already_updated
            && !repeating
            && let Loadable::Loaded(queue) = &mut self.queue
        {
            let accounted = queue
                .currently_playing
                .as_ref()
                .is_some_and(|item| item.uri() == now.uri)
                && !(same_uri && new_occurrence);
            if !accounted
                && queue
                    .queue
                    .first()
                    .is_some_and(|item| item.uri() == now.uri)
            {
                let item = queue.queue.remove(0);
                queue.currently_playing = Some(item);
            }
        }
        self.last_now_playing_uri = Some(now.uri.clone());
        if now.local {
            self.last_now_playing_sequence = self.local.track_sequence;
        }
        self.resume_context = self.playing_context_uri();
        self.resume_track = Some(now.uri.clone());
        self.resume_position_ms = 0;
        if now.local
            && !now.is_episode
            && let Some(id) = &now.id
            && !self.track_cache.contains_key(id)
            && self.track_requests.insert(id.clone())
        {
            self.backend.api(ApiRequest::Track { id: id.clone() });
        }
        self.request_contains(vec![now.uri.clone()]);
        if matches!(self.page(), Page::Queue) || self.show_queue_panel {
            self.refresh_queue(true);
        }
        if self.show_lyrics_panel {
            self.request_lyrics();
        }
    }

    /// LRCLIB's outages come in short bursts, so a failure stays "looking"
    /// and is asked again a few times before it is shown.
    fn lyrics_failed(
        &mut self,
        error: String,
        now: Instant,
    ) -> Loadable<Option<crate::lyrics::Lyrics>> {
        let Some(wait) = LYRICS_RETRIES.get(self.lyrics_retries) else {
            return Loadable::Failed(error);
        };
        self.lyrics_retries += 1;
        self.lyrics_retry_at = Some(now + *wait);
        Loadable::Loading
    }

    /// Asks again once the wait is over, if the song is still the one waiting.
    fn retry_lyrics_when_due(&mut self, now: Instant) {
        if self.lyrics_retry_at.is_none_or(|at| now < at) {
            return;
        }
        self.lyrics_retry_at = None;
        self.lyrics = Loadable::NotLoaded;
        self.request_lyrics();
    }

    /// Asks for the playing track's lyrics unless they are here or on the
    /// way. Podcasts have no lyrics to ask for.
    /// The cache key of the words the shown lyrics need, when they are
    /// synced and could have words.
    fn words_key(&self) -> Option<std::path::PathBuf> {
        let Loadable::Loaded(Some(lyrics)) = &self.lyrics else {
            return None;
        };
        let uri = self.lyrics_uri.as_deref()?;
        if !lyrics.synced || !uri.starts_with("spotify:track:") {
            return None;
        }
        let lines: Vec<(u32, &str)> = lyrics
            .lines
            .iter()
            .map(|line| (line.at_ms.unwrap_or_default(), line.text.as_str()))
            .collect();
        Some(crate::words::cache_path(
            &self.dirs.lyrics_cache_dir(),
            uri,
            &lines,
        ))
    }

    /// Asks for the shown lyrics' words to be placed in the song.
    fn request_words(&mut self) {
        if self.offline || self.words_key().is_none() {
            return;
        }
        let (Some(uri), Loadable::Loaded(Some(lyrics))) = (&self.lyrics_uri, &self.lyrics) else {
            return;
        };
        if self
            .lyric_words
            .as_ref()
            .is_some_and(|words| &words.uri == uri)
        {
            return;
        }
        self.backend.send(Command::Words {
            uri: uri.clone(),
            lines: lyrics
                .lines
                .iter()
                .map(|line| (line.at_ms.unwrap_or_default(), line.text.clone()))
                .collect(),
            ahead: false,
        });
    }

    /// While lyrics are shown, the next song's words are placed ahead of
    /// it, so they light up from its first line.
    pub fn prepare_next_words(&mut self) {
        if self.offline {
            return;
        }
        let Some(crate::api::models::PlayableItem::Track(next)) = self
            .queue
            .get()
            .and_then(|queue| queue.queue.first())
            .cloned()
        else {
            return;
        };
        if self.words_prepared.as_deref() == Some(next.uri.as_str()) {
            return;
        }
        self.words_prepared = Some(next.uri.clone());
        self.backend.send(Command::PrepareWords {
            uri: next.uri.clone(),
            query: Some(crate::lyrics::Query {
                artist: next
                    .artists
                    .first()
                    .map(|artist| artist.name.clone())
                    .unwrap_or_default(),
                title: next.name,
                album: next.album.map(|album| album.name).unwrap_or_default(),
                duration_ms: next.duration_ms,
            }),
        });
    }

    pub fn request_lyrics(&mut self) {
        let Some(now) = self.now_playing() else {
            return;
        };
        if self.lyrics_uri.as_deref() == Some(now.uri.as_str())
            && !matches!(self.lyrics, Loadable::NotLoaded | Loadable::Failed(_))
        {
            return;
        }
        if self.lyrics_uri.as_deref() != Some(now.uri.as_str()) {
            self.lyrics_retries = 0;
            self.lyrics_retry_at = None;
        }
        self.lyrics_uri = Some(now.uri.clone());
        self.lyrics_following = true;
        self.lyrics_line_shown = None;
        if now.is_episode || self.offline {
            self.lyrics = Loadable::Loaded(None);
            return;
        }
        self.lyrics = Loadable::Loading;
        self.backend.send(Command::Lyrics(Box::new(LyricsRequest {
            uri: now.uri,
            query: crate::lyrics::Query {
                artist: now
                    .artists
                    .first()
                    .map(|artist| artist.name.clone())
                    .unwrap_or_default(),
                title: now.title,
                album: now.album_name,
                duration_ms: now.duration_ms,
            },
        })));
    }

    /// Runs time-based engine housekeeping and returns the delay until the
    /// next scheduled timer (or `None` when nothing is scheduled).
    fn tick_core(&mut self, ctx: UiCtx<'_>) -> Option<Duration> {
        let now = Instant::now();
        let mut next_wake: Option<Duration> = None;
        let mut wake_in = |delay: Duration| {
            next_wake = Some(next_wake.map_or(delay, |current| current.min(delay)));
        };

        // A session that never lists the playlists leaves them to the Web
        // API after a moment.
        if let Some(check) = &self.playlists_check {
            let elapsed = check.started.elapsed();
            if elapsed > Duration::from_secs(15) {
                self.playlists_check = None;
                self.load_playlists();
            } else {
                wake_in(Duration::from_secs(15) - elapsed);
            }
        }
        self.toasts
            .retain(|toast| toast.created.elapsed() < TOAST_LIFETIME);

        if self.is_connected() && !self.offline {
            self.request_resume_track();
            self.ensure_resume_context_loaded();
        }

        if self.is_connected() && !self.offline {
            self.poll_remote_if_due();
            if let Some(due) = self.remote_recheck_at {
                let now_check = Instant::now();
                if now_check >= due {
                    self.remote_recheck_at = None;
                    self.poll_remote(true);
                } else {
                    wake_in(due.saturating_duration_since(now_check));
                }
            }
            if self.show_devices
                && !self.devices_loading
                && self
                    .devices_fetched_at
                    .is_none_or(|at| at.elapsed() > DEVICES_FRESH)
            {
                self.refresh_devices();
            }
            if (self.show_queue_panel || matches!(self.page(), Page::Queue))
                && !self.queue.is_loading()
                && self
                    .queue_fetched_at
                    .is_none_or(|at| at.elapsed() > QUEUE_FRESH)
            {
                self.refresh_queue(false);
            }
            if let Some(due) = self.liked_recheck_at {
                if Instant::now() >= due && !self.liked_songs.refreshing() {
                    self.liked_recheck_at = None;
                    self.refresh_liked_songs();
                } else {
                    wake_in(
                        due.saturating_duration_since(Instant::now())
                            .max(Duration::from_millis(100)),
                    );
                }
            }
            if let Some(due) = self.queue_recheck_at {
                if Instant::now() >= due {
                    self.queue_recheck_at = None;
                    self.refresh_queue(true);
                } else {
                    wake_in(
                        due.saturating_duration_since(Instant::now())
                            .max(Duration::from_millis(50)),
                    );
                }
            }
        }

        if let Some(typed) = self.search.typed_at {
            if typed.elapsed() >= SEARCH_DEBOUNCE {
                self.search.typed_at = None;
                let query = self.search.query.trim().to_string();
                self.run_search(query);
            } else {
                wake_in(SEARCH_DEBOUNCE - typed.elapsed());
            }
        }

        if self.last_eviction.elapsed() > Duration::from_secs(20) {
            self.last_eviction = now;
            let _ = ctx;
            self.evict_stale_pages();
            self.checkpoint_home();
        }
        self.retry_lyrics_when_due(Instant::now());
        if let Some(at) = self.lyrics_retry_at {
            wake_in(at.saturating_duration_since(Instant::now()));
        }
        if self.settings_dirty {
            let elapsed = self.last_settings_save.elapsed();
            if elapsed > Duration::from_secs(2) {
                self.save_settings();
            } else {
                wake_in(Duration::from_secs(2) - elapsed);
            }
        }
        // The resume point moves while music plays, and nothing else marks the
        // session for saving then. Refresh it now and again so a session that
        // ends without a goodbye (a shutdown, a crash) resumes close by.
        if self.now_playing().is_some_and(|now| now.playing)
            && self.last_session_save.elapsed() > SESSION_REFRESH
        {
            self.session_dirty = true;
        }
        if self.session_dirty {
            let elapsed = self.last_session_save.elapsed();
            if elapsed > Duration::from_secs(2) {
                self.save_session();
            } else {
                wake_in(Duration::from_secs(2) - elapsed);
            }
        }
        next_wake
    }

    /// Sends equalizer settings to the player and marks them for saving.
    fn push_eq(&mut self) {
        if let Ok(mut shared) = self.eq.lock() {
            *shared = eq_settings(&self.settings);
        }
        self.settings_dirty = true;
    }

    fn handle_proxy_restored(
        &mut self,
        config: crate::settings::ProxyConfig,
        password: Option<crate::credentials::ProxyPassword>,
    ) {
        if self.last_proxy_applied == 0 {
            self.applied_proxy = config;
        }
        if !self.proxy_form_edited
            && self.proxy_request == 0
            && let Some(password) = password
        {
            self.settings.restore_proxy_password(&password);
        }
    }

    fn handle_proxy_password_stored(&mut self) {
        if self.settings.proxy_password_legacy {
            self.settings.proxy_password_legacy = false;
            self.save_settings();
        }
    }

    fn request_proxy(&mut self, sign_in: bool) {
        let config = match self.settings.proxy_config() {
            Ok(config) => config,
            Err(error) => {
                self.toast_error(error);
                return;
            }
        };
        self.proxy_request = self.proxy_request.wrapping_add(1);
        let request = self.proxy_request;
        self.pending_proxy_preferences
            .insert(request, self.settings.proxy_preferences());
        self.sign_in_request = sign_in.then_some(request);
        self.backend.send(if sign_in {
            Command::SignIn {
                request,
                config,
                client_id: self.settings.web_client_id.clone(),
            }
        } else {
            Command::ApplyProxy { request, config }
        });
    }

    fn handle_proxy_applied(
        &mut self,
        request: u64,
        config: crate::settings::ProxyConfig,
        result: Result<bool, String>,
    ) {
        let Some(preferences) = self.pending_proxy_preferences.remove(&request) else {
            return;
        };
        if request < self.last_proxy_applied {
            return;
        }
        match result {
            Ok(restarted) => {
                self.last_proxy_applied = request;
                self.applied_proxy = config;
                self.applied_proxy_preferences = preferences;
                self.save_settings();
                if restarted && self.local_ready {
                    self.toast(gettext(
                        self.locale,
                        "Proxy applied. Restarting local playback.",
                    ));
                } else {
                    self.toast(gettext(self.locale, "Proxy settings applied"));
                }
            }
            Err(error) => {
                let message = gettext(
                    self.locale,
                    "Proxy could not be applied: {error}. Previous connection settings are still in use.",
                )
                .replace("{error}", &error.to_string());
                if self.sign_in_request == Some(request)
                    && matches!(self.auth, AuthStatus::Starting)
                {
                    // The sign-in this request carried never started.
                    self.auth = AuthStatus::Failed(message);
                } else {
                    self.toast_error(message);
                }
            }
        }
        if self.sign_in_request == Some(request) {
            self.sign_in_request = None;
        }
    }

    fn save_settings(&mut self) {
        if self.settings.proxy_password_legacy {
            self.settings_dirty = false;
            self.last_settings_save = Instant::now();
            // Do not erase the sole surviving legacy password when the native
            // store is locked. Its successful callback owns retrying this save.
            return;
        }
        self.settings_dirty = false;
        self.last_settings_save = Instant::now();
        if self.offline {
            // Demo data must never overwrite the person's real preferences.
            return;
        }
        let mut saved = self.settings.clone();
        self.applied_proxy_preferences.apply_to(&mut saved);
        saved.save_with(&self.files, self.dirs.settings_file());
    }

    fn handle_media_commands(&mut self) {
        let Some(commands) = self.media_controls.as_ref().map(MediaControls::commands) else {
            return;
        };
        for command in commands {
            let playing = self.now_playing().is_some_and(|now| now.playing);
            let action = match command {
                MediaCommand::Play => (!playing).then_some(Action::TogglePlay),
                MediaCommand::Pause | MediaCommand::Stop => playing.then_some(Action::TogglePlay),
                MediaCommand::PlayPause => Some(Action::TogglePlay),
                MediaCommand::Next => Some(Action::Next),
                MediaCommand::Previous => Some(Action::Previous),
                MediaCommand::SeekBy(offset) => Some(Action::SeekBy(offset)),
                MediaCommand::SetPosition { track_id, position } => self
                    .now_playing()
                    .filter(|now| now.uri == track_id)
                    .map(|_| Action::Seek(u32::try_from(position.as_millis()).unwrap_or(u32::MAX))),
                MediaCommand::SetVolume(volume) => Some(Action::SetVolume(
                    (volume.clamp(0.0, 1.0) * 100.0).round() as u8,
                )),
                MediaCommand::SetShuffle(shuffle) => Some(Action::SetShuffle(shuffle)),
                MediaCommand::SetRepeat(mode) => Some(Action::SetRepeat(match mode {
                    MediaRepeat::Off => RepeatMode::Off,
                    MediaRepeat::Track => RepeatMode::Track,
                    MediaRepeat::Playlist => RepeatMode::Context,
                })),
                MediaCommand::OpenUri(uri) => {
                    if crate::link::search_query(&uri).is_some() {
                        crate::link::parse(&uri).map(Action::OpenLink)
                    } else {
                        Some(Action::PlayContext {
                            uri,
                            offset_uri: None,
                            offset_index: None,
                        })
                    }
                }
                MediaCommand::Raise => Some(Action::ShowWindow),
                MediaCommand::Quit => Some(Action::Quit),
            };
            if let Some(action) = action {
                self.actions.push(action);
            }
        }
    }

    fn media_art_file_headless(&mut self, waker: &Waker, url: &str) -> Option<PathBuf> {
        if let Some((known, file)) = &self.media_art
            && known == url
        {
            return Some(file.clone());
        }
        match self.backend.art().cached_file(url) {
            Some(file) => {
                self.media_art = Some((url.to_owned(), file.clone()));
                Some(file)
            }
            None => {
                self.backend.art().prefetch_with_waker(waker, url);
                None
            }
        }
    }

    fn sync_media_controls_headless(&mut self, waker: &Waker) {
        let art_file = self
            .now_playing()
            .and_then(|now| now.art_url)
            .and_then(|url| self.media_art_file_headless(waker, &url));
        self.sync_media_controls_with(art_file);
    }

    fn sync_media_controls_with(&mut self, art_file: Option<PathBuf>) {
        let state = match self.now_playing() {
            Some(now) => MediaState {
                // Still loading reads as paused: Play resumes it.
                playback: if now.playing {
                    MediaPlayback::Playing
                } else {
                    MediaPlayback::Paused
                },
                track: Some(MediaTrack {
                    id: now.uri.clone(),
                    title: now.title.clone(),
                    artists: now
                        .artists
                        .iter()
                        .map(|artist| artist.name.clone())
                        .collect(),
                    album: now.album_name.clone(),
                    duration: Some(Duration::from_millis(u64::from(now.duration_ms))),
                    art_file,
                    art_url: now.art_url.clone(),
                    url: Some(now.uri.clone()),
                    ..MediaTrack::default()
                }),
                position: Duration::from_millis(u64::from(now.position_ms)),
                volume: Some(f64::from(now.volume_percent) / 100.0),
                shuffle: Some(now.shuffle),
                repeat: Some(match now.repeat {
                    RepeatMode::Off => MediaRepeat::Off,
                    RepeatMode::Track => MediaRepeat::Track,
                    RepeatMode::Context => MediaRepeat::Playlist,
                }),
                ..MediaState::default()
            },
            None => MediaState {
                volume: Some(1.0),
                shuffle: Some(false),
                repeat: Some(MediaRepeat::Off),
                ..MediaState::default()
            },
        };
        if let Some(controls) = &mut self.media_controls {
            controls.update(state);
        }
    }

    // ---- loading ---------------------------------------------------------------

    /// Keeps the saved playlists when the session's playlist list has the
    /// same playlists and the saved list is less than a day old; otherwise
    /// reads them again. Waits for both the list and the saved copy.
    fn settle_playlists_check(&mut self) {
        let Some(check) = &self.playlists_check else {
            return;
        };
        let (Some(rootlist), true) = (&check.rootlist, check.cache_settled) else {
            return;
        };
        let fresh = self
            .library
            .playlists_fetched_at
            .is_some_and(|at| jiff::Timestamp::now().as_second() - at < 24 * 60 * 60);
        let same = self.library.playlists.get().is_some_and(|list| {
            let known: HashSet<&str> = list.iter().map(|playlist| playlist.uri.as_str()).collect();
            rootlist.len() == known.len() && rootlist.iter().all(|uri| known.contains(uri.as_str()))
        });
        self.playlists_check = None;
        if fresh && same && self.library.playlists_from_cache {
            log::info!("the saved playlists match the account's; not reading them again");
            self.library.playlists_from_cache = false;
            let uris: Vec<String> = self
                .library
                .playlists
                .get()
                .map(|list| list.iter().map(|playlist| playlist.uri.clone()).collect())
                .unwrap_or_default();
            for uri in uris {
                self.record_saved(uri, true);
            }
        } else {
            self.load_playlists();
        }
    }

    fn load_playlists(&mut self) {
        if self.library.playlists.is_loading() || self.library.playlists_refresh.is_some() {
            return;
        }
        // A list already shown stays up until the fresh one is whole, and
        // through a reload that fails: the last session's, or this one's
        // after a playlist was made, renamed or deleted.
        if self.library.playlists.get().is_some() {
            self.library.playlists_refresh = Some(Vec::new());
        } else {
            self.library.playlists = Loadable::Loading;
        }
        self.library.playlists_next = None;
        self.library.playlists_asked = None;
        self.library.playlists_generation += 1;
        self.backend.api(ApiRequest::MyPlaylists {
            offset: 0,
            generation: self.library.playlists_generation,
        });
    }

    pub fn ensure_loaded(&mut self, page: Page) {
        if !self.is_connected() {
            return;
        }
        match page {
            Page::Home => self.load_home(false),
            Page::TopSongs => self.load_top_songs(false),
            Page::Search => {}
            Page::LikedSongs => self.ensure_liked_songs(),
            Page::Albums => {
                if !self.library.albums.loaded_once {
                    self.load_more(Page::Albums);
                }
            }
            Page::Artists => {
                if !self.library.artists.loaded_once {
                    self.load_more(Page::Artists);
                }
            }
            Page::Podcasts => {
                if !self.library.shows.loaded_once {
                    self.load_more(Page::Podcasts);
                }
            }
            Page::Episodes => {
                if !self.library.episodes.loaded_once {
                    self.load_more(Page::Episodes);
                }
            }
            Page::Playlist(id) => {
                let needs_generation = self
                    .playlist_pages
                    .get(&id)
                    .is_none_or(|page| page.generation == 0);
                if needs_generation {
                    self.load_generation += 1;
                    self.playlist_pages
                        .entry(id.clone())
                        .or_default()
                        .generation = self.load_generation;
                }
                let page = self.playlist_pages.entry(id.clone()).or_default();
                let generation = page.generation;
                if page.playlist.needs_load() {
                    page.playlist = Loadable::Loading;
                    self.backend.api(ApiRequest::Playlist {
                        id: id.clone(),
                        generation,
                    });
                }
                if !page.items.loaded_once && page.items.can_load_more() {
                    page.items.loading = true;
                    self.backend.api(ApiRequest::PlaylistItems {
                        id: id.clone(),
                        offset: 0,
                        generation,
                    });
                    // Disk progress is adopted only if Spotify's snapshot
                    // still matches.
                    self.backend.send(Command::LoadPlaylistCache {
                        id: id.clone(),
                        generation,
                    });
                }
                self.request_contains(vec![format!("spotify:playlist:{id}")]);
            }
            Page::Album(id) => {
                if !self.album_pages.contains_key(&id) {
                    self.load_generation = self.load_generation.wrapping_add(1);
                    self.album_pages.insert(
                        id.clone(),
                        AlbumPage {
                            generation: self.load_generation,
                            ..Default::default()
                        },
                    );
                }
                let page = self.album_pages.entry(id.clone()).or_default();
                if page.album.needs_load() {
                    page.album = Loadable::Loading;
                    self.backend.api(ApiRequest::Album { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:album:{id}")]);
            }
            Page::Artist(id) => {
                let page = self.artist_pages.entry(id.clone()).or_default();
                if page.artist.needs_load() {
                    page.artist = Loadable::Loading;
                    self.backend.api(ApiRequest::Artist { id: id.clone() });
                }
                let filter = page.filter;
                self.load_artist_albums(&id, filter);
                if page_related_needs_load(&self.artist_pages, &id) {
                    if let Some(page) = self.artist_pages.get_mut(&id) {
                        page.related = Loadable::Loading;
                    }
                    self.backend
                        .api(ApiRequest::RelatedArtists { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:artist:{id}")]);
            }
            Page::Show(id) => {
                let page = self.show_pages.entry(id.clone()).or_default();
                if page.show.needs_load() {
                    page.show = Loadable::Loading;
                    self.backend.api(ApiRequest::Show { id: id.clone() });
                }
                self.request_contains(vec![format!("spotify:show:{id}")]);
            }
            Page::Radio(seed) => self.load_radio(&seed),
            Page::User(id) => {
                let page = self.user_pages.entry(id.clone()).or_default();
                if page.profile.needs_load() {
                    page.profile = Loadable::Loading;
                    self.backend.send(Command::UserProfile(id));
                }
            }
            Page::Queue => self.refresh_queue(true),
            Page::Settings => {}
        }
    }

    fn load_artist_albums(&mut self, id: &str, filter: DiscographyFilter) {
        let Some(page) = self.artist_pages.get_mut(id) else {
            return;
        };
        let list = page.albums.entry(filter.groups().to_string()).or_default();
        if !list.loaded_once && list.can_load_more() {
            list.loading = true;
            self.backend.api(ApiRequest::ArtistAlbums {
                id: id.to_string(),
                groups: filter.groups().to_string(),
                offset: 0,
            });
        }
    }

    fn load_home(&mut self, force: bool) {
        if self.home.requested
            && !force
            && self
                .home
                .loaded_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(600))
        {
            return;
        }
        self.home.requested = true;
        self.home.loaded_at = Some(Instant::now());
        self.home.generation += 1;
        let generation = self.home.generation;
        // Made for You and the recommendations come only from the shared
        // app; within twelve hours the saved ones stand.
        self.home.shared_fresh = !force
            && !self.home.discover.is_empty()
            && self
                .home
                .shared_fetched_at
                .is_some_and(|at| jiff::Timestamp::now().as_second() - at < 12 * 60 * 60);
        if self.home.recently_played.get().is_none() {
            self.home.recently_played = Loadable::Loading;
        }
        if self.home.top_artists.get().is_none() {
            self.home.top_artists = Loadable::Loading;
        }
        if self.home.top_tracks.get().is_none() {
            self.home.top_tracks = Loadable::Loading;
        }
        self.backend.api(ApiRequest::RecentlyPlayed {
            who: RecentsFor::Home,
            generation,
            before: None,
            limit: HOME_RECENTS,
        });
        self.backend.api(ApiRequest::TopArtists { generation });
        self.backend.api(ApiRequest::TopTracks {
            offset: 0,
            full: false,
            generation,
        });
        self.backend.api(ApiRequest::SpotifyHome { generation });
        self.home.discover_pending.clear();
        let made_up = !self.home.shared_fresh && self.home.spotify.get().is_none();
        for term in DISCOVER_TERMS.iter().filter(|_| made_up) {
            self.home
                .discover_pending
                .insert((*term).to_string(), Loadable::Loading);
            if !self.home.discover.contains_key(*term) {
                self.home
                    .discover
                    .insert((*term).to_string(), Loadable::Loading);
            }
            self.backend.api(ApiRequest::Discover {
                term: (*term).to_string(),
                generation,
            });
        }
        // The podcast shelf reads from the saved shows. The first page of
        // them is the one the Podcasts shelf of the library asks for.
        if self.library.shows.loaded_once {
            self.request_home_episodes();
        } else if !self.library.shows.loading && self.library.shows.error.is_none() {
            self.load_more(Page::Podcasts);
        }
    }

    /// Asks for the newest episodes of the most recently saved podcasts,
    /// once per Home refresh. Known audiobooks are skipped: librespot cannot
    /// play them.
    fn request_home_episodes(&mut self) {
        if self.home.podcasts_generation == self.home.generation {
            return;
        }
        self.home.podcasts_generation = self.home.generation;
        let shows: Vec<Show> = self
            .library
            .shows
            .items
            .iter()
            .map(|saved| &saved.show)
            .filter(|show| !show.id.is_empty() && !self.audiobook_shows.contains(&show.uri))
            .take(HOME_PODCAST_SHOWS)
            .cloned()
            .collect();
        if shows.is_empty() {
            self.home.podcasts.clear();
            return;
        }
        self.backend.api(ApiRequest::HomeEpisodes {
            shows,
            generation: self.home.generation,
        });
    }

    fn load_top_songs(&mut self, force: bool) {
        if self.home.top_songs_loading || (!force && self.home.top_songs_complete) {
            return;
        }
        self.home.top_songs = Loadable::Loading;
        self.home.top_songs_loading = true;
        self.home.top_songs_complete = false;
        self.home.top_songs_generation += 1;
        self.backend.api(ApiRequest::TopTracks {
            offset: 0,
            full: true,
            generation: self.home.top_songs_generation,
        });
    }

    pub fn load_recents(&mut self, force: bool) {
        if self.recents.loading {
            return;
        }
        if self.recents.complete && !force {
            return;
        }
        if force {
            self.recents.reset();
        }
        self.recents.loading = true;
        self.recents.error = None;
        self.recents_generation = self.recents_generation.wrapping_add(1);
        let generation = self.recents_generation;
        // This endpoint paginates backwards, so its continuation cursor is
        // named `before`.
        let before = self.recents.after.clone();
        self.backend.api(ApiRequest::RecentlyPlayed {
            who: RecentsFor::Panel,
            generation,
            before,
            limit: RECENTS_PAGE,
        });
    }

    pub fn load_more_recents(&mut self) {
        if !self.recents.can_load_more() {
            return;
        }
        self.load_recents(false);
    }

    pub fn load_more(&mut self, page: Page) {
        match page {
            Page::LikedSongs => {
                if !self.liked_songs.cache_checked {
                    self.ensure_liked_songs();
                } else if let Some(offset) = self.liked_songs.request_offset() {
                    self.backend.api(ApiRequest::SavedTracks {
                        offset,
                        generation: self.liked_songs.generation,
                    });
                    self.library.liked.loading = true;
                }
            }
            Page::Albums => {
                let list = &mut self.library.albums;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedAlbums { offset });
                }
            }
            Page::Artists => {
                let list = &mut self.library.artists;
                if list.can_load_more() {
                    list.loading = true;
                    self.backend.api(ApiRequest::FollowedArtists {
                        after: list.after.clone(),
                    });
                }
            }
            Page::Podcasts => {
                let list = &mut self.library.shows;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedShows { offset });
                }
            }
            Page::Episodes => {
                let list = &mut self.library.episodes;
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend.api(ApiRequest::SavedEpisodes { offset });
                }
            }
            Page::Playlist(id) => {
                if self.playlist_pages.get(&id).is_some_and(|page| {
                    page.pending_writes > 0
                        || page.optimistic_snapshot.is_some()
                        || page.items.error.is_some()
                }) {
                    return;
                }
                let restart_from_top = self.playlist_pages.get(&id).is_some_and(|page| {
                    page.items.base_offset > 0
                        && (!page.filter.trim().is_empty()
                            || self.table_sorts.contains_key(&Page::Playlist(id.clone())))
                });
                if restart_from_top {
                    self.load_playlist_items_at(&id, 0);
                    return;
                }
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    let list = &mut page.items;
                    if let Some(offset) = list
                        .window_request
                        .or(list.next_offset)
                        .filter(|_| list.can_load_more())
                    {
                        list.loading = true;
                        self.backend.api(ApiRequest::PlaylistItems {
                            id,
                            offset,
                            generation: page.generation,
                        });
                    }
                }
            }
            Page::Album(id) => {
                if let Some(page) = self.album_pages.get_mut(&id) {
                    let list = &mut page.tracks;
                    if list.base_offset > 0
                        && self.table_sorts.contains_key(&Page::Album(id.clone()))
                    {
                        self.load_generation = self.load_generation.wrapping_add(1);
                        page.generation = self.load_generation;
                        let total = list.total;
                        list.reset();
                        list.total = total;
                    }
                    if let Some(offset) = list
                        .window_request
                        .or(list.next_offset)
                        .filter(|_| list.can_load_more())
                    {
                        list.loading = true;
                        self.backend.api(ApiRequest::AlbumTracks {
                            id,
                            offset,
                            generation: page.generation,
                        });
                    }
                }
            }
            Page::Show(id) => {
                if let Some(page) = self.show_pages.get_mut(&id) {
                    let list = &mut page.episodes;
                    if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                        list.loading = true;
                        self.backend.api(ApiRequest::ShowEpisodes { id, offset });
                    }
                }
            }
            Page::Home => {
                if let Some(offset) = self.library.playlists_next.take() {
                    self.library.playlists_asked = Some(offset);
                    self.backend.api(ApiRequest::MyPlaylists {
                        offset,
                        generation: self.library.playlists_generation,
                    });
                }
            }
            _ => {}
        }
    }

    fn load_playlist_items_at(&mut self, id: &str, offset: u32) {
        let generation = {
            let Some(page) = self.playlist_pages.get_mut(id) else {
                return;
            };
            self.load_generation += 1;
            page.generation = self.load_generation;
            let total = page.items.total;
            page.items.reset_at(offset);
            page.items.total = total;
            page.items.loading = true;
            page.tail_checked = false;
            page.cache_restored_through = None;
            page.pending_cache = None;
            page.cache_checked = true;
            page.cache_append_valid = false;
            page.local_additions.clear();
            page.optimistic_snapshot = None;
            page.snapshot_rechecks = 0;
            page.refresh_after_write = false;
            page.generation
        };
        self.backend.api(ApiRequest::PlaylistItems {
            id: id.to_string(),
            offset,
            generation,
        });
    }

    fn reload(&mut self, page: Page) {
        match &page {
            Page::Home => self.load_home(true),
            Page::TopSongs => self.load_top_songs(true),
            Page::LikedSongs => {
                self.refresh_liked_songs();
                return;
            }
            Page::Albums => self.library.albums.reset(),
            Page::Artists => self.library.artists.reset(),
            Page::Podcasts => self.library.shows.reset(),
            Page::Episodes => self.library.episodes.reset(),
            Page::Playlist(id) => {
                if let Some(playlist) = self.playlist_pages.get_mut(id) {
                    playlist.items.loading = true;
                    playlist.items.error = None;
                    if playlist.pending_writes > 0 || playlist.optimistic_snapshot.is_some() {
                        // A refresh must not turn pre-write rows into a new,
                        // apparently current generation. Wait for the write's
                        // metadata confirmation before asking for rows.
                        playlist.refresh_after_write = true;
                        playlist.snapshot_rechecks = 0;
                        if playlist.pending_writes == 0 {
                            self.backend.api(ApiRequest::Playlist {
                                id: id.clone(),
                                generation: playlist.generation,
                            });
                        }
                        return;
                    }
                    playlist.refresh_after_write = false;
                    self.load_generation += 1;
                    playlist.generation = self.load_generation;
                    playlist.items.clear_windows();
                    playlist.cache_checked = true;
                    playlist.cache_restored_through = None;
                    playlist.pending_cache = None;
                    playlist.cache_append_valid = false;
                    playlist.local_additions.clear();
                    playlist.optimistic_snapshot = None;
                    playlist.snapshot_rechecks = 0;
                    self.backend.api(ApiRequest::Playlist {
                        id: id.clone(),
                        generation: playlist.generation,
                    });
                    self.backend.api(ApiRequest::PlaylistItems {
                        id: id.clone(),
                        offset: 0,
                        generation: playlist.generation,
                    });
                    return;
                }
            }
            Page::Album(id) => {
                self.album_pages.remove(id);
            }
            Page::Artist(id) => {
                self.artist_pages.remove(id);
            }
            Page::Show(id) => {
                self.show_pages.remove(id);
            }
            Page::User(id) => {
                self.user_pages.remove(id);
            }
            Page::Radio(seed) => {
                // A new mix replaces the songs on screen when it arrives.
                if self.refresh_radio(seed) {
                    return;
                }
                self.radio_pages.remove(seed);
            }
            Page::Queue => self.queue = Loadable::NotLoaded,
            _ => {}
        }
        self.ensure_loaded(page);
    }

    fn poll_remote(&mut self, _immediate: bool) {
        if !self.is_connected() {
            return;
        }
        self.remote_poll_pending = true;
        self.remote_polled_at = Instant::now();
        self.remote_poll_seq += 1;
        self.backend.api(ApiRequest::PlaybackState {
            seq: self.remote_poll_seq,
        });
    }

    fn refresh_devices(&mut self) {
        if !self.is_connected() || self.devices_loading {
            return;
        }
        self.devices_loading = true;
        self.backend.api(ApiRequest::Devices);
    }

    fn refresh_queue(&mut self, force: bool) {
        if !self.is_connected() {
            return;
        }
        if self.resume_only() && matches!(self.queue, Loadable::Loaded(_)) {
            // Nothing is playing anywhere and the queue on show is the
            // remembered one; a fetch could only replace it with less.
            return;
        }
        if self.queue.is_loading() && !force {
            return;
        }
        if !matches!(self.queue, Loadable::Loaded(_)) {
            self.queue = Loadable::Loading;
        }
        self.queue_fetched_at = Some(Instant::now());
        self.queue_seq += 1;
        self.backend.api(ApiRequest::Queue {
            seq: self.queue_seq,
        });
    }

    /// A chosen row of Next up plays at once, and the rows above it go
    /// with it: skips consume the queue, so the playing context and the
    /// songs queued after the chosen one stay intact. Loading the queue's
    /// rows as a fresh list instead used to take seconds, threw the
    /// context away, and left Spotify's copy of the queue to reappear.
    fn play_queue_item(&mut self, index: usize, uri: String) {
        self.resume_load = None;
        if self.resume_only() {
            // Nothing is playing anywhere, so there is no live queue to
            // consume: play the shown rows as a plain list.
            let uris: Vec<String> = self
                .queue
                .get()
                .map(|queue| {
                    queue
                        .queue
                        .iter()
                        .map(|item| item.uri().to_string())
                        .collect()
                })
                .unwrap_or_default();
            if uris.is_empty() {
                return;
            }
            let (uris, index) = cap_uris(&uris, index as u32);
            self.play_request(PlayRequest::tracks(uris).starting_at_index(index), false);
            return;
        }
        let mut skips = index + 1;
        let mut consumed: Vec<String> = Vec::new();
        if let Loadable::Loaded(queue) = &mut self.queue {
            // The click names a song; if the rows shifted under the
            // pointer, the song wins over the row number.
            let position = match queue.queue.get(index) {
                Some(item) if item.uri() == uri => Some(index),
                _ => queue.queue.iter().position(|item| item.uri() == uri),
            };
            let Some(position) = position else {
                self.refresh_queue(true);
                return;
            };
            skips = position + 1;
            let mut items: Vec<_> = queue.queue.drain(..=position).collect();
            let chosen = items.pop().expect("the chosen row was just drained");
            consumed = items.iter().map(|item| item.uri().to_string()).collect();
            consumed.push(chosen.uri().to_string());
            queue.currently_playing = Some(chosen);
        }
        for gone in &consumed {
            if let Some(at) = self.manual_queue.iter().position(|queued| queued == gone) {
                self.remove_manual_queue_row(at);
            }
        }
        self.expect_track(uri.clone(), 0);
        self.queue_start_pending = Some(self.target());
        self.set_play_pending(vec![uri]);
        self.optimistic_playing = Some((true, Instant::now()));
        match self.target() {
            Target::Local => {
                for _ in 0..skips {
                    self.backend.player(PlayerCommand::Next);
                }
            }
            Target::Remote(device_id) => {
                // With nothing to act on, one call earns the "pick
                // something first" toast; a skip per row would repeat it.
                if device_id.is_none() && self.remote_fresh().is_none() {
                    self.remote(RemoteAction::Next, None);
                    return;
                }
                for _ in 0..skips {
                    self.remote(RemoteAction::Next, device_id.clone());
                }
            }
        }
    }

    /// How many leading rows of Next up are songs the user queued here,
    /// so the view can give them their own section.
    pub fn queued_rows_len(&self) -> usize {
        match &self.queue {
            Loadable::Loaded(queue) => {
                Self::end_of_queued_rows(&queue.queue, self.queued_by_hand())
            }
            _ => 0,
        }
    }

    /// The songs the user queued here. Before resume, the saved manual
    /// queue, which splits the restored rows.
    fn queued_by_hand(&self) -> &[String] {
        if self.manual_queue.is_empty() && self.resume_only() {
            &self.resume_queue
        } else {
            &self.manual_queue
        }
    }

    /// Whether the local player is the active target, so its queue can be
    /// rewritten directly. Neither the Web API nor librespot can reorder or
    /// insert into a live queue; the only way to change one is to clear it
    /// and re-add its songs in the new order, which only reaches the
    /// engine actually playing them.
    pub fn queue_locally_reorderable(&self) -> bool {
        self.local.is_active() && matches!(self.target(), Target::Local)
    }

    /// Whether the active local queue has rows that can be cleared.
    pub fn can_clear_queue(&self) -> bool {
        self.queue_locally_reorderable() && self.queued_rows_len() > 0
    }

    /// Clears manually queued tracks while keeping the context's upcoming rows.
    fn clear_queue(&mut self) {
        if !matches!(self.target(), Target::Local) {
            return;
        }
        self.pending_album_queues.clear();
        self.last_album_queue = None;
        // `queue_one` writes every queued song to both lists, so they hold
        // the same wishes and adding their counts asks for twice the rows
        // Next up was given. The extra row taken is the context's own copy
        // of that song, which stays. Neither list alone is the count
        // either: a pending add outlives its `manual_queue` entry once the
        // song starts.
        // Take as many rows as the longer of the two holds.
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for uri in &self.manual_queue {
            *counts.entry(uri.clone()).or_insert(0) += 1;
        }
        let mut pending: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for addition in &self.pending_queue_adds {
            *pending.entry(addition.item.uri()).or_insert(0) += 1;
        }
        for (uri, count) in pending {
            let held = counts.entry(uri.to_string()).or_insert(0);
            *held = (*held).max(count);
        }
        if let Loadable::Loaded(queue) = &mut self.queue {
            // Remove one row per queued copy, starting at the front.
            queue.queue.retain(|item| match counts.get_mut(item.uri()) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    false
                }
                _ => true,
            });
        }
        let cleared: std::collections::HashSet<String> = counts.into_keys().collect();
        if !self.manual_queue.is_empty() {
            self.session_dirty = true;
        }
        self.manual_queue.clear();
        self.pending_queue_adds.clear();
        if !cleared.is_empty() {
            self.queue_cleared = Some((cleared, Instant::now()));
        }
        self.backend.player(PlayerCommand::ClearQueue);
        // Refresh to remove queued tracks added by another client.
        self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
        self.toast(gettext(self.locale, "Queue cleared"));
    }

    /// Current and upcoming track URIs, deduplicated in playback order.
    pub fn queue_playlist_uris(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut uris = Vec::new();
        let queued = self.queue.get();
        let rows = self.now_playing().map(|now| now.uri).into_iter().chain(
            queued
                .iter()
                .flat_map(|queue| queue.queue.iter().map(|item| item.uri().to_string())),
        );
        for uri in rows {
            if seen.insert(uri.clone()) {
                uris.push(uri);
            }
        }
        uris
    }

    /// Name for a playlist created from the queue.
    pub fn queue_playlist_name(&self) -> String {
        if let Some(context) = self.playing_context_uri()
            && let Some(name) = self.station_name(&context)
        {
            return name;
        }
        let today = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
        gettext(self.locale, "Queue {date}").replace("{date}", &today)
    }

    /// Saves the queue as a new playlist.
    fn save_queue_as_playlist(&mut self) {
        let uris = self.queue_playlist_uris();
        if uris.is_empty() {
            return;
        }
        let name = self.queue_playlist_name();
        self.actions.push(Action::CreatePlaylist {
            name,
            public: false,
            add_uris: uris,
        });
    }

    /// Shows the head of Next up as playing before the engine confirms it.
    fn pop_queue_head(&mut self) {
        // A restored or stale queue has no trustworthy relationship to the
        // current player state. It can stay visible while the live queue is
        // fetched, but must not be used to guess where Next will land.
        let current = self.current_track_uri();
        let Loadable::Loaded(queue) = &mut self.queue else {
            return;
        };
        let anchored =
            current.as_deref() == queue.currently_playing.as_ref().map(PlayableItem::uri);
        if !anchored || queue.queue.is_empty() {
            return;
        }
        let item = queue.queue.remove(0);
        let uri = item.uri().to_string();
        // The queue row already has the song's details, so the player bar
        // can name it now instead of waiting for the engine to load it.
        let track = match &item {
            crate::api::models::PlayableItem::Track(track) => Some(track.clone()),
            crate::api::models::PlayableItem::Episode(_) => None,
        };
        queue.currently_playing = Some(item);
        if let Some(track) = track
            && let Some(id) = util::uri_id(&uri)
        {
            self.track_cache.entry(id.to_owned()).or_insert(track);
        }
        self.consume_manual_queue_head(&uri);
        self.queue_start_pending = Some(self.target());
        self.expect_track(uri, 0);
    }

    /// Consume one queued occurrence, retaining any later copy of this song.
    fn consume_manual_queue_head(&mut self, uri: &str) {
        if self.manual_queue.first().map(String::as_str) == Some(uri) {
            self.remove_manual_queue_row(0);
        }
    }

    fn remove_manual_queue_row(&mut self, index: usize) {
        self.manual_queue.remove(index);
        self.pending_queue_adds.retain_mut(|addition| {
            if addition.manual_index == index {
                return false;
            }
            if addition.manual_index > index {
                addition.manual_index -= 1;
            }
            true
        });
        self.session_dirty = true;
    }

    /// Whether a fetched queue predates the latest local change.
    fn queue_fetch_is_stale(&self, fetched: &Queue) -> bool {
        if self.queue_stale_retries >= QUEUE_STALE_RETRIES {
            return false;
        }
        // A recent play or pop must be reflected in the fetched current row.
        if let Some(intent) = &self.intent_track
            && (intent.at.elapsed() < PLAYBACK_HOLD || self.queued_play.is_some())
            && fetched
                .currently_playing
                .as_ref()
                .is_none_or(|item| item.uri() != intent.uri)
        {
            return true;
        }
        // For local playback, reject a fetched current row that differs from
        // the engine's current track.
        if self.local.is_active()
            && let Some(track) = &self.local.track
            && fetched
                .currently_playing
                .as_ref()
                .is_some_and(|item| item.uri() != track.uri)
        {
            return true;
        }
        // A cleared row still on top means the clear has not landed yet.
        if let Some((cleared, at)) = &self.queue_cleared
            && at.elapsed() < PLAYBACK_HOLD
            && fetched
                .queue
                .first()
                .is_some_and(|item| cleared.contains(item.uri()))
        {
            return true;
        }
        // An unchanged context order right after toggling shuffle means the
        // reordered queue has not landed yet.
        if let Some(pending) = &self.queue_shuffle_pending
            && pending.at.elapsed() < PLAYBACK_HOLD
            && fetched.currently_playing.as_ref().map(|item| item.uri())
                == pending.current_uri.as_deref()
        {
            let at = Self::end_of_queued_rows(&fetched.queue, &self.manual_queue);
            let fetched_context = &fetched.queue[at..];
            if fetched_context.len() == pending.context_uris.len()
                && fetched_context
                    .iter()
                    .map(|item| item.uri())
                    .eq(pending.context_uris.iter().map(String::as_str))
            {
                return true;
            }
        }
        // A local reorder or positional insert is optimistic; reject a
        // fetch whose queued rows have not caught up to it yet.
        if let Some(at) = self.queue_reorder_pending
            && at.elapsed() < PLAYBACK_HOLD
            && !fetched
                .queue
                .iter()
                .take(self.manual_queue.len())
                .map(|item| item.uri())
                .eq(self.manual_queue.iter().map(String::as_str))
        {
            return true;
        }
        false
    }

    fn run_search(&mut self, query: String) {
        if crate::jam::is_invitation(&query) {
            self.search.query.clear();
            self.actions.push(Action::OpenLink(query));
            return;
        }
        if !query.is_empty()
            && query == self.search.committed
            && !self.search.results.needs_load()
            && self.search.error.is_none()
        {
            return;
        }
        self.search.serial += 1;
        self.search.committed = query.clone();
        self.search.playlists = None;
        self.search.error = None;
        self.search.catalogue_pending = !query.is_empty();
        self.search.playlists_pending = false;
        // Results and their query change together. In particular, a failed
        // new query must never inherit the previous query's songs.
        self.search.results = if query.is_empty() {
            Loadable::NotLoaded
        } else {
            Loadable::Loading
        };
        // An empty query also cancels requests waiting on Spotify's quota.
        self.backend.api(ApiRequest::Search {
            query,
            serial: self.search.serial,
        });
    }

    fn show_search_playlists(&mut self) {
        let Some((serial, page)) = self.search.playlists.as_ref() else {
            return;
        };
        if *serial != self.search.serial {
            return;
        }
        if self.search.results_serial != *serial || self.search.results.get().is_none() {
            self.search.results = Loadable::Loaded(Default::default());
            self.search.results_serial = *serial;
        }
        if let Some(results) = self.search.results.get_mut() {
            results.playlists = Some(page.clone());
        }
    }

    fn search_failed(&mut self, part: &str, error: impl std::fmt::Display) {
        let message = format!("{part}: {error}");
        self.search.error = Some(match self.search.error.take() {
            Some(previous) => format!("{previous}\n{message}"),
            None => message,
        });
        if self.search.results_serial != self.search.serial
            && !self.search.catalogue_pending
            && !self.search.playlists_pending
        {
            self.search.results = Loadable::Failed(self.search.error.clone().unwrap());
        }
    }

    /// Resolves user IDs that do not have a cached display name.
    pub fn request_user_names(&mut self, ids: Vec<String>) {
        let unknown: Vec<String> = ids
            .into_iter()
            .filter(|id| !self.user_names.contains_key(id))
            .collect();
        if unknown.is_empty() {
            return;
        }
        for id in &unknown {
            self.user_names.insert(id.clone(), None);
        }
        self.backend.send(Command::UserNames(unknown));
    }

    fn request_album_types<'a>(&mut self, albums: impl IntoIterator<Item = &'a Album>) {
        let mut uris = Vec::new();
        for album in albums {
            if album.is_single_release()
                && !album.uri.is_empty()
                && self.album_types_requested.insert(album.uri.clone())
            {
                uris.push(album.uri.clone());
            }
        }
        if !uris.is_empty() {
            self.backend.album_types(uris);
        }
    }

    pub fn request_contains(&mut self, uris: Vec<String>) {
        let mut batch = Vec::new();
        for uri in uris {
            if uri.is_empty()
                || self.saved.contains_key(&uri)
                || self.saved_pending.contains(&uri)
                || uri.starts_with("spotify:local")
            {
                continue;
            }
            self.saved_pending.insert(uri.clone());
            batch.push(uri);
            if batch.len() == CONTAINS_BATCH {
                self.backend.api(ApiRequest::Contains {
                    uris: std::mem::take(&mut batch),
                });
            }
        }
        if !batch.is_empty() {
            self.backend.api(ApiRequest::Contains { uris: batch });
        }
    }

    // ---- api responses -------------------------------------------------------

    /// The cover URLs a playlist shows before an upload, to tell its new
    /// picture from the old one once Spotify answers.
    pub(crate) fn cover_urls_before_upload(&self, id: &str) -> Vec<String> {
        self.uploaded_covers
            .get(id)
            .map(|pending| pending.previous_urls.clone())
            .unwrap_or_else(|| {
                self.playlist_pages
                    .get(id)
                    .and_then(|page| page.playlist.get())
                    .into_iter()
                    .chain(
                        self.library
                            .playlists
                            .get()
                            .into_iter()
                            .flatten()
                            .filter(|playlist| playlist.id == id),
                    )
                    .flat_map(|playlist| playlist.images.iter().map(|image| image.url.clone()))
                    .collect()
            })
    }

    fn set_playlist_cover(&mut self, id: &str, cover: &crate::playlist_cover::Cover) {
        if let Some(page) = self.playlist_pages.get_mut(id)
            && let Some(playlist) = page.playlist.get_mut()
        {
            playlist.images = cover_images(cover);
        }
        if let Some(playlists) = self.library.playlists.get_mut() {
            for playlist in playlists.iter_mut().filter(|playlist| playlist.id == id) {
                playlist.images = cover_images(cover);
            }
        }
    }

    fn reconcile_playlist_cover(&mut self, id: &str, images: &mut Vec<crate::api::models::Image>) {
        let Some(pending) = self.uploaded_covers.get_mut(id) else {
            return;
        };
        let new_urls = !images.is_empty()
            && images
                .iter()
                .all(|image| !pending.previous_urls.contains(&image.url));
        if new_urls {
            if pending.checking.is_none() {
                pending.checking = Some(images.clone());
                self.backend.send(Command::CheckPlaylistCover {
                    id: id.into(),
                    request: pending.request,
                    cover: pending.cover.clone(),
                    images: images.clone(),
                });
            } else if pending.checking.as_ref() != Some(images) {
                // Keep the latest candidate while one image is being checked.
                // Do not start parallel downloads for every metadata reply.
                pending.next_images = Some(images.clone());
            }
        }
        *images = cover_images(&pending.cover);
        if !new_urls {
            self.recheck_playlist_cover(id);
        }
    }

    fn recheck_playlist_cover(&mut self, id: &str) {
        if let Some(pending) = self.uploaded_covers.get_mut(id)
            && pending.rechecks_left > 0
            && let Some(page) = self.playlist_pages.get(id)
        {
            pending.rechecks_left -= 1;
            self.backend.api(ApiRequest::Playlist {
                id: id.into(),
                generation: page.generation,
            });
        }
    }

    fn cover_checked(
        &mut self,
        id: &str,
        request: u64,
        images: Vec<crate::api::models::Image>,
        result: Result<bool, String>,
    ) {
        let Some(pending) = self.uploaded_covers.get_mut(id) else {
            return;
        };
        if pending.request != request || pending.checking.as_ref() != Some(&images) {
            return;
        }
        pending.checking = None;
        let next_images = pending.next_images.take();
        let should_recheck = matches!(result, Ok(false));
        match result {
            Ok(true) => {
                self.uploaded_covers.remove(id);
                if let Some(page) = self.playlist_pages.get_mut(id)
                    && let Some(playlist) = page.playlist.get_mut()
                {
                    playlist.images = images.clone();
                }
                if let Some(playlists) = self.library.playlists.get_mut() {
                    for playlist in playlists.iter_mut().filter(|playlist| playlist.id == id) {
                        playlist.images = images.clone();
                    }
                }
            }
            Ok(false) => {
                pending
                    .previous_urls
                    .extend(images.into_iter().map(|image| image.url));
            }
            // The successful upload stays visible if its artwork cannot be
            // checked. A later metadata refresh can try that URL again.
            Err(_) => {}
        }
        if self.uploaded_covers.contains_key(id) {
            if let Some(mut next) = next_images {
                self.reconcile_playlist_cover(id, &mut next);
            } else if should_recheck {
                self.recheck_playlist_cover(id);
            }
        }
    }

    fn handle_api(&mut self, mut response: ApiResponse) {
        if let Some(page) = api::page_for_response(&response) {
            self.note_api_page(page);
        }
        match &mut response {
            ApiResponse::Playlist {
                id,
                result: Ok(playlist),
                generation,
            } => {
                if self
                    .playlist_pages
                    .get(id)
                    .is_some_and(|page| page.generation == *generation)
                {
                    self.reconcile_playlist_cover(id, &mut playlist.images);
                }
            }
            ApiResponse::MyPlaylists {
                result: Ok(page), ..
            } => {
                for playlist in &mut page.items {
                    self.reconcile_playlist_cover(&playlist.id, &mut playlist.images);
                }
            }
            _ => {}
        }
        match response {
            ApiResponse::Me(result) => match result {
                Ok(user) => {
                    // Spotify only takes playback commands from Premium
                    // accounts, here or on any device, so a Free account
                    // is told once rather than left pressing play.
                    let free = user
                        .product
                        .as_deref()
                        .is_some_and(|product| product != "premium");
                    if free && !self.premium_notice_shown {
                        self.premium_notice_shown = true;
                        self.dialog = Some(Dialog::PremiumNeeded);
                    }
                    let new_account = self.user_id() != Some(user.id.as_str());
                    if new_account {
                        self.backend.send(Command::LoadHomeCache {
                            account_id: user.id.clone(),
                        });
                        self.rootlist = self
                            .rootlist_cache
                            .as_ref()
                            .filter(|cached| cached.account_id == user.id)
                            .map(|cached| cached.entries.clone())
                            .unwrap_or_default();
                        self.editable_by_grant.clear();
                    }
                    self.user = Some(user);
                    let page = self.page().clone();
                    self.ensure_loaded(page);
                    if let Some(now) = self.now_playing() {
                        self.request_contains(vec![now.uri]);
                    }
                }
                Err(error) => {
                    if matches!(
                        error,
                        crate::api::ApiError::SignInExpired {
                            api_source: crate::api::ApiSource::Shared
                        }
                    ) && self.web_app.is_some()
                    {
                        // Spotify refused the shared grant; the personal app
                        // carries the session on its own.
                    } else if matches!(error, crate::api::ApiError::SignInExpired { .. }) {
                        self.auth = AuthStatus::Failed(
                            gettext(
                                self.locale,
                                "Your Spotify sign-in expired. Please sign in again.",
                            )
                            .into_owned(),
                        );
                    } else {
                        self.toast_error(
                            gettext(self.locale, "Couldn't load your profile: {error}")
                                .replace("{error}", &error.to_string()),
                        );
                    }
                }
            },
            ApiResponse::Devices(result) => {
                self.devices_loading = false;
                self.devices_fetched_at = Some(Instant::now());
                match result {
                    Ok(devices) => {
                        self.devices = devices;
                        if let Some((name, since)) = self.pending_transfer_to.clone() {
                            let matching = self
                                .devices
                                .iter()
                                .find(|device| device.name == name)
                                .and_then(|device| device.id.clone());
                            if let Some(id) = matching {
                                self.pending_transfer_to = None;
                                self.transfer(id);
                            } else if since.elapsed() > Duration::from_secs(20) {
                                self.pending_transfer_to = None;
                            } else {
                                self.devices_fetched_at = None;
                            }
                        }
                        if let Some(selected) = &self.selected_device
                            && !self
                                .devices
                                .iter()
                                .any(|device| device.id.as_deref() == Some(selected.as_str()))
                        {
                            self.selected_device = None;
                        }
                    }
                    Err(error) => self.toast_error(
                        gettext(self.locale, "Couldn't list devices: {error}")
                            .replace("{error}", &error.to_string()),
                    ),
                }
            }
            ApiResponse::PlaybackState { seq, result } => {
                if seq != self.remote_poll_seq {
                    // An older poll finishing late describes the past.
                    return;
                }
                self.remote_poll_pending = false;
                match result {
                    Ok(state) => {
                        self.remote_read_failed = false;
                        self.reconcile_remote_volume(seq, state.as_ref());
                        let previous_uri = self.remote.as_ref().and_then(|remote| {
                            remote
                                .state
                                .item
                                .as_ref()
                                .map(|item| item.uri().to_string())
                        });
                        let previous_shuffle = self
                            .remote
                            .as_ref()
                            .map(|remote| remote.state.shuffle_state);
                        let held_repeat = self.repeat_hold.take().filter(|(mode, at)| {
                            at.elapsed() < REMOTE_REPEAT_HOLD
                                && state
                                    .as_ref()
                                    .is_some_and(|state| state.repeat_state != *mode)
                        });
                        self.repeat_hold = held_repeat.clone();
                        self.remote = state.map(|mut state| {
                            if let Some((mode, _)) = held_repeat {
                                state.repeat_state = mode;
                            }
                            RemoteSnapshot {
                                state,
                                received_at: Instant::now(),
                            }
                        });
                        if let (Some(previous), Some(current)) = (
                            previous_shuffle,
                            self.remote
                                .as_ref()
                                .map(|remote| remote.state.shuffle_state),
                        ) && previous != current
                            && self
                                .shuffle_set_at
                                .is_none_or(|at| at.elapsed() > Duration::from_secs(5))
                        {
                            // Accept shuffle changes from another device.
                            self.shuffle_wanted = current;
                        }
                        if let Some(context) = self
                            .remote
                            .as_ref()
                            .and_then(|remote| remote.state.context.as_ref())
                            .map(|context| context.uri.clone())
                        {
                            // Ignore the old context while a takeover settles.
                            let stale = self.assumed_context.as_ref().is_some_and(|assumed| {
                                assumed.at.elapsed() < ASSUMED_CONTEXT_HOLD
                                    && assumed.uri != context
                            });
                            if !stale {
                                self.note_recent_context(&context);
                            }
                        }
                        let uri = self.remote.as_ref().and_then(|remote| {
                            remote
                                .state
                                .item
                                .as_ref()
                                .map(|item| item.uri().to_string())
                        });
                        self.reconcile_remote_track_intent(seq, uri.as_deref());
                        if let Some(remote) = &self.remote
                            && let Some(device) = &remote.state.device
                            && device.id.is_some()
                            && let Some(known) =
                                self.devices.iter_mut().find(|known| known.id == device.id)
                        {
                            known.is_active = true;
                            known.volume_percent = device.volume_percent;
                        }
                        if let Some((wanted, _)) = self.optimistic_playing
                            && self
                                .remote
                                .as_ref()
                                .is_some_and(|remote| remote.state.is_playing == wanted)
                        {
                            self.optimistic_playing = None;
                        }
                        if uri != previous_uri {
                            self.on_now_playing_changed();
                        }
                    }
                    Err(error) => {
                        self.remote_read_failed = true;
                        log::debug!("playback state unavailable: {error}");
                    }
                }
            }
            ApiResponse::Queue { seq, mut result } => {
                if seq != self.queue_seq {
                    // A newer request supersedes this response.
                    return;
                }
                if let Ok(fetched) = &mut result {
                    self.drop_queue_padding(fetched);
                }
                // Partial reads and read failures must preserve optimistic
                // additions while Spotify is still accepting their writes.
                let writing_queue = self
                    .pending_queue_batches
                    .values()
                    .any(|target| *target == self.target());
                if writing_queue
                    || result
                        .as_ref()
                        .is_ok_and(|fetched| self.queue_fetch_is_stale(fetched))
                {
                    // Keep the optimistic queue and retry after a stale
                    // response. Accept Spotify's state after the retry limit.
                    self.queue_stale_retries = self.queue_stale_retries.saturating_add(1);
                    self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
                    return;
                }
                self.queue_stale_retries = 0;
                if result.is_ok() {
                    self.queue_cleared = None;
                    self.queue_shuffle_pending = None;
                    self.queue_reorder_pending = None;
                }
                self.queue = Loadable::from_result(result);
                self.reconcile_pending_queue();
                if let Some(queue) = self.queue.get() {
                    let uris: Vec<String> = queue
                        .queue
                        .iter()
                        .map(|item| item.uri().to_string())
                        .collect();
                    self.request_contains(uris);
                }
            }
            ApiResponse::RecentlyPlayed {
                who,
                generation,
                limit,
                result,
            } => match who {
                RecentsFor::Home => {
                    if generation != self.home.generation {
                        return;
                    }
                    if let Ok(page) = &result {
                        self.note_recent_contexts(&page.items);
                        self.home_cache_dirty = true;
                    }
                    let items = result
                        .as_ref()
                        .map(|page| page.items.clone())
                        .map_err(|error| error.to_string());
                    self.home.recently_played.refresh(items);
                }
                RecentsFor::Panel => {
                    if generation != self.recents_generation {
                        return;
                    }
                    self.recents.loading = false;
                    self.recents.loaded_once = true;
                    match result {
                        Ok(page) => self.absorb_recents(page, limit),
                        Err(error) => self.recents.error = Some(error.to_string()),
                    }
                }
            },
            ApiResponse::TopTracks {
                offset,
                full,
                generation,
                result,
            } => {
                if full {
                    if generation != self.home.top_songs_generation {
                        return;
                    }
                    match result {
                        Ok(page) => {
                            let received = page.items.len() as u32;
                            let tracks = page.items;
                            let uris: Vec<String> =
                                tracks.iter().map(|track| track.uri.clone()).collect();
                            self.request_contains(uris);
                            if offset == 0 {
                                self.home.top_songs = Loadable::Loaded(tracks);
                            } else if let Some(current) = self.home.top_songs.get_mut() {
                                current.extend(tracks);
                            }
                            if page.next.is_some() && received > 0 && offset + received < 100 {
                                self.backend.api(ApiRequest::TopTracks {
                                    offset: offset + received,
                                    full: true,
                                    generation,
                                });
                            } else {
                                self.home.top_songs_loading = false;
                                self.home.top_songs_complete = true;
                            }
                        }
                        Err(error) => {
                            self.home.top_songs.refresh(Err::<Vec<Track>, _>(error));
                            self.home.top_songs_loading = false;
                        }
                    }
                } else if generation == self.home.generation
                    && let Ok(page) = result
                {
                    let tracks = page.items;
                    let seeds: Vec<String> = tracks
                        .iter()
                        .filter_map(|track| track.id.clone())
                        .take(5)
                        .collect();
                    let fresh = (self.home.shared_fresh
                        && self.home.recommendations.get().is_some())
                        || self.home.spotify.get().is_some();
                    if !seeds.is_empty() && !fresh {
                        if self.home.recommendations.get().is_none() {
                            self.home.recommendations = Loadable::Loading;
                        }
                        self.backend.api(ApiRequest::Recommendations {
                            seed_tracks: seeds,
                            seed_artists: Vec::new(),
                            generation,
                        });
                    }
                    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
                    self.request_contains(uris);
                    self.home.top_tracks = Loadable::Loaded(tracks);
                    self.home_cache_dirty = true;
                } else if generation == self.home.generation
                    && offset == 0
                    && let Err(error) = result
                    && self.home.top_tracks.get().is_none()
                {
                    self.home.top_tracks = Loadable::Failed(error.to_string());
                }
            }
            ApiResponse::TopArtists { generation, result } => {
                if generation != self.home.generation {
                    return;
                }
                self.home_cache_dirty |= result.is_ok();
                self.home.top_artists.refresh(result);
            }
            ApiResponse::SpotifyHome { generation, result } => {
                if generation != self.home.generation {
                    return;
                }
                if let Err(error) = &result {
                    log::debug!("Spotify's Home: {error}");
                }
                self.home_cache_dirty |= result.is_ok();
                self.home.spotify_fetched |= result.is_ok();
                self.home.spotify.refresh(result);
            }
            ApiResponse::Recommendations { generation, result } => {
                if generation != self.home.generation {
                    return;
                }
                if let Ok(tracks) = &result {
                    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
                    self.request_contains(uris);
                }
                self.home_cache_dirty |= result.is_ok();
                if result.is_ok() {
                    self.home.shared_fetched_at = Some(jiff::Timestamp::now().as_second());
                }
                self.home.recommendations.refresh(result);
            }
            ApiResponse::Discover {
                term,
                generation,
                result,
            } => {
                if generation != self.home.generation {
                    return;
                }
                let filtered = result.map(|playlists| {
                    let mut seen = std::collections::HashSet::new();
                    let mut matching: Vec<Playlist> = playlists
                        .into_iter()
                        .filter(|playlist| {
                            let owner = playlist.owner.id.as_deref().unwrap_or("");
                            is_made_for_you(&playlist.name, &term)
                                && (owner == "spotify" || playlist.owner_name() == "Spotify")
                                && seen.insert(playlist.name.to_lowercase())
                        })
                        .collect();
                    matching.truncate(6);
                    matching
                });
                self.home
                    .discover_pending
                    .insert(term, Loadable::from_result(filtered));
                let complete = DISCOVER_TERMS.iter().all(|term| {
                    self.home
                        .discover_pending
                        .get(*term)
                        .is_some_and(|result| !result.is_loading())
                });
                if complete {
                    self.home.discover = std::mem::take(&mut self.home.discover_pending);
                    self.home_cache_dirty = true;
                    self.home.shared_fetched_at = Some(jiff::Timestamp::now().as_second());
                }
            }
            // A reload reads the playlists from the top again under a new
            // generation, so a page any earlier load asked for no longer
            // continues the list, whatever its offset. Within one load, only
            // the later page on its way is taken, and only once.
            ApiResponse::MyPlaylists {
                offset, generation, ..
            } if generation != self.library.playlists_generation
                || (offset > 0 && self.library.playlists_asked != Some(offset)) => {}
            ApiResponse::MyPlaylists { offset, result, .. } => match result {
                Ok(page) => {
                    self.library.playlists_asked = None;
                    let next_offset = page.next_offset();
                    if let Some(gathered) = self.library.playlists_refresh.as_mut() {
                        if offset == 0 {
                            gathered.clear();
                        }
                        gathered.extend(page.items);
                        if next_offset.is_none() {
                            let fresh = self.library.playlists_refresh.take().unwrap_or_default();
                            self.library.playlists = Loadable::Loaded(fresh);
                            self.library.playlists_from_cache = false;
                        }
                    } else {
                        match &mut self.library.playlists {
                            Loadable::Loaded(existing) if offset > 0 => existing.extend(page.items),
                            slot => *slot = Loadable::Loaded(page.items),
                        }
                    }
                    self.home_cache_dirty |= next_offset.is_none();
                    if next_offset.is_none() {
                        self.library.playlists_fetched_at =
                            Some(jiff::Timestamp::now().as_second());
                    }
                    self.fill_blank_playlists();
                    self.library.playlists_next = next_offset;
                    if next_offset.is_some() {
                        self.load_more(Page::Home);
                    } else {
                        // Load folder order after all playlists arrive.
                        self.backend.send(Command::Rootlist);
                    }
                    let listed_uris: Vec<String> = self
                        .library
                        .playlists
                        .get()
                        .map(|playlists| playlists.iter().map(|p| p.uri.clone()).collect())
                        .unwrap_or_default();
                    for uri in listed_uris {
                        self.record_saved(uri, true);
                    }
                    if let Some(playlists) = self.library.playlists.get() {
                        for listed in playlists {
                            // A header read over the streaming session
                            // lacks what the list carries; pages that
                            // arrived before the list take it now.
                            if let Some(playlist) = self
                                .playlist_pages
                                .get_mut(&listed.id)
                                .and_then(|page| page.playlist.get_mut())
                            {
                                playlist.fill_from(listed);
                            }
                        }
                    }
                }
                Err(error) => {
                    self.library.playlists_asked = None;
                    if self.library.playlists_refresh.take().is_some() {
                        // The last session's list stays; the next load
                        // tries again.
                        log::warn!("couldn't refresh the playlists: {error}");
                    } else if offset == 0 {
                        self.library.playlists = Loadable::Failed(error.to_string());
                    } else {
                        // The list so far stays, and the page that failed is
                        // the one to ask for again.
                        self.library.playlists_next = Some(offset);
                        self.toast_error(
                            gettext(self.locale, "Couldn't load more playlists: {error}")
                                .replace("{error}", &error.to_string()),
                        );
                    }
                }
            },
            ApiResponse::Playlist {
                id,
                generation,
                mut result,
            } => {
                if self
                    .playlist_pages
                    .get(&id)
                    .is_none_or(|page| page.generation != generation || page.pending_writes > 0)
                {
                    return;
                }
                let stale_write_snapshot = self.playlist_pages.get_mut(&id).is_some_and(|page| {
                    let Some(expected) = page.optimistic_snapshot.as_deref() else {
                        return false;
                    };
                    let reported = result
                        .as_ref()
                        .ok()
                        .and_then(|playlist| playlist.snapshot_id.as_deref());
                    if reported == Some(expected) {
                        page.optimistic_snapshot = None;
                        page.snapshot_rechecks = 0;
                        false
                    } else if result.is_ok() {
                        page.snapshot_rechecks = page.snapshot_rechecks.saturating_add(1);
                        true
                    } else {
                        false
                    }
                });
                if stale_write_snapshot {
                    let page = self.playlist_pages.get_mut(&id).unwrap();
                    if page.snapshot_rechecks <= 3 {
                        self.backend.api(ApiRequest::Playlist { id, generation });
                    } else {
                        page.refresh_after_write = false;
                        page.items.fail(
                            gettext(
                                self.locale,
                                "Spotify hasn't confirmed your playlist changes yet. Try refreshing again.",
                            )
                            .into_owned(),
                        );
                    }
                    return;
                }
                if let Ok(playlist) = &mut result {
                    // A header read over the streaming session lacks what
                    // the Web API gave: the account's own name, and the
                    // library list's public flag, owner name, and cover.
                    if playlist.owner.display_name.is_none() {
                        playlist.owner.display_name = self.own_name(playlist.owner.id.as_deref());
                    }
                    if let Some(listed) = self.library_entry(&id) {
                        playlist.fill_from(listed);
                    }
                }
                let mut refresh_rows = false;
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    if page.refresh_after_write || page.optimistic_snapshot.is_some() {
                        match &result {
                            Ok(_) => refresh_rows = page.refresh_after_write,
                            Err(error) => {
                                page.refresh_after_write = false;
                                page.items.fail(friendly_page_error(self.locale, error));
                            }
                        }
                    }
                    let old_snapshot = page
                        .playlist
                        .get()
                        .and_then(|playlist| playlist.snapshot_id.as_deref());
                    let new_snapshot = result
                        .as_ref()
                        .ok()
                        .and_then(|playlist| playlist.snapshot_id.as_deref());
                    if old_snapshot.is_some() && old_snapshot != new_snapshot {
                        page.items.clear_windows();
                        page.cache_saved_through = None;
                        page.cache_saved_rows = 0;
                        page.cache_saved_total = None;
                        page.cache_append_valid = false;
                        page.cache_restored_through = None;
                    }
                    if let Ok(playlist) = &result
                        && page.items.loaded_once
                    {
                        page.items.total = Some(playlist.track_total());
                    }
                    page.playlist.refresh(result);
                }
                self.try_adopt_playlist_cache(&id);
                self.checkpoint_playlist_cache(&id);
                if refresh_rows {
                    self.reload(Page::Playlist(id));
                }
            }
            ApiResponse::PlaylistItems {
                id,
                offset,
                generation,
                result,
            } => {
                if self.playlist_pages.get(&id).is_none_or(|page| {
                    page.generation != generation
                        || page.pending_writes > 0
                        || page.optimistic_snapshot.is_some()
                }) {
                    return;
                }
                let mut uris = Vec::new();
                let mut adders: Vec<String> = Vec::new();
                let mut tracks = Vec::new();
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    match result {
                        _ if page
                            .cache_restored_through
                            .is_some_and(|cached| offset < cached)
                            && page.items.window_request != Some(offset) =>
                        {
                            // The initial request was already in flight when
                            // a longer cached prefix was restored.
                        }
                        Ok(mut items) => {
                            note_availability(&mut self.playlist_availability, &items.items);
                            fill_availability(&self.playlist_availability, &mut items.items);
                            tracks = items
                                .items
                                .iter()
                                .filter_map(|item| match item.playable() {
                                    Some(PlayableItem::Track(track)) => Some(track.clone()),
                                    _ => None,
                                })
                                .collect();
                            uris = items
                                .items
                                .iter()
                                .filter_map(|item| item.playable())
                                .map(|item| item.uri().to_string())
                                .collect();
                            adders = items
                                .items
                                .iter()
                                .filter_map(|item| item.added_by.as_ref()?.id.clone())
                                .filter(|id| !id.is_empty())
                                .collect();
                            page.contributors.extend(adders.iter().cloned());
                            let extends_checkpoint = offset > 0
                                && page.items.base_offset == 0
                                && page.items.loaded_once
                                && page.items.window_request.is_none()
                                && page.items.windows.is_empty()
                                && page.items.next_offset == Some(offset)
                                && page.items.total == Some(items.total)
                                && page.items.items.len() <= offset as usize
                                && page.items_generation == generation;
                            if !extends_checkpoint {
                                page.cache_append_valid = false;
                            }
                            page.items.absorb(offset, items);
                            page.items_generation = generation;
                        }
                        Err(error) => page.items.fail(friendly_page_error(self.locale, &error)),
                    }
                }
                for track in &tracks {
                    self.remember_track_recording(track);
                }
                self.request_contains(uris);
                self.request_user_names(adders);
                self.sample_playlist_tail(&id);
                self.checkpoint_playlist_cache(&id);
                // A sorted table means the whole list, not the loaded part.
                if self.table_sorts.contains_key(&Page::Playlist(id.clone())) {
                    self.load_more(Page::Playlist(id));
                }
            }
            ApiResponse::PlaylistSample {
                id,
                generation,
                result,
            } => {
                if self
                    .playlist_pages
                    .get(&id)
                    .is_none_or(|page| page.generation != generation)
                {
                    return;
                }
                let mut adders: Vec<String> = Vec::new();
                if let Ok(items) = result
                    && let Some(page) = self.playlist_pages.get_mut(&id)
                {
                    adders = items
                        .items
                        .iter()
                        .filter_map(|item| item.added_by.as_ref()?.id.clone())
                        .filter(|id| !id.is_empty())
                        .collect();
                    page.contributors.extend(adders.iter().cloned());
                }
                self.request_user_names(adders);
            }
            ApiResponse::PlaylistCreated(result) => {
                self.playlist_busy = false;
                match result {
                    Ok(playlist) => {
                        self.toast(
                            gettext(self.locale, "Created {name}")
                                .replace("{name}", &playlist.name),
                        );
                        if let Some(playlists) = self.library.playlists.get_mut() {
                            playlists.insert(0, playlist.clone());
                        }
                        self.record_saved(playlist.uri.clone(), true);
                        // A new playlist is empty: its page starts loaded, so
                        // the songs it was made with show the moment they are
                        // added, instead of an empty copy fetched before they
                        // land.
                        self.load_generation += 1;
                        let page = self.playlist_pages.entry(playlist.id.clone()).or_default();
                        page.generation = self.load_generation;
                        page.items_generation = self.load_generation;
                        page.playlist = Loadable::Loaded(playlist.clone());
                        page.items.loaded_once = true;
                        page.items.total = Some(0);
                        page.items.next_offset = None;
                        page.cache_checked = true;
                        if let Some(Dialog::CreatePlaylist { add_uris, .. }) = self.dialog.take()
                            && !add_uris.is_empty()
                        {
                            let described = std::mem::take(&mut self.created_with);
                            let items = add_uris
                                .iter()
                                .map(|uri| {
                                    described
                                        .iter()
                                        .find(|item| item.uri() == uri)
                                        .cloned()
                                        .or_else(|| self.known_song(uri))
                                        .unwrap_or_else(|| bare_item(uri))
                                })
                                .collect();
                            self.add_to_playlist_now(
                                playlist.id.clone(),
                                playlist.name.clone(),
                                items,
                                None,
                            );
                        }
                        self.open(Page::Playlist(playlist.id));
                    }
                    Err(error) => self.toast_error(
                        gettext(self.locale, "Couldn't create the playlist: {error}")
                            .replace("{error}", &error.to_string()),
                    ),
                }
            }
            ApiResponse::PlaylistCoverUploaded {
                id,
                request,
                previous_urls,
                cover,
                result,
            } => {
                if self.cover_uploads.get(&id) != Some(&request) {
                    return;
                }
                self.cover_uploads.remove(&id);
                match result {
                    Ok(()) => {
                        self.set_playlist_cover(&id, &cover);
                        self.uploaded_covers.insert(
                            id.clone(),
                            crate::playlist_cover::PendingCover {
                                cover,
                                previous_urls,
                                request,
                                checking: None,
                                next_images: None,
                                rechecks_left: 3,
                            },
                        );
                        if let Some(page) = self.playlist_pages.get(&id) {
                            self.backend.api(ApiRequest::Playlist {
                                id,
                                generation: page.generation,
                            });
                        }
                        self.toast(gettext(self.locale, "Playlist cover updated"));
                    }
                    Err(error) => self.toast_error(cover_error(self.locale, &error)),
                }
            }
            ApiResponse::PlaylistUpdated { id, result } => {
                self.playlist_busy = false;
                match result {
                    Ok(()) => {
                        self.toast(gettext(self.locale, "Playlist updated"));
                        self.playlist_pages.remove(&id);
                        // The list takes the change it knows of, rather than
                        // read every page again over the shared app's quota.
                        if let Some(changes) = self.pending_playlist_details.remove(&id)
                            && let Some(playlist) =
                                self.library.playlists.get_mut().and_then(|list| {
                                    list.iter_mut().find(|playlist| playlist.id == id)
                                })
                        {
                            if let Some(name) = changes.name {
                                playlist.name = name;
                            }
                            if let Some(description) = changes.description {
                                playlist.description = Some(description);
                            }
                            if let Some(public) = changes.public {
                                playlist.public = Some(public);
                            }
                            self.home_cache_dirty = true;
                        } else {
                            self.load_playlists();
                        }
                        if matches!(self.page(), Page::Playlist(current) if *current == id) {
                            self.ensure_loaded(Page::Playlist(id));
                        }
                    }
                    Err(error) => self.toast_error(
                        gettext(self.locale, "Couldn't update the playlist: {error}")
                            .replace("{error}", &error.to_string()),
                    ),
                }
            }
            ApiResponse::PlaylistDuplicatesChecked {
                playlist_id,
                playlist_name,
                items,
                position,
                result,
            } => match result {
                Ok(duplicate_uris) if duplicate_uris.is_empty() => {
                    self.add_to_playlist_now(playlist_id, playlist_name, items, position)
                }
                Ok(duplicate_uris) => {
                    self.playlist_busy = false;
                    self.dialog = Some(Dialog::ConfirmPlaylistDuplicates {
                        playlist_id,
                        playlist_name,
                        items,
                        position,
                        duplicate_uris,
                    });
                }
                Err(error) => {
                    // A failed read must not take away an edit the account is
                    // still allowed to make. The write reports its own error.
                    log::debug!("could not check playlist for duplicates: {error}");
                    self.add_to_playlist_now(playlist_id, playlist_name, items, position);
                }
            },
            ApiResponse::PlaylistItemsChanged {
                id,
                message,
                result,
            } => {
                self.playlist_busy = false;
                if let Some(page) = self.playlist_pages.get_mut(&id) {
                    page.pending_writes = page.pending_writes.saturating_sub(1);
                }
                match result {
                    Ok(snapshot) => {
                        if !message.is_empty() {
                            self.toast(message);
                        }
                        let mut generation = None;
                        if let Some(page) = self.playlist_pages.get_mut(&id) {
                            if let Some(playlist) = page.playlist.get_mut() {
                                playlist.snapshot_id = snapshot.clone();
                            }
                            if snapshot.is_some() {
                                page.optimistic_snapshot = snapshot.clone();
                                page.snapshot_rechecks = 0;
                            }
                            generation = Some(page.generation);
                        }
                        if let Some(playlists) = self.library.playlists.get_mut() {
                            for playlist in playlists.iter_mut().filter(|p| p.id == id) {
                                playlist.snapshot_id = snapshot.clone();
                            }
                        }
                        self.checkpoint_playlist_cache(&id);
                        // Reconcile metadata in the background. The mutation
                        // response and local rows are enough to keep the page;
                        // no item page is discarded or downloaded again.
                        if let Some(generation) = generation {
                            self.backend.api(ApiRequest::Playlist { id, generation });
                        }
                    }
                    Err(error) => {
                        self.toast_error(
                            gettext(self.locale, "Playlist change failed: {error}")
                                .replace("{error}", &error.to_string()),
                        );
                        if let Some(page) = self.playlist_pages.get_mut(&id) {
                            page.optimistic_snapshot = None;
                            page.snapshot_rechecks = 0;
                        }
                        self.load_playlist_items_at(&id, 0);
                        if let Some(page) = self.playlist_pages.get(&id) {
                            self.backend.api(ApiRequest::Playlist {
                                id,
                                generation: page.generation,
                            });
                        }
                    }
                }
            }
            ApiResponse::PlaylistFollowChanged {
                id,
                followed,
                result,
            } => match result {
                Ok(()) => {
                    self.saved
                        .insert(format!("spotify:playlist:{id}"), followed);
                    self.toast(if followed {
                        gettext(self.locale, "Added to Your Library")
                    } else {
                        gettext(self.locale, "Removed from Your Library")
                    });
                    // The list follows the change itself where it knows the
                    // playlist, rather than read every page again over the
                    // shared app's quota.
                    let known = followed
                        .then(|| {
                            self.playlist_pages
                                .get(&id)
                                .and_then(|page| page.playlist.get())
                                .cloned()
                        })
                        .flatten();
                    match (followed, known, self.library.playlists.get_mut()) {
                        (false, _, Some(list)) => {
                            list.retain(|playlist| playlist.id != id);
                            let uri = format!("spotify:playlist:{id}");
                            self.rootlist.retain(|entry| {
                                !matches!(entry, crate::player::RootlistEntry::Playlist(listed) if *listed == uri)
                            });
                            self.home_cache_dirty = true;
                        }
                        (true, Some(playlist), Some(list)) => {
                            list.retain(|listed| listed.id != id);
                            list.insert(0, playlist);
                            self.home_cache_dirty = true;
                        }
                        _ => self.load_playlists(),
                    }
                    if !followed && matches!(self.page(), Page::Playlist(current) if *current == id)
                    {
                        self.open(Page::Home);
                    }
                }
                Err(error) => {
                    self.saved
                        .insert(format!("spotify:playlist:{id}"), !followed);
                    self.toast_error(
                        gettext(self.locale, "Couldn't update the playlist: {error}")
                            .replace("{error}", &error.to_string()),
                    );
                }
            },
            ApiResponse::SavedTracks {
                offset,
                generation,
                account_id,
                result,
            } => {
                if generation != self.liked_songs.generation
                    || self.user_id() != account_id.as_deref()
                {
                    return;
                }
                let mut more = false;
                let succeeded = result.is_ok();
                match result {
                    Ok(page) => {
                        for item in &page.items {
                            let uri = item.track.uri.clone();
                            self.remember_track_recording(&item.track);
                            if !self.saved_writes.contains_key(&uri)
                                && self.liked_songs.intent(&uri).is_none()
                            {
                                self.set_saved_state(uri, true);
                            }
                        }
                        more = self.liked_songs.absorb(
                            offset,
                            page,
                            jiff::Timestamp::now().as_second(),
                        );
                    }
                    Err(error) => self.liked_songs.fail(error.to_string()),
                }
                if !more {
                    self.sync_liked_songs();
                    self.checkpoint_liked_songs(false);
                }
                // The whole list loads, as Spotify's own apps keep it, so
                // finding a song in it searches every one of them.
                if succeeded {
                    self.load_more(Page::LikedSongs);
                }
            }
            // A reset shelf is read again from the top, so a page asked for
            // before the reset no longer continues it.
            ApiResponse::SavedAlbums { offset, .. }
                if self.library.albums.next_offset != Some(offset) => {}
            ApiResponse::SavedAlbums { offset, result } => match result {
                Ok(page) => {
                    self.request_album_types(page.items.iter().map(|item| &item.album));
                    for item in &page.items {
                        self.record_saved(item.album.uri.clone(), true);
                    }
                    self.home_cache_dirty |= offset == 0;
                    self.library.albums.absorb(offset, page);
                }
                Err(error) => self.library.albums.fail(error.to_string()),
            },
            ApiResponse::FollowedArtists { after, result } => {
                // A reset shelf is read again from the top, so a page asked
                // for before the reset no longer continues it.
                if after != self.library.artists.after {
                    return;
                }
                self.library.artists.loading = false;
                self.library.artists.loaded_once = true;
                match result {
                    Ok(page) => {
                        for artist in &page.items {
                            self.record_saved(artist.uri.clone(), true);
                        }
                        let list = &mut self.library.artists;
                        if after.is_none() {
                            list.items.clear();
                            self.home_cache_dirty = true;
                        }
                        let received = page.items.len();
                        list.items.extend(page.items);
                        let next = page.cursors.and_then(|cursors| cursors.after);
                        list.complete = next.is_none() || received == 0;
                        list.after = next;
                        list.error = None;
                    }
                    Err(error) => self.library.artists.error = Some(error.to_string()),
                }
            }
            ApiResponse::SavedShows { offset, .. }
                if self.library.shows.next_offset != Some(offset) => {}
            ApiResponse::SavedShows { offset, result } => match result {
                Ok(page) => {
                    for item in &page.items {
                        self.record_saved(item.show.uri.clone(), true);
                    }
                    let unknown: Vec<String> = page
                        .items
                        .iter()
                        .map(|item| item.show.uri.clone())
                        .filter(|uri| self.audiobooks_requested.insert(uri.clone()))
                        .collect();
                    if !unknown.is_empty() {
                        self.backend.send(Command::AudiobookShows(unknown));
                    }
                    self.home_cache_dirty |= offset == 0;
                    self.library.shows.absorb(offset, page);
                    if offset == 0 && self.home.requested {
                        self.request_home_episodes();
                    }
                }
                Err(error) => self.library.shows.fail(error.to_string()),
            },
            ApiResponse::HomeEpisodes { generation, .. } if generation != self.home.generation => {}
            ApiResponse::HomeEpisodes { result, .. } => match result {
                Ok(podcasts) => {
                    self.home.podcasts = podcasts;
                    self.home_cache_dirty = true;
                }
                // The shelf is an extra: without an answer it keeps what it
                // showed, or stays hidden.
                Err(error) => log::debug!("podcast episodes for Home unavailable: {error}"),
            },
            ApiResponse::SavedEpisodes { offset, .. }
                if self.library.episodes.next_offset != Some(offset) => {}
            ApiResponse::SavedEpisodes { offset, result } => match result {
                Ok(page) => {
                    for item in &page.items {
                        self.record_saved(item.episode.uri.clone(), true);
                    }
                    self.library.episodes.absorb(offset, page);
                }
                Err(error) => self.library.episodes.fail(error.to_string()),
            },
            ApiResponse::SavedChanged {
                uris,
                saved,
                result,
            } => {
                let current_uris: Vec<String> = uris
                    .iter()
                    .filter(|uri| {
                        self.saved_writes
                            .get(*uri)
                            .is_none_or(|wanted| *wanted == saved)
                    })
                    .cloned()
                    .collect();
                for uri in &uris {
                    if self.saved_writes.get(uri) == Some(&saved) {
                        self.saved_writes.remove(uri);
                    }
                }
                match result {
                    Ok(()) => {
                        for uri in &current_uris {
                            self.set_saved_state(uri.clone(), saved);
                            match util::uri_kind(uri) {
                                Some("track") => {
                                    if self.liked_songs.intent(uri).is_some() {
                                        self.liked_songs.confirm(uri, saved, true);
                                    } else if !saved {
                                        self.library.liked.retain(|item| item.track.uri != *uri);
                                        if let Some(total) = self.library.liked.total.as_mut() {
                                            *total = total.saturating_sub(1);
                                        }
                                    }
                                }
                                Some("album") => self.library.albums.reset(),
                                Some("artist") => self.library.artists.reset(),
                                Some("show") => self.library.shows.reset(),
                                Some("episode") => self.library.episodes.reset(),
                                _ => {}
                            }
                        }
                        let message = match (
                            current_uris.first().and_then(|uri| util::uri_kind(uri)),
                            saved,
                        ) {
                            (Some("track"), true) => gettext(self.locale, "Added to Liked Songs"),
                            (Some("track"), false) => {
                                gettext(self.locale, "Removed from Liked Songs")
                            }
                            (Some("artist"), true) => gettext(self.locale, "Following artist"),
                            (Some("artist"), false) => gettext(self.locale, "Unfollowed artist"),
                            (_, true) => gettext(self.locale, "Saved to Your Library"),
                            (_, false) => gettext(self.locale, "Removed from Your Library"),
                        };
                        if !current_uris.is_empty() {
                            self.toast(message);
                        }
                    }
                    Err(error) => {
                        for uri in &current_uris {
                            self.set_saved_state(uri.clone(), !saved);
                            self.liked_songs.confirm(uri, saved, false);
                        }
                        if !current_uris.is_empty() {
                            self.toast_error(
                                gettext(self.locale, "Couldn't update your library: {error}")
                                    .replace("{error}", &error.to_string()),
                            );
                        }
                    }
                }
                if self.liked_songs.cache_checked && self.liked_songs.has_confirmed_changes() {
                    self.sync_liked_songs();
                    self.checkpoint_liked_songs(true);
                    self.liked_recheck_at = Some(Instant::now() + Duration::from_secs(5));
                }
            }
            ApiResponse::Contains { uris, result } => {
                for uri in &uris {
                    self.saved_pending.remove(uri);
                }
                if let Ok(flags) = result {
                    for (uri, flag) in uris.into_iter().zip(flags) {
                        if !self.saved_writes.contains_key(&uri)
                            && self.liked_songs.intent(&uri).is_none()
                        {
                            self.set_saved_state(uri, flag);
                        }
                    }
                }
            }
            ApiResponse::SearchStarted {
                query,
                serial,
                split,
            } => {
                if serial == self.search.serial && query == self.search.committed {
                    self.search.catalogue_pending = true;
                    self.search.playlists_pending = split;
                }
            }
            ApiResponse::SearchPlaylists {
                query,
                serial,
                result,
            } => {
                if serial != self.search.serial || query != self.search.committed {
                    return;
                }
                self.search.playlists_pending = false;
                match result {
                    Ok(page) => {
                        self.search.playlists = Some((serial, page));
                        self.show_search_playlists();
                        self.settings.remember_search(&query);
                        self.settings_dirty = true;
                    }
                    Err(error) => self.search_failed(&gettext(self.locale, "Playlists"), error),
                }
            }
            ApiResponse::Search {
                query,
                serial,
                result,
            } => {
                if serial != self.search.serial || query != self.search.committed {
                    return;
                }
                self.search.catalogue_pending = false;
                match result {
                    Ok(results) => {
                        let uris = results
                            .tracks
                            .iter()
                            .flat_map(|page| page.items.iter())
                            .map(|track| track.uri.clone())
                            .collect();
                        self.request_contains(uris);
                        self.settings.remember_search(&query);
                        self.settings_dirty = true;
                        self.search.results = Loadable::Loaded(results);
                        self.search.results_serial = serial;
                        self.show_search_playlists();
                    }
                    Err(error) => self.search_failed(&gettext(self.locale, "Search"), error),
                }
            }
            ApiResponse::Artist { id, result } => {
                if result.is_ok()
                    && let Some(page) = self.artist_pages.get_mut(&id)
                    && page.top_tracks.needs_load()
                {
                    page.top_tracks = Loadable::Loading;
                    self.backend
                        .api(ApiRequest::ArtistTopTracks { id: id.clone() });
                }
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.artist = Loadable::from_result(result);
                }
            }
            ApiResponse::ArtistTopTracks { id, result } => {
                if let Ok(tracks) = &result {
                    let uris: Vec<String> = tracks.iter().map(|track| track.uri.clone()).collect();
                    self.request_contains(uris);
                }
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.top_tracks = Loadable::from_result(result);
                }
            }
            ApiResponse::ArtistAlbums {
                id,
                groups,
                offset,
                result,
            } => {
                if let Ok(albums) = &result {
                    self.request_album_types(albums.items.iter());
                }
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    let list = page.albums.entry(groups).or_default();
                    match result {
                        Ok(albums) => list.absorb(offset, albums),
                        Err(error) => list.fail(error.to_string()),
                    }
                }
            }
            ApiResponse::RelatedArtists { id, result } => {
                if let Some(page) = self.artist_pages.get_mut(&id) {
                    page.related = Loadable::from_result(result);
                }
            }
            ApiResponse::Album { id, result } => {
                let mut uris = Vec::new();
                if let Ok(album) = &result {
                    self.request_album_types(std::iter::once(album));
                }
                if let Some(page) = self.album_pages.get_mut(&id) {
                    match result {
                        Ok(mut album) => {
                            if let Some(tracks) = album.tracks.take() {
                                uris = tracks.items.iter().map(|track| track.uri.clone()).collect();
                                page.tracks.absorb(0, tracks);
                            }
                            page.album = Loadable::Loaded(album);
                            if !page.tracks.loaded_once {
                                page.tracks.loading = true;
                                self.backend.api(ApiRequest::AlbumTracks {
                                    id,
                                    offset: 0,
                                    generation: page.generation,
                                });
                            }
                        }
                        Err(error) => page.album = Loadable::Failed(error.to_string()),
                    }
                }
                self.request_contains(uris);
            }
            ApiResponse::AlbumTracks {
                id,
                offset,
                generation,
                result,
            } => {
                if self
                    .album_pages
                    .get(&id)
                    .is_none_or(|page| page.generation != generation)
                {
                    return;
                }
                let mut uris = Vec::new();
                if let Some(page) = self.album_pages.get_mut(&id) {
                    match result {
                        Ok(tracks) => {
                            uris = tracks.items.iter().map(|track| track.uri.clone()).collect();
                            page.tracks.absorb(offset, tracks);
                        }
                        Err(error) => page.tracks.fail(error.to_string()),
                    }
                }
                self.request_contains(uris);
                // A sorted table means the whole list, not the loaded part.
                if self.table_sorts.contains_key(&Page::Album(id.clone())) {
                    self.load_more(Page::Album(id));
                }
            }
            ApiResponse::AlbumQueueTracks {
                request,
                offset,
                result,
            } => {
                self.receive_album_queue(request, offset, result);
            }
            ApiResponse::Show { id, result } => {
                if let Some(page) = self.show_pages.get_mut(&id) {
                    match result {
                        Ok(mut show) => {
                            if let Some(episodes) = show.episodes.take() {
                                page.episodes.absorb(0, episodes);
                            }
                            page.show = Loadable::Loaded(show);
                            if !page.episodes.loaded_once {
                                page.episodes.loading = true;
                                self.backend.api(ApiRequest::ShowEpisodes { id, offset: 0 });
                            }
                        }
                        Err(error) => page.show = Loadable::Failed(error.to_string()),
                    }
                }
            }
            ApiResponse::ShowEpisodes { id, offset, result } => {
                if let Some(page) = self.show_pages.get_mut(&id) {
                    match result {
                        Ok(episodes) => page.episodes.absorb(offset, episodes),
                        Err(error) => page.episodes.fail(error.to_string()),
                    }
                }
            }
            ApiResponse::Track { id, result } => {
                self.track_requests.remove(&id);
                match result {
                    Ok(track) => {
                        self.remember_track_recording(&track);
                        self.request_recording_candidates(&track);
                        if self.liked_songs.update_track(&track) {
                            self.sync_liked_songs();
                        }
                        self.resolve_pasted_song(
                            &format!("spotify:track:{id}"),
                            Some(PlayableItem::Track(track.clone())),
                        );
                        self.track_cache.insert(id.clone(), track);
                        self.track_used.insert(id, Instant::now());
                    }
                    Err(error) => {
                        self.resolve_pasted_song(&format!("spotify:track:{id}"), None);
                        if self.pending_link.as_deref()
                            == Some(format!("spotify:track:{id}").as_str())
                        {
                            self.pending_link = None;
                            self.toast_error(
                                gettext(self.locale, "Cannot open this song: {error}")
                                    .replace("{error}", &error.to_string()),
                            );
                        }
                    }
                }
            }
            // Only a link asks for an episode; its podcast's page is the
            // nearest thing to a page for it.
            ApiResponse::Episode { result, .. } => match result {
                Ok(episode) => match episode.show.filter(|show| !show.id.is_empty()) {
                    Some(show) => self.open(Page::Show(show.id)),
                    None => self.toast_error(gettext(
                        self.locale,
                        "This episode's podcast is not on Spotify",
                    )),
                },
                Err(error) => self.toast_error(
                    gettext(self.locale, "Cannot open this episode: {error}")
                        .replace("{error}", &error.to_string()),
                ),
            },
            ApiResponse::Remote { action, result } => {
                if let RemoteAction::Volume { request } = action {
                    let Some(pending) = &mut self.pending_remote_volume else {
                        return;
                    };
                    if pending.request != request {
                        // An earlier write must not settle a newer adjustment.
                        return;
                    }
                    if result.is_ok() {
                        pending.after_poll = Some(self.remote_poll_seq);
                    }
                }
                if matches!(action, RemoteAction::Play | RemoteAction::Pause) {
                    self.clear_play_pending();
                }
                match result {
                    Ok(()) => {
                        self.remote_recheck_at = Some(Instant::now() + REMOTE_RECHECK);
                        self.remote_command_at = Some(Instant::now());
                        if action == RemoteAction::Shuffle {
                            self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
                            if let Some(pending) = &mut self.queue_shuffle_pending {
                                pending.at = Instant::now();
                            } else {
                                self.note_shuffle_pending();
                            }
                        }
                    }
                    Err(error) => {
                        self.optimistic_playing = None;
                        self.pending_remote_position = None;
                        if matches!(action, RemoteAction::Volume { .. }) {
                            self.pending_remote_volume = None;
                        }
                        if action == RemoteAction::Repeat {
                            self.repeat_hold = None;
                        }
                        if matches!(
                            action,
                            RemoteAction::Play | RemoteAction::Next | RemoteAction::Previous
                        ) {
                            self.queue_start_pending = None;
                            if self.intent_track.as_ref().is_some_and(|intent| {
                                matches!(intent.confirmation, TrackConfirmation::Remote { .. })
                            }) {
                                self.intent_track = None;
                            }
                        }
                        let hint = if error.status() == Some(404) {
                            format!(
                                " {}",
                                gettext(
                                    self.locale,
                                    "Choose a device from the devices menu first."
                                )
                            )
                        } else {
                            String::new()
                        };
                        self.toast_error(format!(
                            "{}: {error}.{hint}",
                            remote_action_label(self.locale, action)
                        ));
                    }
                }
                self.poll_remote_soon();
            }
            ApiResponse::Transferred { device_id, result } => match result {
                Ok(()) => {
                    self.selected_device = Some(device_id);
                    self.show_devices = false;
                    self.remote_command_at = Some(Instant::now());
                    self.poll_remote_soon();
                    self.refresh_devices();
                }
                Err(error) => self.toast_error(
                    gettext(self.locale, "Couldn't switch device: {error}")
                        .replace("{error}", &error.to_string()),
                ),
            },
            ApiResponse::QueueAdded { label: _, result } => match result {
                Ok(()) => {
                    // Refresh the queue after the optimistic update.
                    self.refresh_queue(true);
                }
                Err(error) => self.toast_error(
                    gettext(self.locale, "Couldn't add to queue: {error}")
                        .replace("{error}", &error.to_string()),
                ),
            },
            ApiResponse::QueueBatchAdded {
                request,
                added,
                result,
            } => {
                if self.pending_queue_batches.remove(&request).as_ref() != Some(&self.target()) {
                    return;
                }
                if result.is_err() {
                    self.remove_failed_queue_adds(request, added);
                }
                for addition in &mut self.pending_queue_adds {
                    if addition.write.is_some_and(|(write, _)| write == request) {
                        addition.write = None;
                        addition.at = Instant::now();
                    }
                }
                if let Err(error) = result {
                    self.toast_error(
                        gettext(self.locale, "Couldn't add to queue: {error}")
                            .replace("{error}", &error.to_string()),
                    );
                }
                self.refresh_queue(true);
            }
        }
    }

    /// How often playback is polled: every 20 s while this computer plays,
    /// else every 4 s, but no more often than `REMOTE_POLL_MIN` outside the
    /// window after a command to another device.
    fn regular_poll_interval(&self) -> Duration {
        let pace = match self.target() {
            Target::Local if self.local.is_active() => REMOTE_POLL_IDLE,
            _ => REMOTE_POLL_ACTIVE,
        };
        let floor = if self
            .remote_command_at
            .is_some_and(|at| at.elapsed() < REMOTE_COMMAND_WINDOW)
        {
            Duration::ZERO
        } else {
            REMOTE_POLL_MIN
        };
        pace.max(floor)
    }

    /// When the next playback poll is due, after the last one, and whether
    /// it comes early because a song on another device should have ended.
    fn remote_poll_interval(&self) -> (Duration, bool) {
        let interval = self.regular_poll_interval();
        match self.remote_song_end() {
            Some(end) if end < interval => (end, true),
            _ => (interval, false),
        }
    }

    fn poll_remote_if_due(&mut self) {
        let (interval, song_end) = self.remote_poll_interval();
        let elapsed = self.remote_polled_at.elapsed();
        if self.remote_poll_pending || elapsed < interval {
            return;
        }
        if song_end && elapsed < self.regular_poll_interval() {
            self.remote_end_polled_at = Some(Instant::now());
        }
        self.poll_remote(false);
    }

    /// How long after the last poll the song playing on another device
    /// should end, a second over, so the next song shows soon after it
    /// starts. At most once every `REMOTE_POLL_MIN`.
    fn remote_song_end(&self) -> Option<Duration> {
        if matches!(self.target(), Target::Local)
            || self
                .remote_end_polled_at
                .is_some_and(|at| at.elapsed() < REMOTE_POLL_MIN)
        {
            return None;
        }
        let remote = self
            .remote_fresh()
            .filter(|remote| remote.state.is_playing)?;
        let left = remote
            .state
            .item
            .as_ref()?
            .duration_ms()
            .checked_sub(remote.state.progress_ms?)
            .filter(|left| *left > 0)?;
        let due = remote.received_at + Duration::from_millis(left.into()) + Duration::from_secs(1);
        due.checked_duration_since(self.remote_polled_at)
    }

    fn poll_remote_soon(&mut self) {
        self.remote_polled_at = Instant::now() - REMOTE_POLL_IDLE + Duration::from_millis(700);
    }

    // ---- navigation ------------------------------------------------------------

    pub fn open(&mut self, page: Page) {
        self.touch_page(&page);
        if *self.page() == page {
            self.ensure_loaded(page.clone());
            return;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(page.clone());
        if self.history.len() > 60 {
            self.history.remove(0);
        }
        self.history_index = self.history.len() - 1;
        self.show_devices = false;
        self.ensure_loaded(page.clone());
        self.evict_stale_pages();
    }

    /// Opens the page behind the pending link once the account is signed
    /// in. A song and an episode have no page of their own here, so the
    /// album's and the podcast's open, once Spotify has said which.
    fn open_pending_link(&mut self) {
        let Some(uri) = self.pending_link.clone() else {
            return;
        };
        if self.user.is_none() {
            return;
        }
        if crate::jam::is_invitation(&uri) {
            self.pending_link = None;
            self.backend.send(Command::Jam(JamRequest::Open(uri)));
            return;
        }
        if let Some(query) = crate::link::search_query(&uri) {
            self.pending_link = None;
            self.actions.push(Action::Search(query));
            self.actions.push(Action::FocusSearch);
            return;
        }
        if let Some(page) = Page::from_uri(&uri) {
            self.pending_link = None;
            self.open(page);
            return;
        }
        let id = util::uri_id(&uri).unwrap_or_default().to_string();
        match util::uri_kind(&uri) {
            Some("track") => {
                if let Some(track) = self.read_cached_track(&id) {
                    self.pending_link = None;
                    let album = track
                        .album
                        .as_ref()
                        .map(|album| album.id.clone())
                        .filter(|id| !id.is_empty());
                    match album {
                        Some(album) => self.open(Page::Album(album)),
                        None => self.toast_error(gettext(
                            self.locale,
                            "This song's album is not on Spotify",
                        )),
                    }
                } else if self.track_requests.insert(id.clone()) {
                    // The answer lands in the cache, and the link waits
                    // for the next frame to find it there.
                    self.backend.api(ApiRequest::Track { id });
                }
            }
            Some("episode") => {
                self.pending_link = None;
                self.backend.api(ApiRequest::Episode { id });
            }
            _ => {
                self.pending_link = None;
                self.toast_error(gettext(
                    self.locale,
                    "SwiftSpot cannot open this kind of Spotify link",
                ));
            }
        }
    }

    fn touch_page(&mut self, page: &Page) {
        self.page_used.insert(page.clone(), Instant::now());
    }

    fn read_cached_track(&mut self, id: &str) -> Option<Track> {
        let track = self.track_cache.get(id).cloned()?;
        self.track_used.insert(id.to_owned(), Instant::now());
        Some(track)
    }

    /// Drops page caches that exceed the cap, keeping the open page, the
    /// playing context, and the most recently used pages. History does not
    /// protect a page from the cap. Track metadata is an LRU of 800.
    /// Table-row copies of dropped playlist and album pages go with them.
    fn evict_stale_pages(&mut self) {
        const MAX_PLAYLIST_PAGES: usize = 12;
        const MAX_ALBUM_PAGES: usize = 16;
        const MAX_ARTIST_PAGES: usize = 10;
        const MAX_SHOW_PAGES: usize = 8;
        const MAX_RADIO_PAGES: usize = 6;
        const MAX_USER_PAGES: usize = 6;
        const MAX_TRACK_CACHE: usize = 800;

        let mut protected_playlists = HashSet::new();
        for (id, page) in &self.playlist_pages {
            if page.pending_writes > 0 || page.optimistic_snapshot.is_some() {
                protected_playlists.insert(id.clone());
            }
        }
        let mut protected_albums = HashSet::new();
        let mut protected_artists = HashSet::new();
        let mut protected_shows = HashSet::new();
        let mut protected_radios = HashSet::new();
        let mut protected_users = HashSet::new();
        if let Some(seed) = self
            .playing_context_uri()
            .and_then(|uri| util::station_seed(&uri))
        {
            protected_radios.insert(seed);
        }
        match self.page() {
            Page::Playlist(id) => {
                protected_playlists.insert(id.clone());
            }
            Page::Album(id) => {
                protected_albums.insert(id.clone());
            }
            Page::Artist(id) => {
                protected_artists.insert(id.clone());
            }
            Page::Show(id) => {
                protected_shows.insert(id.clone());
            }
            Page::Radio(seed) => {
                protected_radios.insert(seed.clone());
            }
            Page::User(id) => {
                protected_users.insert(id.clone());
            }
            _ => {}
        }
        if let Some(uri) = self.playing_context_uri()
            && let Some(kind) = util::uri_kind(&uri)
            && let Some(id) = util::uri_id(&uri)
        {
            match kind {
                "playlist" => {
                    protected_playlists.insert(id.to_string());
                }
                "album" => {
                    protected_albums.insert(id.to_string());
                }
                "artist" => {
                    protected_artists.insert(id.to_string());
                }
                "show" => {
                    protected_shows.insert(id.to_string());
                }
                _ => {}
            }
        }
        evict_lru_map(
            &mut self.playlist_pages,
            &self.page_used,
            |id| Page::Playlist(id.to_string()),
            &protected_playlists,
            MAX_PLAYLIST_PAGES,
        );
        evict_lru_map(
            &mut self.album_pages,
            &self.page_used,
            |id| Page::Album(id.to_string()),
            &protected_albums,
            MAX_ALBUM_PAGES,
        );
        evict_lru_map(
            &mut self.artist_pages,
            &self.page_used,
            |id| Page::Artist(id.to_string()),
            &protected_artists,
            MAX_ARTIST_PAGES,
        );
        evict_lru_map(
            &mut self.show_pages,
            &self.page_used,
            |id| Page::Show(id.to_string()),
            &protected_shows,
            MAX_SHOW_PAGES,
        );
        evict_lru_map(
            &mut self.radio_pages,
            &self.page_used,
            |seed| Page::Radio(seed.to_string()),
            &protected_radios,
            MAX_RADIO_PAGES,
        );
        evict_lru_map(
            &mut self.user_pages,
            &self.page_used,
            |id| Page::User(id.to_string()),
            &protected_users,
            MAX_USER_PAGES,
        );
        self.page_used.retain(|page, _| match page {
            Page::Playlist(id) => self.playlist_pages.contains_key(id),
            Page::Album(id) => self.album_pages.contains_key(id),
            Page::Artist(id) => self.artist_pages.contains_key(id),
            Page::Show(id) => self.show_pages.contains_key(id),
            Page::Radio(seed) => self.radio_pages.contains_key(seed),
            Page::User(id) => self.user_pages.contains_key(id),
            _ => true,
        });
        if self.track_cache.len() > MAX_TRACK_CACHE {
            let playing = self.now_playing().and_then(|now| now.id);
            let overflow = self.track_cache.len() - MAX_TRACK_CACHE;
            let mut victims: Vec<(Option<Instant>, String)> = self
                .track_cache
                .keys()
                .filter(|id| playing.as_ref() != Some(*id))
                .map(|id| {
                    let used = self.track_used.get(id).copied();
                    (used, id.clone())
                })
                .collect();
            victims.sort();
            for (_, id) in victims.into_iter().take(overflow) {
                self.track_cache.remove(&id);
                self.track_used.remove(&id);
            }
        }
    }

    // ---- playback --------------------------------------------------------------

    fn remote(&mut self, action: RemoteAction, device_id: Option<String>) {
        if device_id.is_none() && self.remote_fresh().is_none() {
            // Spotify would only answer "no active device found".
            self.clear_play_pending();
            self.toast(gettext(
                self.locale,
                "Nothing is playing. Pick something first",
            ));
            return;
        }
        self.backend.api(ApiRequest::Remote {
            action,
            device_id,
            play: None,
            position_ms: 0,
            percent: 0,
            flag: false,
            repeat: String::new(),
        });
    }

    /// Remembers `uri` as the most recently played context, for the
    /// sidebar's order.
    fn note_recent_context(&mut self, uri: &str) {
        self.session_dirty = true;
        if !Self::is_sidebar_context(uri) {
            return;
        }
        self.recent_contexts.retain(|held| held != uri);
        self.recent_contexts.insert(0, uri.to_string());
        self.recent_contexts.truncate(RECENT_CONTEXTS_KEPT);
    }

    /// Whether the sidebar lists `uri`, so its order has a place for it.
    fn is_sidebar_context(uri: &str) -> bool {
        uri.contains(":playlist:") || uri.contains(":album:") || uri.contains(":collection")
    }

    /// Notes the contexts of a page of history older than every play the
    /// order already holds. They go after it, newest first, and a context
    /// already in the order keeps the place a newer play gave it.
    fn note_older_contexts(&mut self, history: &[crate::api::models::PlayHistory]) {
        for play in history {
            let Some(uri) = play.context.as_ref().map(|context| context.uri.as_str()) else {
                continue;
            };
            if self.recent_contexts.len() >= RECENT_CONTEXTS_KEPT {
                break;
            }
            if Self::is_sidebar_context(uri) && !self.recent_contexts.iter().any(|held| held == uri)
            {
                self.recent_contexts.push(uri.to_string());
                self.session_dirty = true;
            }
        }
    }

    /// Notes every context in a page of play history, oldest first, so
    /// the newest ends up at the front of the sidebar's order.
    fn note_recent_contexts(&mut self, history: &[crate::api::models::PlayHistory]) {
        let contexts: Vec<String> = history
            .iter()
            .rev()
            .filter_map(|play| play.context.as_ref().map(|context| context.uri.clone()))
            .collect();
        for context in contexts {
            self.note_recent_context(&context);
        }
    }

    /// Merges local and Spotify history for the Recent tab.
    fn rebuild_recents(&mut self) {
        self.recents_view = crate::history::merged(self.plays.plays(), &self.recents.items).into();
    }

    /// Adds a page of play history to the Recents tab.
    ///
    /// Repeated plays remain separate. Only identical track-and-time entries
    /// are deduplicated across page boundaries.
    ///
    /// Spotify paginates backwards with `before`; a short page ends the list.
    fn absorb_recents(
        &mut self,
        page: crate::api::models::CursorPage<crate::api::models::PlayHistory>,
        limit: u32,
    ) {
        // A page asked for with a cursor is older than the pages before it.
        if self.recents.after.is_some() {
            self.note_older_contexts(&page.items);
        } else {
            self.note_recent_contexts(&page.items);
        }
        self.recents.error = None;
        let short_page = (page.items.len() as u32) < limit;
        let cursor = page.cursors.as_ref().and_then(|c| c.before.clone());
        let mut seen: std::collections::HashSet<(String, Option<String>)> = self
            .recents
            .items
            .iter()
            .map(|play| (play.track.uri.clone(), play.played_at.clone()))
            .collect();
        let fresh: Vec<crate::api::models::PlayHistory> = page
            .items
            .into_iter()
            .filter(|play| seen.insert((play.track.uri.clone(), play.played_at.clone())))
            .collect();
        let uris: Vec<String> = fresh.iter().map(|play| play.track.uri.clone()).collect();
        self.recents.items.extend(fresh);
        match cursor {
            Some(cursor) if !short_page => self.recents.after = Some(cursor),
            _ => {
                self.recents.complete = true;
                self.recents.after = None;
            }
        }
        // Request saved state only for newly added rows.
        if !uris.is_empty() {
            self.request_contains(uris);
        }
        self.rebuild_recents();
    }

    /// Loaded track URIs for a context, in display order.
    fn context_track_uris(&self, context_uri: &str) -> Option<Vec<String>> {
        let uris: Vec<String> = if let Some(id) = context_uri.strip_prefix("spotify:playlist:") {
            self.playlist_pages
                .get(id)?
                .items
                .items
                .iter()
                .filter_map(|item| item.playable())
                .map(|item| item.uri().to_string())
                .collect()
        } else if let Some(id) = context_uri.strip_prefix("spotify:album:") {
            self.album_pages
                .get(id)?
                .tracks
                .items
                .iter()
                .filter(|track| !track.uri.is_empty())
                .map(|track| track.uri.clone())
                .collect()
        } else if context_uri.ends_with(":collection") {
            self.library
                .liked
                .items
                .iter()
                .map(|item| item.track.uri.clone())
                .collect()
        } else {
            return None;
        };
        (!uris.is_empty()).then_some(uris)
    }

    fn random_track_in(&self, context_uri: &str) -> Option<String> {
        let uris = self.context_track_uris(context_uri)?;
        Some(uris[rand::random_range(0..uris.len())].clone())
    }

    /// Last known context length from the library.
    /// Used to choose a random shuffle offset before rows are loaded.
    fn context_len(&self, context_uri: &str) -> Option<u32> {
        if context_uri.ends_with(":collection") {
            return self.library.liked.total;
        }
        match util::uri_kind(context_uri)? {
            "playlist" => self
                .library
                .playlists
                .get()?
                .iter()
                .find(|playlist| playlist.uri == context_uri)?
                .tracks
                .as_ref()
                .map(|tracks| tracks.total),
            "album" => {
                self.library
                    .albums
                    .items
                    .iter()
                    .find(|saved| saved.album.uri == context_uri)?
                    .album
                    .total_tracks
            }
            "show" => {
                self.library
                    .shows
                    .items
                    .iter()
                    .find(|saved| saved.show.uri == context_uri)?
                    .show
                    .total_episodes
            }
            _ => None,
        }
    }

    /// Where a shuffled play of `context_uri` begins, as an offset by
    /// track URI or by position: a random one of the rows the app holds,
    /// or, when it holds none, a random position within the length the
    /// library knows. Neither, and the play carries no offset at all:
    /// librespot then picks its own random track, and only the Web API,
    /// which would start at track one, needs telling where to go.
    fn shuffle_start(&self, context_uri: &str) -> (Option<String>, Option<u32>) {
        if let Some(uri) = self.random_track_in(context_uri) {
            return (Some(uri), None);
        }
        if matches!(self.target(), Target::Remote(Some(_)))
            && let Some(len) = self.context_len(context_uri)
            && len > 0
        {
            return (None, Some(rand::random_range(0..len)));
        }
        (None, None)
    }

    /// Start a playlist at its first available row, not an unspecified place
    /// in the player's resolved context. A loaded range from the middle is
    /// not the playlist's beginning; without its prefix, request position zero.
    fn playlist_start(&self, id: &str) -> (Option<String>, Option<u32>) {
        let first = self
            .playlist_pages
            .get(id)
            .filter(|page| page.items.base_offset == 0)
            .and_then(|page| {
                page.items.items.iter().find_map(|row| {
                    let item = row.playable()?;
                    if row.is_local
                        || item.uri().is_empty()
                        || matches!(item, PlayableItem::Track(track)
                            if track.is_local || track.is_playable == Some(false))
                    {
                        return None;
                    }
                    Some(item.uri().to_string())
                })
            });
        match first {
            Some(uri) => (Some(uri), None),
            None => (None, Some(0)),
        }
    }

    /// With `shuffle_first`, shuffle is turned on before playback starts,
    /// in one ordered exchange: two independent requests race, and shuffle
    /// sometimes lost.
    fn free_account(&self) -> bool {
        self.user
            .as_ref()
            .and_then(|user| user.product.as_deref())
            .is_some_and(|product| product != "premium")
    }

    /// Opens the browser for the playback grant, unless this computer
    /// already plays or is on its way to.
    fn enable_playback(&mut self) {
        if self.local_ready
            || matches!(
                self.local_playback,
                LocalPlayback::Authorizing | LocalPlayback::Connecting
            )
        {
            return;
        }
        self.backend.send(Command::AuthorizePlayback);
        self.toast(gettext(
            self.locale,
            "Opening your browser to set up local playback",
        ));
    }

    fn play_request(&mut self, request: PlayRequest, shuffle_first: bool) {
        self.resume_load = None;
        // Shuffle applies across contexts until disabled. A selected row still
        // starts first; otherwise choose a random starting track.
        let mut request = request;
        self.queue_shuffle_pending = None;
        if shuffle_first {
            self.shuffle_wanted = true;
            self.shuffle_set_at = Some(Instant::now());
            self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
        }
        let shuffle = shuffle_first || self.shuffle_wanted;
        if request.offset_uri.is_none()
            && request.offset_position.is_none()
            && request.uris.is_empty()
            && let Some(context) = request.context_uri.as_deref()
        {
            if shuffle {
                (request.offset_uri, request.offset_position) = self.shuffle_start(context);
            } else if let Some(id) = context.strip_prefix("spotify:playlist:") {
                (request.offset_uri, request.offset_position) = self.playlist_start(id);
            }
        }
        let mut keys: Vec<String> = Vec::new();
        if let Some(context) = &request.context_uri {
            keys.push(context.clone());
        }
        if let Some(offset) = &request.offset_uri {
            keys.push(offset.clone());
        }
        match request.offset_position {
            // The play starts at a chosen row; only that row is starting.
            Some(position) => {
                if let Some(uri) = request.uris.get(position as usize) {
                    keys.push(uri.clone());
                }
            }
            // No chosen row: the list starts at its first song.
            None if request.offset_uri.is_none() => {
                if let Some(first) = request.uris.first() {
                    keys.push(first.clone());
                }
            }
            None => {}
        }
        let expected_track = keys.iter().find(|key| key.contains(":track:")).cloned();
        if let Some(uri) = expected_track {
            if let Some(context) = &request.context_uri {
                self.cache_track_from_context(context, &uri);
            }
            self.expect_track(uri, request.position_ms);
        } else {
            self.intent_track = None;
        }
        self.set_play_pending(keys);
        if let Some(context) = request.context_uri.clone() {
            self.note_recent_context(&context);
        }
        // Show a context as playing at once, or clear the previous context
        // for a plain track list. Spotify's state catches up behind it.
        self.assumed_context = Some(AssumedContext {
            uri: request.context_uri.clone().unwrap_or_default(),
            shuffle: shuffle.then_some(true),
            at: Instant::now(),
        });
        self.queue_start_pending = Some(self.target());
        match self.target() {
            Target::Local if !self.local.connected => {
                // Hold the request while the local engine reconnects.
                self.queued_play = Some(request);
            }
            Target::Local => {
                self.queued_play = None;
                let mut load = local_load(&request, shuffle);
                load.repeat = Some(self.local_repeat());
                self.local_list = load.context_uri.is_none().then(|| load.uris.clone());
                let shuffle_after = shuffle && load.shuffle.is_none() && !load.uris.is_empty();
                self.backend.player(PlayerCommand::Load(load));
                if shuffle_after {
                    self.backend.player(PlayerCommand::Shuffle(true));
                }
                self.optimistic_playing = Some((true, Instant::now()));
            }
            Target::Remote(Some(device_id)) => {
                self.queued_play = None;
                if shuffle {
                    self.backend.api(ApiRequest::ShufflePlay {
                        device_id: Some(device_id),
                        play: request,
                    });
                } else {
                    self.backend.api(ApiRequest::Remote {
                        action: RemoteAction::Play,
                        device_id: Some(device_id),
                        play: Some(request),
                        position_ms: 0,
                        percent: 0,
                        flag: false,
                        repeat: String::new(),
                    });
                }
                self.optimistic_playing = Some((true, Instant::now()));
            }
            Target::Remote(None) => {
                // No remote device is active, and this computer's player is
                // not ready. Never ask Spotify to play "nowhere": either
                // wait for the connecting engine or ask for a device.
                if matches!(
                    self.local_playback,
                    LocalPlayback::Connecting | LocalPlayback::Authorizing
                ) || (self.settings.playback_authorized
                    && matches!(self.auth, AuthStatus::Starting | AuthStatus::Connecting))
                {
                    self.queued_play = Some(request);
                } else if matches!(self.local_playback, LocalPlayback::Unavailable)
                    && !self.free_account()
                {
                    // Nothing else is playing: this computer is the device
                    // a first play means, so set it up and play there.
                    self.enable_playback();
                    self.queued_play = Some(request);
                } else {
                    self.queue_start_pending = None;
                    self.clear_play_pending();
                    self.queued_play = None;
                    self.toast(gettext(
                        self.locale,
                        "Choose a device, or enable playback on this computer",
                    ));
                    self.refresh_devices();
                }
            }
        }
    }

    /// A playlist's disk cache has been read. Whether or not the page
    /// still matches it, what the Web API said about each song's
    /// availability holds, for rows already shown and rows to come.
    fn receive_playlist_cache(
        &mut self,
        account_id: &str,
        id: &str,
        generation: u64,
        mut cache: Option<PlaylistCache>,
    ) {
        if self.user_id() != Some(account_id) {
            return;
        }
        if let Some(page) = self.playlist_pages.get_mut(id) {
            if page.generation != generation {
                return;
            }
            page.cache_checked = true;
            if let Some(cache) = &mut cache {
                for (uri, playable) in known_availability(&cache.items) {
                    self.playlist_availability
                        .entry(uri.to_string())
                        .or_insert(playable);
                }
                // A cache may arrive after a fresh answer from another page.
                // Correct its flags before this prefix can be adopted.
                for row in &mut cache.items {
                    if let Some(PlayableItem::Track(track)) = row.item.as_mut()
                        && let Some(playable) = self.playlist_availability.get(&track.uri)
                    {
                        if track.is_playable != Some(*playable) {
                            cache.appendable = false;
                        }
                        track.is_playable = Some(*playable);
                    }
                }
                if fill_availability(&self.playlist_availability, &mut page.items.items) {
                    page.items.revision = page.items.revision.wrapping_add(1);
                    page.cache_append_valid = false;
                }
            }
            page.pending_cache = cache;
        }
        self.try_adopt_playlist_cache(id);
        self.checkpoint_playlist_cache(id);
    }

    fn receive_playlist_cache_stored(
        &mut self,
        account_id: &str,
        id: &str,
        generation: u64,
        snapshot: &str,
        success: bool,
    ) {
        self.playlist_cache_write_in_flight = false;
        let same_account = self.user_id() == Some(account_id);
        let should_check_again = same_account
            && self.playlist_pages.get_mut(id).is_some_and(|page| {
                let Some(pending) = page.cache_write_pending.take() else {
                    return false;
                };
                if pending.generation != generation || pending.snapshot != snapshot {
                    page.cache_write_pending = Some(pending);
                    return false;
                }
                let current = page.generation == generation
                    && page.items_generation == generation
                    && page.pending_writes == 0
                    && page
                        .playlist
                        .get()
                        .and_then(|playlist| playlist.snapshot_id.as_deref())
                        == Some(snapshot);
                if !current {
                    page.cache_append_valid = false;
                    return true;
                }
                if success && page.cache_append_valid {
                    page.cache_saved_through = Some(pending.through);
                    page.cache_saved_rows = pending.rows;
                    page.cache_saved_total = Some(pending.total);
                } else if !success {
                    page.cache_append_valid = false;
                }
                success || !pending.replacing
            });
        if should_check_again {
            self.checkpoint_playlist_cache(id);
        }
        if !self.playlist_cache_write_in_flight {
            let waiting: Vec<_> = self
                .playlist_pages
                .keys()
                .filter(|waiting| !same_account || waiting.as_str() != id)
                .cloned()
                .collect();
            for waiting in waiting {
                self.checkpoint_playlist_cache(&waiting);
                if self.playlist_cache_write_in_flight {
                    break;
                }
            }
        }
    }

    /// Adopt a playlist's cached prefix once Spotify confirms its snapshot.
    fn try_adopt_playlist_cache(&mut self, id: &str) {
        let mut uris = Vec::new();
        let mut adders: Vec<String> = Vec::new();
        let mut tracks = Vec::new();
        if let Some(page) = self.playlist_pages.get_mut(id) {
            if page.pending_writes > 0 {
                return;
            }
            let Some(snapshot_now) = page
                .playlist
                .get()
                .and_then(|playlist| playlist.snapshot_id.clone())
            else {
                return;
            };
            let confirmed_total = page.playlist.get().and_then(|playlist| {
                playlist
                    .items_count
                    .as_ref()
                    .or(playlist.tracks.as_ref())
                    .map(|count| count.total)
            });
            match &page.pending_cache {
                Some(cache)
                    if cache.snapshot == snapshot_now
                        && confirmed_total.is_none_or(|total| cache.total == total) => {}
                Some(_) => {
                    // A revision alone cannot validate an inconsistent cache.
                    // Let the live item request establish the rows and order.
                    page.pending_cache = None;
                    return;
                }
                None => return,
            }
            let Some(cache) = page.pending_cache.take() else {
                return;
            };
            let cached_through = cache.next_offset.unwrap_or(cache.total);
            let cached_rows = cache.items.len();
            let cached_total = cache.total;
            let cache_appendable = cache.appendable;
            let loaded_through = if page.items.base_offset == 0 {
                page.items.items.len() as u32
            } else {
                page.items
                    .windows
                    .get(&0)
                    .map_or(0, |items| items.len() as u32)
            };
            if loaded_through >= cached_through {
                page.cache_saved_through = Some(cached_through);
                page.cache_saved_rows = cached_rows;
                page.cache_saved_total = Some(cached_total);
                page.cache_append_valid = false;
                page.pending_cache = None;
                return;
            }
            uris = cache
                .items
                .iter()
                .filter_map(|item| item.playable())
                .map(|item| item.uri().to_string())
                .collect();
            tracks = cache
                .items
                .iter()
                .filter_map(|item| match item.playable() {
                    Some(PlayableItem::Track(track)) => Some(track.clone()),
                    _ => None,
                })
                .collect();
            adders = cache
                .items
                .iter()
                .filter_map(|item| item.added_by.as_ref()?.id.clone())
                .filter(|id| !id.is_empty())
                .collect();
            page.contributors.extend(adders.iter().cloned());
            page.items
                .adopt_cached_prefix(cache.items, cache.total, cache.next_offset);
            page.items_generation = page.generation;
            page.cache_saved_through = Some(cached_through);
            page.cache_saved_rows = cached_rows;
            page.cache_saved_total = Some(cached_total);
            page.cache_append_valid = cache_appendable;
            page.cache_restored_through = Some(cached_through);
        }
        for track in &tracks {
            self.remember_track_recording(track);
        }
        self.request_contains(uris);
        self.request_user_names(adders);
        self.sample_playlist_tail(id);
        if self
            .table_sorts
            .contains_key(&Page::Playlist(id.to_string()))
        {
            self.load_more(Page::Playlist(id.to_string()));
        }
    }

    /// Read the final page once for collaborators when the loaded rows do not
    /// already include it. This also runs after a disk cache wins startup.
    fn sample_playlist_tail(&mut self, id: &str) {
        let request = self.playlist_pages.get_mut(id).and_then(|page| {
            if page.tail_checked || !page.items.loaded_once {
                return None;
            }
            page.tail_checked = true;
            let loaded_end = page
                .items
                .base_offset
                .saturating_add(page.items.items.len().try_into().unwrap_or(u32::MAX));
            let total = page.items.total.filter(|total| *total > loaded_end)?;
            Some(ApiRequest::PlaylistSample {
                id: id.to_string(),
                offset: total.saturating_sub(PLAYLIST_PAGE_SIZE),
                generation: page.generation,
            })
        });
        if let Some(request) = request {
            self.backend.api(request);
        }
    }

    /// Save the first page, every ten pages after that, and the completed list.
    /// Only rows added since the confirmed checkpoint are copied on appends.
    fn checkpoint_playlist_cache(&mut self, id: &str) {
        const CHECKPOINT_ITEMS: u32 = PLAYLIST_PAGE_SIZE * 10;

        if self.playlist_cache_write_in_flight {
            return;
        }
        let command = self.playlist_pages.get_mut(id).and_then(|page| {
            let snapshot = page
                .playlist
                .get()
                .and_then(|playlist| playlist.snapshot_id.clone())?;
            if !page.items.loaded_once
                || page.items.items.is_empty()
                || page.items.base_offset != 0
                || page.items_generation != page.generation
                || page.pending_writes > 0
                || page.cache_write_pending.is_some()
            {
                return None;
            }
            if !page.cache_checked {
                return None;
            }
            let total = page.items.total?;
            let next_offset = page.items.next_offset;
            let through = next_offset.unwrap_or(total);
            let previous = page.cache_saved_through.unwrap_or(0);
            let complete = next_offset.is_none();
            let row_count = page.items.items.len();
            if page.cache_append_valid
                && page.cache_saved_through == Some(through)
                && page.cache_saved_rows == row_count
                && page.cache_saved_total == Some(total)
            {
                return None;
            }
            if previous > 0 && through.saturating_sub(previous) < CHECKPOINT_ITEMS && !complete {
                return None;
            }
            let replacing = !page.cache_append_valid
                || page.cache_saved_through.is_none()
                || page.cache_saved_total != Some(total)
                || page.cache_saved_rows > row_count;
            let rows = if replacing {
                PlaylistCacheRows::Replace(page.items.items.clone())
            } else {
                PlaylistCacheRows::Append {
                    previous_rows: page.cache_saved_rows,
                    previous_offset: previous,
                    items: page.items.items[page.cache_saved_rows..].to_vec(),
                }
            };
            page.cache_write_pending = Some(PlaylistCachePending {
                generation: page.generation,
                snapshot: snapshot.clone(),
                through,
                rows: row_count,
                total,
                replacing,
            });
            if replacing {
                page.cache_append_valid = true;
            }
            Some(Command::StorePlaylistCache {
                id: id.to_string(),
                generation: page.generation,
                snapshot,
                rows,
                total,
                next_offset,
            })
        });
        if let Some(command) = command {
            self.playlist_cache_write_in_flight = true;
            self.backend.send(command);
        }
    }

    /// Play what was playing when the app last closed. `false` when
    /// nothing is known to resume.
    fn resume_last(&mut self) -> bool {
        let Some(track) = self.resume_track.clone() else {
            return false;
        };
        let mut request = match self.resume_context.clone() {
            Some(context) => PlayRequest::context(context).starting_at_uri(track),
            None => PlayRequest::tracks(vec![track]),
        };
        request.position_ms = self.resume_position_ms;
        self.play_resumed(request);
        true
    }

    /// Plays a song at its old place, watching that its context can start it.
    fn play_resumed(&mut self, request: PlayRequest) {
        let expected = request
            .context_uri
            .as_ref()
            .and(request.offset_uri.clone())
            .map(|uri| (uri, request.position_ms));
        self.play_request(request, false);
        self.resume_load = expected;
    }

    fn toggle_play(&mut self) {
        if self.jam_redirects() {
            let command = if self.now_playing().is_some_and(|now| now.playing) {
                crate::jam::Command::Pause
            } else {
                crate::jam::Command::Resume
            };
            self.backend.send(Command::Jam(JamRequest::Send(command)));
            return;
        }
        if self.queued_play.is_some() {
            self.drop_queued_play();
            return;
        }
        let playing = self.now_playing().map(|now| now.playing);
        match self.target() {
            Target::Local => {
                if self.local.is_active() {
                    self.backend.player(PlayerCommand::Toggle);
                } else if let Some(remote) = self.remote_fresh() {
                    // Nothing is playing locally: resume on this computer.
                    let uri = remote
                        .state
                        .item
                        .as_ref()
                        .map(|item| item.uri().to_string());
                    let position = remote.state.progress_ms.unwrap_or(0);
                    if let Some(uri) = uri {
                        // Spotify's last state can lose the context of the
                        // song this computer remembers playing from one.
                        let context = remote
                            .state
                            .context
                            .as_ref()
                            .map(|context| context.uri.clone())
                            .filter(|context| !context.is_empty())
                            .or_else(|| {
                                self.resume_context
                                    .clone()
                                    .filter(|_| self.resume_track.as_ref() == Some(&uri))
                            });
                        let mut request = match context {
                            Some(context) => PlayRequest::context(context).starting_at_uri(uri),
                            None => PlayRequest::tracks(vec![uri]),
                        };
                        request.position_ms = position;
                        self.play_resumed(request);
                        return;
                    }
                    if !self.resume_last() {
                        self.toast(gettext(self.locale, "Pick something to play"));
                    }
                    return;
                } else {
                    if !self.resume_last() {
                        self.toast(gettext(self.locale, "Pick something to play"));
                    }
                    return;
                }
            }
            Target::Remote(device_id) => {
                if device_id.is_none() && self.remote_fresh().is_none() {
                    // Nothing is known to be playing anywhere, which is how
                    // a fresh start looks before the local engine is ready:
                    // pick up where the last run left off, the way the
                    // local branch does. The engine plays it once it is up.
                    if !self.resume_last() {
                        self.toast(gettext(self.locale, "Pick a song, album, or playlist"));
                    }
                    return;
                }
                self.set_play_pending(vec!["::toggle".into()]);
                if playing == Some(true) {
                    self.remote(RemoteAction::Pause, device_id);
                } else {
                    self.remote(RemoteAction::Play, device_id);
                }
            }
        }
        if let Some(playing) = playing {
            self.optimistic_playing = Some((!playing, Instant::now()));
        }
    }

    fn seek(&mut self, position_ms: u32) {
        // Dragging the bar under the remembered song moves the point a press
        // of play will resume from; there is no stream to seek yet.
        if self.now_playing_live().is_none() && self.resume_track.is_some() {
            self.resume_position_ms = position_ms;
            self.session_dirty = true;
            return;
        }
        match self.target() {
            Target::Local => {
                self.pending_local_position = Some((position_ms, Instant::now()));
                self.backend.player(PlayerCommand::Seek(position_ms));
            }
            Target::Remote(device_id) => {
                self.pending_remote_position = Some((position_ms, Instant::now()));
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Seek,
                    device_id,
                    play: None,
                    position_ms,
                    percent: 0,
                    flag: false,
                    repeat: String::new(),
                });
            }
        }
    }

    /// The volume this side set that the engine has yet to confirm, if the
    /// hold is still good. Clears itself once the engine agrees or it expires.
    fn held_local_volume(&mut self, reported: u16) -> Option<u16> {
        match self.pending_local_volume {
            Some((volume, at)) if volume != reported && at.elapsed() < OPTIMISTIC_HOLD => {
                Some(volume)
            }
            _ => {
                self.pending_local_volume = None;
                None
            }
        }
    }

    /// `settle` is false while the slider is still moving: the level is heard
    /// at once, and Spotify is told where it ended up on release.
    /// Whether the playing device takes volume changes from SwiftSpot.
    /// Spotify refuses them for some remote devices, so the controls are
    /// disabled for those rather than failing when used.
    pub fn can_set_volume(&self) -> bool {
        self.now_playing().is_none_or(|now| now.can_set_volume)
    }

    fn set_volume(&mut self, percent: u8, settle: bool) {
        let percent = percent.min(100);
        match self.target() {
            Target::Local => {
                let volume = percent_to_volume(percent);
                self.local.volume = volume;
                self.pending_local_volume = Some((volume, Instant::now()));
                // The engine echoes `VolumeChanged` only while this device
                // holds the Connect session, so the snapshot that would
                // otherwise persist this may never arrive.
                if self.settings.volume != volume {
                    self.settings.volume = volume;
                    self.settings_dirty = true;
                }
                self.backend.player(if settle {
                    PlayerCommand::Volume(volume)
                } else {
                    PlayerCommand::VolumePreview(volume)
                });
            }
            Target::Remote(_) if !settle || !self.can_set_volume() => {}
            Target::Remote(device_id) => {
                self.remote_volume_request += 1;
                self.pending_remote_volume = Some(PendingRemoteVolume {
                    percent,
                    device_id: device_id.clone(),
                    request: self.remote_volume_request,
                    after_poll: None,
                    mismatches: 0,
                });
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Volume {
                        request: self.remote_volume_request,
                    },
                    device_id,
                    play: None,
                    position_ms: 0,
                    percent,
                    flag: false,
                    repeat: String::new(),
                });
            }
        }
    }

    fn reconcile_remote_volume(&mut self, seq: u64, state: Option<&PlaybackState>) {
        let Some(pending) = &mut self.pending_remote_volume else {
            return;
        };
        if pending
            .after_poll
            .is_none_or(|after_poll| seq <= after_poll)
        {
            return;
        }
        let device = state.and_then(|state| state.device.as_ref());
        // A device change ends this intent. Missing volume on the same
        // device confirms neither the write nor a change made elsewhere.
        if device.and_then(|device| device.id.as_deref()) != pending.device_id.as_deref()
            || device.is_none()
        {
            self.pending_remote_volume = None;
            return;
        }
        let Some(reported) = device.and_then(|device| device.volume_percent) else {
            return;
        };
        if reported == pending.percent || pending.mismatches > 0 {
            self.pending_remote_volume = None;
        } else {
            // Even the first poll after a successful write can lag. Check
            // once more before accepting a volume set elsewhere, as for tracks.
            pending.mismatches += 1;
            pending.after_poll = Some(seq);
            self.remote_recheck_at = Some(Instant::now() + REMOTE_RECHECK);
        }
    }

    fn note_shuffle_pending(&mut self) {
        if let Loadable::Loaded(queue) = &self.queue {
            let at = Self::end_of_queued_rows(&queue.queue, &self.manual_queue);
            let context_uris: Vec<String> = queue.queue[at..]
                .iter()
                .map(|item| item.uri().to_string())
                .collect();
            if context_uris.len() > 1 && context_uris.iter().any(|uri| uri != &context_uris[0]) {
                self.queue_shuffle_pending = Some(QueueShufflePending {
                    current_uri: self.current_track_uri(),
                    context_uris,
                    at: Instant::now(),
                });
            } else {
                self.queue_shuffle_pending = None;
            }
        }
    }

    fn set_shuffle(&mut self, shuffle: bool) {
        self.shuffle_wanted = shuffle;
        self.shuffle_set_at = Some(Instant::now());
        self.session_dirty = true;
        if let Some(assumed) = &mut self.assumed_context {
            assumed.shuffle = Some(shuffle);
        }
        self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
        self.note_shuffle_pending();
        match self.target() {
            Target::Local => {
                self.local.shuffle = shuffle;
                self.backend.player(PlayerCommand::Shuffle(shuffle));
            }
            Target::Remote(None) if self.remote_fresh().is_none() => {
                // Keep the mode for Play without requesting an absent device.
            }
            Target::Remote(device_id) => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.state.shuffle_state = shuffle;
                }
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Shuffle,
                    device_id,
                    play: None,
                    position_ms: 0,
                    percent: 0,
                    flag: shuffle,
                    repeat: String::new(),
                });
            }
        }
    }

    /// The repeat mode local playback follows: the engine's once it has a
    /// song, until then the one the last run saved.
    fn local_repeat(&self) -> RepeatMode {
        if self.local.track.is_some() {
            self.local.repeat
        } else {
            self.repeat_wanted
        }
    }

    /// Whether `action` is the listener choosing another song: Next, a
    /// Previous that steps back rather than restarting, or starting
    /// something to play.
    fn changes_song_by_hand(&self, action: &Action) -> bool {
        match action {
            Action::Next
            | Action::PlayContext { .. }
            | Action::PlayUris { .. }
            | Action::PlayEpisode { .. }
            | Action::PlayFromRow { .. } => true,
            Action::Previous => {
                let position = if self.resume_only() {
                    self.resume_position_ms
                } else {
                    self.now_playing().map_or(0, |now| now.position_ms)
                };
                position <= RESTART_BEFORE_PREVIOUS
            }
            _ => false,
        }
    }

    /// Choosing another song ends Repeat One and keeps repeating the rest,
    /// as Spotify's own clients do (#1022). Repeat All and Off stay as they
    /// are.
    fn leave_repeat_one(&mut self) {
        let mode = match self.now_playing() {
            Some(now) => now.repeat,
            None if self.target() == Target::Local => self.local_repeat(),
            None => return,
        };
        if mode == RepeatMode::Track {
            self.set_repeat(RepeatMode::Context);
        }
    }

    fn set_repeat(&mut self, mode: RepeatMode) {
        // Next up leaves Spotify's padding out only with Repeat off.
        self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
        match self.target() {
            Target::Local => {
                self.local.repeat = mode;
                self.repeat_wanted = mode;
                self.session_dirty = true;
                self.backend.player(PlayerCommand::Repeat(mode));
            }
            Target::Remote(None) if self.remote_fresh().is_none() => {
                self.repeat_wanted = mode;
                self.session_dirty = true;
            }
            Target::Remote(device_id) => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.state.repeat_state = mode.api_name().to_string();
                }
                self.repeat_hold = Some((mode.api_name().to_string(), Instant::now()));
                self.backend.api(ApiRequest::Remote {
                    action: RemoteAction::Repeat,
                    device_id,
                    play: None,
                    position_ms: 0,
                    percent: 0,
                    flag: false,
                    repeat: mode.api_name().to_string(),
                });
            }
        }
    }

    fn transfer(&mut self, device_id: String) {
        if self
            .pending_remote_volume
            .as_ref()
            .is_some_and(|pending| pending.device_id.as_deref() != Some(device_id.as_str()))
        {
            self.pending_remote_volume = None;
        }
        if Some(device_id.as_str()) == self.local_device_id.as_deref() {
            self.selected_device = None;
            self.show_devices = false;
            if !self.local.is_active() {
                // Connect transfers the active device's full playback state.
                // A Web API snapshot may be stale and cannot recreate its queue.
                self.queue_start_pending = Some(Target::Local);
                self.resume_load = None;
                self.local_transfer_sequence = Some(self.local.track_sequence);
                self.local_list = None;
                self.resume_queue.clear();
                self.queued_play = None;
                self.intent_track = None;
                self.assumed_context = None;
                self.optimistic_playing = None;
                self.clear_play_pending();
                self.backend.player(PlayerCommand::Transfer);
            }
            self.poll_remote_soon();
            return;
        }
        let play = self.now_playing().is_some_and(|now| now.playing);
        self.selected_device = Some(device_id.clone());
        self.backend.api(ApiRequest::Transfer { device_id, play });
    }

    /// Adds a row to Next up immediately, before the context's upcoming rows.
    fn add_to_queue(&mut self, uri: String, label: String) {
        if util::uri_kind(&uri) == Some("album") {
            self.queue_album(uri, label);
            return;
        }
        if self.queued_moments_ago(&uri) {
            return;
        }
        self.queue_one(uri, label, true);
    }

    /// Whether `uri` was queued so recently that asking again is the same
    /// click arriving twice. Later duplicates are separate asks.
    fn queued_moments_ago(&mut self, uri: &str) -> bool {
        self.expire_pending_queue_adds();
        self.pending_queue_adds
            .iter()
            .any(|pending| pending.item.uri() == uri && pending.at.elapsed() < QUEUE_ADD_DEBOUNCE)
    }

    fn queue_album(&mut self, uri: String, label: String) {
        if self
            .last_album_queue
            .as_ref()
            .is_some_and(|(previous, at)| previous == &uri && at.elapsed() < QUEUE_ADD_DEBOUNCE)
        {
            return;
        }
        let Some(id) = util::uri_id(&uri).map(str::to_string) else {
            return;
        };
        self.last_album_queue = Some((uri, Instant::now()));
        if let Some(page) = self.album_pages.get(&id)
            && page.tracks.is_complete()
        {
            self.queue_album_tracks(page.tracks.items.clone(), label);
            return;
        }
        self.album_queue_serial = self.album_queue_serial.wrapping_add(1);
        let request = self.album_queue_serial;
        self.pending_album_queues.insert(
            request,
            PendingAlbumQueue {
                id: id.clone(),
                label: label.clone(),
                target: self.target(),
                offset: 0,
                tracks: Vec::new(),
            },
        );
        self.backend.api(ApiRequest::AlbumQueueTracks {
            id,
            offset: 0,
            request,
        });
        self.toast(gettext(self.locale, "Loading {name} to queue…").replace("{name}", &label));
    }

    fn receive_album_queue(
        &mut self,
        request: u64,
        offset: u32,
        result: crate::api::client::Result<crate::api::models::Page<Track>>,
    ) {
        let Some(pending) = self.pending_album_queues.get(&request) else {
            return;
        };
        if pending.offset != offset {
            return;
        }
        let mut pending = self.pending_album_queues.remove(&request).unwrap();
        if pending.target != self.target() {
            self.toast_error(gettext(
                self.locale,
                "Playback device changed. Add the album to queue again",
            ));
            return;
        }
        let page = match result {
            Ok(page) if page.offset == offset => page,
            Ok(_) => {
                self.toast_error(gettext(
                    self.locale,
                    "Couldn't load the album's songs in order. Try again",
                ));
                return;
            }
            Err(error) => {
                self.toast_error(
                    gettext(self.locale, "Couldn't add {name} to queue: {error}")
                        .replace("{name}", &pending.label)
                        .replace("{error}", &error.to_string()),
                );
                return;
            }
        };
        let next = page.next_offset();
        if page.next.is_some() && next.is_none() {
            self.toast_error(gettext(
                self.locale,
                "Couldn't load the album's songs in order. Try again",
            ));
            return;
        }
        pending.tracks.extend(page.items);
        if let Some(next) = next {
            if next <= offset {
                self.toast_error(gettext(
                    self.locale,
                    "Couldn't load the album's songs in order. Try again",
                ));
                return;
            }
            pending.offset = next;
            self.backend.api(ApiRequest::AlbumQueueTracks {
                id: pending.id.clone(),
                offset: next,
                request,
            });
            self.pending_album_queues.insert(request, pending);
        } else {
            self.queue_album_tracks(pending.tracks, pending.label);
        }
    }

    fn queue_album_tracks(&mut self, tracks: Vec<Track>, label: String) {
        let pending_start = self.pending_queue_adds.len();
        let mut uris = Vec::new();
        for track in tracks {
            if track.is_local
                || track.is_playable == Some(false)
                || util::uri_kind(&track.uri) != Some("track")
            {
                continue;
            }
            let uri = track.uri.clone();
            let name = track.name.clone();
            if let Some(id) = util::uri_id(&uri) {
                self.remember_track_recording(&track);
                self.track_cache.insert(id.to_string(), track);
            }
            self.show_queued_song(&uri, &name);
            uris.push(uri);
        }
        if uris.is_empty() {
            self.toast_error(gettext(
                self.locale,
                "This album has no playable songs to queue",
            ));
            return;
        }
        let count = uris.len();
        self.toast(
            ngettext(
                self.locale,
                "{count} song from {name} added to queue",
                "{count} songs from {name} added to queue",
                count as u32,
            )
            .replace("{count}", &count.to_string())
            .replace("{name}", &label),
        );
        if self.local.is_active() && self.target() == Target::Local {
            for uri in uris {
                self.backend.player(PlayerCommand::AddToQueue(uri));
            }
            self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
        } else {
            self.write_queue_adds(uris, pending_start);
        }
    }

    /// Queues several songs after the ones already queued, skipping any
    /// queued again within `QUEUE_ADD_DEBOUNCE`, with one combined toast.
    fn queue_many(&mut self, songs: Vec<(String, String)>) {
        // Each picked row is its own ask, so a song picked twice is queued
        // twice. Only an add from an earlier click can make one of them a
        // repeat, so decide that before adding any.
        let repeats: Vec<bool> = songs
            .iter()
            .map(|(uri, _)| self.queued_moments_ago(uri))
            .collect();
        let mut count = 0;
        for ((uri, label), repeat) in songs.into_iter().zip(repeats) {
            if !repeat {
                self.queue_one(uri, label, false);
                count += 1;
            }
        }
        if count > 0 {
            self.queued_toast(count);
        }
    }

    fn queued_toast(&mut self, count: usize) {
        self.toast(
            ngettext(
                self.locale,
                "{count} song added to queue",
                "{count} songs added to queue",
                count as u32,
            )
            .replace("{count}", &count.to_string()),
        );
    }

    /// Replays the manually queued songs on the local engine in their
    /// current order. librespot can only append to or clear a live queue,
    /// so a move or positional insert clears it and re-adds every song.
    fn resync_local_queue(&mut self) {
        self.backend.player(PlayerCommand::ClearQueue);
        for uri in self.manual_queue.clone() {
            if uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:") {
                self.backend.player(PlayerCommand::AddToQueue(uri));
            }
        }
        // Drop any queue fetch already in flight: it was asked for before
        // the move and would otherwise land with the pre-move order. Each
        // change gets the full allowance of stale answers: the engine
        // reports every step of the clear and re-add, and a budget spent on
        // an earlier move would let one of those half-done answers through.
        self.queue_seq += 1;
        self.queue_stale_retries = 0;
        self.queue_reorder_pending = Some(Instant::now());
        self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
    }

    /// Adds one song after existing manual queue entries.
    ///
    /// `announce` is false when a batch should produce one toast.
    fn queue_one(&mut self, uri: String, label: String, announce: bool) {
        let pending_start = self.pending_queue_adds.len();
        self.show_queued_song(&uri, &label);
        if announce {
            self.toast(gettext(self.locale, "{name} added to queue").replace("{name}", &label));
        }
        // Queue tracks and episodes directly on the active local engine.
        // Other targets and item types use the Web API.
        let track_like = uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:");
        if track_like && self.jam_redirects() {
            self.backend.send(Command::Jam(JamRequest::Send(
                crate::jam::Command::AddToQueue(uri),
            )));
            self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
            return;
        }
        if track_like && self.local.is_active() && matches!(self.target(), Target::Local) {
            self.backend.player(PlayerCommand::AddToQueue(uri));
            self.queue_recheck_at = Some(Instant::now() + QUEUE_RECHECK);
            return;
        }
        self.write_queue_adds(vec![uri], pending_start);
    }

    fn show_queued_song(&mut self, uri: &str, label: &str) {
        let item = self.optimistic_queue_item(uri, label);
        self.pending_queue_adds.push(PendingQueueAdd {
            item: item.clone(),
            at: Instant::now(),
            manual_index: self.manual_queue.len(),
            write: None,
        });
        if let Loadable::Loaded(queue) = &self.queue {
            let at = Self::end_of_queued_rows(&queue.queue, &self.manual_queue);
            if let Loadable::Loaded(queue) = &mut self.queue {
                queue.queue.insert(at, item);
            }
        }
        self.manual_queue.push(uri.to_string());
        self.session_dirty = true;
    }

    fn write_queue_adds(&mut self, uris: Vec<String>, pending_start: usize) {
        let target = self.target();
        let device_id = match &target {
            Target::Local => self.local_device_id.clone(),
            Target::Remote(device_id) => device_id.clone(),
        };
        self.album_queue_serial = self.album_queue_serial.wrapping_add(1);
        let request = self.album_queue_serial;
        for (index, addition) in self.pending_queue_adds[pending_start..]
            .iter_mut()
            .enumerate()
        {
            addition.write = Some((request, index));
        }
        self.pending_queue_batches.insert(request, target);
        self.backend.api(ApiRequest::AddManyToQueue {
            request,
            uris,
            device_id,
        });
    }

    /// Roll back only the rejected occurrences, leaving accepted songs,
    /// later additions and matching songs in the playing context intact.
    fn remove_failed_queue_adds(&mut self, request: u64, added: usize) {
        let mut indexes: Vec<_> = self
            .pending_queue_adds
            .iter()
            .filter(|addition| {
                addition
                    .write
                    .is_some_and(|(write, index)| write == request && index >= added)
            })
            .map(|addition| addition.manual_index)
            .collect();
        indexes.sort_unstable();
        for index in indexes.into_iter().rev() {
            let Some(uri) = self.manual_queue.get(index) else {
                continue;
            };
            let occurrence = self.manual_queue[..index]
                .iter()
                .filter(|other| *other == uri)
                .count();
            if let Loadable::Loaded(queue) = &mut self.queue {
                let queued_len = Self::end_of_queued_rows(&queue.queue, &self.manual_queue);
                if let Some(row) = queue.queue[..queued_len]
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.uri() == uri)
                    .nth(occurrence)
                    .map(|(row, _)| row)
                {
                    queue.queue.remove(row);
                }
            }
            self.remove_manual_queue_row(index);
        }
    }

    /// Puts the first `queued_len` rows on show, the Playing next section,
    /// in `manual_queue` order, which is the order the engine is given. A
    /// move works on `manual_queue`; when an accepted answer from Spotify
    /// had those rows in another order, moving the shown row at the same
    /// index would move a different song than the engine does.
    fn show_manual_queue_order(&mut self, queued_len: usize) {
        let Loadable::Loaded(queue) = &mut self.queue else {
            return;
        };
        let queued_len = queued_len.min(queue.queue.len());
        let mut shown: Vec<Option<PlayableItem>> =
            queue.queue.drain(..queued_len).map(Some).collect();
        let mut ordered = Vec::with_capacity(shown.len());
        for uri in &self.manual_queue {
            if let Some(item) = shown
                .iter_mut()
                .find(|item| item.as_ref().is_some_and(|item| item.uri() == uri))
                .and_then(Option::take)
            {
                ordered.push(item);
            }
        }
        ordered.extend(shown.into_iter().flatten());
        queue.queue.splice(0..0, ordered);
    }

    /// Index after manual queue rows and before context rows.
    fn end_of_queued_rows(rows: &[PlayableItem], manual: &[String]) -> usize {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for uri in manual {
            *counts.entry(uri.as_str()).or_insert(0) += 1;
        }
        let mut at = 0;
        for item in rows {
            match counts.get_mut(item.uri()) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    at += 1;
                }
                _ => break,
            }
        }
        at
    }

    /// Spotify pads a short queue out to a fixed length by going round it
    /// again, back to the playing song and on, whatever the Repeat mode.
    /// With Repeat off playback stops where the list ends, so a song
    /// started on its own was listed ten times under Next up though it
    /// plays once. Those rows go; songs queued by hand always stay.
    fn drop_queue_padding(&self, fetched: &mut Queue) {
        let repeat = self.now_playing().map(|now| now.repeat);
        if repeat.unwrap_or(self.repeat_wanted) != RepeatMode::Off {
            return;
        }
        let Some(current) = &fetched.currently_playing else {
            return;
        };
        let queued = Self::end_of_queued_rows(&fetched.queue, self.queued_by_hand());
        Self::drop_wraparound(&mut fetched.queue, queued, current.uri());
    }

    /// Drops the rows after the first `queued` from where they come back
    /// round to `current`, the playing song, and repeat what came before.
    /// The playing song and the row after it must both come round: one
    /// late copy of the playing song may be the list's own.
    fn drop_wraparound(rows: &mut Vec<PlayableItem>, queued: usize, current: &str) {
        let order: Vec<&str> = std::iter::once(current)
            .chain(rows[queued..].iter().map(PlayableItem::uri))
            .collect();
        // From a wrap on, the list lines up with its own start.
        let lines_up = |at: usize| order[at..] == order[..order.len() - at];
        let wrap = (1..order.len().saturating_sub(1)).find(|&at| lines_up(at));
        if let Some(at) = wrap {
            rows.truncate(queued + at - 1);
        }
    }

    /// Queue row built from cached details or a temporary label.
    fn optimistic_queue_item(&self, uri: &str, label: &str) -> PlayableItem {
        let cached = util::uri_id(uri)
            .and_then(|id| self.track_cache.get(id))
            .cloned();
        if let Some(track) = cached {
            return PlayableItem::Track(track);
        }
        if let Some(item) = self.queue.get().and_then(|queue| {
            queue
                .currently_playing
                .iter()
                .chain(&queue.queue)
                .find(|item| item.uri() == uri)
        }) {
            return item.clone();
        }
        if let Some(item) = self
            .playlist_pages
            .values()
            .flat_map(|page| &page.items.items)
            .filter_map(PlaylistItem::playable)
            .find(|item| item.uri() == uri)
        {
            return item.clone();
        }
        let known = self
            .album_pages
            .values()
            .flat_map(|page| &page.tracks.items)
            .chain(self.library.liked.items.iter().map(|saved| &saved.track))
            .chain(
                self.search
                    .results
                    .get()
                    .into_iter()
                    .flat_map(|results| results.tracks.iter().flat_map(|page| &page.items)),
            )
            .find(|track| track.uri == uri);
        if let Some(track) = known {
            return PlayableItem::Track(track.clone());
        }
        PlayableItem::Track(crate::api::models::Track {
            uri: uri.to_string(),
            name: label.to_string(),
            ..Default::default()
        })
    }

    fn expire_pending_queue_adds(&mut self) {
        if !self.pending_queue_batches.is_empty() {
            return;
        }
        self.pending_queue_adds
            .retain(|addition| addition.at.elapsed() < Duration::from_secs(30));
    }

    /// Restores pending additions missing from a stale fetched queue.
    fn reconcile_pending_queue(&mut self) {
        self.expire_pending_queue_adds();
        if self.pending_queue_adds.is_empty() {
            return;
        }
        let Loadable::Loaded(queue) = &mut self.queue else {
            return;
        };
        let fetched: std::collections::HashSet<String> = queue
            .queue
            .iter()
            .map(|item| item.uri().to_string())
            .collect();
        // A fetched or currently playing addition is no longer pending.
        let current = self.current_track_uri();
        self.pending_queue_adds.retain(|addition| {
            !fetched.contains(addition.item.uri())
                && current.as_deref() != Some(addition.item.uri())
        });
        let missing: Vec<PlayableItem> = self
            .pending_queue_adds
            .iter()
            .map(|addition| addition.item.clone())
            .collect();
        let at = match &self.queue {
            Loadable::Loaded(queue) => Self::end_of_queued_rows(&queue.queue, &self.manual_queue),
            _ => 0,
        };
        if let Loadable::Loaded(queue) = &mut self.queue {
            for item in missing.into_iter().rev() {
                queue.queue.insert(at, item);
            }
        }
    }

    /// Answer from the rows already held locally. A found duplicate is
    /// definitive even for a partial playlist; an empty answer is definitive
    /// only when the whole playlist is present.
    fn local_playlist_duplicates(&self, id: &str, items: &[PlayableItem]) -> Option<Vec<String>> {
        let page = self.playlist_pages.get(id)?;
        let existing: HashSet<&str> = page
            .items
            .items
            .iter()
            .filter_map(PlaylistItem::playable)
            .map(PlayableItem::uri)
            .chain(page.local_additions.iter().map(String::as_str))
            .collect();
        let duplicates: Vec<String> = items
            .iter()
            .map(PlayableItem::uri)
            .filter(|uri| existing.contains(*uri))
            .map(str::to_string)
            .collect();
        if !duplicates.is_empty() || page.items.is_complete() {
            Some(duplicates)
        } else {
            None
        }
    }

    fn prepare_playlist_mutation(&mut self, id: &str) {
        let Some(page) = self.playlist_pages.get_mut(id) else {
            return;
        };
        page.pending_writes += 1;
        self.load_generation += 1;
        page.generation = self.load_generation;
        page.items_generation = page.generation;
        // Reads issued before the edit describe the old snapshot and must not
        // be allowed to replace the optimistic rows when they arrive.
        page.items.loading = page.refresh_after_write;
        page.items.clear_windows();
        page.cache_saved_through = None;
        page.cache_saved_rows = 0;
        page.cache_saved_total = None;
        // Let the old write finish before checkpointing the edited snapshot.
        page.cache_append_valid = false;
        page.cache_restored_through = None;
        page.pending_cache = None;
    }

    fn request_playlist_add(
        &mut self,
        playlist_id: String,
        playlist_name: String,
        items: Vec<PlayableItem>,
        position: Option<u32>,
    ) {
        match self.local_playlist_duplicates(&playlist_id, &items) {
            Some(duplicate_uris) if duplicate_uris.is_empty() => {
                self.add_to_playlist_now(playlist_id, playlist_name, items, position);
            }
            Some(duplicate_uris) => {
                self.dialog = Some(Dialog::ConfirmPlaylistDuplicates {
                    playlist_id,
                    playlist_name,
                    items,
                    position,
                    duplicate_uris,
                });
            }
            None => {
                self.playlist_busy = true;
                self.backend.api(ApiRequest::CheckPlaylistDuplicates {
                    playlist_id,
                    playlist_name,
                    items,
                    position,
                });
            }
        }
    }

    fn add_to_playlist_now(
        &mut self,
        playlist_id: String,
        playlist_name: String,
        items: Vec<PlayableItem>,
        position: Option<u32>,
    ) {
        if items.is_empty() {
            return;
        }
        self.dialog = None;
        self.playlist_busy = true;
        for item in &items {
            if let PlayableItem::Track(track) = item {
                self.remember_track_recording(track);
            }
        }

        self.prepare_playlist_mutation(&playlist_id);
        let added = items.len().try_into().unwrap_or(u32::MAX);
        let added_by = self.user_id().map(|id| UserRef {
            id: Some(id.to_string()),
        });
        let added_at = Some(jiff::Timestamp::now().to_string());
        if let Some(page) = self.playlist_pages.get_mut(&playlist_id) {
            let current_total = page.items.total.unwrap_or_else(|| {
                page.items
                    .base_offset
                    .saturating_add(page.items.items.len() as u32)
            });
            let at = position.unwrap_or(current_total);
            let start = page.items.base_offset;
            let end = start.saturating_add(page.items.items.len() as u32);
            for item in &items {
                page.local_additions.insert(item.uri().to_string());
            }
            if page.items.loaded_once && (start..=end).contains(&at) {
                let relative = (at - start) as usize;
                page.items.items.splice(
                    relative..relative,
                    items.iter().cloned().map(|item| PlaylistItem {
                        added_at: added_at.clone(),
                        added_by: added_by.clone(),
                        is_local: false,
                        item: Some(item),
                        track: None,
                    }),
                );
            } else if at < start {
                // A delayed confirmation can arrive after the user opened a
                // different slice. Keep that slice attached to the same rows.
                page.items.base_offset = start.saturating_add(added);
            }
            if page.items.loaded_once
                && at <= end
                && let Some(next) = &mut page.items.next_offset
            {
                *next = next.saturating_add(added);
            }
            page.items.total = Some(current_total.saturating_add(added));
            page.items.revision = page.items.revision.wrapping_add(1);
            if let Some(playlist) = page.playlist.get_mut() {
                increase_playlist_total(playlist, added);
            }
        }
        if let Some(playlists) = self.library.playlists.get_mut()
            && let Some(playlist) = playlists
                .iter_mut()
                .find(|playlist| playlist.id == playlist_id)
        {
            increase_playlist_total(playlist, added);
        }

        self.backend.api(ApiRequest::AddToPlaylist {
            playlist_id,
            playlist_name,
            uris: items.iter().map(|item| item.uri().to_string()).collect(),
            position,
        });
    }

    /// A song this app has already shown, by its URI.
    fn known_song(&mut self, uri: &str) -> Option<PlayableItem> {
        if let Some(item) = self.copied_songs.iter().find(|item| item.uri() == uri) {
            return Some(item.clone());
        }
        let id = uri.strip_prefix("spotify:track:")?;
        self.read_cached_track(id).map(PlayableItem::Track)
    }

    /// Records Spotify's answer for a pasted song link: the song, or `None`
    /// when there is no such song.
    fn resolve_pasted_song(&mut self, uri: &str, item: Option<PlayableItem>) {
        if self.pending_pastes.is_empty() {
            return;
        }
        for paste in &mut self.pending_pastes {
            if !paste.uris.iter().any(|pasted| pasted == uri) {
                continue;
            }
            match &item {
                Some(item) => {
                    paste.found.insert(uri.to_string(), item.clone());
                }
                None => {
                    paste.missing.insert(uri.to_string());
                }
            }
        }
        self.finish_pastes();
    }

    /// Adds every paste whose songs are all known, or known to be missing.
    fn finish_pastes(&mut self) {
        let mut index = 0;
        while index < self.pending_pastes.len() {
            if !self.pending_pastes[index].settled() {
                index += 1;
                continue;
            }
            let paste = self.pending_pastes.remove(index);
            let items: Vec<PlayableItem> = paste
                .uris
                .iter()
                .filter_map(|uri| paste.found.get(uri).cloned())
                .collect();
            let skipped = paste.uris.len() - items.len();
            if skipped > 0 {
                self.toast_error(
                    ngettext(
                        self.locale,
                        "{count} pasted link could not be added",
                        "{count} pasted links could not be added",
                        skipped as u32,
                    )
                    .replace("{count}", &skipped.to_string()),
                );
            }
            if !items.is_empty() {
                self.request_playlist_add(paste.playlist_id, paste.playlist_name, items, None);
            }
        }
    }

    fn set_saved(&mut self, uri: String, saved: bool) {
        if uri.starts_with("spotify:playlist:") {
            self.record_saved(uri.clone(), saved);
            let id = util::uri_id(&uri).unwrap_or_default().to_string();
            self.backend
                .api(ApiRequest::FollowPlaylist { id, follow: saved });
            return;
        }
        self.set_saved_state(uri.clone(), saved);
        self.saved_writes.insert(uri.clone(), saved);
        if self.change_liked_song(&uri, saved) {
            self.sync_liked_songs();
            self.ensure_liked_songs();
        }
        self.backend.api(ApiRequest::SetSaved {
            uris: vec![uri],
            saved,
        });
    }

    // ---- actions -----------------------------------------------------------------

    fn apply_actions_with(&mut self, ctx: UiCtx<'_>) {
        self.frame_now = None;
        let mut actions = std::mem::take(&mut self.actions);
        while !actions.is_empty() {
            for action in actions.drain(..) {
                self.apply_with(action, ctx);
            }
            actions = std::mem::take(&mut self.actions);
        }
    }

    fn leave_lyrics_fullscreen(&mut self, ctx: UiCtx<'_>) {
        if let Some(was_fullscreen) = self.lyrics_fullscreen.take() {
            let _ = ctx;
            self.lyrics_fullscreen_restoring = Some(was_fullscreen);
            self.lyrics_fullscreen_seen = false;
            self.lyrics_line_shown = None;
        }
    }

    pub(crate) fn apply_with(&mut self, action: Action, ctx: UiCtx<'_>) {
        if matches!(
            &action,
            Action::OpenLink(_) | Action::FocusSearch | Action::SignOut
        ) {
            self.leave_lyrics_fullscreen(ctx);
        }
        if self.changes_song_by_hand(&action) {
            self.leave_repeat_one();
        }
        match action {
            Action::OpenLink(uri) => {
                // The window first: a link is the user asking for the
                // app, signed in or not, and the page follows when it can.
                self.pending_link = Some(uri);
                self.open_pending_link();
                self.actions.push(Action::ShowWindow);
            }
            Action::PlayContext {
                uri,
                offset_uri,
                offset_index,
            } => {
                let mut request = PlayRequest::context(uri);
                request.offset_uri = offset_uri;
                request.offset_position = offset_index;
                self.play_request(request, false);
            }
            Action::PlayUris { uris, index } => {
                if uris.is_empty() {
                    return;
                }
                let (uris, index) = cap_uris(&uris, index);
                let request = PlayRequest::tracks(uris).starting_at_index(index);
                self.play_request(request, false);
            }
            Action::PlayEpisode { uri, resume_ms } => {
                // A started episode continues from the place the row or card
                // showed as time left. The playing episode is left where it
                // is playing, not sent back to a saved position older than it.
                let mut request = PlayRequest::tracks(vec![uri.clone()]).starting_at_index(0);
                if self.now_playing().is_none_or(|now| now.uri != uri)
                    && let Some(position_ms) = resume_ms
                {
                    request.position_ms = position_ms;
                }
                self.play_request(request, false);
            }
            Action::PlayFromRow {
                context,
                uri,
                index,
            } => match context {
                RowContext::Context {
                    uri: context_uri, ..
                } => {
                    let request = PlayRequest::context(context_uri).starting_at_uri(uri);
                    self.play_request(request, false);
                }
                RowContext::Uris(uris) => {
                    // The click names a song. A row that plays a list of its
                    // own, as each Recent row does, still hands over its
                    // place in the list on screen; when that place holds
                    // another song, the song wins, as it does in Next up.
                    let index = if uris.get(index as usize).is_some_and(|held| *held == uri) {
                        index
                    } else {
                        uris.iter()
                            .position(|held| *held == uri)
                            .map_or(index, |position| position as u32)
                    };
                    let (uris, index) = cap_uris(uris.as_ref(), index);
                    let request = PlayRequest::tracks(uris).starting_at_index(index);
                    self.play_request(request, false);
                }
                RowContext::Queue => self.play_queue_item(index as usize, uri),
                RowContext::View {
                    uris, context_uri, ..
                } => {
                    let (uris, index) = cap_uris(uris.as_ref(), index);
                    if let Some(uri) = uris.get(index as usize) {
                        self.cache_track_from_context(&context_uri, uri);
                    }
                    let request = PlayRequest::tracks(uris).starting_at_index(index);
                    self.play_request(request, false);
                    self.note_recent_context(&context_uri);
                    self.assumed_context = Some(AssumedContext {
                        uri: context_uri,
                        shuffle: self.shuffle_wanted.then_some(true),
                        at: Instant::now(),
                    });
                }
            },
            Action::TogglePlay => self.toggle_play(),
            Action::Next if self.resume_only() => {
                self.step_resume(true);
            }
            Action::Next if self.jam_redirects() => {
                self.backend
                    .send(Command::Jam(JamRequest::Send(crate::jam::Command::Next)));
            }
            Action::Next => {
                self.resume_load = None;
                // Move the queue head to the playing row immediately. Do not
                // pop when there is no active playback target.
                let target = self.target();
                if !matches!(target, Target::Remote(None)) || self.remote_fresh().is_some() {
                    self.pop_queue_head();
                }
                match target {
                    Target::Local => self.backend.player(PlayerCommand::Next),
                    Target::Remote(device_id) => self.remote(RemoteAction::Next, device_id),
                }
            }
            // Previous restarts after three seconds and otherwise steps back,
            // matching librespot.
            Action::Previous if self.jam_redirects() => {
                self.backend.send(Command::Jam(JamRequest::Send(
                    crate::jam::Command::Previous,
                )));
            }
            Action::Previous if self.resume_only() => {
                if self.resume_position_ms > RESTART_BEFORE_PREVIOUS {
                    self.resume_position_ms = 0;
                    self.session_dirty = true;
                } else {
                    self.step_resume(false);
                }
            }
            Action::Previous => {
                self.resume_load = None;
                // Next names its expected destination so the row moves before
                // the engine does. Previous has no forward queue row to name;
                // discard that expectation or a quick Next, Previous leaves
                // the row marker stuck on the skipped-to song.
                self.intent_track = None;
                if self.changes_song_by_hand(&Action::Previous) {
                    self.queue_start_pending = Some(self.target());
                }
                match self.target() {
                    Target::Local => self.backend.player(PlayerCommand::Previous),
                    Target::Remote(device_id) => self.remote(RemoteAction::Previous, device_id),
                }
            }
            Action::Seek(position_ms) => self.seek(position_ms),
            Action::SeekBy(offset) => {
                if let Some(now) = self.now_playing() {
                    let target = (i64::from(now.position_ms) + offset)
                        .clamp(0, i64::from(now.duration_ms))
                        as u32;
                    self.seek(target);
                }
            }
            Action::SetVolume(percent) => {
                self.volume_before_mute = None;
                self.set_volume(percent, true);
            }
            Action::PreviewVolume(percent) => self.set_volume(percent, false),
            Action::VolumeBy(delta) => {
                if let Some(now) = self.now_playing() {
                    let next =
                        (i16::from(now.volume_percent) + i16::from(delta)).clamp(0, 100) as u8;
                    self.volume_before_mute = None;
                    self.set_volume(next, true);
                } else if self.is_connected() {
                    let current = volume_to_percent(self.local.volume);
                    let next = (i16::from(current) + i16::from(delta)).clamp(0, 100) as u8;
                    self.set_volume(next, true);
                }
            }
            Action::ToggleMute => {
                if !self.can_set_volume() {
                    return;
                }
                let current = self
                    .now_playing()
                    .map(|now| now.volume_percent)
                    .unwrap_or_else(|| volume_to_percent(self.local.volume));
                if current == 0 {
                    let restore = self.volume_before_mute.take().unwrap_or(50).max(5);
                    self.set_volume(restore, true);
                } else {
                    self.volume_before_mute = Some(current);
                    self.set_volume(0, true);
                }
            }
            Action::ToggleShuffle => {
                let shuffle = self
                    .now_playing()
                    .map_or(self.shuffle_wanted, |now| now.shuffle);
                self.set_shuffle(!shuffle);
            }
            Action::SetShuffle(shuffle) => self.set_shuffle(shuffle),
            Action::CycleRepeat => {
                let mode = self
                    .now_playing()
                    .map_or(self.repeat_wanted, |now| now.repeat);
                self.set_repeat(mode.next());
            }
            Action::SetRepeat(mode) => self.set_repeat(mode),
            Action::AddToQueue { uri, label } => self.add_to_queue(uri, label),
            Action::QueueMany { songs } => self.queue_many(songs),
            Action::MoveInQueue { from, to } => {
                if !self.queue_locally_reorderable() {
                    return;
                }
                let queued_len = self.queued_rows_len();
                if from >= queued_len || to > queued_len || from == to || to == from + 1 {
                    return;
                }
                if from < self.manual_queue.len() {
                    let uri = self.manual_queue.remove(from);
                    let at = (if to > from { to - 1 } else { to }).min(self.manual_queue.len());
                    self.manual_queue.insert(at, uri);
                    // Pending additions are tracked by manual_queue index;
                    // shift them the same way the move just shifted the row
                    // they point at, or they end up naming a different song.
                    for addition in &mut self.pending_queue_adds {
                        if addition.manual_index == from {
                            addition.manual_index = at;
                        } else if from < addition.manual_index && addition.manual_index <= at {
                            addition.manual_index -= 1;
                        } else if at <= addition.manual_index && addition.manual_index < from {
                            addition.manual_index += 1;
                        }
                    }
                    self.show_manual_queue_order(queued_len);
                }
                self.session_dirty = true;
                self.resync_local_queue();
            }
            Action::RemoveFromQueue { index } => {
                if !self.queue_locally_reorderable() || index >= self.queued_rows_len() {
                    return;
                }
                self.show_manual_queue_order(self.queued_rows_len());
                let Loadable::Loaded(queue) = &self.queue else {
                    return;
                };
                // The row names its song and which copy of it it is; the
                // manual entry is that copy, wherever Spotify's answer has
                // left gaps.
                let Some(uri) = queue.queue.get(index).map(|row| row.uri().to_string()) else {
                    return;
                };
                let copy = queue.queue[..index]
                    .iter()
                    .filter(|row| row.uri() == uri)
                    .count();
                let Some(entry) = self
                    .manual_queue
                    .iter()
                    .enumerate()
                    .filter(|(_, queued)| **queued == uri)
                    .nth(copy)
                    .map(|(entry, _)| entry)
                else {
                    return;
                };
                if let Loadable::Loaded(queue) = &mut self.queue {
                    queue.queue.remove(index);
                }
                self.remove_manual_queue_row(entry);
                self.resync_local_queue();
            }
            Action::SetSavedMany { uris, saved } => {
                // Unliking a song unlikes it under every release saved, as
                // the heart for the playing song does: one left saved keeps
                // the song liked.
                let uris = if saved {
                    uris
                } else {
                    let mut seen = std::collections::HashSet::new();
                    uris.iter()
                        .flat_map(|uri| {
                            std::iter::once(uri.clone()).chain(self.saved_toggle_targets(uri))
                        })
                        .filter(|uri| seen.insert(uri.clone()))
                        .collect()
                };
                let mut changed_tracks = false;
                for uri in &uris {
                    self.set_saved_state(uri.clone(), saved);
                    self.saved_writes.insert(uri.clone(), saved);
                    changed_tracks |= self.change_liked_song(uri, saved);
                }
                if changed_tracks {
                    self.sync_liked_songs();
                    self.ensure_liked_songs();
                }
                if !uris.is_empty() {
                    self.backend.api(ApiRequest::SetSaved { uris, saved });
                }
            }
            Action::ToggleSaved(uri) => {
                let saved = self.is_saved(&uri).unwrap_or(false);
                let targets = if saved {
                    self.saved_toggle_targets(&uri)
                } else {
                    vec![uri]
                };
                for target in targets {
                    self.set_saved(target, !saved);
                }
            }
            Action::ConfirmAddToPlaylist {
                playlist_id,
                playlist_name,
                items,
                position,
            } => {
                self.add_to_playlist_now(playlist_id, playlist_name, items, position);
            }
            Action::RemoveRowsFromPlaylist { playlist_id, rows } => {
                let snapshot_id = self
                    .playlist_pages
                    .get(&playlist_id)
                    .and_then(|page| page.playlist.get())
                    .and_then(|playlist| playlist.snapshot_id.clone());
                self.prepare_playlist_mutation(&playlist_id);
                if let Some(page) = self.playlist_pages.get_mut(&playlist_id) {
                    let mut gone = rows.clone();
                    gone.sort_by_key(|row| std::cmp::Reverse(row.0));
                    gone.dedup_by_key(|row| row.0);
                    let mut removed = 0u32;
                    for (position, uri) in gone {
                        let Some(index) = position.checked_sub(page.items.base_offset) else {
                            continue;
                        };
                        let index = index as usize;
                        if page
                            .items
                            .items
                            .get(index)
                            .and_then(|item| item.playable())
                            .map(|item| item.uri())
                            == Some(uri.as_str())
                        {
                            page.items.remove_at(index);
                            removed += 1;
                        }
                    }
                    page.items.total = page.items.total.map(|total| total.saturating_sub(removed));
                    page.items.next_offset = page
                        .items
                        .next_offset
                        .map(|offset| offset.saturating_sub(removed));
                    if let Loadable::Loaded(playlist) = &mut page.playlist
                        && let Some(count) =
                            playlist.items_count.as_mut().or(playlist.tracks.as_mut())
                    {
                        count.total = count.total.saturating_sub(removed);
                    }
                }
                self.playlist_busy = true;
                self.backend.api(ApiRequest::RemovePlaylistRows {
                    playlist_id,
                    rows,
                    snapshot_id,
                });
            }
            Action::RemoveFromPlaylist { playlist_id, uris } => {
                let snapshot_id = self
                    .playlist_pages
                    .get(&playlist_id)
                    .and_then(|page| page.playlist.get())
                    .and_then(|playlist| playlist.snapshot_id.clone());
                self.prepare_playlist_mutation(&playlist_id);
                if let Some(page) = self.playlist_pages.get_mut(&playlist_id) {
                    page.local_additions
                        .retain(|uri| !uris.iter().any(|removed| removed == uri));
                    let before = page.items.items.len();
                    page.items.retain(|item| {
                        item.playable()
                            .is_none_or(|playable| !uris.iter().any(|uri| uri == playable.uri()))
                    });
                    let removed = (before - page.items.items.len()) as u32;
                    page.items.total = page.items.total.map(|total| total.saturating_sub(removed));
                    page.items.next_offset = page
                        .items
                        .next_offset
                        .map(|offset| offset.saturating_sub(removed));
                    if let Loadable::Loaded(playlist) = &mut page.playlist
                        && let Some(count) =
                            playlist.items_count.as_mut().or(playlist.tracks.as_mut())
                    {
                        count.total = count.total.saturating_sub(removed);
                    }
                }
                self.playlist_busy = true;
                self.backend.api(ApiRequest::RemoveFromPlaylist {
                    playlist_id,
                    uris,
                    snapshot_id,
                });
            }
            Action::MoveInPlaylist {
                playlist_id,
                from,
                to,
            } => {
                let snapshot_id = self
                    .playlist_pages
                    .get(&playlist_id)
                    .and_then(|page| page.playlist.get())
                    .and_then(|playlist| playlist.snapshot_id.clone());
                self.prepare_playlist_mutation(&playlist_id);
                if let Some(page) = self.playlist_pages.get_mut(&playlist_id) {
                    let base = page.items.base_offset;
                    if from >= base && to >= base {
                        page.items
                            .reorder((from - base) as usize, (to - base) as usize);
                    }
                }
                self.playlist_busy = true;
                self.backend.api(ApiRequest::ReorderPlaylist {
                    playlist_id,
                    range_start: from,
                    insert_before: to,
                    snapshot_id,
                });
            }
            Action::CreatePlaylist {
                name,
                public,
                add_uris,
            } => {
                self.playlist_busy = true;
                self.dialog = Some(Dialog::CreatePlaylist {
                    name: name.clone(),
                    public,
                    add_uris,
                });
                self.backend.api(ApiRequest::CreatePlaylist {
                    name,
                    public,
                    description: String::new(),
                });
            }
            Action::UpdatePlaylist {
                id,
                name,
                description,
                public,
            } => {
                self.dialog = None;
                let changes = self.changed_playlist_details(&id, name, description, public);
                if changes.kept_description {
                    self.toast_error(gettext(
                        self.locale,
                        "Spotify doesn't let apps remove a playlist description, so it was kept",
                    ));
                }
                if changes.name.is_some()
                    || changes.description.is_some()
                    || changes.public.is_some()
                {
                    self.playlist_busy = true;
                    self.backend.api(ApiRequest::UpdatePlaylist {
                        id: id.clone(),
                        name: changes.name.clone(),
                        description: changes.description.clone(),
                        public: changes.public,
                    });
                    self.pending_playlist_details.insert(id, changes);
                }
            }
            Action::DeletePlaylist(id) => {
                self.dialog = None;
                self.record_saved(format!("spotify:playlist:{id}"), false);
                if let Some(playlists) = self.library.playlists.get_mut() {
                    playlists.retain(|playlist| playlist.id != id);
                }
                self.backend
                    .api(ApiRequest::FollowPlaylist { id, follow: false });
            }
            Action::Transfer(device_id) => self.transfer(device_id),
            Action::RefreshDevices => {
                self.devices_fetched_at = None;
                self.refresh_devices();
                self.backend.send(Command::DiscoverReceivers);
            }
            Action::ClearQueue => self.clear_queue(),
            Action::SaveQueueAsPlaylist => self.save_queue_as_playlist(),
            Action::SaveRadio(seed) => self.save_radio(&seed),
            Action::RefreshQueue => self.refresh_queue(true),
            Action::StartJam => self.start_jam(),
            Action::EndJam => self.backend.send(Command::Jam(JamRequest::End)),
            Action::LeaveJam => self.backend.send(Command::Jam(JamRequest::Leave)),
            Action::RemoveFromJam(member) => {
                self.backend.send(Command::Jam(JamRequest::Remove(member)));
            }
            Action::JoinJam(token) => {
                self.jam_state.pending_invite = None;
                self.jam_state.busy = true;
                self.backend.send(Command::Jam(JamRequest::Join(token)));
            }
            Action::Search(query) => {
                self.search.query = query.clone();
                self.search.typed_at = None;
                self.open(Page::Search);
                self.run_search(query.trim().to_string());
            }
            Action::ForgetSearch(query) => {
                self.settings.search_history.retain(|entry| entry != &query);
                self.settings_dirty = true;
            }
            Action::FocusSearch => {
                self.search.focus_requested = true;
                if !matches!(self.page(), Page::Search) {
                    self.open(Page::Search);
                }
            }
            Action::LoadMore(page) => self.load_more(page),
            Action::LoadMoreRecents => self.load_more_recents(),
            Action::LoadMoreArtistAlbums(id) => {
                let Some(page) = self.artist_pages.get_mut(&id) else {
                    return;
                };
                let groups = page.filter.groups().to_string();
                let list = page.albums.entry(groups.clone()).or_default();
                if let Some(offset) = list.next_offset.filter(|_| list.can_load_more()) {
                    list.loading = true;
                    self.backend
                        .api(ApiRequest::ArtistAlbums { id, groups, offset });
                }
            }
            Action::Reload(page) => self.reload(page),
            Action::SignIn => {
                // The Client ID typed on the sign-in screen is kept for the
                // next sign-in, as Settings keeps it.
                self.save_settings();
                self.request_proxy(true);
            }
            Action::CancelSignIn => {
                self.backend.send(Command::CancelSignIn);
                self.sign_in_url = None;
                self.auth = AuthStatus::SignedOut;
            }
            Action::ConfigurePersonalWebApp => {
                self.save_settings();
                self.backend.send(Command::ConfigurePersonalWebApp(
                    self.settings.web_client_id.clone(),
                ));
            }
            Action::SignOut => {
                self.backend.send(Command::SignOut);
                self.history = vec![Page::Home];
                self.history_index = 0;
            }
            Action::ReloadThemes => {}
            Action::SettingsChanged => {
                self.settings_dirty = true;
            }
            Action::RestartEngine => {
                self.save_settings();
                let config = engine_config(
                    &self.dirs,
                    &self.settings,
                    self.applied_proxy.clone(),
                    std::sync::Arc::clone(&self.eq),
                    std::sync::Arc::clone(&self.karaoke),
                );
                self.backend.send(Command::RestartEngine(config));
                if self.local_ready {
                    self.toast(gettext(self.locale, "Restarting local playback"));
                }
            }
            Action::ShowWindow => {
                if self.window_hidden {
                    // No window exists; the outer loop creates one.
                    self.wants_show = true;
                } else {
                    // A minimized window has to be restored before it can be
                    // focused: Focus alone leaves it in the Dock, and the
                    // platform draws no frames while it is down there, so
                    // nothing arrives to ask a second time.
                }
            }
            Action::EnablePlayback => {
                if self.free_account() {
                    self.toast_error(gettext(self.locale, "Local playback needs Spotify Premium"));
                } else {
                    self.enable_playback();
                }
            }
            Action::ToggleEq => {
                self.settings.eq_on = !self.settings.eq_on;
                self.push_eq();
            }
            Action::SetEqBand(band, gain_db) => {
                if let Some(slot) = self.settings.eq_bands_db.get_mut(band) {
                    *slot = gain_db.clamp(-crate::eq::RANGE_DB, crate::eq::RANGE_DB);
                    self.push_eq();
                }
            }
            Action::SetEqPreamp(gain_db) => {
                self.settings.eq_preamp_db =
                    gain_db.clamp(-crate::eq::RANGE_DB, crate::eq::RANGE_DB);
                self.push_eq();
            }
            Action::ApplyEqPreset(index) => {
                if let Some(preset) = crate::eq::PRESETS.get(index) {
                    self.settings.eq_bands_db = preset.bands_db;
                    self.settings.eq_on = true;
                    self.push_eq();
                }
            }
            Action::SetKaraoke(on) => {
                self.karaoke.set_enabled(on);
                if matches!(self.karaoke_model, KaraokeModelState::Failed(_)) {
                    self.karaoke_model = KaraokeModelState::Idle;
                }
                // A model put in place since startup needs no fetching.
                if on
                    && !self.karaoke.has_model()
                    && crate::karaoke::model::installed(&self.dirs.karaoke_dir())
                {
                    self.karaoke.set_loader(Some(karaoke_loader(&self.dirs)));
                }
                if on
                    && !self.karaoke.has_model()
                    && self.karaoke_model != KaraokeModelState::Downloading
                {
                    self.karaoke_model = KaraokeModelState::Downloading;
                    self.backend.send(Command::KaraokeModel);
                }
            }
            Action::SetVocalLevel(level) => {
                self.settings.karaoke_vocal_level = level.clamp(0.0, 1.0);
                self.karaoke
                    .set_vocal_level(self.settings.karaoke_vocal_level);
                self.settings_dirty = true;
            }
            Action::Quit => {
                self.quit_requested = true;
            }
        }
    }

    /// The details the edit dialog actually changed, compared with the
    /// playlist it was opened from. Spotify rejects an empty description
    /// with "Attribute description is empty", so clearing one sends nothing
    /// for it rather than failing the rename saved alongside. The dialog
    /// shows the description without its HTML, so an untouched one is not
    /// sent back stripped either.
    fn changed_playlist_details(
        &self,
        id: &str,
        name: String,
        description: String,
        public: Option<bool>,
    ) -> PlaylistDetailChanges {
        let original = self
            .playlist_pages
            .get(id)
            .and_then(|page| page.playlist.get())
            .or_else(|| {
                self.library
                    .playlists
                    .get()
                    .and_then(|playlists| playlists.iter().find(|playlist| playlist.id == id))
            });
        let original_description = original
            .and_then(|playlist| playlist.description.as_deref())
            .map(util::strip_html)
            .unwrap_or_default();
        let cleared = description.is_empty();
        PlaylistDetailChanges {
            name: original
                .is_none_or(|playlist| playlist.name != name)
                .then_some(name),
            kept_description: cleared && !original_description.is_empty(),
            description: (!cleared && description != original_description).then_some(description),
            public: public
                .filter(|public| original.is_none_or(|playlist| playlist.public != Some(*public))),
        }
    }

    /// Whether Spotify asks this app's player commands to go through the
    /// Jam it is a guest in, to the host's device.
    pub(crate) fn jam_redirects(&self) -> bool {
        self.jam_state
            .active
            .as_ref()
            .is_some_and(|jam| !jam.hosting && jam.redirects_commands)
    }

    /// Starts a Jam on the device playing now, or on this computer when
    /// nothing is.
    fn start_jam(&mut self) {
        let device = match self.target() {
            Target::Remote(Some(id)) => Some(id),
            _ if self.local_ready => self.local_device_id.clone(),
            _ => None,
        };
        let Some(device) = device else {
            self.toast_error(gettext(
                self.locale,
                "Set up playback on this computer to start a Jam",
            ));
            return;
        };
        self.jam_state.busy = true;
        self.backend
            .send(Command::Jam(JamRequest::Start { device }));
    }

    fn receive_jam(&mut self, jam: Option<crate::jam::Jam>) {
        let opened_here = std::mem::take(&mut self.jam_state.busy) && jam.is_some();
        self.jam_state.active = jam;
        if opened_here {
            self.show_devices = false;
            self.queue_tab = QueueTab::Queue;
        }
        self.refresh_queue(true);
    }

    pub fn toast(&mut self, message: impl Into<String>) {
        self.toasts.push(Toast {
            message: message.into(),
            kind: ToastKind::Info,
            created: Instant::now(),
        });
        self.toasts.truncate(4);
    }

    pub fn toast_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("{message}");
        self.toasts.push(Toast {
            message,
            kind: ToastKind::Error,
            created: Instant::now(),
        });
    }

    /// Whether songs may be added to `playlist` from here: the account
    /// owns it, Spotify flags it collaborative, or the rootlist says the
    /// account was invited to it.
    pub fn can_edit_playlist(&self, playlist: &Playlist) -> bool {
        let owned = self.user_id().is_some_and(|user| playlist.owned_by(user));
        owned || playlist.collaborative || self.editable_by_grant.contains(&playlist.uri)
    }
}

impl App {
    /// Runs one pass of the headless engine loop (for `swiftspot-engine`)
    /// without egui, fonts, tray, or windowing. Returns the maximum duration
    /// the caller should sleep before running again, or `None` when the
    /// engine can sleep indefinitely until woken by an external event.
    pub fn engine_step(&mut self, waker: &Waker) -> Option<Duration> {
        self.serve_api();
        self.handle_events();
        self.open_pending_link();
        self.handle_media_commands();
        let tick_wake = self.tick_core(None);
        self.note_listening();
        self.apply_actions_with(None);
        self.sync_media_controls_headless(waker);
        self.push_api_events();
        self.next_engine_wake(tick_wake)
    }

    /// The soonest moment the engine has something of its own to do: a
    /// poll falling due, a refresh of what the app shows, an optimistic hold
    /// running out, or a song played long enough to count. Anything else
    /// wakes it through the `Waker`.
    fn next_engine_wake(&self, mut wake: Option<Duration>) -> Option<Duration> {
        let now = Instant::now();
        let mut at_most = |due: Instant| {
            let delay = due.saturating_duration_since(now);
            wake = Some(wake.map_or(delay, |current| current.min(delay)));
        };
        if !self.pending_play_keys.is_empty()
            && self.queued_play.is_none()
            && let Some(at) = self.pending_play_at
        {
            at_most(at + Duration::from_secs(8));
        }
        if let Some((_, at)) = self.optimistic_playing {
            at_most(at + PLAYBACK_HOLD);
        }
        for (_, at) in [self.pending_local_position, self.pending_remote_position]
            .into_iter()
            .flatten()
        {
            at_most(at + OPTIMISTIC_HOLD);
        }
        if let Some(assumed) = &self.assumed_context {
            at_most(assumed.at + ASSUMED_CONTEXT_HOLD);
        }
        if self.is_connected() && !self.offline {
            if !self.remote_poll_pending {
                at_most(self.remote_polled_at + self.remote_poll_interval().0);
            }
            // This pass already asked for whatever had never been fetched.
            if self.show_devices && !self.devices_loading {
                at_most(self.devices_fetched_at.unwrap_or(now) + DEVICES_FRESH);
            }
            if (self.show_queue_panel || matches!(self.page(), Page::Queue))
                && !self.queue.is_loading()
            {
                at_most(self.queue_fetched_at.unwrap_or(now) + QUEUE_FRESH);
            }
        }
        if self.now_playing().is_some_and(|now| now.playing) {
            at_most(self.last_session_save + SESSION_REFRESH);
        }
        if let Some(left) = self.listening_left() {
            at_most(now + left);
        }
        wake
    }

    /// How much more of the playing song must be heard before it counts as
    /// played, while it plays and has not counted yet.
    fn listening_left(&self) -> Option<Duration> {
        let listening = self.listening.as_ref()?;
        let since = listening.playing_since?;
        if listening.recorded {
            return None;
        }
        let duration_ms = self.now_playing_live()?.duration_ms;
        let heard = listening.listened + since.elapsed();
        Some(
            crate::history::counts_after(duration_ms)
                .saturating_sub(heard)
                .max(Duration::from_millis(50)),
        )
    }

    /// Records a track after enough active listening time.
    ///
    /// Paused time and seeking do not count. Each track is recorded once.
    fn note_listening(&mut self) {
        let Some(now) = self.now_playing_live() else {
            self.listening = None;
            return;
        };
        let listening = match &mut self.listening {
            Some(held) if held.uri == now.uri => held,
            _ => {
                self.listening = Some(Listening {
                    uri: now.uri.clone(),
                    listened: std::time::Duration::ZERO,
                    playing_since: now.playing.then(Instant::now),
                    recorded: false,
                });
                return;
            }
        };
        match (now.playing, listening.playing_since) {
            // Add the completed interval when playback pauses.
            (false, Some(since)) => {
                listening.listened += since.elapsed();
                listening.playing_since = None;
            }
            (true, None) => listening.playing_since = Some(Instant::now()),
            _ => {}
        }
        if listening.recorded {
            return;
        }
        let listened = listening.listened
            + listening
                .playing_since
                .map(|since| since.elapsed())
                .unwrap_or_default();
        if listened < crate::history::counts_after(now.duration_ms) {
            return;
        }
        listening.recorded = true;
        self.plays
            .record(crate::history::played_track(&now), jiff::Timestamp::now());
        self.plays.save_with(&self.files, self.dirs.history_file());
        self.rebuild_recents();
    }

    /// Persist state when a window closes (to the tray or for good).
    pub fn save_state(&mut self) {
        self.save_settings();
        self.save_session();
        self.files.flush(crate::file_writer::FLUSH_TIMEOUT);
    }

    /// Write the restorable session: page, recents, resume point, sorts.
    fn save_session(&mut self) {
        self.session_dirty = false;
        self.last_session_save = Instant::now();
        if let Some(now) = self.now_playing() {
            // A paused song from a sorted view has no context to report, but
            // its playlist is still what it plays from (#1013).
            let same_song = self.resume_track.as_deref() == Some(now.uri.as_str());
            self.resume_context = self
                .playing_context_uri()
                .or_else(|| self.resume_context.clone().filter(|_| same_song));
            self.resume_track = Some(now.uri.clone());
            self.resume_position_ms = now.position_ms;
        }
        if !self.offline {
            SessionState {
                last_page: Some(self.page().encode()),
                recent_contexts: self.recent_contexts.clone(),
                last_context: self.resume_context.clone(),
                last_track: self.resume_track.clone(),
                last_position_ms: self.resume_position_ms,
                collapsed_folders: self.collapsed_folders.clone(),
                rootlist: self.rootlist_cache.clone(),
                last_added_queue: if self.resume_queue.is_empty() {
                    self.manual_queue.clone()
                } else {
                    // Never resumed this session; the owed queue carries over.
                    self.resume_queue.clone()
                },
                last_queue_rows: self
                    .queue
                    .get()
                    .map(|queue| queue.queue.iter().take(30).cloned().collect())
                    .unwrap_or_default(),
                shuffle_on: self.shuffle_wanted,
                repeat: self.local_repeat().api_name().into(),
                sorts: self
                    .table_sorts
                    .iter()
                    .map(|(page, sort)| (page.encode(), *sort))
                    .collect(),
                window_size: self.last_window_size.or(self.session_window_size),
                window_pos: self.last_window_pos.or(self.session_window_pos),
                queue_tab: Some(self.queue_tab.encode().to_string()),
                lyrics_fullscreen_from: self.lyrics_fullscreen.map(|fullscreen| {
                    crate::settings::WindowMode {
                        fullscreen,
                        maximized: self.lyrics_restore_maximized,
                    }
                }),
            }
            .save_with(&self.files, self.dirs.session_file());
        }
    }

    /// Final teardown at real quit.
    pub fn shutdown(&mut self) {
        // Sent before Shutdown, so the backend writes it before it stops.
        self.checkpoint_home();
        self.save_state();
        self.files.flush(crate::file_writer::FLUSH_TIMEOUT);
        self.backend.shutdown();
    }
}

impl App {
    fn ensure_liked_songs(&mut self) {
        if self.liked_songs.cache_loading || self.liked_songs.refreshing() {
            return;
        }
        if !self.liked_songs.cache_checked {
            let Some(account_id) = self.user_id().map(str::to_owned) else {
                return;
            };
            self.load_generation = self.load_generation.wrapping_add(1);
            self.liked_songs.generation = self.load_generation;
            self.liked_songs.cache_loading = true;
            self.library.liked.loading = true;
            self.backend.send(Command::LoadLikedSongsCache {
                account_id,
                generation: self.load_generation,
            });
        } else if !self.liked_songs.fresh(jiff::Timestamp::now().as_second())
            && self.library.liked.error.is_none()
        {
            self.refresh_liked_songs();
        }
    }

    fn receive_liked_cache(
        &mut self,
        account: &str,
        generation: u64,
        cache: Option<crate::liked::Cache>,
    ) {
        if self.user_id() != Some(account)
            || self.liked_songs.generation != generation
            || self.liked_songs.cache_checked
        {
            return;
        }
        self.liked_songs.cache_loading = false;
        self.liked_songs.cache_checked = true;
        if let Some(cache) = cache.filter(|cache| cache.valid_for(account)) {
            self.liked_songs.restore(cache);
        }
        self.sync_liked_songs();
        let liked: Vec<String> = self
            .library
            .liked
            .items
            .iter()
            .map(|item| item.track.uri.clone())
            .filter(|uri| {
                !self.saved_writes.contains_key(uri) && self.liked_songs.intent(uri).is_none()
            })
            .collect();
        for uri in liked {
            self.record_saved(uri, true);
        }
        for item in &self.library.liked.items {
            let uri = &item.track.uri;
            if !self.saved_writes.contains_key(uri)
                && self.liked_songs.intent(uri).is_none()
                && let Some(key) = item.track.recording_key()
            {
                link_recording(
                    &mut self.track_recordings,
                    &mut self.recording_uris,
                    uri.clone(),
                    key.clone(),
                );
                self.saved_recordings.insert(key);
            }
        }
        if !self.liked_songs.fresh(jiff::Timestamp::now().as_second())
            || self.liked_songs.has_confirmed_changes()
        {
            self.refresh_liked_songs();
        } else if self.table_sorts.contains_key(&Page::LikedSongs) {
            self.load_more(Page::LikedSongs);
        }
    }

    fn refresh_liked_songs(&mut self) {
        if !self.liked_songs.cache_checked {
            self.ensure_liked_songs();
            return;
        }
        self.load_generation = self.load_generation.wrapping_add(1);
        self.liked_songs.start_refresh(self.load_generation);
        self.library.liked.loading = true;
        self.load_more(Page::LikedSongs);
    }

    fn sync_liked_songs(&mut self) {
        self.liked_songs.sync_view(&mut self.library.liked);
        for (uri, saved) in self.liked_songs.intents() {
            self.set_saved_state(uri, saved);
        }
    }

    /// Writes the Home shelves to disk once Spotify has answered for some.
    fn checkpoint_home(&mut self) {
        if !self.home_cache_dirty {
            return;
        }
        if let Some(account) = self.user_id().map(str::to_owned) {
            self.home_cache_dirty = false;
            let cache = crate::home_cache::Cache::capture(account, &self.home, &self.library);
            self.backend.send(Command::StoreHomeCache(cache));
        }
    }

    fn checkpoint_liked_songs(&mut self, force: bool) {
        if let Some(account) = self.user_id().map(str::to_owned)
            && let Some(cache) = self.liked_songs.checkpoint(account, force)
        {
            self.backend.send(Command::StoreLikedSongsCache(cache));
        }
    }

    fn change_liked_song(&mut self, uri: &str, saved: bool) -> bool {
        let Some(id) = uri.strip_prefix("spotify:track:") else {
            return false;
        };
        self.liked_songs.seed(&self.library.liked);
        let track = self
            .track_cache
            .get(id)
            .cloned()
            .or_else(|| {
                self.library
                    .liked
                    .items
                    .iter()
                    .find(|item| item.track.uri == uri)
                    .map(|item| item.track.clone())
            })
            .or_else(|| {
                self.now_playing_item().and_then(|item| match item {
                    PlayableItem::Track(track) if track.uri == uri => Some(track),
                    _ => None,
                })
            })
            .or_else(|| {
                self.home
                    .top_tracks
                    .get()
                    .and_then(|tracks| tracks.iter().find(|track| track.uri == uri).cloned())
            })
            .or_else(|| {
                self.search
                    .results
                    .get()
                    .and_then(|results| results.tracks.as_ref())
                    .and_then(|tracks| tracks.items.iter().find(|track| track.uri == uri).cloned())
            });
        let missing = track.is_none();
        let track = track.unwrap_or_else(|| Track {
            uri: uri.to_string(),
            id: Some(id.to_string()),
            name: gettext(self.locale, "Loading…").into_owned(),
            ..Default::default()
        });
        self.liked_songs.change(uri.to_string(), saved, track);
        if saved && missing && self.track_requests.insert(id.to_string()) {
            self.backend.api(ApiRequest::Track { id: id.to_string() });
        }
        true
    }
}

fn same_recording_hint(left: &Track, right: &Track) -> bool {
    left.uri != right.uri
        && !left.name.is_empty()
        && left.name == right.name
        && left.duration_ms > 0
        && left.duration_ms == right.duration_ms
        && !left.artists.is_empty()
        && left.artists.len() == right.artists.len()
        && left
            .artists
            .iter()
            .zip(&right.artists)
            .all(|(left, right)| match (&left.id, &right.id) {
                (Some(left), Some(right)) => left == right,
                _ => left.name == right.name,
            })
}

fn increase_playlist_total(playlist: &mut Playlist, added: u32) {
    if let Some(items) = &mut playlist.items_count {
        items.total = items.total.saturating_add(added);
    } else if let Some(tracks) = &mut playlist.tracks {
        tracks.total = tracks.total.saturating_add(added);
    } else {
        playlist.items_count = Some(TrackCount { total: added });
    }
}

/// Loads karaoke's vocal model from where it is fetched to.
fn karaoke_loader(dirs: &AppDirs) -> crate::karaoke::Loader {
    let package = crate::karaoke::model::package(&dirs.karaoke_dir());
    std::sync::Arc::new(move || crate::karaoke::coreml::load(&package))
}

pub fn engine_config(
    dirs: &AppDirs,
    settings: &Settings,
    proxy: crate::settings::ProxyConfig,
    eq: crate::eq::SharedEq,
    karaoke: crate::karaoke::SharedKaraoke,
) -> EngineConfig {
    EngineConfig {
        eq,
        karaoke,
        device_name: settings.device_name.trim().to_string(),
        bitrate_kbps: settings.bitrate,
        normalisation: settings.normalisation,
        autoplay: settings.autoplay,
        gapless: settings.gapless,
        backend: settings.platform_backend(),
        buffer_ms: settings.audio_buffer_ms,
        audio_device: settings
            .audio_device
            .clone()
            .filter(|device| !device.trim().is_empty()),
        initial_volume: settings.volume,
        volume_dir: dirs.volume_dir(),
        audio_cache_dir: settings.audio_cache.then(|| dirs.audio_cache_dir()),
        audio_cache_limit: Some(settings.audio_cache_mb.max(64) * 1024 * 1024),
        proxy,
    }
}

/// The equalizer as the settings describe it.
pub fn eq_settings(settings: &Settings) -> crate::eq::EqSettings {
    crate::eq::EqSettings {
        on: settings.eq_on,
        preamp_db: settings.eq_preamp_db,
        bands_db: settings.eq_bands_db,
        balance: settings.balance,
        mono: settings.mono,
    }
    .clamped()
}

pub fn volume_to_percent(volume: u16) -> u8 {
    ((u32::from(volume) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)) as u8
}

pub fn percent_to_volume(percent: u8) -> u16 {
    ((u32::from(percent.min(100)) * u32::from(u16::MAX)) / 100) as u16
}

fn page_related_needs_load(pages: &HashMap<String, ArtistPage>, id: &str) -> bool {
    pages.get(id).is_some_and(|page| page.related.needs_load())
}

fn remote_action_label(locale: Locale, action: RemoteAction) -> Cow<'static, str> {
    match action {
        RemoteAction::Play => gettext(locale, "Couldn't start playback"),
        RemoteAction::Pause => gettext(locale, "Couldn't pause"),
        RemoteAction::Next => gettext(locale, "Couldn't skip"),
        RemoteAction::Previous => gettext(locale, "Couldn't go back"),
        RemoteAction::Seek => gettext(locale, "Couldn't seek"),
        RemoteAction::Volume { .. } => gettext(locale, "Couldn't change the volume"),
        RemoteAction::Shuffle => gettext(locale, "Couldn't change shuffle"),
        RemoteAction::Repeat => gettext(locale, "Couldn't change repeat"),
    }
}

/// Availability explicitly reported for a song; session-only rows are unknown.
fn known_availability(items: &[PlaylistItem]) -> impl Iterator<Item = (&str, bool)> {
    items.iter().filter_map(|item| match item.playable()? {
        PlayableItem::Track(track) => Some((track.uri.as_str(), track.is_playable?)),
        _ => None,
    })
}

/// A fresh Web API answer takes precedence over anything remembered from disk.
fn note_availability(availability: &mut HashMap<String, bool>, items: &[PlaylistItem]) {
    availability
        .extend(known_availability(items).map(|(uri, playable)| (uri.to_string(), playable)));
}

/// Grey out the songs the Web API has said this account cannot play,
/// among rows that do not say. A page read over the streaming session
/// says nothing of the account's market; a song the Web API had greyed
/// out stays so wherever it recurs, and rows it never described stay
/// unknown.
fn fill_availability(availability: &HashMap<String, bool>, items: &mut [PlaylistItem]) -> bool {
    let mut changed = false;
    for item in items {
        if let Some(PlayableItem::Track(track)) = item.item.as_mut()
            && track.is_playable.is_none()
            && availability.get(&track.uri) == Some(&false)
        {
            track.is_playable = Some(false);
            changed = true;
        }
    }
    changed
}

/// The playback engine reports in English from its own thread. The
/// messages the interface recognises are shown in the user's language.
fn engine_error_text(locale: Locale, error: &str) -> String {
    if error == crate::sink::NO_DEVICE {
        return gettext(
            locale,
            "No audio output device was found. Connect or enable one, then press play again.",
        )
        .into_owned();
    }
    error.to_owned()
}

fn friendly_page_error(locale: Locale, error: &crate::api::ApiError) -> String {
    match error.status() {
        Some(403) | Some(404) => gettext(
            locale,
            "Spotify doesn't make this playlist's songs available to third-party apps.",
        )
        .into_owned(),
        _ => error.to_string(),
    }
}

/// Whether a Spotify-owned playlist named `name` is the personal one the
/// Made for you shelf looks for under `term`. The name has to be the term
/// itself, or "Daily Mix" with a number: Spotify also makes "<Artist> Mix",
/// "This Is <Artist>", and "<Artist> Radio" for every artist, and an artist
/// called "Discover Weekly" put those on the shelf (#89).
fn is_made_for_you(name: &str, term: &str) -> bool {
    let name = name.trim().to_lowercase();
    let term = term.to_lowercase();
    if name == term {
        return true;
    }
    term == "daily mix"
        && name
            .strip_prefix("daily mix ")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// What the engine is told to play. A single song goes as a context of
/// its own rather than a list of one: Spotify resolves a track's URI as a
/// context, and a context with a URI is what librespot's autoplay carries
/// on from when it ends, the way one song from a search does in Spotify.
fn local_load(request: &PlayRequest, shuffle: bool) -> LoadSpec {
    let single_song = request.context_uri.is_none()
        && request.uris.len() == 1
        && request.uris[0].contains(":track:");
    if single_song {
        return LoadSpec {
            context_uri: Some(request.uris[0].clone()),
            position_ms: request.position_ms,
            play: true,
            ..LoadSpec::default()
        };
    }
    // A plain track list must not load shuffled when a row was chosen:
    // librespot shuffles the list first and then cannot find the chosen
    // row in it, falls back to nowhere, and replays what was on. The list
    // loads straight and shuffle is switched on right after the load,
    // which also matches Spotify: the chosen song plays, the rest shuffle.
    let chosen = request.offset_uri.is_some() || request.offset_position.is_some();
    let list = request.context_uri.is_none();
    LoadSpec {
        context_uri: request.context_uri.clone(),
        uris: request.uris.clone(),
        offset_uri: request.offset_uri.clone(),
        offset_index: request.offset_position,
        position_ms: request.position_ms,
        play: true,
        shuffle: (shuffle && !(list && chosen)).then_some(true),
        repeat: None,
        autoplay: false,
    }
}

/// Returns the final track as the autoplay seed when a plain list ends.
fn autoplay_seed(
    list: Option<&[String]>,
    autoplay: bool,
    before: &LocalState,
    after: &LocalState,
) -> Option<String> {
    if !autoplay
        || !after.connected
        || after.playback != Playback::Stopped
        || before.playback != Playback::Playing
    {
        return None;
    }
    let track = before.track.as_ref()?;
    if list?.last() != Some(&track.uri) || track.duration_ms == 0 {
        return None;
    }
    let near_end = before.position_now() + 3_000 >= track.duration_ms;
    near_end.then(|| track.uri.clone())
}

/// Caps large track lists at 500 items starting from the selected row.
fn cap_uris(uris: &[String], index: u32) -> (Vec<String>, u32) {
    const MAX: usize = 500;
    if uris.len() <= MAX {
        return (uris.to_vec(), index);
    }
    let start = (index as usize).min(uris.len().saturating_sub(1));
    let end = (start + MAX).min(uris.len());
    (uris[start..end].to_vec(), 0)
}

fn evict_lru_map<V>(
    map: &mut HashMap<String, V>,
    used: &HashMap<Page, Instant>,
    to_page: impl Fn(&str) -> Page,
    protected: &HashSet<String>,
    max: usize,
) {
    if map.len() <= max {
        return;
    }
    let overflow = map.len() - max;
    let mut victims: Vec<(Option<Instant>, String)> = map
        .keys()
        .filter(|id| !protected.contains(*id))
        .map(|id| {
            let used = used.get(&to_page(id)).copied();
            (used, id.clone())
        })
        .collect();
    victims.sort();
    for (_, id) in victims.into_iter().take(overflow) {
        map.remove(&id);
    }
}

fn cover_images(cover: &crate::playlist_cover::Cover) -> Vec<crate::api::models::Image> {
    vec![crate::api::models::Image {
        url: cover.uri.clone(),
        width: None,
        height: None,
    }]
}

fn cover_error(locale: Locale, error: &crate::api::client::ApiError) -> String {
    match error.status() {
        Some(401) => gettext(
            locale,
            "Spotify sign-in expired. Sign in again, then retry the cover upload.",
        )
        .into_owned(),
        Some(403) => gettext(
            locale,
            "Spotify refused this cover. Check that you own the playlist, then sign in again to grant image upload permission. If using a personal app, reconnect it in Settings too.",
        )
        .into_owned(),
        Some(413) => {
            gettext(locale, "Spotify rejected the image size. Choose a smaller image.").into_owned()
        }
        _ => gettext(locale, "Couldn't upload the cover: {error}. Try again.")
            .replace("{error}", &error.to_string()),
    }
}

mod api;
mod radio;

/// A song or an episode known only by its URI, for a row Spotify fills in
/// once it answers.
fn bare_item(uri: &str) -> PlayableItem {
    let id = uri.rsplit(':').next().unwrap_or_default().to_string();
    if uri.starts_with("spotify:episode:") {
        PlayableItem::Episode(crate::api::models::Episode {
            uri: uri.to_string(),
            id,
            ..Default::default()
        })
    } else {
        PlayableItem::Track(Track {
            uri: uri.to_string(),
            id: Some(id),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn a_held_seek_moves_on_while_the_song_plays() {
        let second = Duration::from_secs(1);
        assert_eq!(held_position(30_000, second, true, 200_000), 31_000);
        assert_eq!(held_position(30_000, second, false, 200_000), 30_000);
        assert_eq!(held_position(199_500, second, true, 200_000), 200_000);
        assert_eq!(held_position(5_000, second, true, 0), 5_000);
    }

    #[test]
    fn waker_returns_immediately_when_signaled_before_or_during_wait() {
        let waker = Waker::default();
        waker.wake();
        let started = Instant::now();
        waker.wait(Some(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_millis(200));

        let wake_bg = waker.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            wake_bg.wake();
        });
        let started = Instant::now();
        waker.wait(Some(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn headless_engine_sleeps_indefinitely_when_signed_out_and_idle() {
        let dir =
            std::env::temp_dir().join(format!("swiftspot-engine-idle-{}", rand::random::<u64>()));
        let dirs = AppDirs {
            config: dir.join("config"),
            state: dir.join("state"),
            cache: dir.join("cache"),
        };
        let _ = dirs.ensure();
        let waker = Waker::default();
        let mut app = App::new(
            &waker,
            dirs,
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        );
        app.auth = AuthStatus::SignedOut;
        let wait = app.engine_step(&waker);
        assert_eq!(
            wait, None,
            "an idle signed-out engine should sleep until woken instead of polling"
        );
        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_pushes_a_page_event_when_an_api_answer_arrives() {
        let dir = std::env::temp_dir().join(format!("ss-pg-{:08x}", rand::random::<u32>()));
        let dirs = AppDirs {
            config: dir.join("config"),
            state: dir.join("state"),
            cache: dir.join("cache"),
        };
        let _ = dirs.ensure();
        let waker = Waker::default();
        let mut app = App::new(
            &waker,
            dirs,
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        );
        app.auth = AuthStatus::SignedOut;
        let (_inbox, messages) = app.connect_front_end(&waker);
        app.engine_step(&waker);
        while messages.try_recv().is_ok() {}

        app.handle_api(ApiResponse::Devices(Ok(Vec::new())));
        app.engine_step(&waker);

        let saw_page = messages.try_iter().any(|line| {
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            event["event"] == "page" && event["data"]["page"] == "devices"
        });
        assert!(saw_page, "expected a pushed page event for devices");

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_found_adder_name_pushes_the_playlists_that_show_it() {
        let dir = std::env::temp_dir().join(format!("ss-un-{:08x}", rand::random::<u32>()));
        let dirs = AppDirs {
            config: dir.join("config"),
            state: dir.join("state"),
            cache: dir.join("cache"),
        };
        let _ = dirs.ensure();
        let waker = Waker::default();
        let mut app = App::new(
            &waker,
            dirs,
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        );
        app.auth = AuthStatus::SignedOut;
        let mut shared = crate::model::PlaylistPage::default();
        shared.contributors.insert("kasia-id".into());
        app.playlist_pages.insert("mix".into(), shared);
        app.playlist_pages
            .insert("solo".into(), crate::model::PlaylistPage::default());
        let (_inbox, messages) = app.connect_front_end(&waker);
        app.engine_step(&waker);
        while messages.try_recv().is_ok() {}

        app.set_user_name("kasia-id".into(), Some("Kasia".into()));
        app.engine_step(&waker);

        let pages: Vec<String> = messages
            .try_iter()
            .filter_map(|line| {
                let event: serde_json::Value = serde_json::from_str(&line).unwrap();
                (event["event"] == "page")
                    .then(|| event["data"]["page"].as_str().unwrap().to_string())
            })
            .collect();
        assert_eq!(pages, ["playlist:mix"]);

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_pushes_only_the_saved_flags_that_changed() {
        let dir = std::env::temp_dir().join(format!("ss-sv-{:08x}", rand::random::<u32>()));
        let dirs = AppDirs {
            config: dir.join("config"),
            state: dir.join("state"),
            cache: dir.join("cache"),
        };
        let _ = dirs.ensure();
        let waker = Waker::default();
        let mut app = App::new(
            &waker,
            dirs,
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        );
        app.auth = AuthStatus::SignedOut;
        let (_inbox, messages) = app.connect_front_end(&waker);
        app.engine_step(&waker);
        while messages.try_recv().is_ok() {}

        app.record_saved("spotify:track:a".into(), true);
        app.record_saved("spotify:track:a".into(), true);
        app.engine_step(&waker);
        let events: Vec<serde_json::Value> = messages
            .try_iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(&line).unwrap())
            .collect();
        let saved: Vec<_> = events.iter().filter(|e| e["event"] == "saved").collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(
            saved[0]["data"],
            serde_json::json!({ "spotify:track:a": true })
        );
        assert!(
            !events.iter().any(|e| e["event"] == "library"),
            "a saved flag alone must not reload the library"
        );

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headless_settings_round_trip_preserves_gui_theme_caches() {
        let raw = serde_json::json!({
            "theme": "dark",
            "custom_theme": "gruvbox.json",
            "custom_theme_cache": { "filename": "gruvbox.json", "palette": { "dark": true } },
            "system_theme_cache": { "filename": "omarchy.json", "palette": { "dark": true } }
        });
        let settings: Settings = serde_json::from_value(raw.clone()).unwrap();
        let encoded = serde_json::to_value(&settings).unwrap();
        assert_eq!(encoded["custom_theme_cache"], raw["custom_theme_cache"]);
        assert_eq!(encoded["system_theme_cache"], raw["system_theme_cache"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Episode, Image, ResumePoint, SavedTrack};

    fn test_app(name: &str) -> App {
        let root =
            std::env::temp_dir().join(format!("swiftspot-{name}-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        App::new(
            &Waker::default(),
            AppDirs {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
            },
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        )
    }

    pub(crate) fn headless_app() -> App {
        let root =
            std::env::temp_dir().join(format!("swiftspot-volume-test-{}", std::process::id()));
        let dirs = AppDirs {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        };
        let mut app = App::new(
            &Waker::default(),
            dirs,
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        );
        app.local_ready = true;
        app
    }

    fn shown_availability(app: &mut App, id: &str) -> Option<bool> {
        let page = &app.playlist_pages[id];
        match page.items.items[0].playable().expect("a playable row") {
            PlayableItem::Track(track) => track.is_playable,
            _ => panic!("a track row"),
        }
    }

    /// #623: the missing-output message the sink reports reaches the user.
    #[test]
    fn a_missing_audio_output_is_reported_as_the_sink_words_it() {
        assert_eq!(
            engine_error_text(Locale::English, crate::sink::NO_DEVICE),
            crate::sink::NO_DEVICE
        );
        assert_eq!(
            engine_error_text(Locale::English, "Something else"),
            "Something else"
        );
    }

    /// The playlist just started stays lit after the first seconds even
    /// while the last poll, taken before the click, still names what the
    /// phone was playing; only a poll from after the click may say
    /// otherwise.
    #[test]
    fn a_poll_from_before_the_click_does_not_move_the_sidebar_light() {
        // #given a poll from the phone's playlist, then a playlist started here
        let mut app = headless_app();
        app.remote = Some(RemoteSnapshot {
            state: PlaybackState {
                context: Some(crate::api::models::Context {
                    uri: "spotify:playlist:phone".into(),
                    ..Default::default()
                }),
                is_playing: true,
                ..Default::default()
            },
            received_at: Instant::now() - Duration::from_secs(30),
        });
        app.assumed_context = Some(AssumedContext {
            uri: "spotify:playlist:here".into(),
            shuffle: None,
            at: Instant::now() - ASSUMED_CONTEXT_HOLD - Duration::from_secs(1),
        });
        app.optimistic_playing = Some((true, Instant::now()));

        // #then the hold is over, yet the stale poll does not win
        assert_eq!(
            app.playing_context_uri().as_deref(),
            Some("spotify:playlist:here")
        );

        // #when a poll taken after the click still reports the phone's playlist
        app.remote.as_mut().expect("a snapshot").received_at = Instant::now();

        // #then Spotify's word from after the click stands
        assert_eq!(
            app.playing_context_uri().as_deref(),
            Some("spotify:playlist:phone")
        );
    }

    /// #1013: closing keeps the playlist a song plays from when nothing
    /// reports it any more, and Play picks that playlist up again even when
    /// Spotify's last state has the song without it.
    #[test]
    fn the_playing_playlist_survives_a_restart_without_a_reported_context() {
        // #given a paused song from a sorted playlist, long after the click
        let mut app = headless_app();
        app.local = LocalState {
            connected: true,
            playback: Playback::Paused,
            track: Some(crate::player::LocalTrack {
                uri: "spotify:track:song".into(),
                duration_ms: 240_000,
                ..Default::default()
            }),
            position_ms: 5_000,
            ..Default::default()
        };
        app.resume_context = Some("spotify:playlist:kept".into());
        app.resume_track = Some("spotify:track:song".into());
        app.assumed_context = Some(AssumedContext {
            uri: "spotify:playlist:kept".into(),
            shuffle: None,
            at: Instant::now() - ASSUMED_CONTEXT_HOLD - Duration::from_secs(1),
        });

        // #when the session is saved
        app.save_session();

        // #then the playlist is kept
        assert_eq!(app.resume_context.as_deref(), Some("spotify:playlist:kept"));

        // #when a restart shows Spotify's last state of that song, context lost
        app.local = LocalState {
            connected: true,
            ..Default::default()
        };
        app.assumed_context = None;
        app.local_ready = true;
        app.local_device_id = Some("this-computer".into());
        app.remote = Some(RemoteSnapshot {
            state: PlaybackState {
                device: Some(Device {
                    id: Some("this-computer".into()),
                    ..Default::default()
                }),
                item: Some(PlayableItem::Track(Track {
                    uri: "spotify:track:song".into(),
                    ..Default::default()
                })),
                progress_ms: Some(5_000),
                ..Default::default()
            },
            received_at: Instant::now(),
        });
        app.backend.take_player_commands();
        app.toggle_play();

        // #then Play starts the playlist at that song
        let load = app
            .backend
            .take_player_commands()
            .into_iter()
            .find_map(|command| match command {
                PlayerCommand::Load(spec) => Some(spec),
                _ => None,
            })
            .expect("Play loads the song");
        assert_eq!(load.context_uri.as_deref(), Some("spotify:playlist:kept"));
        assert_eq!(load.offset_uri.as_deref(), Some("spotify:track:song"));
        assert_eq!(load.position_ms, 5_000);

        // A song played on its own stays on its own.
        let mut app = headless_app();
        app.local = LocalState {
            connected: true,
            playback: Playback::Paused,
            track: Some(crate::player::LocalTrack {
                uri: "spotify:track:other".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        app.resume_context = Some("spotify:playlist:kept".into());
        app.resume_track = Some("spotify:track:song".into());
        app.save_session();
        assert_eq!(app.resume_context, None);
    }

    /// #1121: a poll that arrives after the click but still names the song
    /// from before it keeps its old context to itself, playing or paused.
    /// Once a poll names the playing song, Spotify's context stands.
    #[test]
    fn a_lagging_poll_does_not_light_the_playlist_left_behind() {
        // #given a song from an album, started after a playlist's song
        let mut app = headless_app();
        app.local = LocalState {
            connected: true,
            playback: Playback::Playing,
            track: Some(crate::player::LocalTrack {
                uri: "spotify:track:album-song".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        app.assumed_context = Some(AssumedContext {
            uri: "spotify:album:here".into(),
            shuffle: None,
            at: Instant::now() - ASSUMED_CONTEXT_HOLD - Duration::from_secs(1),
        });
        let poll = |song: &str| RemoteSnapshot {
            state: PlaybackState {
                context: Some(crate::api::models::Context {
                    uri: "spotify:playlist:left".into(),
                    ..Default::default()
                }),
                item: Some(PlayableItem::Track(Track {
                    uri: song.into(),
                    ..Default::default()
                })),
                is_playing: true,
                ..Default::default()
            },
            received_at: Instant::now(),
        };

        // #when a late poll still tells the story from before the click
        app.remote = Some(poll("spotify:track:playlist-song"));

        // #then the album stays lit, and pausing changes nothing
        assert_eq!(
            app.playing_context_uri().as_deref(),
            Some("spotify:album:here")
        );
        app.local.playback = Playback::Paused;
        assert_eq!(
            app.playing_context_uri().as_deref(),
            Some("spotify:album:here")
        );

        // #when a poll names the playing song, its context is believed
        app.local.playback = Playback::Playing;
        app.remote = Some(poll("spotify:track:album-song"));
        assert_eq!(
            app.playing_context_uri().as_deref(),
            Some("spotify:playlist:left")
        );
    }

    /// A song the Web API had greyed out stays greyed out when the session
    /// reads the rows, at every place it recurs, whether the Web API's word
    /// came from an earlier page or from the disk cache of a visit the
    /// playlist has changed since. Rows it never described stay unknown,
    /// and a song it later calls playable is no longer greyed out.
    #[test]
    fn a_session_read_keeps_a_songs_known_unavailability() {
        let mut app = headless_app();
        app.user = Some(User {
            id: "alice".into(),
            ..Default::default()
        });
        let row = |uri: &str, is_playable: Option<bool>| crate::api::models::PlaylistItem {
            item: Some(PlayableItem::Track(Track {
                uri: uri.into(),
                is_playable,
                ..Default::default()
            })),
            ..Default::default()
        };
        let items =
            |id: &str, rows: Vec<crate::api::models::PlaylistItem>| ApiResponse::PlaylistItems {
                id: id.into(),
                offset: 0,
                generation: 0,
                result: Ok(crate::api::models::Page {
                    total: rows.len() as u32,
                    limit: 50,
                    items: rows,
                    ..Default::default()
                }),
            };
        let rows = |app: &App, id: &str| {
            app.playlist_pages[id]
                .items
                .items
                .iter()
                .map(|item| match item.playable() {
                    Some(PlayableItem::Track(track)) => track.is_playable,
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // The Web API's word from an earlier page of the same list.
        app.playlist_pages
            .insert("pl1".into(), PlaylistPage::default());
        app.handle_api(items(
            "pl1",
            vec![
                row("spotify:track:gone", Some(false)),
                row("spotify:track:fine", Some(true)),
                row("spotify:track:gone", Some(false)),
            ],
        ));
        app.handle_api(items(
            "pl1",
            vec![
                row("spotify:track:gone", None),
                row("spotify:track:fine", None),
                row("spotify:track:new", None),
                row("spotify:track:gone", None),
            ],
        ));
        assert_eq!(rows(&app, "pl1"), [Some(false), None, None, Some(false)]);

        // The Web API's word from the disk cache of another visit, read
        // after the session page arrived and never adopted, the playlist
        // having changed since.
        app.playlist_pages.insert(
            "pl2".into(),
            PlaylistPage {
                playlist: Loadable::Loaded(Playlist {
                    id: "pl2".into(),
                    snapshot_id: Some("now".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        app.handle_api(items(
            "pl2",
            vec![
                row("spotify:track:other", None),
                row("spotify:track:fine", None),
            ],
        ));
        app.receive_playlist_cache(
            "alice",
            "pl2",
            0,
            Some(PlaylistCache {
                snapshot: "then".into(),
                items: vec![row("spotify:track:other", Some(false))],
                total: 1,
                next_offset: None,
                appendable: false,
            }),
        );
        assert_eq!(
            rows(&app, "pl2"),
            [Some(false), None],
            "the cache's word reaches the rows already shown"
        );
        assert!(
            app.playlist_pages["pl2"].pending_cache.is_none(),
            "though the stale cache itself is not adopted"
        );

        // A song the Web API later calls playable is no longer greyed out.
        app.handle_api(items("pl2", vec![row("spotify:track:other", Some(true))]));
        app.handle_api(items("pl2", vec![row("spotify:track:other", None)]));
        assert_eq!(rows(&app, "pl2"), [None]);
    }

    fn availability_row(playable: Option<bool>) -> PlaylistItem {
        PlaylistItem {
            item: Some(PlayableItem::Track(Track {
                uri: "spotify:track:availability".into(),
                is_playable: playable,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn receive_availability_rows(app: &mut App, id: &str, playable: Option<bool>) {
        app.playlist_pages
            .entry(id.into())
            .or_insert_with(|| PlaylistPage {
                playlist: Loadable::Loaded(Playlist {
                    id: id.into(),
                    snapshot_id: Some("now".into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
        app.handle_api(ApiResponse::PlaylistItems {
            id: id.into(),
            offset: 0,
            generation: 0,
            result: Ok(crate::api::models::Page {
                items: vec![availability_row(playable)],
                total: 1,
                limit: 50,
                ..Default::default()
            }),
        });
    }

    fn old_availability_cache(playable: bool) -> Option<PlaylistCache> {
        Some(PlaylistCache {
            snapshot: "then".into(),
            items: vec![availability_row(Some(playable))],
            total: 1,
            next_offset: None,
            appendable: false,
        })
    }

    #[test]
    fn playlist_availability_does_not_follow_an_account_switch() {
        let mut app = headless_app();
        app.user = Some(User {
            id: "alice".into(),
            ..Default::default()
        });
        receive_availability_rows(&mut app, "pl1", Some(false));
        app.handle_auth(AuthStatus::SignedOut);
        app.handle_auth(AuthStatus::Connected {
            username: "bob".into(),
        });
        app.user = Some(User {
            id: "bob".into(),
            ..Default::default()
        });
        receive_availability_rows(&mut app, "pl1", None);
        app.receive_playlist_cache("alice", "pl1", 0, old_availability_cache(false));
        assert_eq!(shown_availability(&mut app, "pl1"), None);
    }

    #[test]
    fn playlist_availability_prefers_fresh_answers_to_late_disk_caches() {
        for fresh in [true, false] {
            let mut app = headless_app();
            app.user = Some(User {
                id: "alice".into(),
                ..Default::default()
            });
            receive_availability_rows(&mut app, "pl1", Some(fresh));
            receive_availability_rows(&mut app, "pl2", None);
            app.receive_playlist_cache("alice", "pl2", 0, old_availability_cache(!fresh));
            receive_availability_rows(&mut app, "pl3", None);
            assert_eq!(
                shown_availability(&mut app, "pl3"),
                (!fresh).then_some(false)
            );
        }
    }

    #[test]
    fn playlist_availability_from_disk_reaches_rows_already_drawn() {
        let mut app = headless_app();
        app.user = Some(User {
            id: "alice".into(),
            ..Default::default()
        });
        receive_availability_rows(&mut app, "pl1", None);
        assert_eq!(shown_availability(&mut app, "pl1"), None);
        app.receive_playlist_cache("alice", "pl1", 0, old_availability_cache(false));
        assert_eq!(shown_availability(&mut app, "pl1"), Some(false));
    }

    #[test]
    fn playlist_availability_stays_fresh_when_a_cached_prefix_is_adopted() {
        let mut app = headless_app();
        app.user = Some(User {
            id: "alice".into(),
            ..Default::default()
        });
        receive_availability_rows(&mut app, "pl1", Some(true));
        app.playlist_pages.insert(
            "pl2".into(),
            PlaylistPage {
                playlist: Loadable::Loaded(Playlist {
                    id: "pl2".into(),
                    snapshot_id: Some("then".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        app.receive_playlist_cache("alice", "pl2", 0, old_availability_cache(false));
        assert_eq!(shown_availability(&mut app, "pl2"), Some(true));
    }

    /// A header read over the streaming session carries no public flag,
    /// and the edit dialog fills its switch from it, so the library list's
    /// answer stands in.
    #[test]
    fn a_header_without_a_public_flag_takes_the_library_lists() {
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.library.playlists = Loadable::Loaded(vec![Playlist {
            id: "pl1".into(),
            public: Some(true),
            ..Playlist::default()
        }]);
        app.playlist_pages.insert(
            "pl1".into(),
            PlaylistPage {
                generation: 1,
                ..Default::default()
            },
        );
        app.handle_api(ApiResponse::Playlist {
            id: "pl1".into(),
            generation: 1,
            result: Ok(Playlist {
                id: "pl1".into(),
                name: "Mine".into(),
                ..Playlist::default()
            }),
        });
        let playlist = app.playlist_pages["pl1"].playlist.get().unwrap();
        assert_eq!(playlist.public, Some(true));
        assert_eq!(playlist.name, "Mine", "the rest is Spotify's answer");

        // Spotify's own answer outranks the list, and a playlist the list
        // does not hold stays unknown rather than guessed.
        app.handle_api(ApiResponse::Playlist {
            id: "pl1".into(),
            generation: 1,
            result: Ok(Playlist {
                id: "pl1".into(),
                public: Some(false),
                ..Playlist::default()
            }),
        });
        assert_eq!(
            app.playlist_pages["pl1"].playlist.get().unwrap().public,
            Some(false)
        );
        app.playlist_pages.insert(
            "pl2".into(),
            PlaylistPage {
                generation: 1,
                ..Default::default()
            },
        );
        app.handle_api(ApiResponse::Playlist {
            id: "pl2".into(),
            generation: 1,
            result: Ok(Playlist {
                id: "pl2".into(),
                ..Playlist::default()
            }),
        });
        assert_eq!(
            app.playlist_pages["pl2"].playlist.get().unwrap().public,
            None
        );

        // The list can arrive after the header: the page takes the flag
        // then, and a flag Spotify already gave stays.
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: app.library.playlists_generation,
            result: Ok(crate::api::models::Page {
                items: vec![
                    Playlist {
                        id: "pl1".into(),
                        public: Some(true),
                        ..Playlist::default()
                    },
                    Playlist {
                        id: "pl2".into(),
                        public: Some(true),
                        ..Playlist::default()
                    },
                ],
                ..Default::default()
            }),
        });
        assert_eq!(
            app.playlist_pages["pl2"].playlist.get().unwrap().public,
            Some(true)
        );
        assert_eq!(
            app.playlist_pages["pl1"].playlist.get().unwrap().public,
            Some(false)
        );
    }

    /// A page of the library's playlists, `limit` 2, that says whether
    /// more follow.
    fn playlist_page(ids: &[&str], offset: u32, total: u32) -> crate::api::models::Page<Playlist> {
        crate::api::models::Page {
            items: ids
                .iter()
                .map(|id| Playlist {
                    id: (*id).into(),
                    uri: format!("spotify:playlist:{id}"),
                    ..Playlist::default()
                })
                .collect(),
            total,
            limit: 2,
            offset,
            next: (offset + (ids.len() as u32) < total).then(|| "more".to_string()),
        }
    }

    fn listed_playlists(app: &App) -> Option<Vec<String>> {
        app.library.playlists.get().map(|playlists| {
            playlists
                .iter()
                .map(|playlist| playlist.id.clone())
                .collect::<Vec<_>>()
        })
    }

    fn playlist_ids(ids: &[&str]) -> Option<Vec<String>> {
        Some(ids.iter().map(|id| (*id).to_string()).collect())
    }

    fn signed_in_with_saved_playlists(saved: &[&str], fetched_hours_ago: i64) -> App {
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.handle_auth(AuthStatus::Connected {
            username: "me".into(),
        });
        app.user = Some(User {
            id: "me".into(),
            ..User::default()
        });
        let library = Library {
            playlists: Loadable::Loaded(playlist_page(saved, 0, saved.len() as u32).items),
            playlists_fetched_at: Some(
                jiff::Timestamp::now().as_second() - fetched_hours_ago * 60 * 60,
            ),
            ..Library::default()
        };
        app.handle_backend_events(vec![Event::HomeCache {
            account_id: "me".into(),
            cache: Some(crate::home_cache::Cache::capture(
                "me".into(),
                &HomeData::default(),
                &library,
            )),
        }]);
        app
    }

    fn session_lists(app: &mut App, ids: &[&str]) {
        app.handle_backend_events(vec![Event::Rootlist {
            result: Ok(crate::player::Rootlist {
                entries: ids
                    .iter()
                    .map(|id| {
                        crate::player::RootlistEntry::Playlist(format!("spotify:playlist:{id}"))
                    })
                    .collect(),
                editable: Default::default(),
            }),
        }]);
    }

    /// The saved playlists stand when the session, which has no quota,
    /// lists the same ones and they are less than a day old: reading every
    /// page again went over the shared app's rate limit at each start.
    #[test]
    fn saved_playlists_the_session_confirms_are_not_read_again() {
        let mut app = signed_in_with_saved_playlists(&["a", "b"], 2);
        let generation = app.library.playlists_generation;
        session_lists(&mut app, &["b", "a"]);
        assert!(app.playlists_check.is_none());
        assert_eq!(
            app.library.playlists_generation, generation,
            "nothing read again"
        );
        assert_eq!(listed_playlists(&app), playlist_ids(&["a", "b"]));
        assert_eq!(app.is_saved("spotify:playlist:a"), Some(true));
    }

    /// A playlist made or followed elsewhere, or a day gone by, reads them
    /// all again.
    #[test]
    fn saved_playlists_are_read_again_when_they_differ_or_are_old() {
        let mut changed = signed_in_with_saved_playlists(&["a", "b"], 2);
        let generation = changed.library.playlists_generation;
        session_lists(&mut changed, &["a", "b", "c"]);
        assert_ne!(changed.library.playlists_generation, generation);

        let mut old = signed_in_with_saved_playlists(&["a", "b"], 30);
        let generation = old.library.playlists_generation;
        session_lists(&mut old, &["a", "b"]);
        assert_ne!(old.library.playlists_generation, generation);
    }

    /// A later page of the library's playlists that fails, as when Spotify
    /// rate limits it, can be asked for again: the list stopped at its
    /// first page for good.
    #[test]
    fn a_failed_later_playlist_page_can_be_asked_for_again() {
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.load_playlists();
        let generation = app.library.playlists_generation;
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation,
            result: Ok(playlist_page(&["a", "b"], 0, 3)),
        });
        assert_eq!(app.library.playlists_asked, Some(2));
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation,
            result: Err(crate::api::ApiError::RateLimited),
        });
        assert_eq!(listed_playlists(&app), playlist_ids(&["a", "b"]));
        assert_eq!(
            app.library.playlists_next,
            Some(2),
            "the failed page is next"
        );
        app.load_more(Page::Home);
        assert_eq!(app.library.playlists_asked, Some(2));
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation,
            result: Ok(playlist_page(&["c"], 2, 3)),
        });
        assert_eq!(listed_playlists(&app), playlist_ids(&["a", "b", "c"]));
    }

    /// Following, unfollowing or editing a playlist reads the library's
    /// playlists again from the top. A later page asked for before that
    /// belongs to the old list: taking it made that page the whole list,
    /// and the pages it led on to ran beside the new ones and repeated them.
    #[test]
    fn a_playlist_page_asked_for_before_a_reload_is_not_taken() {
        let mut app = headless_app();
        app.backend.set_offline(true);

        app.load_playlists();
        let old = app.library.playlists_generation;
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: old,
            result: Ok(playlist_page(&["a", "b"], 0, 3)),
        });
        // The second page is on its way when following a playlist reloads.
        app.handle_api(ApiResponse::PlaylistFollowChanged {
            id: "new".into(),
            followed: true,
            result: Ok(()),
        });
        // The list shown stays up while the reload gathers its pages.
        assert!(app.library.playlists_refresh.is_some());
        assert_eq!(listed_playlists(&app), playlist_ids(&["a", "b"]));
        let new = app.library.playlists_generation;
        assert_ne!(new, old, "a reload is a new load");
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation: old,
            result: Ok(playlist_page(&["c"], 2, 3)),
        });
        assert_eq!(
            listed_playlists(&app),
            playlist_ids(&["a", "b"]),
            "the late page is not taken into the list"
        );
        assert_eq!(
            app.library.playlists_refresh.as_deref().map(<[_]>::len),
            Some(0),
            "nor into the reload"
        );
        assert_eq!(app.library.playlists_next, None, "and asks for nothing");

        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: new,
            result: Ok(playlist_page(&["new", "a"], 0, 4)),
        });
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation: new,
            result: Ok(playlist_page(&["b", "c"], 2, 4)),
        });
        let whole = playlist_ids(&["new", "a", "b", "c"]);
        assert_eq!(listed_playlists(&app), whole);
        // Another answer for a page already taken adds nothing.
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation: new,
            result: Ok(playlist_page(&["b", "c"], 2, 4)),
        });
        assert_eq!(listed_playlists(&app), whole);
    }

    /// An offset does not say which load a page belongs to. Once the
    /// reloaded list has asked for its own second page, the old load's
    /// second page, arriving late at the same offset, is still not taken:
    /// taking it ended the list early and dropped the real second page.
    #[test]
    fn an_old_playlist_page_at_the_offset_the_reload_asked_for_is_not_taken() {
        let mut app = headless_app();
        app.backend.set_offline(true);

        app.load_playlists();
        let old = app.library.playlists_generation;
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: old,
            result: Ok(playlist_page(&["a", "b"], 0, 3)),
        });
        assert_eq!(app.library.playlists_asked, Some(2));
        app.handle_api(ApiResponse::PlaylistFollowChanged {
            id: "new".into(),
            followed: true,
            result: Ok(()),
        });
        let new = app.library.playlists_generation;
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: new,
            result: Ok(playlist_page(&["new", "a"], 0, 4)),
        });
        assert_eq!(
            app.library.playlists_asked,
            Some(2),
            "the reloaded list asks for the same offset"
        );

        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation: old,
            result: Ok(playlist_page(&["c"], 2, 3)),
        });
        assert_eq!(
            listed_playlists(&app),
            playlist_ids(&["a", "b"]),
            "the old load's page is not taken, and the shown list stays"
        );
        assert_eq!(
            app.library
                .playlists_refresh
                .as_ref()
                .map(|gathered| gathered
                    .iter()
                    .map(|playlist| playlist.id.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["new", "a"]),
            "the reload keeps only its own page"
        );
        assert_eq!(
            app.library.playlists_asked,
            Some(2),
            "the reloaded list still waits for its own page"
        );

        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation: new,
            result: Ok(playlist_page(&["b", "c"], 2, 4)),
        });
        assert_eq!(
            listed_playlists(&app),
            playlist_ids(&["new", "a", "b", "c"])
        );
    }

    /// A later page that failed is no longer on its way, so another answer
    /// for it is not taken into the list.
    #[test]
    fn a_failed_playlist_page_is_no_longer_awaited() {
        let mut app = headless_app();
        app.backend.set_offline(true);

        app.load_playlists();
        let generation = app.library.playlists_generation;
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation,
            result: Ok(playlist_page(&["a", "b"], 0, 3)),
        });
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation,
            result: Err(crate::api::ApiError::RateLimited),
        });
        assert_eq!(app.library.playlists_asked, None);
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 2,
            generation,
            result: Ok(playlist_page(&["c"], 2, 3)),
        });
        assert_eq!(listed_playlists(&app), playlist_ids(&["a", "b"]));
    }

    /// Signing out forgets the library, but not which load came last: a
    /// page the old account asked for does not become the next account's
    /// list.
    #[test]
    fn a_playlist_page_asked_for_before_sign_out_is_not_taken() {
        let mut app = headless_app();
        app.backend.set_offline(true);

        app.load_playlists();
        let old = app.library.playlists_generation;
        app.reset_data();
        app.load_playlists();
        assert_ne!(app.library.playlists_generation, old);
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: old,
            result: Ok(playlist_page(&["theirs"], 0, 1)),
        });
        assert!(app.library.playlists.is_loading());
    }

    /// The streaming session does not always name a playlist's owner. The
    /// account's own name stands in for its own lists, the library list's
    /// for the rest it holds, in whichever order the answers arrive, and
    /// a name Spotify gave stays.
    #[test]
    fn a_header_without_an_owner_name_takes_a_known_one() {
        use crate::api::models::{Owner, User};
        let owned_by = |id: &str, name: Option<&str>| Owner {
            id: Some(id.into()),
            display_name: name.map(str::to_string),
            uri: None,
        };
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.user = Some(User {
            id: "me".into(),
            display_name: Some("Mine".into()),
            ..User::default()
        });
        app.library.playlists = Loadable::Loaded(vec![Playlist {
            id: "pl2".into(),
            owner: owned_by("other", Some("Molly C.")),
            ..Playlist::default()
        }]);
        for id in ["pl1", "pl2", "pl3"] {
            app.playlist_pages.insert(
                id.into(),
                PlaylistPage {
                    generation: 1,
                    ..Default::default()
                },
            );
        }
        let header = |id: &str, owner: Owner| ApiResponse::Playlist {
            id: id.into(),
            generation: 1,
            result: Ok(Playlist {
                id: id.into(),
                owner,
                ..Playlist::default()
            }),
        };
        let shown = |app: &App, id: &str| {
            app.playlist_pages[id]
                .playlist
                .get()
                .unwrap()
                .owner_name()
                .to_string()
        };
        app.handle_api(header("pl1", owned_by("me", None)));
        app.handle_api(header("pl2", owned_by("other", None)));
        app.handle_api(header("pl3", owned_by("nobody", None)));
        assert_eq!(shown(&app, "pl1"), "Mine", "the account's own name");
        assert_eq!(shown(&app, "pl2"), "Molly C.", "the library list's");
        assert_eq!(
            shown(&app, "pl3"),
            "nobody",
            "the id until someone names them"
        );

        // The list can arrive after the header: the page takes the name
        // then, and a name Spotify already gave stays.
        app.handle_api(header("pl2", owned_by("other", Some("Molly"))));
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: app.library.playlists_generation,
            result: Ok(crate::api::models::Page {
                items: vec![
                    Playlist {
                        id: "pl2".into(),
                        owner: owned_by("other", Some("Molly C.")),
                        ..Playlist::default()
                    },
                    Playlist {
                        id: "pl3".into(),
                        owner: owned_by("nobody", Some("Nobody")),
                        ..Playlist::default()
                    },
                ],
                ..Default::default()
            }),
        });
        assert_eq!(shown(&app, "pl3"), "Nobody");
        assert_eq!(shown(&app, "pl2"), "Molly");
    }

    /// The streaming session carries no cover for a playlist without one
    /// of its own, where the Web API composes a mosaic; the library list
    /// holds that mosaic, in whichever order the answers arrive.
    #[test]
    fn a_header_without_a_cover_takes_the_library_lists() {
        let cover = |url: &str| {
            vec![Image {
                url: url.into(),
                width: Some(640),
                height: Some(640),
            }]
        };
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.library.playlists = Loadable::Loaded(vec![Playlist {
            id: "pl1".into(),
            images: cover("https://mosaic.scdn.co/640/pl1"),
            ..Playlist::default()
        }]);
        for id in ["pl1", "pl2", "pl3"] {
            app.playlist_pages.insert(
                id.into(),
                PlaylistPage {
                    generation: 1,
                    ..Default::default()
                },
            );
        }
        let header = |id: &str, images: Vec<Image>| ApiResponse::Playlist {
            id: id.into(),
            generation: 1,
            result: Ok(Playlist {
                id: id.into(),
                images,
                ..Playlist::default()
            }),
        };
        let shown = |app: &App, id: &str| {
            app.playlist_pages[id]
                .playlist
                .get()
                .unwrap()
                .images
                .iter()
                .map(|image| image.url.clone())
                .collect::<Vec<_>>()
        };
        app.handle_api(header("pl1", Vec::new()));
        app.handle_api(header("pl2", cover("https://i.scdn.co/image/own")));
        app.handle_api(header("pl3", Vec::new()));
        assert_eq!(
            shown(&app, "pl1"),
            ["https://mosaic.scdn.co/640/pl1"],
            "the library list's mosaic"
        );
        assert_eq!(
            shown(&app, "pl2"),
            ["https://i.scdn.co/image/own"],
            "a cover of its own stays"
        );
        assert!(shown(&app, "pl3").is_empty(), "nothing to take it from");

        // The list can arrive after the header: the page takes the cover
        // then, and one the header carried stays.
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: app.library.playlists_generation,
            result: Ok(crate::api::models::Page {
                items: vec![
                    Playlist {
                        id: "pl2".into(),
                        images: cover("https://mosaic.scdn.co/640/pl2"),
                        ..Playlist::default()
                    },
                    Playlist {
                        id: "pl3".into(),
                        images: cover("https://mosaic.scdn.co/640/pl3"),
                        ..Playlist::default()
                    },
                ],
                ..Default::default()
            }),
        });
        assert_eq!(shown(&app, "pl3"), ["https://mosaic.scdn.co/640/pl3"]);
        assert_eq!(shown(&app, "pl2"), ["https://i.scdn.co/image/own"]);
    }

    #[test]
    fn volume_conversions_round_trip() {
        assert_eq!(volume_to_percent(u16::MAX), 100);
        assert_eq!(volume_to_percent(0), 0);
        assert_eq!(volume_to_percent(percent_to_volume(70)), 70);
        assert_eq!(percent_to_volume(200), u16::MAX);
    }

    /// The session remembers fullscreen lyrics and the mode they left, and a
    /// session file from before the field existed still loads.
    #[test]
    fn the_session_remembers_the_mode_fullscreen_lyrics_left() {
        let mut app = test_app("lyrics-fullscreen-session");
        app.lyrics_fullscreen = Some(false);
        app.lyrics_restore_maximized = true;
        app.save_session();
        app.files.flush(crate::file_writer::FLUSH_TIMEOUT);
        let session = SessionState::load(&app.dirs.session_file());
        assert_eq!(
            session.lyrics_fullscreen_from,
            Some(crate::settings::WindowMode {
                fullscreen: false,
                maximized: true,
            })
        );

        app.lyrics_fullscreen = None;
        app.save_session();
        app.files.flush(crate::file_writer::FLUSH_TIMEOUT);
        let text = std::fs::read_to_string(app.dirs.session_file()).unwrap();
        assert!(!text.contains("lyrics_fullscreen_from"));
        assert_eq!(
            SessionState::load(&app.dirs.session_file()).lyrics_fullscreen_from,
            None
        );
        app.backend.shutdown();
        let _ = std::fs::remove_dir_all(app.dirs.config.parent().unwrap());
    }

    /// The song the last session ended on is shown, paused, at the position
    /// it stopped at, so a cold start does not look like an empty player.
    #[test]
    fn the_remembered_song_is_shown_paused_at_its_position() {
        use crate::api::models::{Album, ArtistRef, Track};
        let mut app = headless_app();
        app.resume_track = Some("spotify:track:abc".into());
        app.resume_position_ms = 19_566;
        assert!(
            app.now_playing().is_none(),
            "nothing to show until the song's details arrive"
        );
        app.track_cache.insert(
            "abc".into(),
            Track {
                id: Some("abc".into()),
                uri: "spotify:track:abc".into(),
                name: "Karma Police".into(),
                artists: vec![ArtistRef {
                    id: None,
                    name: "Radiohead".into(),
                    uri: None,
                }],
                album: Some(Album {
                    id: "ok".into(),
                    name: "OK Computer".into(),
                    ..Default::default()
                }),
                duration_ms: 264_000,
                ..Default::default()
            },
        );
        let now = app.now_playing().expect("the remembered song is shown");
        assert!(now.resuming);
        assert!(!now.playing, "it is shown paused, not played");
        assert_eq!(now.title, "Karma Police");
        assert_eq!(now.subtitle, "Radiohead");
        assert_eq!(now.album_name, "OK Computer");
        assert_eq!(now.duration_ms, 264_000);
        assert_eq!(
            now.position_ms, 19_566,
            "the bar sits where the listener left it, not at zero"
        );
    }

    /// Drawing the remembered song must not be mistaken for a song that
    /// started: that would rewind the very position it exists to show.
    #[test]
    fn showing_the_remembered_song_keeps_its_position() {
        use crate::api::models::Track;
        let mut app = headless_app();
        app.resume_track = Some("spotify:track:abc".into());
        app.resume_position_ms = 19_566;
        app.track_cache.insert(
            "abc".into(),
            Track {
                id: Some("abc".into()),
                uri: "spotify:track:abc".into(),
                duration_ms: 264_000,
                ..Default::default()
            },
        );
        app.on_now_playing_changed();
        assert_eq!(app.resume_position_ms, 19_566);
        app.save_session();
        assert_eq!(
            app.resume_position_ms, 19_566,
            "closing again must not lose the position"
        );
    }

    /// Dragging the bar before pressing play moves where play will land.
    #[test]
    fn seeking_the_remembered_song_moves_the_resume_point() {
        let mut app = headless_app();
        app.resume_track = Some("spotify:track:abc".into());
        app.resume_position_ms = 19_566;
        app.seek(90_000);
        assert_eq!(app.resume_position_ms, 90_000);
    }

    /// Play resumes the remembered track at its saved position.
    #[test]
    fn pressing_play_on_a_cold_start_does_not_restart_the_song() {
        let mut app = headless_app();
        app.resume_context = Some("spotify:playlist:pl1".into());
        app.resume_track = Some("spotify:track:abc".into());
        app.resume_position_ms = 19_566;
        app.toggle_play();
        let request = app
            .queued_play
            .as_ref()
            .expect("the resumed play is held for the engine");
        assert_eq!(
            request.context_uri.as_deref(),
            Some("spotify:playlist:pl1"),
            "it resumes inside the playlist it was left in"
        );
        assert_eq!(request.offset_uri.as_deref(), Some("spotify:track:abc"));
        assert_eq!(
            request.position_ms, 19_566,
            "the song resumes where it stopped, not at zero"
        );
    }

    /// A started episode resumes where Spotify left it; a finished or
    /// unstarted one starts over, and a place past the end stays inside it.
    #[test]
    fn an_episode_resumes_only_where_it_was_left_unfinished() {
        let episode = |fully_played: bool, resume_position_ms: u32| Episode {
            duration_ms: 2_400_000,
            resume_point: Some(ResumePoint {
                fully_played,
                resume_position_ms,
            }),
            ..Episode::default()
        };
        assert_eq!(episode(false, 900_000).resume_ms(), Some(900_000));
        assert_eq!(episode(true, 2_390_000).resume_ms(), None);
        assert_eq!(episode(false, 0).resume_ms(), None);
        assert_eq!(episode(false, 9_000_000).resume_ms(), Some(2_399_999));
        assert_eq!(Episode::default().resume_ms(), None);
    }

    /// A media key can arrive before startup has reported that the saved
    /// local player is connecting. Its intent must survive that race.
    #[test]
    fn pressing_play_while_startup_connects_is_held_for_the_player() {
        let mut app = headless_app();
        app.local_ready = false;
        app.local_playback = LocalPlayback::Unavailable;
        app.auth = AuthStatus::Starting;
        app.settings.playback_authorized = true;
        app.resume_track = Some("spotify:track:abc".into());

        app.toggle_play();

        assert!(app.queued_play.is_some());
        assert!(!app.show_devices, "startup is not a missing-device error");
    }

    /// With nothing playing anywhere, a first play sets up this computer
    /// and plays there once it is ready, instead of asking for a device.
    #[test]
    fn a_first_play_sets_up_this_computer_and_plays_there() {
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.local_ready = false;
        app.local_playback = LocalPlayback::Unavailable;
        app.settings.playback_authorized = false;
        app.user = Some(User {
            product: Some("premium".into()),
            ..Default::default()
        });

        app.play_request(PlayRequest::context("spotify:playlist:pl1"), false);

        assert!(!app.settings.playback_authorized);
        assert!(app.queued_play.is_some());
        assert!(!app.show_devices);

        app.local.connected = true;
        app.handle_playback(LocalPlayback::Ready {
            device_id: "local".into(),
        });
        assert!(app.queued_play.is_none(), "the ready player took it");
        assert!(app.settings.playback_authorized);
        assert_eq!(app.target(), Target::Local);
    }

    fn app_with_a_queued_first_play() -> App {
        let mut app = headless_app();
        app.backend.set_offline(true);
        app.local_ready = false;
        app.local_playback = LocalPlayback::Unavailable;
        app.user = Some(User {
            product: Some("premium".into()),
            ..Default::default()
        });
        app.play_request(PlayRequest::context("spotify:playlist:pl1"), false);
        app.handle_playback(LocalPlayback::Authorizing);
        assert!(app.queued_play.is_some());
        app
    }

    #[test]
    fn cancelling_the_playback_grant_drops_the_queued_play() {
        let mut app = app_with_a_queued_first_play();
        app.settings.playback_authorized = true;

        app.handle_playback(LocalPlayback::Unavailable);

        assert!(app.queued_play.is_none());
        assert!(app.queue_start_pending.is_none());
        assert!(app.intent_track.is_none());
        assert!(!app.any_play_pending());
        assert!(!app.settings.playback_authorized);
    }

    #[test]
    fn signing_out_drops_the_queued_play() {
        let mut app = app_with_a_queued_first_play();

        app.handle_auth(AuthStatus::SignedOut);

        assert!(app.queued_play.is_none());
        assert!(app.intent_track.is_none());
    }

    #[test]
    fn a_refused_grant_drops_the_queued_play() {
        let mut app = app_with_a_queued_first_play();

        app.handle_auth(AuthStatus::Refused(crate::backend::SignInProblem::Expired));

        assert!(app.queued_play.is_none());
    }

    #[test]
    fn pause_drops_a_queued_play_instead_of_replaying_it() {
        let mut app = app_with_a_queued_first_play();

        app.toggle_play();

        assert!(app.queued_play.is_none());
        assert!(app.intent_track.is_none());
        assert!(!app.any_play_pending());
    }

    /// A pending play and an optimistic hold wake the engine when they run
    /// out, not on a fast tick.
    #[test]
    fn holds_wake_the_engine_when_they_run_out() {
        let mut app = headless_app();
        app.auth = AuthStatus::SignedOut;
        assert_eq!(app.next_engine_wake(None), None);

        app.set_play_pending(vec!["spotify:track:a".into()]);
        let wake = app.next_engine_wake(None).expect("a pending play ends");
        assert!(wake > Duration::from_secs(7) && wake <= Duration::from_secs(8));

        app.optimistic_playing = Some((true, Instant::now()));
        let wake = app.next_engine_wake(None).expect("the hold ends");
        assert!(wake > PLAYBACK_HOLD - Duration::from_secs(1) && wake <= PLAYBACK_HOLD);

        app.pending_local_position = Some((1_000, Instant::now()));
        let wake = app.next_engine_wake(None).expect("the seek hold ends");
        assert!(wake <= OPTIMISTIC_HOLD);
        app.backend.shutdown();
    }

    /// A free account cannot play here, so it is still asked for a device.
    #[test]
    fn a_first_play_on_a_free_account_asks_for_a_device() {
        let mut app = headless_app();
        app.local_ready = false;
        app.local_playback = LocalPlayback::Unavailable;
        app.user = Some(User {
            product: Some("free".into()),
            ..Default::default()
        });

        app.play_request(PlayRequest::context("spotify:playlist:pl1"), false);

        assert!(!app.settings.playback_authorized);
        assert!(app.queued_play.is_none());
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.message.starts_with("Choose a device"))
        );
        assert!(
            !app.show_devices,
            "the app's picker asks for devices itself"
        );
    }

    /// A restored repeat mode drives local playback until the engine has a
    /// song of its own.
    #[test]
    fn repeat_survives_a_restart_until_the_engine_has_a_song() {
        let mut app = headless_app();
        app.repeat_wanted = RepeatMode::from_api(RepeatMode::Track.api_name());
        assert_eq!(
            app.local_repeat(),
            RepeatMode::Track,
            "the first load after a restart repeats the song"
        );
        app.local.track = Some(crate::player::LocalTrack::default());
        app.local.repeat = RepeatMode::Off;
        assert_eq!(
            app.local_repeat(),
            RepeatMode::Off,
            "once playing, the engine's mode is the one saved"
        );
    }

    #[test]
    fn repeat_with_no_device_is_kept_for_the_first_load() {
        let mut app = headless_app();
        app.local_ready = false;
        app.remote = None;
        app.repeat_wanted = RepeatMode::Track;

        app.set_repeat(RepeatMode::Context);

        assert_eq!(app.local_repeat(), RepeatMode::Context);
        assert!(app.repeat_hold.is_none(), "nothing went to a device");
    }

    /// The saved queue is restored only with its remembered track.
    #[test]
    fn the_saved_queue_follows_the_resumed_song_only() {
        let mut app = headless_app();
        app.resume_track = Some("spotify:track:abc".into());
        app.resume_queue = vec!["spotify:track:q1".into(), "spotify:track:q2".into()];
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:other".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.on_now_playing_changed();
        assert!(
            app.resume_queue.is_empty(),
            "a fresh start lets the saved queue go"
        );
        assert!(app.session_dirty);
    }

    /// A selected list row loads without shuffle, then enables shuffle.
    #[test]
    fn a_chosen_row_in_a_list_never_loads_shuffled() {
        let request = PlayRequest::tracks(vec!["spotify:track:a".into(), "spotify:track:b".into()])
            .starting_at_index(1);
        let load = local_load(&request, true);
        assert_eq!(load.shuffle, None);
        assert_eq!(load.offset_index, Some(1));
        // Without a chosen row the list may shuffle from the start.
        let request = PlayRequest::tracks(vec!["spotify:track:a".into(), "spotify:track:b".into()]);
        assert_eq!(local_load(&request, true).shuffle, Some(true));
        // A context play keeps its shuffled load; the offset was already
        // picked to match.
        let request = PlayRequest::context("spotify:playlist:x").starting_at_uri("spotify:track:a");
        assert_eq!(local_load(&request, true).shuffle, Some(true));
    }

    fn queued_song(uri: &str) -> crate::api::models::PlayableItem {
        crate::api::models::PlayableItem::Track(crate::api::models::Track {
            uri: uri.into(),
            ..Default::default()
        })
    }

    fn loaded_queue(current: &str, next: &[&str]) -> Loadable<Queue> {
        Loadable::Loaded(Queue {
            currently_playing: Some(queued_song(current)),
            queue: next.iter().map(|uri| queued_song(uri)).collect(),
        })
    }

    fn queue_uris(app: &App) -> (Option<String>, Vec<String>) {
        let queue = app.queue.get().expect("the queue stays loaded");
        (
            queue
                .currently_playing
                .as_ref()
                .map(|item| item.uri().to_string()),
            queue
                .queue
                .iter()
                .map(|item| item.uri().to_string())
                .collect(),
        )
    }

    /// Remote state is polled rather than pushed. Ignore a poll already in
    /// flight and one lagging answer, then accept a second report.
    #[test]
    fn remote_track_intent_waits_for_a_fresh_confirming_poll() {
        let mut app = headless_app();
        app.local_ready = false;
        app.selected_device = Some("phone".into());
        app.remote_poll_seq = 7;
        app.expect_track("spotify:track:wanted".into(), 0);

        app.reconcile_remote_track_intent(7, Some("spotify:track:old"));
        assert!(
            app.intent_track.is_some(),
            "the in-flight poll predates the command"
        );

        app.reconcile_remote_track_intent(8, Some("spotify:track:old"));
        assert!(
            app.intent_track.is_some(),
            "one stale Spotify answer is retried"
        );

        app.reconcile_remote_track_intent(9, Some("spotify:track:actual"));
        assert!(
            app.intent_track.is_none(),
            "the second fresh report settles playback"
        );
    }

    /// A track is removed from Next up as soon as it starts.
    #[test]
    fn a_song_starting_consumes_its_queue_row() {
        let mut app = headless_app();
        app.queue = loaded_queue("spotify:track:a", &["spotify:track:b", "spotify:track:c"]);
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:b".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.on_now_playing_changed();
        let (current, next) = queue_uris(&app);
        assert_eq!(current.as_deref(), Some("spotify:track:b"));
        assert_eq!(next, vec!["spotify:track:c"]);
    }

    /// Spotify pads a short queue by going round it again, even with Repeat
    /// off, so a song started on its own, as a Recent row starts it, showed
    /// ten times under Next up though it plays once. Only the padding goes:
    /// a playlist's own later copy of the song, a copy queued by hand, and
    /// a real loop with Repeat on stay.
    #[test]
    fn spotify_padding_a_short_queue_is_not_next_up() {
        fn answer(app: &mut App, next: &[&str]) -> Vec<String> {
            app.handle_api(ApiResponse::Queue {
                seq: app.queue_seq,
                result: Ok(Queue {
                    currently_playing: Some(queued_song("spotify:track:a")),
                    queue: next.iter().map(|uri| queued_song(uri)).collect(),
                }),
            });
            queue_uris(app).1
        }
        let mut app = headless_app();
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:a".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        let (a, b, c) = ("spotify:track:a", "spotify:track:b", "spotify:track:c");

        assert!(
            answer(&mut app, &[a; 10]).is_empty(),
            "a song started on its own plays once"
        );
        assert_eq!(
            answer(&mut app, &[b, a, b, a, b, a, b, a, b, a]),
            vec![b],
            "a list of two plays through once"
        );
        assert_eq!(
            answer(&mut app, &[b, a, c]),
            vec![b, a, c],
            "a playlist holding the song again"
        );
        assert_eq!(
            answer(&mut app, &[b, c, a]),
            vec![b, c, a],
            "one late copy may be the playlist's own"
        );

        app.manual_queue = vec![a.into()];
        assert_eq!(
            answer(&mut app, &[a; 10]),
            vec![a],
            "a copy queued by hand plays"
        );

        app.manual_queue.clear();
        app.queue_recheck_at = None;
        app.set_repeat(RepeatMode::Context);
        assert!(app.queue_recheck_at.is_some(), "Next up is fetched again");
        assert_eq!(
            answer(&mut app, &[a; 10]),
            vec![a; 10],
            "with Repeat on it does play again"
        );
    }

    /// A queue saved with Spotify's padding, as earlier versions saved it,
    /// comes back without it.
    #[test]
    fn a_saved_queue_comes_back_without_spotifys_padding() {
        let root = std::env::temp_dir().join(format!(
            "spotifast-queue-padding-test-{}",
            std::process::id()
        ));
        let dirs = || AppDirs {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        };
        std::fs::create_dir_all(&dirs().state).unwrap();
        SessionState {
            last_track: Some("spotify:track:a".into()),
            last_queue_rows: vec![queued_song("spotify:track:a"); 10],
            repeat: "off".into(),
            ..SessionState::default()
        }
        .save(&dirs().session_file());
        let mut app = App::new(
            &Waker::default(),
            dirs(),
            Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
            },
        );
        assert!(queue_uris(&app).1.is_empty());
        app.backend.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pending addition is not restored after its track starts.
    #[test]
    fn a_played_pending_add_is_not_put_back() {
        let mut app = headless_app();
        app.pending_queue_adds = vec![PendingQueueAdd {
            item: queued_song("spotify:track:b"),
            at: Instant::now(),
            manual_index: 0,
            write: None,
        }];
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:b".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.queue = loaded_queue("spotify:track:b", &["spotify:track:c"]);
        app.reconcile_pending_queue();
        let (_, next) = queue_uris(&app);
        assert_eq!(next, vec!["spotify:track:c"], "no resurrected row on top");
        assert!(
            app.pending_queue_adds.is_empty(),
            "the add has been consumed"
        );
    }

    fn album_queue_track(id: &str) -> Track {
        Track {
            uri: format!("spotify:track:{id}"),
            name: id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn queue_album_fetches_all_pages_before_appending_and_ignores_old_pages() {
        let mut app = test_app("album-queue-pages");
        app.queue = loaded_queue("spotify:track:current", &["spotify:track:context"]);
        app.add_to_queue("spotify:album:album".into(), "Album".into());
        let request = app.album_queue_serial;
        let requests = app.backend.take_queue_requests();
        assert!(matches!(
            &requests[..],
            [ApiRequest::AlbumQueueTracks { offset: 0, .. }]
        ));
        let page = crate::api::models::Page {
            items: vec![album_queue_track("one"), album_queue_track("two")],
            offset: 0,
            limit: 2,
            total: 3,
            next: Some("next".into()),
        };
        app.handle_api(ApiResponse::AlbumQueueTracks {
            request,
            offset: 0,
            result: Ok(page.clone()),
        });
        assert_eq!(queue_uris(&app).1, ["spotify:track:context"]);
        let requests = app.backend.take_queue_requests();
        assert!(matches!(
            &requests[..],
            [ApiRequest::AlbumQueueTracks { offset: 2, .. }]
        ));
        app.handle_api(ApiResponse::AlbumQueueTracks {
            request,
            offset: 0,
            result: Ok(page),
        });
        assert!(app.backend.take_queue_requests().is_empty());
        app.handle_api(ApiResponse::AlbumQueueTracks {
            request,
            offset: 2,
            result: Ok(crate::api::models::Page {
                items: vec![album_queue_track("one")],
                offset: 2,
                limit: 2,
                total: 3,
                next: None,
            }),
        });
        assert_eq!(
            queue_uris(&app).1,
            [
                "spotify:track:one",
                "spotify:track:two",
                "spotify:track:one",
                "spotify:track:context"
            ]
        );
        assert!(app.pending_album_queues.is_empty());
        app.backend.shutdown();
    }

    #[test]
    fn remote_album_queue_keeps_its_rows_while_rate_limited_writes_are_pending() {
        let mut app = test_app("album-queue-stale");
        app.queue = loaded_queue("spotify:track:current", &["spotify:track:context"]);
        let old = app.queue.get().unwrap().clone();
        app.queue_album_tracks(
            vec![
                album_queue_track("a"),
                album_queue_track("b"),
                album_queue_track("a"),
            ],
            "Album".into(),
        );
        let request = app.album_queue_serial;
        for addition in &mut app.pending_queue_adds {
            addition.at = Instant::now() - Duration::from_secs(60);
        }
        app.expire_pending_queue_adds();
        assert_eq!(app.pending_queue_adds.len(), 3);
        for _ in 0..u16::from(u8::MAX) + 2 {
            app.handle_api(ApiResponse::Queue {
                seq: app.queue_seq,
                result: Ok(old.clone()),
            });
            assert_eq!(
                queue_uris(&app).1,
                [
                    "spotify:track:a",
                    "spotify:track:b",
                    "spotify:track:a",
                    "spotify:track:context"
                ]
            );
        }
        app.handle_api(ApiResponse::Queue {
            seq: app.queue_seq,
            result: Err(crate::api::ApiError::RateLimited),
        });
        assert_eq!(
            queue_uris(&app).1,
            [
                "spotify:track:a",
                "spotify:track:b",
                "spotify:track:a",
                "spotify:track:context"
            ]
        );
        app.handle_api(ApiResponse::QueueBatchAdded {
            request,
            added: 3,
            result: Ok(()),
        });
        assert!(app.pending_queue_batches.is_empty());
        assert!(
            app.pending_queue_adds
                .iter()
                .all(|addition| addition.at.elapsed() < Duration::from_secs(1))
        );
        app.backend.shutdown();
    }

    #[test]
    fn rejected_queue_copies_leave_accepted_later_and_context_copies_intact() {
        let mut app = test_app("queue-partial-failure");
        app.queue = loaded_queue("spotify:track:current", &["spotify:track:a"]);
        app.queue_album_tracks(
            vec![
                album_queue_track("a"),
                album_queue_track("a"),
                album_queue_track("b"),
            ],
            "Album".into(),
        );
        let album = app.album_queue_serial;
        // Bypass click debounce: this is a separately selected occurrence.
        app.queue_one("spotify:track:a".into(), "Later A".into(), false);
        let later = app.album_queue_serial;
        app.handle_api(ApiResponse::QueueBatchAdded {
            request: album,
            added: 1,
            result: Err(crate::api::ApiError::Status {
                status: 403,
                message: "Rejected".into(),
            }),
        });
        assert_eq!(app.manual_queue, ["spotify:track:a", "spotify:track:a"]);
        assert_eq!(
            queue_uris(&app).1,
            ["spotify:track:a", "spotify:track:a", "spotify:track:a"]
        );
        assert_eq!(app.pending_queue_adds.len(), 2);
        assert_eq!(app.pending_queue_adds[1].manual_index, 1);
        assert_eq!(app.pending_queue_adds[1].write, Some((later, 0)));
        app.handle_api(ApiResponse::QueueBatchAdded {
            request: later,
            added: 0,
            result: Err(crate::api::ApiError::QuotaExhausted),
        });
        assert_eq!(app.manual_queue, ["spotify:track:a"]);
        assert_eq!(queue_uris(&app).1, ["spotify:track:a", "spotify:track:a"]);
        app.backend.shutdown();
    }

    #[test]
    fn pending_single_keeps_its_name_and_duration_during_and_after_a_cooldown() {
        let mut app = test_app("queue-single-cooldown");
        app.queue = loaded_queue("spotify:track:current", &["spotify:track:context"]);
        let old = app.queue.get().unwrap().clone();
        let track = Track {
            duration_ms: 215_000,
            ..album_queue_track("single")
        };
        app.album_pages.insert(
            "album".into(),
            AlbumPage {
                tracks: PagedList {
                    items: vec![track.clone()],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.add_to_queue(track.uri.clone(), track.name.clone());
        let request = app.album_queue_serial;
        app.album_pages.clear();
        app.track_cache.clear();
        app.pending_queue_adds[0].at = Instant::now() - Duration::from_secs(90);
        app.handle_api(ApiResponse::Queue {
            seq: app.queue_seq,
            result: Ok(old.clone()),
        });
        let row = &app.queue.get().unwrap().queue[0];
        assert_eq!(row.name(), track.name);
        assert_eq!(row.duration_ms(), track.duration_ms);
        app.handle_api(ApiResponse::QueueBatchAdded {
            request,
            added: 1,
            result: Ok(()),
        });
        app.handle_api(ApiResponse::Queue {
            seq: app.queue_seq,
            result: Ok(old),
        });
        let row = &app.queue.get().unwrap().queue[0];
        assert_eq!(row.uri(), track.uri);
        assert_eq!(row.name(), track.name);
        assert_eq!(row.duration_ms(), track.duration_ms);
        app.backend.shutdown();
    }

    #[test]
    fn pending_album_queue_cannot_survive_clear_sign_out_or_device_change() {
        for cancel in ["clear", "sign-out", "device", "failure"] {
            let mut app = test_app(&format!("album-queue-cancel-{cancel}"));
            app.local_ready = true;
            app.queue = loaded_queue("spotify:track:current", &["spotify:track:context"]);
            app.add_to_queue("spotify:album:album".into(), "Album".into());
            let request = app.album_queue_serial;
            app.backend.take_queue_requests();
            match cancel {
                "clear" => app.clear_queue(),
                "sign-out" => app.reset_data(),
                "device" => app.selected_device = Some("phone".into()),
                "failure" => app.handle_api(ApiResponse::AlbumQueueTracks {
                    request,
                    offset: 0,
                    result: Err(crate::api::ApiError::RateLimited),
                }),
                _ => unreachable!(),
            }
            app.handle_api(ApiResponse::AlbumQueueTracks {
                request,
                offset: 0,
                result: Ok(crate::api::models::Page {
                    items: vec![album_queue_track("late")],
                    ..Default::default()
                }),
            });
            assert!(app.manual_queue.is_empty());
            assert!(app.pending_album_queues.is_empty());
            assert!(app.backend.take_queue_requests().is_empty());
            assert!(app.backend.take_queued_tracks().is_empty());
            app.backend.shutdown();
        }
    }

    /// A response superseded by a newer request is ignored.
    #[test]
    fn an_overtaken_queue_answer_is_dropped_unread() {
        let mut app = headless_app();
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:a".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.queue = loaded_queue("spotify:track:a", &["spotify:track:b"]);
        app.queue_seq = 2;
        let old_story = Queue {
            currently_playing: Some(queued_song("spotify:track:a")),
            queue: Vec::new(),
        };
        app.handle_api(ApiResponse::Queue {
            seq: 1,
            result: Ok(old_story),
        });
        let (_, next) = queue_uris(&app);
        assert_eq!(
            next,
            vec!["spotify:track:b"],
            "the overtaken answer changed nothing"
        );
        let current_story = Queue {
            currently_playing: Some(queued_song("spotify:track:a")),
            queue: vec![
                queued_song("spotify:track:b"),
                queued_song("spotify:track:c"),
            ],
        };
        app.handle_api(ApiResponse::Queue {
            seq: 2,
            result: Ok(current_story),
        });
        let (_, next) = queue_uris(&app);
        assert_eq!(next, vec!["spotify:track:b", "spotify:track:c"]);
    }

    /// Rule: a row you queued is put back until Spotify confirms it, in
    /// its place after the queued section, not on top of it.
    #[test]
    fn a_missing_queued_row_returns_to_its_place() {
        let mut app = headless_app();
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:a".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.manual_queue = vec!["spotify:track:b".into(), "spotify:track:c".into()];
        app.pending_queue_adds = vec![PendingQueueAdd {
            item: queued_song("spotify:track:c"),
            at: Instant::now(),
            manual_index: 1,
            write: None,
        }];
        // Spotify's answer knows b already but not c yet.
        app.queue = loaded_queue(
            "spotify:track:a",
            &["spotify:track:b", "spotify:track:ctx1"],
        );
        app.reconcile_pending_queue();
        let (_, next) = queue_uris(&app);
        assert_eq!(
            next,
            vec!["spotify:track:b", "spotify:track:c", "spotify:track:ctx1"],
            "the missing row comes back after the queued section"
        );
    }

    /// The view splits the queue where the user's own songs end; rows
    /// queued elsewhere or belonging to the context stay below the line.
    #[test]
    fn the_queued_section_covers_only_the_users_own_rows() {
        let mut app = headless_app();
        app.manual_queue = vec!["spotify:track:b".into(), "spotify:track:c".into()];
        app.queue = loaded_queue(
            "spotify:track:a",
            &[
                "spotify:track:b",
                "spotify:track:c",
                "spotify:track:ctx1",
                "spotify:track:c",
            ],
        );
        assert_eq!(
            app.queued_rows_len(),
            2,
            "the context's own copy of c does not count as queued"
        );
        app.manual_queue.clear();
        assert_eq!(app.queued_rows_len(), 0);
    }

    /// Changing shuffle rechecks the queue to reflect the new playback order.
    #[test]
    fn changing_shuffle_schedules_a_queue_recheck() {
        let mut app = headless_app();
        app.queue = loaded_queue("spotify:track:a", &["spotify:track:b"]);
        assert!(app.queue_recheck_at.is_none());
        app.set_shuffle(true);
        assert!(
            app.queue_recheck_at.is_some(),
            "toggling shuffle asks Spotify for the reordered queue"
        );
    }

    /// Toggling shuffle for local playback rechecks the queue, reveals the
    /// new playback order without losing hand-queued songs, and ignores an
    /// old response from before the toggle.
    #[test]
    fn shuffling_local_playback_updates_queue_and_keeps_user_songs() {
        let mut app = headless_app();
        app.auth = AuthStatus::Connected {
            username: "alice".into(),
        };
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:playing".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.manual_queue = vec!["spotify:track:manual1".into()];
        app.queue = loaded_queue(
            "spotify:track:playing",
            &[
                "spotify:track:manual1",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
                "spotify:track:ctx3",
            ],
        );
        assert_eq!(app.queued_rows_len(), 1);

        // Toggle shuffle on local player.
        app.set_shuffle(true);
        assert!(
            app.queue_recheck_at.is_some(),
            "toggling shuffle schedules a queue recheck"
        );

        // Start the queue refresh with a new sequence number.
        app.refresh_queue(true);
        let seq = app.queue_seq;

        // A response from before the shuffle command is dropped unread.
        let old_response = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:manual1"),
                queued_song("spotify:track:ctx1"),
                queued_song("spotify:track:ctx2"),
                queued_song("spotify:track:ctx3"),
            ],
        };
        app.handle_api(ApiResponse::Queue {
            seq: seq - 1,
            result: Ok(old_response),
        });
        let (_, next) = queue_uris(&app);
        assert_eq!(
            next,
            vec![
                "spotify:track:manual1",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
                "spotify:track:ctx3",
            ],
            "the superseded pre-shuffle response is dropped unread"
        );

        // When the fresh response arrives, the context rows reflect the new shuffle
        // order while manually queued rows stay on top.
        let shuffled_response = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:manual1"),
                queued_song("spotify:track:ctx3"),
                queued_song("spotify:track:ctx1"),
                queued_song("spotify:track:ctx2"),
            ],
        };
        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(shuffled_response),
        });

        let (current, next) = queue_uris(&app);
        assert_eq!(current.as_deref(), Some("spotify:track:playing"));
        assert_eq!(
            next,
            vec![
                "spotify:track:manual1",
                "spotify:track:ctx3",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
            ],
            "the user's song stays on top and the shuffled context order is shown"
        );
        assert_eq!(
            app.queued_rows_len(),
            1,
            "the user's queued section is preserved"
        );
    }

    /// Toggling shuffle for remote playback rechecks the queue on confirmation,
    /// adopts the new shuffle order while keeping hand-queued songs, and drops
    /// an overtaken response.
    #[test]
    fn shuffling_remote_playback_updates_queue_and_keeps_user_songs() {
        let mut app = headless_app();
        app.auth = AuthStatus::Connected {
            username: "alice".into(),
        };
        app.local_ready = false;
        app.selected_device = Some("speaker".into());
        app.remote = Some(RemoteSnapshot {
            state: PlaybackState {
                device: Some(crate::api::models::Device {
                    id: Some("speaker".into()),
                    name: "Speaker".into(),
                    is_active: true,
                    ..Default::default()
                }),
                item: Some(crate::api::models::PlayableItem::Track(
                    crate::api::models::Track {
                        uri: "spotify:track:playing".into(),
                        ..Default::default()
                    },
                )),
                is_playing: true,
                shuffle_state: false,
                ..Default::default()
            },
            received_at: Instant::now(),
        });
        app.manual_queue = vec!["spotify:track:manual1".into()];
        app.queue = loaded_queue(
            "spotify:track:playing",
            &[
                "spotify:track:manual1",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
            ],
        );
        assert_eq!(app.queued_rows_len(), 1);

        // Turn on shuffle on the remote target.
        app.set_shuffle(true);
        assert!(app.queue_recheck_at.is_some());

        // Spotify confirms the remote shuffle action.
        app.handle_api(ApiResponse::Remote {
            action: RemoteAction::Shuffle,
            result: Ok(()),
        });
        assert!(
            app.queue_recheck_at.is_some(),
            "confirmed remote shuffle rechecks the queue"
        );

        // Fetch the queue with a new sequence number.
        app.refresh_queue(true);
        let seq = app.queue_seq;

        // An older response is ignored.
        let stale = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:manual1"),
                queued_song("spotify:track:ctx1"),
                queued_song("spotify:track:ctx2"),
            ],
        };
        app.handle_api(ApiResponse::Queue {
            seq: seq - 1,
            result: Ok(stale),
        });
        assert_eq!(
            queue_uris(&app).1,
            vec![
                "spotify:track:manual1",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
            ]
        );

        // The confirming response shows the new shuffle order.
        let shuffled = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:manual1"),
                queued_song("spotify:track:ctx2"),
                queued_song("spotify:track:ctx1"),
            ],
        };
        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(shuffled),
        });

        let (current, next) = queue_uris(&app);
        assert_eq!(current.as_deref(), Some("spotify:track:playing"));
        assert_eq!(
            next,
            vec![
                "spotify:track:manual1",
                "spotify:track:ctx2",
                "spotify:track:ctx1",
            ],
            "remote shuffle preserves hand-queued songs and updates context rows"
        );
        assert_eq!(app.queued_rows_len(), 1);
    }

    /// A stale queue answer whose current track does not match the active
    /// local player is rejected rather than accepted after a shuffle toggle.
    #[test]
    fn stale_queue_after_shuffle_is_retried_and_not_accepted() {
        let mut app = headless_app();
        app.auth = AuthStatus::Connected {
            username: "alice".into(),
        };
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:playing".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.queue = loaded_queue(
            "spotify:track:playing",
            &["spotify:track:ctx1", "spotify:track:ctx2"],
        );

        app.set_shuffle(true);
        app.refresh_queue(true);
        let seq = app.queue_seq;

        // A stale response that reports an old playing track is rejected.
        let stale_track_response = Queue {
            currently_playing: Some(queued_song("spotify:track:old_song")),
            queue: vec![
                queued_song("spotify:track:ctx2"),
                queued_song("spotify:track:ctx1"),
            ],
        };
        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(stale_track_response),
        });

        assert_eq!(
            queue_uris(&app).1,
            vec!["spotify:track:ctx1", "spotify:track:ctx2"],
            "the queue remains uncorrupted by the stale response"
        );
        assert_eq!(app.queue_stale_retries, 1);
        assert!(app.queue_recheck_at.is_some());
    }

    /// A lagging queue answer after a shuffle toggle with the same current song
    /// and sequence is rejected and retried until the new order arrives.
    #[test]
    fn lagging_queue_after_shuffle_is_retried_until_new_order_arrives() {
        let mut app = headless_app();
        app.auth = AuthStatus::Connected {
            username: "alice".into(),
        };
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:playing".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.manual_queue = vec!["spotify:track:manual1".into()];
        app.queue = loaded_queue(
            "spotify:track:playing",
            &[
                "spotify:track:manual1",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
                "spotify:track:ctx3",
            ],
        );

        app.set_shuffle(true);
        app.refresh_queue(true);
        let seq = app.queue_seq;

        // A lagging response arrives with the same currently playing track and
        // the current sequence, but the context rows are still in the old order.
        let lagging_response = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:manual1"),
                queued_song("spotify:track:ctx1"),
                queued_song("spotify:track:ctx2"),
                queued_song("spotify:track:ctx3"),
            ],
        };

        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(lagging_response.clone()),
        });

        // The stale pre-shuffle order is rejected, preserving the existing queue,
        // incrementing the retry counter, and scheduling a prompt recheck.
        assert_eq!(app.queue_stale_retries, 1);
        assert!(app.queue_recheck_at.is_some());
        assert_eq!(
            queue_uris(&app).1,
            vec![
                "spotify:track:manual1",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
                "spotify:track:ctx3",
            ]
        );

        // A second lagging response also retries.
        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(lagging_response),
        });
        assert_eq!(app.queue_stale_retries, 2);
        assert!(app.queue_recheck_at.is_some());

        // Once the reordered response arrives, it is accepted and pending state clears.
        let new_order_response = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:manual1"),
                queued_song("spotify:track:ctx3"),
                queued_song("spotify:track:ctx1"),
                queued_song("spotify:track:ctx2"),
            ],
        };

        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(new_order_response),
        });

        assert_eq!(app.queue_stale_retries, 0);
        assert!(app.queue_shuffle_pending.is_none());
        assert_eq!(
            queue_uris(&app).1,
            vec![
                "spotify:track:manual1",
                "spotify:track:ctx3",
                "spotify:track:ctx1",
                "spotify:track:ctx2",
            ]
        );
    }

    /// If Spotify returns an unchanged queue order after shuffle, SwiftSpot
    /// retries up to the limit and then accepts the result as a bounded fallback.
    #[test]
    fn unchanged_shuffle_result_has_bounded_fallback() {
        let mut app = headless_app();
        app.auth = AuthStatus::Connected {
            username: "alice".into(),
        };
        app.local.track = Some(crate::player::LocalTrack {
            uri: "spotify:track:playing".into(),
            ..Default::default()
        });
        app.local.playback = Playback::Playing;
        app.queue = loaded_queue(
            "spotify:track:playing",
            &["spotify:track:ctx1", "spotify:track:ctx2"],
        );

        app.set_shuffle(true);
        app.refresh_queue(true);
        let seq = app.queue_seq;

        let unchanged_response = Queue {
            currently_playing: Some(queued_song("spotify:track:playing")),
            queue: vec![
                queued_song("spotify:track:ctx1"),
                queued_song("spotify:track:ctx2"),
            ],
        };

        // Responses with the pre-shuffle order are rejected as stale and scheduled for retry.
        for expected_retries in 1..=QUEUE_STALE_RETRIES {
            app.handle_api(ApiResponse::Queue {
                seq,
                result: Ok(unchanged_response.clone()),
            });
            assert_eq!(app.queue_stale_retries, expected_retries);
            assert!(app.queue_recheck_at.is_some());
        }

        // The next response exceeds the retry limit, so SwiftSpot accepts it.
        app.handle_api(ApiResponse::Queue {
            seq,
            result: Ok(unchanged_response),
        });
        assert_eq!(app.queue_stale_retries, 0);
        assert!(app.queue_shuffle_pending.is_none());
        assert_eq!(
            queue_uris(&app).1,
            vec!["spotify:track:ctx1", "spotify:track:ctx2"]
        );
    }

    fn search_page(id: &str) -> crate::api::models::Page<Playlist> {
        crate::api::models::Page {
            items: vec![Playlist {
                id: id.into(),
                ..Playlist::default()
            }],
            ..Default::default()
        }
    }

    fn catalogue_answer() -> crate::api::models::SearchResults {
        crate::api::models::SearchResults {
            tracks: Some(crate::api::models::Page {
                items: vec![Track::default()],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn searching(app: &mut App, query: &str) -> u64 {
        app.search.query = query.into();
        app.run_search(query.into());
        let serial = app.search.serial;
        app.handle_api(ApiResponse::SearchStarted {
            query: query.into(),
            serial,
            split: true,
        });
        serial
    }

    #[test]
    fn each_half_of_a_search_shows_as_it_arrives() {
        for playlists_first in [false, true] {
            let mut app = test_app("search-halves");
            let serial = searching(&mut app, "radiohead");
            let catalogue = ApiResponse::Search {
                query: "radiohead".into(),
                serial,
                result: Ok(catalogue_answer()),
            };
            let playlists = ApiResponse::SearchPlaylists {
                query: "radiohead".into(),
                serial,
                result: Ok(search_page("p")),
            };
            if playlists_first {
                app.handle_api(playlists);
                let shown = app
                    .search
                    .results
                    .get()
                    .expect("playlists appear before catalogue");
                assert_eq!(shown.playlists.as_ref().unwrap().items[0].id, "p");
                assert!(shown.tracks.is_none());
                assert!(app.search.catalogue_pending);
                app.handle_api(catalogue);
            } else {
                app.handle_api(catalogue);
                let shown = app.search.results.get().expect("results");
                assert_eq!(shown.tracks.as_ref().unwrap().items.len(), 1);
                assert!(shown.playlists.is_none());
                app.handle_api(playlists);
            }
            let shown = app.search.results.get().expect("results");
            assert_eq!(shown.tracks.as_ref().unwrap().items.len(), 1);
            assert_eq!(shown.playlists.as_ref().unwrap().items[0].id, "p");
        }
    }

    #[test]
    fn playlists_from_an_older_search_never_join_a_newer_one() {
        let mut app = test_app("search-stale-playlists");
        let stale = searching(&mut app, "radiohead");
        app.handle_api(ApiResponse::SearchPlaylists {
            query: "radiohead".into(),
            serial: stale,
            result: Ok(search_page("stale")),
        });
        let serial = searching(&mut app, "portishead");
        app.handle_api(ApiResponse::Search {
            query: "portishead".into(),
            serial,
            result: Ok(catalogue_answer()),
        });
        let shown = app.search.results.get().expect("results");
        assert!(shown.playlists.is_none());
    }

    #[test]
    fn failed_search_halves_never_mix_queries_or_discard_the_successful_half() {
        for playlists_fail in [false, true] {
            for failure_first in [false, true] {
                let mut app = test_app("search-half-failure");
                let old = searching(&mut app, "old");
                app.handle_api(ApiResponse::Search {
                    query: "old".into(),
                    serial: old,
                    result: Ok(catalogue_answer()),
                });
                let serial = searching(&mut app, "new");
                assert!(
                    app.search.results.get().is_none(),
                    "old songs cannot label a new query"
                );
                let failure = if playlists_fail {
                    ApiResponse::SearchPlaylists {
                        query: "new".into(),
                        serial,
                        result: Err(crate::api::ApiError::RateLimited),
                    }
                } else {
                    ApiResponse::Search {
                        query: "new".into(),
                        serial,
                        result: Err(crate::api::ApiError::RateLimited),
                    }
                };
                let success = if playlists_fail {
                    ApiResponse::Search {
                        query: "new".into(),
                        serial,
                        result: Ok(catalogue_answer()),
                    }
                } else {
                    ApiResponse::SearchPlaylists {
                        query: "new".into(),
                        serial,
                        result: Ok(search_page("new")),
                    }
                };
                if failure_first {
                    app.handle_api(failure);
                    assert!(matches!(app.search.results, Loadable::Loading));
                    app.handle_api(success);
                } else {
                    app.handle_api(success);
                    assert!(app.search.results.get().is_some());
                    app.handle_api(failure);
                }
                let shown = app.search.results.get().expect("successful half survives");
                assert_eq!(shown.tracks.is_some(), playlists_fail);
                assert_eq!(shown.playlists.is_some(), !playlists_fail);
                assert_eq!(app.search.results_serial, serial);
                assert!(app.search.error.is_some());
                assert!(!app.search.catalogue_pending && !app.search.playlists_pending);
                app.backend.shutdown();
            }
        }
    }

    #[test]
    fn both_search_failures_finish_loading_and_the_same_query_can_retry() {
        let mut app = test_app("search-both-fail");
        let serial = searching(&mut app, "new");
        app.handle_api(ApiResponse::Search {
            query: "new".into(),
            serial,
            result: Err(crate::api::ApiError::RateLimited),
        });
        app.handle_api(ApiResponse::SearchPlaylists {
            query: "new".into(),
            serial,
            result: Err(crate::api::ApiError::RateLimited),
        });
        assert!(matches!(app.search.results, Loadable::Failed(_)));
        assert!(!app.search.catalogue_pending && !app.search.playlists_pending);
        app.run_search("new".into());
        assert!(app.search.serial > serial);
        assert!(app.search.error.is_none());
        assert!(matches!(app.search.results, Loadable::Loading));
        app.backend.shutdown();
    }

    #[test]
    fn clearing_or_signing_out_rejects_both_late_search_halves() {
        for sign_out in [false, true] {
            let mut app = test_app("search-cancel");
            let serial = searching(&mut app, "old");
            if sign_out {
                app.reset_data();
            } else {
                app.run_search(String::new());
            }
            app.handle_api(ApiResponse::Search {
                query: "old".into(),
                serial,
                result: Ok(catalogue_answer()),
            });
            app.handle_api(ApiResponse::SearchPlaylists {
                query: "old".into(),
                serial,
                result: Ok(search_page("old")),
            });
            assert!(matches!(app.search.results, Loadable::NotLoaded));
            assert!(app.search.playlists.is_none());
            assert!(!app.search.catalogue_pending && !app.search.playlists_pending);
            app.backend.shutdown();
        }
    }

    fn test_cover() -> crate::playlist_cover::Cover {
        test_cover_color([220, 20, 40])
    }

    fn test_cover_color(color: [u8; 3]) -> crate::playlist_cover::Cover {
        let pixels = image::RgbImage::from_pixel(16, 16, image::Rgb(color));
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        crate::playlist_cover::prepare(bytes.get_ref()).unwrap()
    }

    #[test]
    fn a_failed_cover_upload_says_why_and_a_success_refreshes_library_art() {
        let mut app = test_app("cover-upload");
        let cover = test_cover();
        app.cover_uploads.insert("pl1".into(), 1);
        app.handle_api(ApiResponse::PlaylistCoverUploaded {
            id: "pl1".into(),
            request: 1,
            previous_urls: vec![],
            cover: cover.clone(),
            result: Err(crate::api::ApiError::Status {
                status: 403,
                message: "Forbidden".into(),
            }),
        });
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.message.contains("sign in again"))
        );
        assert!(!app.uploaded_covers.contains_key("pl1"));
        app.library.playlists = Loadable::Loaded(vec![Playlist {
            id: "pl1".into(),
            ..Default::default()
        }]);
        app.cover_uploads.insert("pl1".into(), 2);
        app.handle_api(ApiResponse::PlaylistCoverUploaded {
            id: "pl1".into(),
            request: 2,
            previous_urls: vec![],
            cover: cover.clone(),
            result: Ok(()),
        });
        assert_eq!(
            app.library.playlists.get().unwrap()[0].images[0].url,
            cover.uri
        );
        assert!(app.uploaded_covers.contains_key("pl1"));
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: app.library.playlists_generation,
            result: Ok(crate::api::models::Page {
                items: vec![Playlist {
                    id: "pl1".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        });
        assert_eq!(
            app.library.playlists.get().unwrap()[0].images[0].url,
            cover.uri,
            "lagging metadata must not undo the uploaded artwork"
        );
        app.backend.shutdown();
    }

    #[test]
    fn a_late_url_from_an_earlier_upload_cannot_replace_the_new_cover() {
        let mut app = test_app("cover-lagging-earlier-upload");
        app.backend.set_offline(true);
        app.library.playlists = Loadable::Loaded(vec![Playlist {
            id: "pl1".into(),
            images: vec![Image {
                url: "original".into(),
                ..Default::default()
            }],
            ..Default::default()
        }]);
        let current = test_cover_color([20, 40, 220]);
        for request in [1, 2] {
            app.cover_uploads.insert("pl1".into(), request);
            app.handle_api(ApiResponse::PlaylistCoverUploaded {
                id: "pl1".into(),
                request,
                previous_urls: vec!["original".into()],
                cover: if request == 1 {
                    test_cover()
                } else {
                    current.clone()
                },
                result: Ok(()),
            });
        }
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation: app.library.playlists_generation,
            result: Ok(crate::api::models::Page {
                items: vec![Playlist {
                    id: "pl1".into(),
                    images: vec![Image {
                        url: "https://art.example/earlier-upload.jpg".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
        });
        assert_eq!(
            app.library.playlists.get().unwrap()[0].images[0].url,
            current.uri,
            "a new URL does not establish which upload it contains"
        );
        let earlier = vec![Image {
            url: "https://art.example/earlier-upload.jpg".into(),
            ..Default::default()
        }];
        // A check started for the superseded upload cannot confirm request 2.
        app.cover_checked("pl1", 1, earlier.clone(), Ok(true));
        assert_eq!(app.uploaded_covers["pl1"].request, 2);
        let latest = vec![Image {
            url: "https://art.example/current-upload.jpg".into(),
            ..Default::default()
        }];
        let mut latest_reply = latest.clone();
        app.reconcile_playlist_cover("pl1", &mut latest_reply);
        assert_eq!(latest_reply[0].url, current.uri);
        assert_eq!(app.uploaded_covers["pl1"].checking, Some(earlier.clone()));
        assert_eq!(app.uploaded_covers["pl1"].next_images, Some(latest.clone()));
        app.cover_checked("pl1", 2, earlier, Ok(false));
        assert_eq!(app.uploaded_covers["pl1"].checking, Some(latest.clone()));
        assert_eq!(
            app.library.playlists.get().unwrap()[0].images[0].url,
            current.uri
        );
        // A failed download also preserves the preview and remains retryable.
        app.cover_checked("pl1", 2, latest.clone(), Err("offline".into()));
        assert!(app.uploaded_covers["pl1"].checking.is_none());
        app.reconcile_playlist_cover("pl1", &mut latest.clone());
        app.cover_checked("pl1", 2, latest.clone(), Ok(true));
        assert!(!app.uploaded_covers.contains_key("pl1"));
        assert_eq!(app.library.playlists.get().unwrap()[0].images, latest);
        app.backend.shutdown();
    }

    #[test]
    fn confirmed_spotify_art_retires_the_override_and_accepts_later_changes() {
        for confirm_from_library in [false, true] {
            let mut app = test_app("confirmed-cover");
            let remote = |url: &str| {
                vec![crate::api::models::Image {
                    url: url.into(),
                    width: None,
                    height: None,
                }]
            };
            let playlist = |url: &str| Playlist {
                id: "pl1".into(),
                images: remote(url),
                ..Default::default()
            };
            app.library.playlists = Loadable::Loaded(vec![playlist("old")]);
            let page = app.playlist_pages.entry("pl1".into()).or_default();
            page.playlist = Loadable::Loaded(playlist("old"));
            let generation = page.generation;
            let cover = test_cover();
            app.cover_uploads.insert("pl1".into(), 1);
            app.handle_api(ApiResponse::PlaylistCoverUploaded {
                id: "pl1".into(),
                request: 1,
                previous_urls: vec!["old".into()],
                cover: cover.clone(),
                result: Ok(()),
            });
            app.handle_api(ApiResponse::Playlist {
                id: "pl1".into(),
                generation: generation + 1,
                result: Ok(playlist("stale-generation")),
            });
            assert!(app.uploaded_covers.contains_key("pl1"));
            for url in ["old", "confirmed", "changed-elsewhere"] {
                if confirm_from_library {
                    app.handle_api(ApiResponse::MyPlaylists {
                        offset: 0,
                        generation: app.library.playlists_generation,
                        result: Ok(crate::api::models::Page {
                            items: vec![playlist(url)],
                            ..Default::default()
                        }),
                    });
                } else {
                    app.handle_api(ApiResponse::Playlist {
                        id: "pl1".into(),
                        generation,
                        result: Ok(playlist(url)),
                    });
                }
                if url == "confirmed" {
                    assert_eq!(
                        app.library.playlists.get().unwrap()[0].images[0].url,
                        cover.uri
                    );
                    app.cover_checked("pl1", 1, remote(url), Ok(true));
                }
                let expected = if url == "old" { &cover.uri } else { url };
                if confirm_from_library {
                    assert_eq!(
                        app.library.playlists.get().unwrap()[0].images[0].url,
                        expected
                    );
                } else {
                    assert_eq!(
                        app.playlist_pages["pl1"].playlist.get().unwrap().images[0].url,
                        expected
                    );
                }
                assert_eq!(app.uploaded_covers.contains_key("pl1"), url == "old");
                if url == "confirmed" {
                    assert_eq!(app.library.playlists.get().unwrap()[0].images[0].url, url);
                    assert_eq!(
                        app.playlist_pages["pl1"].playlist.get().unwrap().images[0].url,
                        url
                    );
                }
            }
            app.backend.shutdown();
        }
    }

    /// Two lookups of one adder can overlap; the one that found nothing
    /// must not turn a shown name back into the bare id.
    #[test]
    fn a_failed_name_lookup_keeps_the_name_already_found() {
        let mut app = test_app("adder-name-race");
        app.request_user_names(vec!["kasia-id".into()]);
        app.set_user_name("kasia-id".into(), Some("Kasia".into()));
        let revision = app.user_names_revision;
        app.set_user_name("kasia-id".into(), None);
        assert_eq!(
            app.user_names.get("kasia-id"),
            Some(&Some("Kasia".to_string()))
        );
        assert_eq!(app.user_names_revision, revision, "nothing to redraw");
        // A renamed account still shows its new name.
        app.set_user_name("kasia-id".into(), Some("Kasia M.".into()));
        assert_eq!(
            app.user_names.get("kasia-id"),
            Some(&Some("Kasia M.".to_string()))
        );
        // An id nobody could name stays unnamed, to be shown as the id.
        app.set_user_name("nobody".into(), None);
        assert_eq!(app.user_names.get("nobody"), Some(&None));
        app.backend.shutdown();
    }

    /// Home's shelves from the last session fill in at start-up, for the
    /// signed-in account only, and never over an answer from Spotify.
    #[test]
    fn home_shelves_from_disk_fill_only_what_spotify_has_not_answered() {
        let mut app = headless_app();
        app.user = Some(User {
            id: "alice".into(),
            ..Default::default()
        });
        let track = |uri: &str| Track {
            uri: uri.into(),
            ..Default::default()
        };
        let last_session = HomeData {
            top_tracks: Loadable::Loaded(vec![track("spotify:track:old")]),
            top_artists: Loadable::Loaded(vec![Artist::default()]),
            ..Default::default()
        };
        let cache = |account: &str| Event::HomeCache {
            account_id: account.into(),
            cache: Some(crate::home_cache::Cache::capture(
                account.into(),
                &last_session,
                &Library::default(),
            )),
        };

        // #given another account's shelves arrive
        app.handle_backend_events(vec![cache("bob")]);
        // #then they are ignored
        assert!(app.home.top_artists.get().is_none());

        // #given Spotify answered for top songs first
        app.home.top_tracks = Loadable::Loaded(vec![track("spotify:track:new")]);
        app.home.top_artists = Loadable::Loading;
        app.handle_backend_events(vec![cache("alice")]);

        // #then the answer stays and the waiting shelf is filled
        assert_eq!(
            app.home.top_tracks.get().unwrap()[0].uri,
            "spotify:track:new"
        );
        assert_eq!(app.home.top_artists.get().map(Vec::len), Some(1));
        // A restored copy is not written back as though it were new.
        assert!(!app.home_cache_dirty);
    }

    /// The last session's playlists fill the sidebar at start-up, and a
    /// refresh replaces them whole: the list never drops to one page and
    /// grows back, and a failed refresh leaves it up.
    #[test]
    fn cached_playlists_stay_until_the_whole_fresh_list_arrives() {
        let signed_in = || {
            let mut app = headless_app();
            app.user = Some(User {
                id: "alice".into(),
                ..Default::default()
            });
            app
        };
        let playlist = |id: String| Playlist {
            uri: format!("spotify:playlist:{id}"),
            name: id.clone(),
            id,
            ..Default::default()
        };
        let last = Library {
            playlists: Loadable::Loaded((0..80).map(|n| playlist(format!("old{n}"))).collect()),
            ..Default::default()
        };
        let cache = crate::home_cache::Cache::capture("alice".into(), &HomeData::default(), &last);
        let restore = |app: &mut App| {
            app.handle_backend_events(vec![Event::HomeCache {
                account_id: "alice".into(),
                cache: Some(cache.clone()),
            }]);
        };

        // #given the live load is on its way when the cache arrives
        let mut app = signed_in();
        app.load_playlists();
        let generation = app.library.playlists_generation;
        restore(&mut app);
        assert_eq!(app.library.playlists.get().map(Vec::len), Some(80));

        // #when the first of two pages answers
        let page = |offset: u32, ids: std::ops::Range<u32>, more: bool| ApiResponse::MyPlaylists {
            offset,
            generation,
            result: Ok(crate::api::models::Page {
                items: ids.map(|n| playlist(format!("new{n}"))).collect(),
                total: 75,
                limit: 50,
                offset,
                next: more.then(|| "more".to_string()),
            }),
        };
        app.handle_api(page(0, 0..50, true));
        // #then the shown list is still the whole old one
        assert_eq!(app.library.playlists.get().map(Vec::len), Some(80));
        assert_eq!(app.library.playlists.get().unwrap()[0].id, "old0");

        // #when the last page answers, the fresh list replaces it whole
        app.handle_api(page(50, 50..75, false));
        let shown = app.library.playlists.get().unwrap();
        assert_eq!(shown.len(), 75);
        assert_eq!(shown[0].id, "new0");
        assert!(!app.library.playlists_from_cache);

        // #given another launch whose refresh fails
        let mut app = signed_in();
        app.load_playlists();
        let generation = app.library.playlists_generation;
        restore(&mut app);
        app.handle_api(ApiResponse::MyPlaylists {
            offset: 0,
            generation,
            result: Err(crate::api::ApiError::RateLimited),
        });
        // #then the last session's list stays up
        assert_eq!(app.library.playlists.get().map(Vec::len), Some(80));
        assert!(app.library.playlists_refresh.is_none());
    }

    /// Fresh shelves from Spotify are written once, not on every answer.
    #[test]
    fn fresh_home_shelves_are_checkpointed_once() {
        let mut app = headless_app();
        app.user = Some(User {
            id: "alice".into(),
            ..Default::default()
        });
        app.handle_api(ApiResponse::TopArtists {
            generation: app.home.generation,
            result: Ok(vec![Artist::default()]),
        });
        assert!(app.home_cache_dirty);
        app.checkpoint_home();
        assert!(!app.home_cache_dirty);

        // #then a failed refresh does not ask for another write
        app.handle_api(ApiResponse::TopArtists {
            generation: app.home.generation,
            result: Err(crate::api::ApiError::RateLimited),
        });
        assert!(!app.home_cache_dirty);
    }

    #[test]
    fn known_albums_include_liked_and_playlist_tracks() {
        let mut app = test_app("known-track-albums");
        let album = |id: &str| Album {
            id: id.to_string(),
            ..Default::default()
        };
        app.library.liked.items.push(SavedTrack {
            track: Track {
                album: Some(album("liked-album")),
                ..Default::default()
            },
            ..Default::default()
        });
        app.playlist_pages.insert(
            "playlist".to_string(),
            PlaylistPage {
                items: PagedList {
                    items: vec![PlaylistItem {
                        item: Some(PlayableItem::Track(Track {
                            album: Some(album("playlist-album")),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        assert_eq!(app.known_album("liked-album").unwrap().id, "liked-album");
        assert_eq!(
            app.known_album("playlist-album").unwrap().id,
            "playlist-album"
        );
        app.backend.shutdown();
    }
}
