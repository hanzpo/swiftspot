import AppKit
import Foundation
import Observation

@Observable
@MainActor
final class AppModel {
    enum Connection: Equatable {
        case connecting
        case connected
        case failed(String)
    }

    var connection = Connection.connecting
    var hello: Hello?

    // MARK: Navigation

    /// The sidebar's choice, the root of the page stack.
    var root: Route = .home {
        didSet {
            if root != oldValue {
                path = []
                forwardStack = []
                pageChanged()
            }
        }
    }

    /// Music's Find field: filters the page on show.
    var findText = ""
    /// How the song list on show is sorted, from the toolbar's menu or its
    /// headings; nil is the list's own order.
    var listSort: SongSort?

    private func pageChanged() {
        findText = ""
        listSort = nil
    }
    /// Pages opened on top of the root, as Back and Forward walk them.
    var path: [Route] = []
    private var forwardStack: [Route] = []
    var route: Route { path.last ?? root }

    func open(_ route: Route) {
        guard route != self.route else { return }
        forwardStack = []
        path.append(route)
        pageChanged()
    }

    func back() {
        guard let last = path.popLast() else { return }
        forwardStack.append(last)
        pageChanged()
    }

    func forward() {
        guard let next = forwardStack.popLast() else { return }
        path.append(next)
        pageChanged()
    }

    var canGoBack: Bool { !path.isEmpty }
    var canGoForward: Bool { !forwardStack.isEmpty }

    // MARK: Library

    var playlists: [Playlist] = [] {
        didSet { editablePlaylists = playlists.filter { $0.editable == true } }
    }
    /// The playlists this account may add songs to, for the song menus.
    private(set) var editablePlaylists: [Playlist] = []
    var sidebarRows: [SidebarRow] = []
    var likedContext: String?
    /// Bumped after anything that changes what pages show, so they reload.
    var revision = 0

    // MARK: Search

    var searchText = ""
    /// Set to put the cursor in the search field, as Command-F does.
    var searchFocused = false

    // MARK: Player

    var player = PlayerState.empty {
        didSet {
            if player.uri != playingUri { playingUri = player.uri }
            let playing = player.playing == true
            if playing != isPlaying { isPlaying = playing }
        }
    }
    /// The playing song and whether it plays, apart from the rest of
    /// `player`, so song rows redraw only when these change and not on
    /// every volume or position update.
    private(set) var playingUri: String?
    private(set) var isPlaying = false
    /// When `player` arrived, so the position can count on between reports.
    var playerAt = Date()
    /// A seek being dragged, as a fraction.
    var scrubbing: Double?
    var volumeDragging: Int?
    /// The vocal level while its slider is held.
    var vocalDragging: Double?

    // MARK: Panels

    enum Panel: String { case queue, lyrics }
    var panel: Panel?
    var queue: QueueState?
    var jam: JamState?
    var lyrics: LyricsState?
    var devices: [Device] = []
    /// Whether this Mac can play; see `DevicesState.localPlayback`.
    var localPlayback = "ready"
    var localPlaybackError: String?
    var showDevices = false
    /// The full-window player, as Music's Full Screen Player.
    var showNowPlaying = false

    // MARK: Sleep timer

    enum SleepTimer: Equatable {
        case off
        /// Pauses at this time.
        case at(Date)
        /// Pauses when this song gives way to another.
        case afterSong(String)
    }

    var sleepTimer = SleepTimer.off
    private var sleepTask: Task<Void, Never>?

    var sleepAt: Date? {
        if case .at(let date) = sleepTimer { date } else { nil }
    }

    func setSleepTimer(minutes: Int?) {
        sleepTask?.cancel()
        sleepTask = nil
        guard let minutes else {
            sleepTimer = .off
            return
        }
        let at = Date().addingTimeInterval(TimeInterval(minutes * 60))
        sleepTimer = .at(at)
        sleepTask = Task {
            try? await Task.sleep(for: .seconds(at.timeIntervalSinceNow))
            guard !Task.isCancelled else { return }
            sleepTimer = .off
            if player.playing == true { toggle() }
        }
        show("Sleep timer set")
    }

    func sleepAtEndOfSong() {
        sleepTask?.cancel()
        sleepTask = nil
        guard let uri = player.uri else { return }
        sleepTimer = .afterSong(uri)
        show("Pausing after this song")
    }

