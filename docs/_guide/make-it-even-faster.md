---
title: Your Own Spotify App
description: "Setting up the Spotify app SwiftSpot signs in with, and scrolling performance."
nav_order: 6
---

## Scrolling and large libraries

The sidebar is drawn alongside every page, so its sorting work can affect
scrolling elsewhere in the window. Library ordering now looks up each row's
position once per sort instead of repeatedly searching the full saved order.
This reduces the work each frame needs for large libraries while preserving
your chosen sort, pins, and folders.

The main window draws as needed and uses the platform's frame pacing. The
MilkDrop frame-rate setting applies to its separate visualizer window.

## Your own Spotify app

SwiftSpot signs in through a free Spotify app of your own, registered on
Spotify's developer website. You do not write code or install anything
else. The app SwiftSpot used to share no longer takes new sign-ins, and
Spotify gives no single app the allowance to serve everyone.

The sign-in screen walks you through it:

1. Open the [Spotify developer dashboard](https://developer.spotify.com/dashboard)
   and click **Create app**. Any name will do. Spotify asks that the account
   have Premium.
2. Under **Redirect URIs**, add exactly `http://127.0.0.1:8989/login`. The
   sign-in screen has a **Copy** button for it.
3. Under **APIs used**, tick **Web API**, accept the terms, and save.
4. Paste the app's **Client ID** into the sign-in screen and click
   **Sign In with Spotify**.

SwiftSpot checks with Spotify that it knows the Client ID before your
browser opens. If Spotify then shows "Invalid redirect URI", the address in
step 2 is missing or differs; fix it in the dashboard and sign in again.
Spotify lets only the account that created the app use it.

To move to another app later, open **Settings ▸ Account** and click
**Change…**.

## API rate limits

Spotify limits how often an app can ask for information. If you create
several apps, they share your account's allowance, following Spotify's
[July 2026 quota update](https://developer.spotify.com/blog/2026-07-23-web-api-quota-updates).
Apps created since November 2024 cannot read Spotify-owned playlists,
recommendations or related artists. Setting up playback on this computer
lets SwiftSpot read playlists through its music connection instead.
[How it connects](/how-it-connects/) explains which connection each
feature uses.

Spotify allows these apps ten search results at a time for each type.
