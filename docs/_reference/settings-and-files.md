---
title: Settings & Files
description: Configuration, credential and cache locations, and what is safe to delete.
nav_order: 0
---

## Where things live

Settings, state and the logs are in
`~/Library/Application Support/com.hanzpo.swiftspot/`; the caches are in
`~/Library/Caches/com.hanzpo.swiftspot/`. Below, `Support` and `Caches`
stand for these two folders.

| What | Where | Safe to delete? |
| --- | --- | --- |
| Settings | `Support/settings.json` | Yes, you lose preferences |
| Spotify grants | Keychain | Use Sign out in Settings |
| Credential revocation markers | `Support/credential-storage/` | Keep after a failed sign-out deletion |
| Legacy shared Web API grant | `Support/shared_web_api_token.json` | Removed after migration or sign-out |
| Legacy personal Web API grant | `Support/personal_web_api_token.json` | Removed after migration or sign-out |
| Legacy playback credential | `Support/credentials/` | Removed after migration or sign-out |
| Proxy password | Keychain | Clear the password and apply the manual proxy settings |
| Legacy proxy password | `Support/proxy_password` | Removed after protected migration |
| Last session | `Support/session.json` | Yes |
| Play history | `Support/history.json` | Yes |
| Audio cache | `Caches/audio/` | Always |
| Artwork cache | `Caches/art/` | Always |
| Lyrics cache | `Caches/lyrics/` | Always |
| Karaoke's vocal model | `Caches/karaoke/` | Always; downloaded again when karaoke is next turned on |
| Account-scoped playlist page cache | `Caches/playlists/<account-id>/` | Always |
| Account-scoped Home shelves and library rows | `Caches/home/` | Always |
| Last run's log | `Support/swiftspot.log` | Always |
| Crash log | `Support/panic.log` | Always |

Clearing caches never signs you out. Sign-out from Settings covers the shared
and personal Web API grants and the independent playback credential.

The following credential storage is available since 0.8.0.

Durable grants use the **Keychain**, under the service name
`com.hanzpo.swiftspot`. Entries are separated by application state
location and grant type; Web grants carry their Client ID and must verify as
the same account. Playback and receiver activation require that account too.
Non-secret settings and session data remain readable JSON. Native credential
protection reduces exposure from copying ordinary application files. It does
not protect a usable session from arbitrary code running as the same user.

A locked or unavailable keychain produces an error without blocking the
interface. A new sign-in can still be used for this session, with no
plaintext fallback file.

On upgrade, each legacy grant is written to the protected store and read back
before its old file is removed. Valid grants migrate without signing in again.
If Spotify rejects a saved refresh grant, only that grant is forgotten so the
next launch cannot keep restoring it. If migration fails, SwiftSpot reports it and
keeps the original so migration can be retried. That grant can still serve the
current session. A successfully migrated grant is never replaced by a stale
legacy copy. Librespot's reusable grant stays in memory until SwiftSpot saves
it through this same store. Volume and disposable audio caches are independent.

Sign-out invalidates pending authorization, refresh, and playback connections,
and cancels pending Spotify requests so their results cannot undo a new sign-in.
It records non-secret revocation markers before deleting the protected entries
and all legacy token files, including temporary copies. A locked store or
filesystem failure is reported. Revocation markers prevent a failed protected
entry deletion from restoring the session on restart; keep these markers when
a deletion failed. Changing the Client ID clears the old app's grant.

Version 0.7.1 and earlier use the legacy unencrypted files listed above. Their
Web API writer requests owner-only permissions for newly created files. Librespot's old writer uses the system's
file defaults. Keep these legacy files, their temporary copies, the
`credentials/` directory, and credential-store exports out of issue attachments
and diagnostic uploads.

Since 0.9.0, proxy passwords use a separate protected entry under the same service name.
They belong to the configured host, port, and username. Editing any of these
fields clears the old password; changing only HTTP/SOCKS5 mode keeps it. Off,
System, and Spotify sign-out retain the saved manual proxy password. Clear the
password and apply the manual settings to forget it. A revocation marker keeps
a failed protected deletion from restoring that password.

Older proxy passwords, whether in `settings.json` or the separate state file,
are migrated and read back before their plaintext copies are removed. If that
fails, the originals remain available for retry. Settings changes stay in
memory until migration succeeds, so a save cannot erase the only password or
associate it with a different proxy address. SwiftSpot reports this condition.
Newly entered passwords have no plaintext fallback. Settings store the confirmed
proxy mode, host, port, and username, never an unapplied draft or password.

Progress through a playlist is periodically cached as a contiguous prefix.
When the playlist has not changed on Spotify, reopening it resumes from that
prefix instead of requesting the same pages again. SwiftSpot validates the
cache against Spotify's playlist snapshot and reported song count before
showing it. A cache with a mismatched count is replaced by live rows even if
its snapshot matches, so stale cached songs cannot choose the playback order.
Successful playlist edits keep that loaded prefix and save it under Spotify's
new snapshot after all pending writes have succeeded. Pending edits remain
visible immediately, but are not saved as confirmed playlist rows. A failed
write reloads the playlist to reconcile the edit.

