//! Windows media overlay and media keys, through the System Media Transport
//! Controls.

use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

const SEEK_STEP: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Toggle,
    Play,
    Pause,
    Next,
    Previous,
    Stop,
    /// Seek relative to the current position, in seconds.
    SeekBy(f64),
    SeekTo(f64),
}

pub struct Controls {
    inner: MediaControls,
}

impl Controls {
    pub fn new(
        hwnd: *mut std::ffi::c_void,
        on_action: impl Fn(Action) + Send + 'static,
    ) -> Option<Self> {
        let config = PlatformConfig {
            display_name: "YouTube Music",
            dbus_name: "ytm_native",
            hwnd: Some(hwnd),
        };
        let mut inner = MediaControls::new(config)
            .map_err(|e| log::warn!("media controls unavailable: {e:?}"))
            .ok()?;
        inner
            .attach(move |event| {
                let action = match event {
                    MediaControlEvent::Toggle => Action::Toggle,
                    MediaControlEvent::Play => Action::Play,
                    MediaControlEvent::Pause => Action::Pause,
                    MediaControlEvent::Next => Action::Next,
                    MediaControlEvent::Previous => Action::Previous,
                    MediaControlEvent::Stop => Action::Stop,
                    MediaControlEvent::Seek(dir) => Action::SeekBy(signed(dir, SEEK_STEP)),
                    MediaControlEvent::SeekBy(dir, by) => Action::SeekBy(signed(dir, by)),
                    MediaControlEvent::SetPosition(MediaPosition(at)) => {
                        Action::SeekTo(at.as_secs_f64())
                    }
                    _ => return,
                };
                on_action(action);
            })
            .map_err(|e| log::warn!("could not attach media controls: {e:?}"))
            .ok()?;
        Some(Self { inner })
    }

    pub fn set_track(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        cover: Option<&str>,
        duration: f64,
    ) {
        let metadata = MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: (!album.is_empty()).then_some(album),
            // Windows fetches the artwork itself, so only from YouTube.
            cover_url: cover.filter(|url| crate::http::allowed(url)),
            duration: (duration > 0.0).then(|| Duration::from_secs_f64(duration)),
        };
        if let Err(e) = self.inner.set_metadata(metadata) {
            log::debug!("could not update media metadata: {e:?}");
        }
    }

    /// `None` means stopped.
    pub fn set_state(&mut self, playing: Option<bool>, position: f64) {
        let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
        let playback = match playing {
            None => MediaPlayback::Stopped,
            Some(true) => MediaPlayback::Playing { progress },
            Some(false) => MediaPlayback::Paused { progress },
        };
        if let Err(e) = self.inner.set_playback(playback) {
            log::debug!("could not update media playback: {e:?}");
        }
    }
}

fn signed(dir: SeekDirection, by: Duration) -> f64 {
    match dir {
        SeekDirection::Forward => by.as_secs_f64(),
        SeekDirection::Backward => -by.as_secs_f64(),
    }
}
