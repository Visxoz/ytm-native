//! Scrobbling to Last.fm: connecting an account through the browser, then
//! telling Last.fm what is playing and what has been played.
//!
//! The API key and secret are built in from the `LASTFM_API_KEY` and
//! `LASTFM_API_SECRET` environment variables; a build without them hides
//! the feature.

use std::{collections::VecDeque, path::PathBuf, time::Duration};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth/";
const API_KEY: Option<&str> = option_env!("LASTFM_API_KEY");
const API_SECRET: Option<&str> = option_env!("LASTFM_API_SECRET");
/// Most scrobbles Last.fm takes in one request.
const BATCH: usize = 50;
/// Plays kept while Last.fm cannot be reached. Last.fm refuses plays more
/// than two weeks old anyway, and this is a fortnight of heavy listening.
const PENDING_LIMIT: usize = 3000;
const TIMEOUT: Duration = Duration::from_secs(20);

fn keys() -> Option<(&'static str, &'static str)> {
    let (key, secret) = (API_KEY?, API_SECRET?);
    (!key.is_empty() && !secret.is_empty()).then_some((key, secret))
}

/// Whether this build can talk to Last.fm.
pub fn available() -> bool {
    keys().is_some()
}

/// A connected Last.fm account. The key does not expire; the user can
/// revoke it on Last.fm, which `Error::SignedOut` then reports.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    key: String,
}

/// One play, as Last.fm records it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Play {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    /// Length in seconds.
    pub duration: Option<u32>,
    /// When the play started, in Unix seconds.
    pub timestamp: i64,
}