    /// Pauses as soon as the song the timer was set on gives way to another.
    private func checkSleepAfterSong(_ state: PlayerState) {
        guard case .afterSong(let song) = sleepTimer else { return }
        let duration = player.durationMs ?? 0
        let looped = state.uri == song
            && state.repeat == "track"
            && (state.positionMs ?? 0) < 5000
            && duration > 5000
            && position(at: Date()) > duration - 5000
        guard state.uri != song || looped else { return }
        sleepTimer = .off
        if state.playing == true { toggle() }
    }

    // MARK: Messages

    var toast: String?
    private var toastTask: Task<Void, Never>?

    private(set) var client: Client?
    /// Continuations waiting for a server push event on a page or surface key.
    private var pageWaiters: [UUID: (keys: Set<String>, resume: () -> Void)] = [:]

    /// Suspends until the engine pushes an update matching any of `keys`,
    /// `timeout` elapses, or the task is cancelled.
    func waitForPage(_ keys: Set<String>, timeout: Duration = .seconds(5)) async {
        let id = UUID()
        await withTaskCancellationHandler {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                var resumed = false
                let finish: @MainActor () -> Void = { [weak self] in
                    guard !resumed else { return }
                    resumed = true
                    self?.pageWaiters.removeValue(forKey: id)
                    continuation.resume()
                }
                pageWaiters[id] = (keys, finish)
                Task { @MainActor in
                    try? await Task.sleep(for: timeout)
                    finish()
                }
            }
        } onCancel: {
            Task { @MainActor [weak self] in
                if let waiter = self?.pageWaiters.removeValue(forKey: id) {
                    waiter.resume()
                }
            }
        }
    }

    private func notifyPageWaiters(_ key: String) {
        let matching = pageWaiters.values.filter { $0.keys.contains(key) }
        for waiter in matching {
            waiter.resume()
        }
    }

    // MARK: Connecting

    init() {
        // For screenshots and debugging: the page and panel to start on.
        let environment = ProcessInfo.processInfo.environment
        if let page = environment["SWIFTSPOT_ROUTE"], let route = Route(page: page) {
            if case .playlist = route { root = route } else if [.home, .search, .recentlyAdded, .allPlaylists, .liked, .albums, .artists, .podcasts, .episodes, .topSongs].contains(route) {
                root = route
            } else {
                path = [route]
            }
        }
        if let query = environment["SWIFTSPOT_SEARCH"] { searchText = query }
        if let text = environment["SWIFTSPOT_FIND"] { findText = text }
        panel = environment["SWIFTSPOT_PANEL"].flatMap(Panel.init(rawValue:))
        showNowPlaying = environment["SWIFTSPOT_NOW_PLAYING"] != nil
    }

    /// Starts the engine linked into this app and greets it. Calling it
    /// again after it is connected does nothing; after a failure it tries
    /// again.
    func connect() async {
        if connection == .connected { return }
        connection = .connecting
        if client == nil {
            let client = Client.shared
            guard client.running else {
                connection = .failed(Client.Failure.notRunning.localizedDescription)
                return
            }
            client.onEvent = { [weak self] event, data in
                self?.handle(event: event, data: data)
            }
            self.client = client
        }
        do {
            guard let client else { return }
            hello = try await client.call("hello", as: Hello.self)
            connection = .connected
            if hello?.signedIn == true {
                await refreshAll()
            }
        } catch {
            connection = .failed(error.localizedDescription)
        }
    }

    func refreshAll() async {
        guard let client else { return }
        if let state = try? await client.call("player.state", as: PlayerState.self) {
            setPlayer(state)
        }
        await loadPlaylists()
        await loadJam()
        revision += 1
    }

    private func handle(event: String, data: Data) {
        switch event {
        case "player":
            if let state = try? JSONDecoder.api.decode(PlayerState.self, from: data) {
                let changedSong = state.uri != player.uri
                setPlayer(state)
                if changedSong {
                    if panel == .queue { reloadQueue() }
                    if panel == .lyrics { reloadLyrics() }
                }
            }
        case "auth":
            if let next = try? JSONDecoder.api.decode(Hello.self, from: data) {
                if next.user?.id != hello?.user?.id { saved = [:] }
                let signedIn = next.signedIn && hello?.signedIn != true
                hello = next
                if signedIn { Task { await refreshAll() } }
            }
        case "devices":
            if let state = try? JSONDecoder.api.decode(DevicesState.self, from: data) {
                devices = state.devices
                localPlayback = state.localPlayback
                localPlaybackError = state.localError
            }
        case "saved":
            if let changes = try? JSONDecoder().decode([String: Bool].self, from: data) {
                saved.merge(changes) { _, new in new }
            }
        case "library":
            notifyPageWaiters("library")
            notifyPageWaiters("playlists")
            Task {
                await loadPlaylists()
                revision += 1
            }
        case "page":
            struct PageUpdate: Decodable { var page: String }
            if let update = try? JSONDecoder.api.decode(PageUpdate.self, from: data) {
                notifyPageWaiters(update.page)
            }
        case "queue":
            notifyPageWaiters("queue")
            if panel == .queue { reloadQueue() }
        case "jam":
            reloadJam()
        case "lyrics":
            notifyPageWaiters("lyrics")
            // Words placed in the song: a lyrics view shows them sung.
            if lyrics != nil { reloadLyrics() }
        case "toast":
            struct Message: Decodable { var message: String }
            if let toast = try? JSONDecoder.api.decode(Message.self, from: data) {
                show(toast.message)
            }
        default:
            break
        }
    }

    private func setPlayer(_ state: PlayerState) {
        checkSleepAfterSong(state)
        // A drag on the volume keeps the slider where the hand put it.
        var state = state
        if let volume = volumeDragging { state.volume = volume }
        if let level = vocalDragging { state.karaoke?.level = level }
        if state.karaoke?.status == "failed", player.karaoke?.status != "failed" {
            show("Karaoke couldn't start: \(state.karaoke?.error ?? "the vocal model failed")")
        }
        player = state
        playerAt = Date()
    }

    func show(_ message: String) {
        toast = message
        toastTask?.cancel()
        toastTask = Task {
            try? await Task.sleep(for: .seconds(3))
            if !Task.isCancelled { toast = nil }
        }
    }

    // MARK: Requests

    func call<T: Decodable>(_ method: String, _ params: [String: Any] = [:], as type: T.Type) async throws -> T {
        guard let client else { throw Client.Failure.closed }
        return try await client.call(method, params, as: type)
    }

    /// Sends a command whose reply only matters if it failed.
    func send(_ method: String, _ params: [String: Any] = [:], done: String? = nil) {
        guard let client else { return }
        Task {
            do {
                _ = try await client.call(method, params)
                if let done { show(done) }
            } catch {
                show(error.localizedDescription)
            }
        }
    }

    func loadPlaylists() async {
        for _ in 0..<120 {
            guard let list = try? await call("library.playlists", as: PlaylistList.self) else { return }
            // A reload in progress or one that failed keeps the list shown.
            if let fresh = list.playlists { playlists = fresh }
            if let rows = list.rows { sidebarRows = rows }
            likedContext = list.likedContext ?? likedContext
            if !list.loading { return }
            await waitForPage(["playlists", "library"], timeout: .seconds(3))
            if Task.isCancelled { return }
        }
    }


    // MARK: Playback

    /// Plays a song from a list. A list `reordered` from its context's own
    /// order, sorted or filtered, plays in the order shown.
    func play(_ track: Track, in context: String?, tracks: [Track]? = nil, reordered: Bool = false) {
        var params: [String: Any] = ["uri": track.uri]
        if let context, reordered, let tracks {
            params["context"] = context
            params["uris"] = tracks.map(\.uri)
            params["index"] = tracks.firstIndex { $0.id == track.id } ?? 0
        } else if let context, context.hasPrefix("spotify:station:") {
            // A radio plays the songs it shows, under the station's name.
            let list = tracks ?? [track]
            params["context"] = context
            params["uris"] = list.map(\.uri)
            params["index"] = list.firstIndex { $0.id == track.id } ?? 0
        } else if let context {
            params["context"] = context
            params["index"] = track.position
            if let place = track.spotifyPosition { params["spotify_position"] = place }
        } else if track.isEpisode {
            if let resume = track.resumeMs, track.played != true { params["resume_ms"] = resume }
        } else if let tracks {
            // A list with no Spotify context of its own (search results,
            // an artist's top songs) plays as the songs on screen.
            params["uris"] = tracks.map(\.uri)
            params["index"] = tracks.firstIndex { $0.id == track.id } ?? 0
        }
        player.uri = track.uri
        player.title = track.name
        player.subtitle = track.artistNames
        player.artists = track.artists
        player.album = track.album
        player.isEpisode = track.isEpisode
        player.showId = track.showId
        player.image = track.image
        player.saved = nil
        player.durationMs = track.durationMs
        player.positionMs = 0
        player.playing = true
        player.hasSong = true
        playerAt = Date()
        send("player.play", params)
        confirmPlayer()
    }

    private var confirmTask: Task<Void, Never>?

    /// Asks for the player once the engine has had time to act, so a play
    /// that failed, and sent no news, does not leave its guess on show.
    private func confirmPlayer() {
        confirmTask?.cancel()
        confirmTask = Task {
            try? await Task.sleep(for: .seconds(2.5))
            guard !Task.isCancelled else { return }
            if let state = try? await call("player.state", as: PlayerState.self), !Task.isCancelled {
                setPlayer(state)
            }
        }
    }

    func play(context: String, shuffle: Bool? = nil, uris: [String]? = nil) {
        if let shuffle {
            player.shuffle = shuffle
            send("player.shuffle", ["on": shuffle])
        }
        var params: [String: Any] = ["context": context]
        // A radio has no list of its own on Spotify: it plays the songs
        // it shows, shuffled from a random one when asked.
        if let uris, !uris.isEmpty {
            params["uris"] = uris
            let start = shuffle == true ? Int.random(in: 0..<uris.count) : 0
            params["uri"] = uris[start]
            params["index"] = start
        }
        send("player.play", params)
        confirmPlayer()
    }

    func toggle() {
        guard player.hasSong else { return }
        if player.playing == true {
            player.positionMs = position(at: Date())
        }
        player.playing?.toggle()
        playerAt = Date()
        send("player.toggle")
    }

    func next() { send("player.next") }
    func previous() { send("player.previous") }

    func toggleShuffle() {
        let on = !(player.shuffle ?? false)
        player.shuffle = on
        send("player.shuffle", ["on": on])
    }

    func cycleRepeat() {
        let next = switch player.repeat {
        case "off", nil: "context"
        case "context": "track"
        default: "off"
        }
        player.repeat = next
        send("player.repeat", ["mode": next])
    }

    /// Whether each song or item is saved, as the engine last pushed it or
    /// as just clicked, ahead of the page's own copy.
    var saved: [String: Bool] = [:]

    /// Saves songs to Liked Songs, or takes them out.
    func setSaved(_ tracks: [Track], _ saved: Bool) {
        if tracks.contains(where: { $0.uri == player.uri }) { player.saved = saved }
        for track in tracks { self.saved[track.uri] = saved }
        send(
            "library.set", ["uris": tracks.map(\.uri), "saved": saved],
            done: saved ? "Added to Liked Songs" : "Removed from Liked Songs")
    }

    func toggleSaved(_ uri: String) {
        let now = !(player.uri == uri ? player.saved ?? false : saved[uri] ?? false)
        if uri == player.uri { player.saved = now }
        saved[uri] = now
        send("library.toggle", ["uri": uri])
    }

    func seek(to fraction: Double) {
        let position = Int(fraction * Double(player.durationMs ?? 0))
        seek(ms: position)
    }

    func seek(ms: Int) {
        player.positionMs = max(0, min(ms, player.durationMs ?? ms))
        playerAt = Date()
        send("player.seek", ["position_ms": player.positionMs ?? 0])
    }

    func skip(seconds: Int) {
        seek(ms: position(at: Date()) + seconds * 1000)
    }

    func setVolume(_ percent: Int, done: Bool) {
        player.volume = percent
        volumeDragging = done ? nil : percent
        send("player.volume", ["percent": percent, "done": done])
    }

    func changeVolume(by delta: Int) {
        setVolume(max(0, min(100, (player.volume ?? 0) + delta)), done: true)
    }

    func toggleMute() { send("player.mute") }

    /// Takes the voice out of the song, or puts it back. The first time,
    /// the vocal model downloads.
    func toggleKaraoke() {
        let on = !(player.karaoke?.on ?? false)
        player.karaoke?.on = on
        player.karaoke?.status = on ? "preparing" : "off"
        send("karaoke.set", ["on": on])
    }

    func setVocalLevel(_ level: Double, done: Bool) {
        player.karaoke?.level = level
        vocalDragging = done ? nil : level
        send("karaoke.set", ["level": level])
    }

    /// Where the song is now, counted on from the last report.
    func position(at date: Date) -> Int {
        let reported = player.positionMs ?? 0
        guard player.playing == true, player.loading != true else { return reported }
        let elapsed = Int(date.timeIntervalSince(playerAt) * 1000)
        return min(reported + elapsed, player.durationMs ?? Int.max)
    }

    // MARK: Queue

    func togglePanel(_ panel: Panel) {
        self.panel = self.panel == panel ? nil : panel
        if self.panel == .queue { reloadQueue() }
        if self.panel == .lyrics { reloadLyrics() }
    }

    private var queueTask: Task<Void, Never>?
    private var lyricsTask: Task<Void, Never>?
    private var jamTask: Task<Void, Never>?

    /// Loads the queue again, dropping a load still running, so an older
    /// answer never lands on top of a newer one.
    func reloadQueue() {
        queueTask?.cancel()
        queueTask = Task { await loadQueue() }
    }

    func reloadLyrics() {
        lyricsTask?.cancel()
        lyricsTask = Task { await loadLyrics() }
    }

    func reloadJam() {
        jamTask?.cancel()
        jamTask = Task { await loadJam() }
    }

    func loadQueue() async {
        for _ in 0..<120 {
            guard let state = try? await call("queue.get", as: QueueState.self), !Task.isCancelled else { return }
            queue = state
            if !state.loading { return }
            await waitForPage(["queue"], timeout: .seconds(3))
            if Task.isCancelled { return }
        }
    }

    func addToQueue(_ tracks: [Track]) {
        send(
            "queue.add",
            ["songs": tracks.map { ["uri": $0.uri, "name": $0.name] }],
            done: tracks.count == 1 ? "Added to queue" : "Added \(tracks.count) songs to queue")
        if panel == .queue { reloadQueue() }
    }

    func playQueued(at index: Int, queued: Bool) {
        send("queue.play", ["index": index, "queued": queued])
        reloadQueue()
    }

    /// Moves a song queued by hand; `to` is the row it goes before.
    func moveQueued(from: Int, to: Int) {
        if var queued = queue?.queued {
            queued.move(fromOffsets: IndexSet(integer: from), toOffset: to)
            queue?.queued = queued
        }
        send("queue.move", ["from": from, "to": to])
    }

    /// Takes one song queued by hand out of the queue.
    func removeQueued(at index: Int) {
        if var queued = queue?.queued, queued.indices.contains(index) {
            queued.remove(at: index)
            queue?.queued = queued
        }
        send("queue.remove", ["index": index])
    }

    func clearQueue() {
        queue?.queued = []
        send("queue.clear")
    }

    // MARK: Lyrics

    func loadLyrics() async {
        for attempt in 0..<120 {
            guard let state = try? await call("lyrics.get", as: LyricsState.self), !Task.isCancelled else { return }
            lyrics = state
            // A song just played here is shown before the engine has moved
            // to it, and its answer can still be the last song's: ask
            // again, for a while, until it is this song's.
            if !state.loading, state.uri == player.uri || !player.hasSong || attempt >= 15 { return }
            await waitForPage(["lyrics"], timeout: .seconds(2))
            if Task.isCancelled { return }
        }
    }

    // MARK: Devices

    func loadDevices() async {
        send("devices.refresh")
        for attempt in 0..<10 {
            guard let state = try? await call("devices.list", as: DevicesState.self) else { return }
            devices = state.devices
            localPlayback = state.localPlayback
            localPlaybackError = state.localError
            if !state.loading && attempt > 0 { return }
            await waitForPage(["devices"], timeout: .milliseconds(600))
            if Task.isCancelled { return }
        }
    }

    /// Opens the browser to let this Mac play, as Spotify asks once.
    func enablePlayback() {
        localPlayback = "authorizing"
        send("auth.enable_playback")
    }

    func transfer(to device: Device) {
        for index in devices.indices { devices[index].active = devices[index].id == device.id }
        send("devices.transfer", ["id": device.id])
    }

    // MARK: Jam

    func loadJam() async {
        let waiting = jam?.busy == true && jam?.jam == nil
        guard let state = try? await call("jam.get", as: JamState.self), !Task.isCancelled else { return }
        jam = state
        if waiting, state.jam != nil, panel != .queue {
            panel = .queue
            await loadQueue()
        }
    }

    func startJam() {
        jam?.busy = true
        send("jam.start")
    }

    func endJam() { send("jam.end") }
    func leaveJam() { send("jam.leave") }
    func removeFromJam(_ member: JamMember) { send("jam.remove", ["member": member.id]) }
    func joinJam(_ invite: JamInvite) {
        jam?.invite = nil
        jam?.busy = true
        send("jam.join", ["token": invite.token])
    }
    func declineJam() {
        jam?.invite = nil
        send("jam.decline")
    }

    func copyJamLink() {
        guard let link = jam?.jam?.inviteUrl else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(link, forType: .string)
        show("Invite link copied")
    }

    // MARK: Library edits

    /// Songs waiting for the New Playlist sheet, which opens while set.
    var newPlaylistSongs: [Track]?
    var editingPlaylist: Playlist?

    func requestNewPlaylist(with tracks: [Track] = []) { newPlaylistSongs = tracks }

    func createPlaylist(named name: String, isPublic: Bool, with tracks: [Track] = []) {
        send(
            "playlist.create",
            // Songs described, as for adding, so the new rows show whole.
            [
                "name": name, "public": isPublic,
                "items": tracks.filter { !$0.isEpisode }.map(\.described),
                "uris": tracks.filter(\.isEpisode).map(\.uri),
            ],
            done: "Created “\(name)”")
    }

    func add(_ tracks: [Track], to playlist: Playlist) {
        // Songs go with what the server needs to show their rows at once;
        // episodes go by URI.
        let songs = tracks.filter { !$0.isEpisode }.map(\.described)
        let episodes = tracks.filter(\.isEpisode).map(\.uri)
        send(
            "playlist.add",
            ["id": playlist.id, "items": songs, "uris": episodes],
            done: "Added to \(playlist.name)")
        revision += 1
    }

    func remove(_ tracks: [Track], from playlistID: String) {
        send(
            "playlist.remove",
            // Each row by its place in Spotify's copy, so only it leaves,
            // not every copy of its song.
            ["id": playlistID, "items": tracks.map { ["uri": $0.uri, "position": $0.spotifyPosition ?? $0.position] }])
        revision += 1
    }

    /// Moves a song one place up or down its playlist.
    func move(_ track: Track, in playlistID: String, by step: Int) {
        let from = track.spotifyPosition ?? track.position
        // Spotify inserts before `to`, counted before the song leaves.
        let to = step < 0 ? from - 1 : from + 2
        guard to >= 0 else { return }
        send("playlist.move", ["id": playlistID, "from": from, "to": to])
        revision += 1
    }

    func rename(_ playlist: Playlist, to name: String, description: String?) {
        var params: [String: Any] = ["id": playlist.id, "name": name]
        if let description { params["description"] = description }
        send("playlist.edit", params)
    }

    func delete(_ playlist: Playlist) {
        if route == .playlist(playlist.id) { back() }
        if root == .playlist(playlist.id) { root = .home }
        // Back and Forward never lead to a playlist that's gone.
        let gone = Route.playlist(playlist.id)
        path.removeAll { $0 == gone }
        forwardStack.removeAll { $0 == gone }
        send("playlist.delete", ["id": playlist.id], done: "Removed “\(playlist.name)”")
    }

    func toggleLibrary(uri: String) {
        send("library.toggle", ["uri": uri])
        revision += 1
    }

    func follow(artist id: String, follow: Bool) {
        send("artist.follow", ["id": id, "follow": follow])
        revision += 1
    }

    func startRadio(seed: String) { open(.radio(seed)) }

    func copyLink(_ uri: String) {
        let parts = uri.split(separator: ":")
        let link = parts.count >= 3 ? "https://open.spotify.com/\(parts[1])/\(parts[2])" : uri
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(link, forType: .string)
        show("Link copied")
    }

    // MARK: Account

    func signIn(clientId: String) { send("auth.sign_in", ["client_id": clientId]) }
    func signOut() { send("auth.sign_out") }
}
