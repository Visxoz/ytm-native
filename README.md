<div align="center">

<img src="docs/banner.svg" width="100%" alt="ytm-native: YouTube Music, as a real Windows app">

<br>

**Fast, light and fully native: no browser tab, no Electron, just music.**

<br>

[![Latest release](https://img.shields.io/github/v/release/Visxoz/ytm-native?style=for-the-badge&label=release&color=FF0033)](https://github.com/Visxoz/ytm-native/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/Visxoz/ytm-native/total?style=for-the-badge&color=FF0033)](https://github.com/Visxoz/ytm-native/releases)
[![Windows](https://img.shields.io/badge/platform-Windows-0078D4?style=for-the-badge&logo=windows11&logoColor=white)](#install)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-B7410E?style=for-the-badge&logo=rust&logoColor=white)](#under-the-hood)

<br>

<a href="https://github.com/Visxoz/ytm-native/releases/latest">
  <img src="https://img.shields.io/badge/Download%20for%20Windows-FF0033?style=for-the-badge&logo=windows11&logoColor=white" alt="Download for Windows" height="44">
</a>

<br><br>

<img src="docs/screenshot.png" width="900" alt="ytm-native showing an artist page, with the queue open and a song playing">

</div>

<br>

## Why ytm-native

<table>
<tr>
<td width="50%" valign="top">

### Light as a feather
A single portable exe that starts in a moment and stays around 160 MB while you browse. No runtime to install, nothing running in the background.

</td>
<td width="50%" valign="top">

### Sounds better
Always the best stream YouTube offers, and **256 kbps Opus** with YouTube Premium. Volume levels out between songs, like on YouTube Music.

</td>
</tr>
<tr>
<td width="50%" valign="top">

### Feels at home
A clean dark interface with headers tinted from the artwork, a collapsible library, smooth animations, and lyrics and the queue a click away.

</td>
<td width="50%" valign="top">

### Made for Windows
Media keys, the Windows media overlay, Discord status, and a window that remembers where you left it, down to the song and position.

</td>
</tr>
<tr>
<td width="50%" valign="top">

### Plays like YouTube Music
Search starts a radio after the song you pick, autoplay keeps going, and shuffle, repeat, play next and the queue all work as you'd expect.

</td>
<td width="50%" valign="top">

### Your library, safely
Sign in on Google's own page to get your playlists, albums, artists and Liked music. Your sign-in is stored encrypted, for your Windows account only.

</td>
</tr>
</table>

## Take a look

<table>
<tr>
<td width="62%" align="center" valign="top">
<img src="docs/artist.png" alt="An artist page with its header tinted from the artwork">
<br><sub><b>Pages that feel alive.</b> Headers take their colour from the artwork.</sub>
</td>
<td width="38%" align="center" valign="top">
<img src="docs/queue.png" alt="The queue panel, with the playing song highlighted">
<br><sub><b>Up next and lyrics</b>, one click away.</sub>
</td>
</tr>
<tr>
<td colspan="2" align="center">
<img src="docs/player.png" alt="The player bar, showing an Opus stream">
<br><sub><b>Always the best stream</b>, with the codec and bitrate right there.</sub>
</td>
</tr>
</table>

## Install

1. Download `ytm-native_<version>_x64-portable.exe` from the [latest release](https://github.com/Visxoz/ytm-native/releases/latest).
2. Put it wherever you like and run it. There's nothing to install.
3. Optionally, choose **Sign in** and then **Sign in with Google** to bring your library over.

> [!NOTE]
> The exe isn't code-signed yet, so Windows SmartScreen may say it "protected your PC". Choose **More info**, then **Run anyway**.

> [!TIP]
> Signing in uses Microsoft Edge WebView2, which comes with Windows 11 and most Windows 10 PCs. If it's missing, you can paste cookies instead; see [Signing in](#signing-in).

## Shortcuts

| Keys | Action |
| :-- | :-- |
| <kbd>Space</kbd> | Play or pause |
| <kbd>Ctrl</kbd> + <kbd>K</kbd> | Search |
| <kbd>Ctrl</kbd> + <kbd>B</kbd> | Show or hide the library |
| <kbd>Ctrl</kbd> + <kbd>←</kbd> / <kbd>→</kbd> | Previous or next song |
| <kbd>Alt</kbd> + <kbd>←</kbd> | Back |
| Right-click a song | Play next, add to queue, radio, go to artist or album |

## Signing in

Choose **Sign in**, then **Sign in with Google**, and sign in on Google's page as usual. The window closes by itself once you're in.

<details>
<summary><b>Prefer pasting cookies?</b></summary>
<br>

Open music.youtube.com in a private browser window and sign in. Copy either a cookies.txt export or the `Cookie` request header from the developer tools (Network tab), paste it under **Paste cookies instead**, and close the private window. YouTube rotates the cookies of sessions that stay open.

</details>

> [!WARNING]
> ytm-native is an unofficial client. It uses YouTube's undocumented API, which is against YouTube's terms of service. Google could ask you to confirm you're not a bot or sign the session out, and could in principle restrict the account. A separate account is the safest choice.

<br>

## Under the hood

<details>
<summary><b>How it works</b></summary>
<br>

The interface, the player and the audio pipeline are all native Rust. A browser engine only appears while you sign in, to show Google's page.

| Part | Built on |
| :-- | :-- |
| YouTube Music data: search, pages, radio, lyrics, library | [`rustypipe`](https://codeberg.org/ThetaDev/rustypipe) |
| Stream URLs | YouTube's player endpoint, called directly (`src/streams.rs`) |
| Demuxing and AAC | [`symphonia`](https://github.com/pdeljanov/Symphonia) |
| Opus | libopus 1.3.1, as Rust ([`unsafe-libopus`](https://crates.io/crates/unsafe-libopus)) |
| Audio output | [`cpal`](https://github.com/RustAudio/cpal), through WASAPI |
| Interface | [Slint](https://slint.dev), drawn by Skia on the GPU through Direct3D 12 |
| Networking | `reqwest` over Windows SChannel and HTTP/2 |
| Google sign-in | WebView2 through [`wry`](https://github.com/tauri-apps/wry), only while signing in |
| Media keys | [`souvlaki`](https://github.com/Sinono3/souvlaki) |

Most YouTube clients get stream URLs scrambled by YouTube's player JavaScript. The visionOS client gets plain ones, so the app asks as that client and needs no JavaScript engine. With Premium, the app unscrambles the web player's URLs itself, in a short-lived QuickJS context with a cut-down copy of the player. When YouTube changes this, `src/streams.rs` and `src/premium` are the places to update.

Streams download in ranges and are decoded in the app; the next song's stream is looked up ahead of time. Skia draws on the GPU, so scrolling costs little CPU, and animations only run while something changes.

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
cargo build --release  # target/release/ytm-native.exe
cargo test
```

The patches:

- `i-slint-common` memory-maps the fallback fonts instead of reading them into memory, which saves about 18 MB.
- `i-slint-renderer-skia` draws fallback fonts for CJK and emoji from those mappings instead of private copies.
- `rustypipe` opens playlists the signed-in user owns, and returns an error instead of crashing when YouTube refuses a visitor ID.

Settings, caches and your sign-in live in `%APPDATA%\ytm-native`; the sign-in is encrypted with Windows' DPAPI. `YTM_LOG=info` turns on logging in release builds.

</details>

<br>

<div align="center">

<sub>Not affiliated with, endorsed by or connected to YouTube or Google. YouTube Music is a trademark of Google LLC.</sub>

</div>
