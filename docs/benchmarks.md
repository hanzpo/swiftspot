# Benchmarks

Measured on 2026-11-25 on an Apple M5 MacBook with 24 GB of memory and
macOS 27.2.1.

| | SwiftSpot 1.2.0 | Spotifast 0.17.2 | Spotify 1.3.10 |
|---|---|---|---|
| Download size | 21 MB | 66 MB | 422 MB |
| Processes | 1 | 1 | 6 |
| Memory, idle | 109 MB | 177 MB | 726 MB |
| Memory, playing | 117 MB | 195 MB | 783 MB |
| CPU time per minute, idle | 0.03 s | 0.62 s | 1.73 s |
| CPU time per minute, playing | 1.44 s | 1.86 s | 2.74 s |
| Wakeups per minute, idle | 23 | 82 | 780 |
| Threads, idle | 21 | 26 | 160 |

Against the Spotify app, SwiftSpot uses 6.7 times less memory, 58 times
less CPU time at idle, about half the CPU time while playing, and is a
20 times smaller download. Against Spotifast, it uses 38% less memory,
20 times less CPU time at idle, and 23% less CPU time while playing.

## How it was measured

- All three apps were signed in to the same Spotify Premium account, with
  their main window open, and ran one at a time.
- Each app had 30 seconds to settle after launch, then was measured for 60
  seconds idle, then for 60 seconds playing the same song. The Mac was
  muted.
- Memory is macOS's physical footprint (`footprint`), the figure Activity
  Monitor shows as Memory, summed over all of an app's processes.
- CPU time is the total from `ps` over the window. Wakeups are idle wakeups
  from `top`.
- Spotifast used its built-in librespot player, not Spotify Soloist, so it
  and SwiftSpot play through the same engine.
- Download size is the installed app bundle.

## Limits

- One run per app, except Spotify idle, measured twice (1.51 s and 1.73 s
  of CPU time per minute, 713 MB and 726 MB). The table uses the second run.
- One Mac and one library. A larger library raises memory in every app.
- SwiftSpot and Spotifast stream at 320 kbps. Spotify was set to Very High
  (320 kbps), which was not checked by the script.

Raw results are in `benchmark-results-2026-11-25.jsonl`. To run it again,
quit all three apps and run `python3 scripts/bench/bench.py`.