#[derive(Debug)]
pub enum Error {
    /// The account's access was revoked; it has to connect again.
    SignedOut,
    /// The user has not yet allowed access in the browser.
    NotYetAllowed,
    /// Last.fm will never take the request as it is: invalid parameters,
    /// or a token that expired.
    Rejected(String),
    /// Last.fm could not be reached or could not answer; try again later.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::SignedOut => f.write_str("Last.fm no longer allows access"),
            Error::NotYetAllowed => f.write_str("access not yet allowed"),
            Error::Rejected(e) | Error::Failed(e) => f.write_str(e),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> Error {
    Error::Failed(e.to_string())
}

type Params = Vec<(String, String)>;

/// Adds `api_sig`: the MD5 of every parameter's name and value in name
/// order, then the secret, as Last.fm requires of every signed call.
fn sign(params: &mut Params, secret: &str) {
    params.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut md5 = Md5::new();
    for (name, value) in params.iter() {
        md5.update(name);
        md5.update(value);
    }
    md5.update(secret);
    params.push(("api_sig".into(), format!("{:x}", md5.finalize())));
}

#[derive(Deserialize)]
struct Failure {
    error: u32,
    #[serde(default)]
    message: String,
}

async fn call<T: DeserializeOwned>(method: &str, mut params: Params) -> Result<T, Error> {
    let (key, secret) = keys().ok_or_else(|| failed("Last.fm isn't available in this build"))?;
    params.push(("method".into(), method.into()));
    params.push(("api_key".into(), key.into()));
    sign(&mut params, secret);
    // Unsigned, as Last.fm leaves it out of the signature.
    params.push(("format".into(), "json".into()));
    let text = crate::http::client()
        .post(API_URL)
        .header(
            reqwest::header::USER_AGENT,
            concat!("ytm-native/", env!("CARGO_PKG_VERSION")),
        )
        .form(&params)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(failed)?
        .text()
        .await
        .map_err(failed)?;
    // Errors come as `{"error": code, "message": ...}`, whatever the status.
    if let Ok(failure) = serde_json::from_str::<Failure>(&text) {
        let message = format!("Last.fm error {}: {}", failure.error, failure.message);
        return Err(match failure.error {
            // Authentication failed, invalid parameters, expired token.
            4 | 6 | 15 => Error::Rejected(message),
            // Invalid session key.
            9 => Error::SignedOut,
            // Unauthorised token.
            14 => Error::NotYetAllowed,
            _ => Error::Failed(message),
        });
    }
    serde_json::from_str(&text).map_err(failed)
}

/// Starts connecting an account, returning the page where the user allows
/// access and the token to pass to `session` once they have.
pub async fn authorise() -> Result<(url::Url, String), Error> {
    #[derive(Deserialize)]
    struct Token {
        token: String,
    }
    let Token { token } = call("auth.getToken", Params::new()).await?;
    let (key, _) = keys().ok_or_else(|| failed("Last.fm isn't available in this build"))?;
    let page = url::Url::parse_with_params(AUTH_URL, [("api_key", key), ("token", &token)])
        .map_err(failed)?;
    Ok((page, token))
}

/// The session for `token`, once the user has allowed access.
pub async fn session(token: &str) -> Result<Session, Error> {
    #[derive(Deserialize)]
    struct Reply {
        session: Session,
    }
    let params = vec![("token".into(), token.into())];
    Ok(call::<Reply>("auth.getSession", params).await?.session)
}

/// The parameters describing `play`, with `suffix` after each name, as
/// `[0]` in a batch.
fn push_play(params: &mut Params, play: &Play, suffix: &str) {
    params.push((format!("artist{suffix}"), play.artist.clone()));
    params.push((format!("track{suffix}"), play.track.clone()));
    if let Some(album) = &play.album {
        params.push((format!("album{suffix}"), album.clone()));
    }
    if let Some(duration) = play.duration {
        params.push((format!("duration{suffix}"), duration.to_string()));
    }
}

/// Shows `play` as what the account is listening to now.
pub async fn now_playing(session: &Session, play: &Play) -> Result<(), Error> {
    let mut params = vec![("sk".into(), session.key.clone())];
    push_play(&mut params, play, "");
    call::<serde::de::IgnoredAny>("track.updateNowPlaying", params)
        .await
        .map(drop)
}

/// Records `plays`, a batch from `Pending::batch`.
pub async fn scrobble(session: &Session, plays: &[Play]) -> Result<(), Error> {
    let mut params = vec![("sk".into(), session.key.clone())];
    for (i, play) in plays.iter().enumerate() {
        let suffix = format!("[{i}]");
        push_play(&mut params, play, &suffix);
        params.push((format!("timestamp{suffix}"), play.timestamp.to_string()));
    }
    call::<serde::de::IgnoredAny>("track.scrobble", params)
        .await
        .map(drop)
}

/// Whether a play heard for `heard` of a track `length` long counts: the
/// track is over 30 seconds, and half of it or four minutes has played.
pub fn counts(heard: Duration, length: Duration) -> bool {
    length > Duration::from_secs(30) && heard >= (length / 2).min(Duration::from_secs(240))
}

fn session_path() -> PathBuf {
    crate::settings::data_dir().join("lastfm.bin")
}

/// The connected account, encrypted like the YouTube sign-in.
pub fn load_session() -> Option<Session> {
    serde_json::from_str(&crate::secret::read(&session_path())?).ok()
}

pub fn save_session(session: &Session) {
    let saved = serde_json::to_string(session)
        .map_err(std::io::Error::other)
        .and_then(|json| crate::secret::write(&session_path(), &json));
    if let Err(e) = saved {
        log::warn!("could not save the Last.fm account: {e}");
    }
}

pub fn remove_session() {
    let _ = std::fs::remove_file(session_path());
}

fn pending_path() -> PathBuf {
    crate::settings::data_dir().join("scrobbles.json")
}

/// Plays Last.fm has yet to accept, oldest first, kept across runs so none
/// are lost while it cannot be reached.
#[derive(Default, Serialize, Deserialize)]
pub struct Pending {
    plays: VecDeque<Play>,
    /// The account the plays were heard under, so they never go to another
    /// that connects after its access was revoked.
    #[serde(default)]
    account: Option<String>,
    #[serde(skip)]
    changed: bool,
}

impl Pending {
    pub fn load() -> Self {
        crate::settings::read_json(&pending_path()).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.plays.is_empty()
    }

