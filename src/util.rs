//! Small helpers shared across the application.

/// Tears the id out of `spotify:track:abc` and friends.
pub fn uri_id(uri: &str) -> Option<&str> {
    uri.rsplit(':').next().filter(|id| !id.is_empty())
}

pub fn uri_kind(uri: &str) -> Option<&str> {
    let mut parts = uri.split(':');
    parts.next()?;
    parts.next()
}

/// Spotify's radio station seeded by a song, playlist, album, or artist.
pub fn station_uri(seed: &str) -> Option<String> {
    let kind = uri_kind(seed)?;
    let id = uri_id(seed)?;
    (seed == format!("spotify:{kind}:{id}")
        && matches!(kind, "track" | "playlist" | "album" | "artist"))
    .then(|| format!("spotify:station:{kind}:{id}"))
}

/// The song, playlist, album, or artist a radio station is seeded by.
pub fn station_seed(station: &str) -> Option<String> {
    let seed = format!("spotify:{}", station.strip_prefix("spotify:station:")?);
    station_uri(&seed).is_some().then_some(seed)
}

pub fn open_spotify_url(uri: &str) -> Option<String> {
    let kind = uri_kind(uri)?;
    let id = uri_id(uri)?;
    Some(format!("https://open.spotify.com/{kind}/{id}"))
}

/// Strips the HTML Spotify embeds in playlist descriptions.
pub fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Atomically replaces `path` with `temporary` on the current platform.
pub(crate) fn replace_file(
    temporary: &std::path::Path,
    path: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_stations_map_to_their_seeds_and_back() {
        for kind in ["track", "playlist", "album", "artist"] {
            let seed = format!("spotify:{kind}:4uLU6hMCjMI75M1A2tKUQC");
            let station = station_uri(&seed).expect("a station");
            assert_eq!(
                station,
                format!("spotify:station:{kind}:4uLU6hMCjMI75M1A2tKUQC")
            );
            assert_eq!(station_seed(&station).as_deref(), Some(seed.as_str()));
        }
        for seed in [
            "spotify:show:abc",
            "spotify:episode:abc",
            "spotify:user:me:collection",
            "spotify:track:",
            "track:abc",
        ] {
            assert_eq!(station_uri(seed), None, "{seed}");
        }
        assert_eq!(station_seed("spotify:playlist:abc"), None);
    }

    #[test]
    fn uris() {
        assert_eq!(uri_id("spotify:track:abc"), Some("abc"));
        assert_eq!(uri_kind("spotify:playlist:x"), Some("playlist"));
        assert_eq!(
            open_spotify_url("spotify:album:z").as_deref(),
            Some("https://open.spotify.com/album/z")
        );
    }

    #[test]
    fn html_is_stripped() {
        assert_eq!(
            strip_html("Hi <a href=\"x\">there</a> &amp; you"),
            "Hi there & you"
        );
        assert_eq!(strip_html("ONE&#x2F;TWO&#x2F;THREE"), "ONE/TWO/THREE");
    }
}
