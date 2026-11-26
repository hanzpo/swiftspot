# SwiftSpot

A native Spotify client for the MacOS.

Install with `brew install hanzpo/tap/swiftspot`

Instead of using Electron and wasting about a gigabyte of RAM to render UI, this has a SwiftUI frontend built on top of a small Rust backend that uses librespot to play audio, bringing RAM usage down to about 110 MB.

| | SwiftSpot | Spotify |
|---|---|---|
| Memory, idle | 109 MB | 726 MB |
| CPU time per minute, idle | 0.03 s | 1.73 s |
| CPU time per minute, playing | 1.44 s | 2.74 s |
| Download size | 21 MB | 422 MB |

Measured on an M5 MacBook, both apps signed in with their windows open. Spotifast is included in [the full results](docs/benchmarks.md).

![SwiftSpot's Home](screenshots/home.png)

Additional features
- Karaoke mode - A local model removes vocals from a song in real time so you can sing along with the lyrics page
- Liquid glass/other Apple-style UI elements
- A terminal player, `swiftspot`, that controls the app from any terminal

## In the terminal

The app includes `swiftspot`, a terminal front end for the running app. It shows your library, playlists, search, the queue and lyrics, with the cover in terminals that draw images (Rex, Ghostty, kitty, WezTerm). It plays through the app, so both stay in step, and it starts the app in the background when needed.

```sh
ln -s /Applications/SwiftSpot.app/Contents/Helpers/swiftspot ~/.local/bin/swiftspot
swiftspot          # the player; press ? for keys
swiftspot toggle   # also next, previous, like, now
```

The app and the terminal player talk over a socket in your private temporary folder that only you can open. Nothing is served on the network.

### Rex

`rex/swiftspot.lua` adds SwiftSpot to [Rex](https://www.superlogical.com/rex): an "Open SwiftSpot" action that opens the player beside the current pane, and actions for play or pause, next, previous and like. Copy it beside your Rex config and load it from `~/.config/rex/init.lua`:

```lua
require("swiftspot").setup{
  keys = { open = "cmd+shift+m", toggle = "cmd+shift+p" },
}
```