In 0.8.0, playlist checkpoints began streaming their JSON to a temporary
file on a background file worker. Saving a large playlist no longer needed a
second complete JSON buffer in memory. That update kept the cache format and
checkpoint order unchanged, and a failed write left the previous cache in place.

Since 0.9.0, reading a playlist cache also uses
a small buffer on a background file worker. The full JSON file no longer
stays in memory alongside the loaded songs. Existing caches remain readable;
missing or invalid caches are ignored and fetched again as before.

Playlist checkpoints now keep new rows in a separate append-only data file.
An atomically replaced manifest records the committed byte length, row count,
snapshot, and next Spotify offset. A failed append leaves the previous
checkpoint readable; a later write discards the unfinished tail. Older JSON
caches remain readable and are retained when a new checkpoint is written.
Reloads, edits, and changes to previously loaded rows write a fresh data file
instead of appending to the old prefix. A small lock file in the account cache
directory coordinates reads and writes; reopening the cache removes row files
left without a manifest reference by an interrupted replacement.

Since 0.10.0, the artwork loader shares downloaded image bytes with
the background cache writer instead of making a separate copy. Visible library
cards and collection covers request 640-pixel artwork for sharper HiDPI output;
compact rows and softened placeholders keep using 64-pixel thumbnails. Up to 64
softened 256-pixel covers may be retained in addition to the existing artwork
budget, and their downloaded image bytes are released after decoding. A failed
cache write does not prevent the downloaded image from being displayed.

The following Liked Songs caching behavior is available since 0.9.0.

Liked Songs metadata is stored separately under `liked-songs/` in the cache
directory, one JSON file per account. Only the verified account's rows are
shown. Fresh cached pages are reused for 15 minutes; older pages refresh in
the background. Refreshing keeps the last usable rows until their replacement
is ready, and a failed refresh leaves those rows visible. The refresh control
requests current data immediately. Partial caches resume from their next page.
Like and Unlike change the rows immediately, and confirmed edits survive a
restart even if Spotify's next read still reports the old state. This cache
contains metadata, not offline audio, and can be deleted without signing out.

Home's shelves (recently played, top artists and songs, made-for-you
playlists and podcast episodes) are stored under `home/` in the cache
directory, one JSON file per account. Once Spotify confirms the account,
Home shows the last session's shelves while it asks for fresh ones, and a
shelf Spotify has already answered keeps that answer. The same file keeps
the first 50 saved albums, followed artists and saved podcasts, which the
library shows until Spotify's first page replaces them, and the whole
playlist list, which a refresh replaces only once every page has arrived. The file is rewritten
at most every 20 seconds after new shelves arrive, and when the app quits.

The last good playlist folder tree is kept in `session.json`, scoped to the
account that supplied it. This keeps folders visible when local playback is
temporarily unavailable. Live session data is still required for edit grants.

Since 0.8.0, memory caches retain the open page,
the playing context, and a limited set of recently used playlist, album,
artist, and show pages. Older pages reload when revisited, using the saved
playlist prefix when its snapshot still matches. Pending playlist edits and
their rows stay in memory until the write and its snapshot are confirmed,
even if this temporarily exceeds the usual page limit. Track metadata is
limited to 800 cached tracks; navigation and periodic cleanup trim old entries.

## settings.json

Settings are stored in one readable JSON file and written atomically. The
app's Settings window changes these fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `device_name` | `SwiftSpot` | Name on Spotify Connect |
| `bitrate` | `320` | 96, 160, or 320 kbps |
| `normalisation` | `false` | Volume normalisation |
| `autoplay` | `true` | Keep playing similar music at the end |
| `gapless` | `true` | Gapless playback |
| `audio_cache_mb` | `1024` | On-disk audio cache budget |
| `eq_on` | `false` | The equalizer shapes local playback |
| `eq_preamp_db` | `0` | The preamp, in decibels, -12 to 12 |
| `eq_bands_db` | ten zeros | The bands from 60 Hz to 16 kHz, in decibels, -12 to 12 |
| `karaoke_vocal_level` | `0` | How much of the voice karaoke leaves, 0 to 1 |
| `web_client_id` | none | The Client ID of your own Spotify app, which SwiftSpot signs in with |
| `proxy_mode` | `system` | `off`, `system`, `http`, or `socks` |
| `proxy_host` | none | Host of a manual HTTP or SOCKS5 proxy |
| `proxy_port` | none | Port of a manual HTTP or SOCKS5 proxy |
| `proxy_username` | none | Optional proxy login for Web requests; authenticated local playback proxying is not supported |

Fields written by the old egui app (themes, sidebar and window layout) are
kept unchanged so an older settings file stays readable, but nothing uses
them.

## Home shelves

You can hide **Made for you** and **Recommended for you**
from Home independently. Quit SwiftSpot before editing `settings.json`, then
restart it. Add this field to hide both:

```json
"home": {
  "made_for_you": { "visible": false },
  "recommendations": { "visible": false }
}
```

Set either `visible` value to `true` to show that shelf again. Omitted
preferences keep both shelves visible. Other Home sections keep their normal
order and contents. This changes what is displayed; hidden shelves still
refresh in the background.
