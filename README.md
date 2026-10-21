<div align="center">

<img src="app-icon.png" width="96" alt="ytm-native icon">

# ytm-native

**A fully native YouTube Music client for Windows, written in Rust.**<br>
No WebView, no browser engine: the UI, the player and the audio pipeline are all native.

![Rust](https://img.shields.io/badge/Rust-2024-B7410E?logo=rust&logoColor=white)
![Slint](https://img.shields.io/badge/UI-Slint-2379F4)
![Windows](https://img.shields.io/badge/platform-Windows-0078D4)

<img src="docs/screenshot.png" width="820" alt="ytm-native playing a song with the queue open">

</div>

## Why

The `master` branch is ytm-lite, a Tauri window around music.youtube.com. This branch is a personal experiment in the opposite direction: rebuild the client itself, talking to YouTube's unofficial API, with its own interface.

## What it does

- **A Spotify-style interface in YouTube Music red.** Rounded panels, a collapsible Your Library sidebar with your playlists, page headers tinted from the artwork, a sticky title bar, quick picks on Home, and animated hover, press, panel and page transitions.
- **Browse and search.** Home (trending, new releases, top artists, charts, moods), Explore (moods, genres, new albums and videos), search with suggestions, and artist, album, playlist and genre pages.
- **Plays like YouTube Music.** Playing a song from search starts a radio after it, and autoplay keeps going when the queue ends. Up Next supports play next, add to queue, remove and clear; shuffle reorders what's coming and restores the order when turned off; repeat all and repeat one.
- **Lyrics** in the side panel when YouTube has them.
- **Native audio.** Streams are downloaded in ranges, demuxed and decoded in-process and played through WASAPI. It always picks the best stream YouTube offers, normally Opus at about 160 kbps. The next track's stream is resolved ahead of time, and YouTube's loudness normalisation is applied.
- **Windows integration.** Media keys and the Windows media overlay, a single instance, and the window's size and position are remembered.
- **Discord Rich Presence**, ported from ytm-lite.
- **Your library.** Sign in by pasting your YouTube cookies to see your playlists, albums, artists and listening history.

Keyboard: <kbd>Space</kbd> play/pause, <kbd>Ctrl</kbd>+<kbd>K</kbd> search, <kbd>Ctrl</kbd>+<kbd>B</kbd> sidebar, <kbd>Ctrl</kbd>+<kbd>←</kbd>/<kbd>→</kbd> previous/next, <kbd>Alt</kbd>+<kbd>←</kbd> back. Right-click a song for more actions.

## How it works

| Part | Crate or tool |
| --- | --- |
| YouTube Music data (search, pages, radio, lyrics, library) | [`rustypipe`](https://codeberg.org/ThetaDev/rustypipe) |
| Stream URLs | YouTube's player endpoint, called directly (`src/streams.rs`) |
| Demuxing and AAC | [`symphonia`](https://github.com/pdeljanov/Symphonia) |
| Opus | libopus 1.3.1, as Rust ([`unsafe-libopus`](https://crates.io/crates/unsafe-libopus)) |
| Audio output | [`cpal`](https://github.com/RustAudio/cpal) (WASAPI) |
| UI | [Slint](https://slint.dev) with Skia drawing on the GPU (Direct3D 12) |
| HTTPS | Windows SChannel through `reqwest` |
| Media keys | [`souvlaki`](https://github.com/Sinono3/souvlaki) |

Most YouTube clients get stream URLs scrambled by YouTube's player JavaScript. The visionOS client gets plain ones, so the app asks as that client and needs no JavaScript engine or external tool. When YouTube changes this, `src/streams.rs` is the one place to update.

## Footprint

Measured on the release build while playing Opus:

| | |
| --- | --- |
| CPU | about 1% of one core |
| Memory | about 100 MB in Task Manager, 165 MB working set |
| Download | one 22 MB exe, no runtime to install |

Skia draws on the GPU through Direct3D 12, so scrolling takes about a quarter of one core rather than all of it; the graphics driver accounts for about 50 MB of the memory. Without a usable GPU, Skia draws into a CPU buffer instead. Animations run only while something changes; the equaliser steps at 8 frames a second rather than repainting continuously. libopus decodes about 700 times faster than real time.

## Build

You need Rust (the MSVC toolchain).

```sh
cargo run             # debug build with logging
cargo build --release # target/release/ytm-native.exe
cargo test
```

Settings, the rustypipe cache and your sign-in live in `%APPDATA%\ytm-native`.

## Signing in

Open music.youtube.com in a private browser window and sign in. Then copy either a cookies.txt export or the `Cookie` request header from the developer tools (Network tab), paste it into **Sign in**, and close the private window. YouTube rotates the cookies of sessions that stay open.

This is an unofficial client using undocumented APIs; a dedicated account is the safer choice.
