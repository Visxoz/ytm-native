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

- **Browse and search.** Home (trending, new releases, top artists, charts, moods), Explore (moods, genres, new albums and videos), search with suggestions, and artist, album, playlist and genre pages.
- **Plays like YouTube Music.** Playing a song from search starts a radio after it, and autoplay keeps going when the queue ends. Up Next supports play next, add to queue, remove and clear; shuffle reorders what's coming and restores the order when turned off; repeat all and repeat one.
- **Lyrics** in the side panel when YouTube has them.
- **Native audio.** Streams are downloaded in ranges, demuxed and decoded in-process (AAC through Symphonia, Opus through `opus-decoder`) and played through WASAPI. The next track's stream is resolved ahead of time, and YouTube's loudness normalisation is applied.
- **Windows integration.** Media keys and the Windows media overlay, a single instance, and the window's size and position are remembered.
- **Discord Rich Presence**, ported from ytm-lite.
- **Your library.** Sign in by pasting your YouTube cookies to see your playlists, albums, artists and listening history.

Keyboard: <kbd>Space</kbd> play/pause, <kbd>Ctrl</kbd>+<kbd>F</kbd> search, <kbd>Ctrl</kbd>+<kbd>←</kbd>/<kbd>→</kbd> previous/next, <kbd>Alt</kbd>+<kbd>←</kbd> back. Right-click a song for more actions.

## How it works

| Part | Crate or tool |
| --- | --- |
| YouTube Music data (search, pages, radio, lyrics, library) | [`rustypipe`](https://codeberg.org/ThetaDev/rustypipe) |
| Stream URLs | [yt-dlp](https://github.com/yt-dlp/yt-dlp), downloaded to `%APPDATA%\ytm-native\bin` on first run and kept up to date |
| Demuxing and AAC | [`symphonia`](https://github.com/pdeljanov/Symphonia) |
| Opus | [`opus-decoder`](https://crates.io/crates/opus-decoder) (pure Rust) |
| Audio output | [`cpal`](https://github.com/RustAudio/cpal) (WASAPI) |
| UI | [Slint](https://slint.dev) |
| Media keys | [`souvlaki`](https://github.com/Sinono3/souvlaki) |

Stream URLs are the one part handled outside Rust. YouTube scrambles them with its player JavaScript, which changes every few weeks. rustypipe's deobfuscator no longer keeps up, so the app hands that single step to yt-dlp, which is actively maintained. yt-dlp needs a JavaScript runtime such as Deno or Node.js on `PATH` to solve YouTube's challenges.

AAC is the default. The pure-Rust Opus decoder is correct but uses about ten times the CPU (roughly 20% of a core against 2%), so Opus is a setting.

## Build

You need Rust (the MSVC toolchain).

```sh
cargo run             # debug build with logging
cargo build --release # target/release/ytm-native.exe
cargo test
```

Settings, the rustypipe cache, cookies and yt-dlp live in `%APPDATA%\ytm-native`.

## Signing in

Open music.youtube.com in a private browser window and sign in. Then copy either a cookies.txt export or the `Cookie` request header from the developer tools (Network tab), paste it into **Sign in**, and close the private window. YouTube rotates the cookies of sessions that stay open.

This is an unofficial client using undocumented APIs; a dedicated account is the safer choice.
