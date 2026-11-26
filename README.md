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
