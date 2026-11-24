# SwiftSpot

A native Spotify client for the MacOS.

Instead of using Electron and wasting about a gigabyte of RAM to render UI, this has a SwiftUI frontend built on top of a small Rust backend that uses librespot to play audio, bringing RAM usage down to about 140 MB. 

![SwiftSpot's Home](screenshots/home.png)

Additional features
- Karaoke mode - A local model removes vocals from a song in real time so you can sing along with the lyrics page
- Liquid glass/other Apple-style UI elements