    pub fn push(&mut self, play: Play) {
        if self.plays.len() == PENDING_LIMIT {
            self.plays.pop_front();
        }
        self.plays.push_back(play);
        self.changed = true;
    }

    /// Drops every play, as when the account is disconnected.
    pub fn clear(&mut self) {
        self.changed |= !self.plays.is_empty();
        self.plays.clear();
    }

    /// Holds the plays for `account`, dropping any heard under another.
    pub fn for_account(&mut self, account: &str) {
        if self.account.as_deref() == Some(account) {
            return;
        }
        if self.account.is_some() {
            self.clear();
        }
        self.account = Some(account.to_owned());
        self.changed = true;
    }

    /// The oldest plays, as many as go in one request, or just the oldest.
    pub fn batch(&self, one: bool) -> Vec<Play> {
        let size = if one { 1 } else { BATCH };
        self.plays.iter().take(size).cloned().collect()
    }

    /// Drops `sent`, which Last.fm has taken or refused. Matched by value
    /// rather than position, as the oldest play may have made way for a
    /// new one while they were on their way.
    pub fn sent(&mut self, sent: &[Play]) {
        self.plays.retain(|play| !sent.contains(play));
        self.changed = true;
    }

    /// Writes the plays, if they changed since the last save.
    pub fn save(&mut self) {
        if self.changed && crate::settings::save_json(&pending_path(), self, "the scrobbles") {
            self.changed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_parameters_in_name_order() {
        let mut params: Params = vec![
            ("token".into(), "tok\u{e9}".into()),
            ("method".into(), "auth.getSession".into()),
            ("api_key".into(), "key".into()),
        ];
        sign(&mut params, "secret");
        assert_eq!(
            params.last(),
            Some(&(
                "api_sig".to_owned(),
                "d918bc2b2785863c1487e7a79435acb2".to_owned()
            )),
            "md5 of api_keykeymethodauth.getSessiontokentok\u{e9}secret"
        );
    }

    #[test]
    fn counts_half_a_track_or_four_minutes() {
        let secs = Duration::from_secs;
        assert!(!counts(secs(20), secs(30)), "30 seconds is too short");
        assert!(!counts(secs(89), secs(180)));
        assert!(counts(secs(90), secs(180)));
        assert!(counts(secs(240), secs(1200)), "four minutes of a long one");
        assert!(!counts(secs(239), secs(1200)));
    }

    #[test]
    fn keeps_pending_plays_in_order_within_the_limit() {
        let play = |n: i64| Play {
            artist: "A".into(),
            track: n.to_string(),
            album: None,
            duration: None,
            timestamp: n,
        };
        let mut pending = Pending::default();
        for n in 0..PENDING_LIMIT as i64 + 2 {
            pending.push(play(n));
        }
        assert_eq!(pending.plays.len(), PENDING_LIMIT);
        let batch = pending.batch(false);
        assert_eq!(batch.len(), BATCH);
        assert_eq!(batch[0].timestamp, 2, "the oldest were dropped");
        assert_eq!(pending.batch(true).len(), 1);

        // A new play pushes out the oldest while the batch is on its way.
        pending.push(play(PENDING_LIMIT as i64 + 2));
        pending.sent(&batch);
        assert_eq!(
            pending.batch(true)[0].timestamp,
            2 + BATCH as i64,
            "only the sent plays are gone"
        );
        // The pushed-out play was one of the batch, so one fewer goes.
        assert_eq!(pending.plays.len(), PENDING_LIMIT - BATCH + 1);

        pending.for_account("a");
        assert!(
            !pending.is_empty(),
            "untagged plays from before go to the account"
        );
        pending.for_account("a");
        assert!(!pending.is_empty(), "the same account keeps its plays");
        pending.for_account("b");
        assert!(pending.is_empty(), "another account gets none of them");
    }
}
