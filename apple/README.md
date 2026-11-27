# SwiftSpot for SwiftUI

The native macOS app. SwiftSpot's Rust engine, which keeps the session,
playback, the queue and the library, is linked into it as a static library
(`src/ffi.rs`), so the app is one process.

## Run it

```sh
cargo build --release --lib
cd apple && swift run
```

`apple/install.sh` builds, signs and installs `SwiftSpot.app`.
`SWIFTSPOT_DATA=/some/folder` keeps a test run's settings, state and
sign-in in that folder and leaves the media keys to the installed app.

## The API

The app hands the engine one JSON object per request (`src/server.rs`). It
sends `{"id": 1, "method": "…", "params": {…}}` and gets back
`{"id": 1, "result": …}` or `{"id": 1, "error": "…"}`. The engine also pushes `{"event": "player", "data": …}` whenever playback
changes, apart from the position simply moving on, `page` (`{"page": "…"}`)
when a page or list finishes loading more data, `lyrics` when the playing
song's words have been placed in time, and `jam` when the account's Jam, a
Jam invitation waiting for an answer, or a Jam being started changes.

| Method | Params | Answers |
| --- | --- | --- |
| `hello` | | app, version, signed-in user, `auth`, `auth_error`, `sign_in_problem`, `client_id`, `web_app`, `shared_grant`, `redirect_uri` |
| `auth.sign_in` | `client_id`? (saved for the next sign-in) | |
| `auth.use_client_id` | `client_id` | |
| `player.state` | | the player (same shape as the `player` event) |
| `player.toggle`, `player.next`, `player.previous` | | |
| `player.seek` | `position_ms` | |
| `player.volume` | `percent` | |
| `player.shuffle` | `on` | |
| `player.repeat` | `mode`: `off`, `context`, `track` | |
| `player.like` | `uri`? (the playing song by default) | |
| `player.play` | `context`? `uri`? `index`? | |
| `karaoke.set` | `on`? `level`? (voice kept, 0 to 1) | |
| `lyrics.get` | | `uri`, `synced`, `lines`: `time_ms`, `text`, `words`? |
| `words.prepare` | `uri` | |
| `queue.add` | `uri`, `name`? | |
| `jam.get` | | `jam` (`hosting`, `host`, `invite_url`, `can_remove`, `members`), `invite`, `busy`, `can_start` |
| `jam.start`, `jam.end`, `jam.leave` | | |
| `jam.remove` | `member` | |
| `jam.open` | `link` (a Jam invitation, short links too) | |
| `jam.join` | `token` | |
| `jam.decline` | | |
| `library.playlists` | | `playlists`, `liked_context` |
| `library.liked` | | `tracks`, `total`, `more`, `context` |
| `library.albums` | | `albums`, `more` |
| `playlist.get` | `id` | `playlist`, `tracks`, `total`, `more` |
| `album.get` | `id` | `album`, `tracks`, `more` |
| `page.more` | `page`, e.g. `playlist:ID`, `liked` | |

`auth` is `starting`, `signed_out`, `waiting_for_browser`, `connecting`,
`connected`, `failed` or `refused`. A refusal's `sign_in_problem` is
`malformed_client_id`, `unknown_client`, `account_not_allowed`,
`shared_retired` or `expired`, and `auth_error` explains it.
`auth.use_client_id` moves a signed-in session to another Spotify app and
opens the browser to authorize it.

The player carries `karaoke`: `available`, `on`, `level`, and `status`
(`off`, `downloading`, `preparing`, `running` or `failed`, with `error`).

A Jam member carries `id`, `name`, `image`, `you` and `host`. An
`invite` (`token`, `host`, `people`) is an opened invitation waiting for
`jam.join` or `jam.decline`. A Jam link searched for opens its invitation.

A line's `words`, once placed, are its words with `marks`: when each
character starts, in milliseconds, and when the word ends.

Lists SwiftSpot is still fetching answer with `"loading": true`; ask again.

A page with song rows (`tracks`) carries `rows_tag`, a hash of those rows.
A request for such a page may also carry `from`, the number of rows the app
already holds, and the `rows_tag` they came with. When the list still starts
with exactly those rows, the answer carries only the rows past them and
`"from": from`; otherwise it carries the whole list and `"from": 0`. The app
asks this way while a long list loads and after each library change, so each
new page of rows costs only its own rows.
The handlers are in `src/app/api.rs`: each reads SwiftSpot's state or queues
the same action its window would, so the app gets the old window's behaviour.
