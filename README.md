<div align="center">

<img src="docs/banner.svg" width="100%" alt="Lily: a native YouTube Music client for Windows">

<br>

[![Release](https://img.shields.io/github/v/release/Visxoz/lily?style=flat&labelColor=18181b&color=d65db1&label=release)](https://github.com/Visxoz/lily/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/Visxoz/lily/total?style=flat&labelColor=18181b&color=d65db1&label=downloads)](https://github.com/Visxoz/lily/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/Visxoz/lily/ci.yml?branch=main&style=flat&labelColor=18181b&label=ci)](https://github.com/Visxoz/lily/actions/workflows/ci.yml)
[![Windows 10 and 11](https://img.shields.io/badge/Windows-10%20%7C%2011-0078d4?style=flat&labelColor=18181b&logo=windows11&logoColor=white)](#install)
[![Rust](https://img.shields.io/badge/Rust-2024-b7410e?style=flat&labelColor=18181b&logo=rust&logoColor=white)](#under-the-hood)
[![Licence](https://img.shields.io/github/license/Visxoz/lily?style=flat&labelColor=18181b&color=3f3f46&label=licence)](LICENSE)

<br>

<a href="https://github.com/Visxoz/lily/releases/latest"><img src="docs/download.svg" width="300" alt="Download for Windows"></a>

<sub>On an iPhone? There's <a href="https://github.com/Visxoz/Lily-ios">Lily for iPhone</a> too.</sub>

<br>

<img src="docs/hero.png" width="100%" alt="Lily in the black look, with an artist page, and synced lyrics tinted from the artwork">

<sub>Black look · lyrics that follow the song, tinted from its artwork</sub>

<br><br>

<img src="docs/stats.svg" width="100%" alt="34 MB, one exe. 0.1% CPU while a song plays. 0 MB of GPU memory once minimised. 256 kbps Opus with YouTube Premium.">

</div>

<br>

<table>
<tr>
<td width="33%" valign="top">

### Sounds better
256 kbps Opus with Premium, gapless playback, a crossfade of your chosen length, volume levelling, a ten-band equaliser, and crossfeed, mono and balance for headphones.

</td>
<td width="33%" valign="top">

### Weighs nothing
One 34 MB exe, drawn by the GPU, using 0.1% of the CPU while it plays. Minimised, it hands back its GPU memory and drops to about 34 MB of RAM.

</td>
<td width="33%" valign="top">

### Looks the part
Pure black and orchid out of the box, or dark or light, in eight other accents including one that follows the song's artwork. Pages are tinted to match.

</td>
</tr>
<tr>
<td width="33%" valign="top">

### All of YouTube Music
Radio, autoplay, Smart Shuffle, repeat, history and search with recent searches. Artist pages carry the whole discography.

</td>
<td width="33%" valign="top">

### Lyrics and queue
Synced lyrics you can click to jump to, beside the page or full screen. A queue you drag into order, edit several songs at a time, and save as a playlist.

</td>
<td width="33%" valign="top">

### At home on Windows
Media keys, the Windows media overlay, Discord, Last.fm. Reopens on the song you left, at the second you left it.

</td>
</tr>
</table>

## Also on iPhone

<a href="https://github.com/Visxoz/Lily-ios"><img src="docs/ios.svg" width="100%" alt="Lily for iPhone: the same Lily, built again in Swift and SwiftUI, free for iOS 26 and later"></a>

[Lily for iPhone](https://github.com/Visxoz/Lily-ios) is Lily built again in Swift and SwiftUI, on Apple's own audio engine, with the same orchid look, Premium streams, gapless playback, crossfade, equaliser and synced lyrics. It adds offline downloads, the lock screen and Control Center, your AirPods and a Liquid Glass tab bar. Add it to AltStore, SideStore or Feather for updates as they come, or grab it from its [releases](https://github.com/Visxoz/Lily-ios/releases/latest).

## Full screen

<img src="docs/fullscreen.png" width="100%" alt="The full-screen player: the artwork, its title and the next song beside large synced lyrics, over the artwork's colours">

The button beside the queue's takes the window full screen: the artwork and its lyrics over the artwork's own colours, with the queue a click away in a drawer. <kbd>Esc</kbd> leaves.

## Make it yours

<table>
<tr>
<td width="50%"><img src="docs/dark.png" alt="The dark look on an artist page, with the queue open"></td>
<td width="50%"><img src="docs/light.png" alt="The light look on an album page"></td>
</tr>
<tr>
<td align="center"><b>Dark</b> · artist pages with the queue beside them</td>
<td align="center"><b>Light</b> · album pages tinted from the cover</td>
</tr>
<tr>
<td width="50%"><img src="docs/home.png" alt="Home, tinted with the accent colour"></td>
<td width="50%"><img src="docs/explore.png" alt="Explore's moods and genres in the light look"></td>
</tr>
<tr>
<td align="center"><b>Home</b> · tinted with your accent</td>
<td align="center"><b>Explore</b> · moods, genres and new releases</td>
</tr>
</table>

Lily starts in pure black with its own orchid. Pick another look and accent in **Settings ▸ Appearance**: Lily, Red, Orange, Green, Teal, Blue, Purple, Pink, or **From the artwork**.

## Sounds as good as YouTube gets

<table>
<tr>
<td width="50%"><img src="docs/quality.svg" alt="Audio quality: Low about 50 kbps, Normal about 130 kbps, High up to 256 kbps with Premium"></td>
<td width="50%"><img src="docs/equaliser.png" alt="The equaliser with the Bass boost preset"></td>
</tr>
</table>

| Feature | What it does |
| --- | --- |
| **Premium streams** | Signed in with Premium on High, songs play in YouTube's 256 kbps formats, and the player bar says so. Music videos swap to their song versions to get them. |
| **Gapless** | The next song is lined up 15 seconds early and follows without a gap, and the screen changes the moment the sound does. |
| **Crossfade** | Fades each song into the next over 1 to 12 seconds (6 by default), cutting the silence at the joins. An album played in order still runs straight on, and skipping never fades. |
| **Equaliser** | Ten bands from 31 Hz to 16 kHz, ±12 dB each, with presets: Flat, Bass boost, Treble boost, Vocal, Rock, Electronic and Acoustic. Boosting turns the rest down to leave headroom. |
| **Normalise volume** | Plays every track at a similar loudness, from YouTube's own measurement: loud ones turned down and quiet ones up, at Normal or a Quiet level 6 dB lower, with a limiter so nothing clips. |
| **No clicks, no drop-outs** | Pausing ramps the sound down, and seeks and skips fade in. Unplug your headphones and the music pauses, or with **Pause when unplugged** off, carries on through the next device. |

## Light enough to forget about

<img src="docs/minimised.svg" width="100%" alt="Window open vs minimised, while a song plays: RAM 91 to 34 MB, committed memory 239 to 40 MB, GPU memory 125 to 0 MB, CPU 0.13 to 0.03 percent">

When you minimise it, the app throws away its GPU surface and drawing caches, and builds them again the moment you bring it back. Pause the music and the audio device stops too, so the app sleeps and Windows can as well.

<details>
<summary><b>How these were measured</b></summary>
<br>

The 0.1.19 release on Windows 11 (16 logical processors), signed in with Premium, sampled with Windows' own performance counters after each state had settled. The minimised figures were taken five minutes in, as memory keeps falling for the first few minutes.

| State | RAM (Task Manager) | Committed | GPU memory | CPU | CPU, one core |
| --- | ---: | ---: | ---: | ---: | ---: |
| Home open, nothing playing | 75 MB | 222 MB | 125 MB | 0% | 0.05% |
| Playing, window open | 91 MB | 239 MB | 125 MB | 0.13% | 2.0% |
| Playing, minimised | 34 MB | 40 MB | 0 MB | 0.03% | 0.5% |
| Paused, minimised | 33 MB | 39 MB | 0 MB | 0.01% | 0.2% |

RAM is the private working set, the figure Task Manager's Memory column shows. Committed is private bytes. GPU memory is dedicated video memory.

</details>

## Install

Grab one from the [latest release](https://github.com/Visxoz/lily/releases/latest):

<table>
<tr>
<td width="50%" valign="top">

**Installer** · `lily_<version>_x64-setup.exe`

Installs for your account only, with no admin prompt, a Start menu entry and an uninstaller.

</td>
<td width="50%" valign="top">

**Portable** · `lily_<version>_x64-portable.exe`

Put it anywhere and run it. Nothing is installed.

</td>
</tr>
</table>

Both keep themselves up to date. When a new version is out, a dot appears on the account button, and **Settings ▸ About ▸ Updates** installs it and restarts. Every download is checked against the release's SHA-256 checksums first.

> [!NOTE]
> The app isn't code-signed yet, so SmartScreen may say it "protected your PC": choose **More info**, then **Run anyway**. If Smart App Control blocks it, right-click the file, choose **Properties**, tick **Unblock**, and run it again.

## Shortcuts

| Keys | Does |
| --- | --- |
| <kbd>Space</kbd> | Play or pause |
| <kbd>Ctrl</kbd> <kbd>←</kbd> / <kbd>Ctrl</kbd> <kbd>→</kbd> | Previous or next song |
| <kbd>Ctrl</kbd> <kbd>K</kbd> | Search (<kbd>Ctrl</kbd> <kbd>F</kbd> and <kbd>Ctrl</kbd> <kbd>L</kbd> work too) |
| <kbd>Ctrl</kbd> <kbd>B</kbd> | Collapse or expand the library sidebar |
| <kbd>Alt</kbd> <kbd>←</kbd> / <kbd>Alt</kbd> <kbd>→</kbd> | Back or forward |
| <kbd>Delete</kbd> | Remove the songs picked in the queue |
| <kbd>Esc</kbd> | Close a dialog, clear the queue's selection, or leave full screen |

Right-click a song to play it next, add it to the queue, start a radio, like it, save it to a playlist, or go to its artist or album. In the queue, <kbd>Ctrl</kbd>-click and <kbd>Shift</kbd>-click pick several songs at once.

## Your account

Choose **Sign in** in the sidebar (or in **Settings ▸ Account**), then **Sign in with Google**, and sign in on Google's own page. The window closes by itself once you're in. Your playlists and Liked music appear in the sidebar, where a right-click pins one to the top, your albums and artists in Library, Home becomes your own feed, and what you play here counts towards your YouTube Music history and recommendations.

Your own playlists can be edited from their page: drag songs into a new order, remove them, or rename, share or delete the playlist from the header's menu (making one public or unlisted needs a YouTube channel on the account). Any other album or playlist can be saved to your library from its header, and artists subscribed to. **Copy link** is in the menus of songs, the queue, cards, page headers, the player bar and the sidebar's playlists.

The sign-in is stored encrypted for your Windows account only, with DPAPI. Signing in uses Microsoft Edge WebView2, which comes with Windows 11 and most Windows 10 PCs.

<details>
<summary><b>Prefer pasting cookies?</b></summary>
<br>

Open music.youtube.com in a private browser window and sign in. Copy either a cookies.txt export or the `Cookie` request header from the developer tools (Network tab), paste it under **Paste cookies instead**, and close the private window. YouTube rotates the cookies of sessions that stay open.

</details>

> [!WARNING]
> Lily is an unofficial client. It uses YouTube's undocumented API, which is against YouTube's terms of service. Google could ask you to confirm you're not a bot or sign the session out, and could in principle restrict the account. A separate account is the safest choice.

## Under the hood

Everything you see and hear is native code, written in Rust. A browser engine only appears while you sign in, to show Google's page.

```mermaid
flowchart LR
    yt(["YouTube Music"]) -->|rustypipe| pages["Pages, search,<br/>radio, lyrics"]
    yt -->|player endpoint| url["Stream URL<br/>visionOS, then Android VR, then iOS"]
    premium["Premium<br/>web player's code, cut down by oxc,<br/>run in a throwaway QuickJS"] -.-> url
    url --> dl["Download<br/>1 MB ranges, into memory"]
    dl --> dec["Decode<br/>Symphonia, libopus"]
    dec --> fx["Levelling, EQ,<br/>gapless, crossfade"]
    fx --> out(["WASAPI"])
    pages --> ui["Slint UI<br/>Skia on Direct3D 12"]
```

<details>
<summary><b>What it's built with</b></summary>
<br>

| Part | Built with |
| --- | --- |
| YouTube Music data | [rustypipe](https://codeberg.org/ThetaDev/rustypipe) |
| Stream URLs | YouTube's player endpoint, called directly (`src/streams`) |
| Premium URLs | [oxc](https://oxc.rs) and [QuickJS](https://github.com/DelSkayn/rquickjs) |
| Demuxing and AAC | [Symphonia](https://github.com/pdeljanov/Symphonia) |
| Opus | libopus 1.3.1, as Rust ([unsafe-libopus](https://crates.io/crates/unsafe-libopus)) |
| Audio output | [cpal](https://github.com/RustAudio/cpal), through WASAPI |
| Interface | [Slint](https://slint.dev), drawn by Skia on the GPU through Direct3D 12 |
| Networking | reqwest over Windows SChannel and HTTP/2, on Tokio |
| Google sign-in | WebView2 through [wry](https://github.com/tauri-apps/wry), only while signing in |
| Media keys | [souvlaki](https://github.com/Sinono3/souvlaki) |

Most YouTube clients get stream URLs that YouTube's player JavaScript has to unscramble. The visionOS client gets plain ones, so the app asks as that client and needs no JavaScript engine, falling back to Android VR and iOS if YouTube changes it. A daily check flags it when one of them stops working. With Premium, the app unscrambles the web player's URLs itself, in a short-lived QuickJS context with a cut-down copy of the player.

The app talks only to YouTube's hosts, plus GitHub for updates and Last.fm if you connect it, each checked against an allowlist. It keeps everything in `%APPDATA%\Lily`, and moves the folder of versions named ytm-native there on its first start.

</details>

<details>
<summary><b>Building from source</b></summary>
<br>

You need Rust with the MSVC toolchain. Three crates are built from `vendor/` with a patch from `patches/` applied, so vendor them once after cloning, and again after updating Slint or rustypipe:

```powershell
./scripts/vendor-patched.ps1
```

```powershell
cargo run              # debug build with logging
cargo build --release  # target/release/lily.exe
cargo test
```

The patches:

- `i-slint-common` memory-maps the fallback fonts instead of reading them into memory.
- `i-slint-renderer-skia` draws those fonts from the mappings, and drops its GPU surface and caches while the window is minimised.
- `rustypipe` opens playlists the signed-in user owns, fetches visitor IDs from YouTube's API rather than the Music home page, and returns an error instead of crashing when YouTube refuses one.

Release builds write warnings, errors and crashes to `%APPDATA%\Lily\lily.log`, with every URL cut down to its host; `YTM_LOG=info` (or `debug`) logs more. A request that crashes fails on its own rather than closing the app.

</details>

## Licence

Lily is free software under the [GNU General Public License, version 3](LICENSE). It builds on [rustypipe](https://codeberg.org/ThetaDev/rustypipe), which is GPL-3.0, and on [Slint](https://slint.dev) under its GPL-3.0 option.

<br>

<div align="center">
<sub>Not affiliated with, endorsed by or connected to YouTube or Google. YouTube Music is a trademark of Google LLC.</sub>
</div>
