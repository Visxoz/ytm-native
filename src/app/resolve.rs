//! Chooses between the Premium and the free stream for a track, and keeps
//! the Premium path's state across songs.

use std::{
    future::Future,
    rc::Rc,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use super::App;
use crate::streams::{self, ResolvedStream, premium};

pub(super) const PREMIUM_UNKNOWN: u8 = 0;
pub(super) const PREMIUM_YES: u8 = 1;
pub(super) const PREMIUM_NO: u8 = 2;
/// Songs in a row without Premium formats before an account not yet known
/// to have Premium counts as without it, as some songs simply have no
/// Premium version.
const NOT_PREMIUM_AFTER: u8 = 3;
/// Cool-off after the player fails to prepare or Premium keeps failing, and
/// between background checks of an account taken as without Premium.
const PREMIUM_BACKOFF: Duration = Duration::from_secs(10 * 60);
const PREMIUM_MAX_ERRORS: u8 = 3;

/// Premium audio state shared with background tasks.
pub(super) struct PremiumShared {
    /// The cut-down player script, loaded once per session.
    player: tokio::sync::Mutex<Option<Arc<premium::Player>>>,
    cookie: RwLock<Option<String>>,
    status: AtomicU8,
    /// Songs in a row that offered no Premium formats.
    misses: AtomicU8,
    /// Premium requests in a row that failed outright.
    errors: AtomicU8,
    /// When preparing the player last failed, to avoid refetching it per song.
    load_failed: Mutex<Option<Instant>>,
    /// The Premium path is skipped until then, after repeated errors.
    paused_until: Mutex<Option<Instant>>,
    /// When an account taken as without Premium is next checked, in the
    /// background. A run of music videos, which never have Premium formats,
    /// can mistake a Premium account for one.
    recheck_at: Mutex<Option<Instant>>,
}

impl PremiumShared {
    /// `known` says the account has been served Premium formats before.
    pub(super) fn new(cookie: Option<String>, known: bool) -> Self {
        Self {
            player: tokio::sync::Mutex::new(None),
            cookie: RwLock::new(cookie),
            status: AtomicU8::new(if known { PREMIUM_YES } else { PREMIUM_UNKNOWN }),
            misses: AtomicU8::new(0),
            errors: AtomicU8::new(0),
            load_failed: Mutex::new(None),
            paused_until: Mutex::new(None),
            recheck_at: Mutex::new(None),
        }
    }

    pub(super) fn status(&self) -> u8 {
        self.status.load(Ordering::Relaxed)
    }

    /// Takes the cookie of a new sign-in, or none on signing out, which says
    /// nothing yet about Premium.
    pub(super) fn set_cookie(&self, cookie: Option<String>) {
        *self.cookie.write().unwrap() = cookie;
        self.status.store(PREMIUM_UNKNOWN, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        *self.recheck_at.lock().unwrap() = None;
    }

    /// Whether to go straight to the free stream.
    fn skipped(&self) -> bool {
        self.status() == PREMIUM_NO
            || self
                .paused_until
                .lock()
                .unwrap()
                .is_some_and(|until| Instant::now() < until)
    }

    /// Whether an account taken as without Premium is due a background
    /// check, claiming it so only one runs.
    fn take_recheck(&self) -> bool {
        if self.status() != PREMIUM_NO {
            return false;
        }
        let now = Instant::now();
        let mut at = self.recheck_at.lock().unwrap();
        if at.is_some_and(|at| now < at) {
            return false;
        }
        *at = Some(now + PREMIUM_BACKOFF);
        true
    }

    fn record_premium(&self) {
        self.status.store(PREMIUM_YES, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
    }

    fn record_miss(&self) {
        let misses = self
            .misses
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if misses >= NOT_PREMIUM_AFTER && self.status() != PREMIUM_YES {
            self.status.store(PREMIUM_NO, Ordering::Relaxed);
            *self.recheck_at.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
        }
    }

    fn record_error(&self) {
        if self.errors.fetch_add(1, Ordering::Relaxed) + 1 >= PREMIUM_MAX_ERRORS {
            self.errors.store(0, Ordering::Relaxed);
            *self.paused_until.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
            premium::diag("Premium requests keep failing; using free streams for 10 minutes");
        }
    }
}

/// `YTM_PREMIUM_DRYRUN=1` times the Premium path while playing free streams.
pub(super) fn premium_dry_run() -> bool {
    std::env::var_os("YTM_PREMIUM_DRYRUN").is_some()
}

async fn premium_player(shared: &PremiumShared) -> Result<Arc<premium::Player>, String> {
    let mut slot = shared.player.lock().await;
    if let Some(player) = slot.as_ref() {
        return Ok(Arc::clone(player));
    }
    if shared
        .load_failed
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < PREMIUM_BACKOFF)
    {
        return Err("waiting before preparing the player again".into());
    }
    match premium::load_player().await {
        Ok(player) => {
            let player = Arc::new(player);
            *slot = Some(Arc::clone(&player));
            Ok(player)
        }
        Err(e) => {
            *shared.load_failed.lock().unwrap() = Some(Instant::now());
            Err(e)
        }
    }
}

/// The best stream the web client has, through the Premium path's solver,
/// for when no client hands out plain URLs.
async fn web_fallback(
    shared: &PremiumShared,
    video_id: &str,
    cookie: &str,
    visitor: &str,
    max_kbps: Option<u64>,
) -> Result<ResolvedStream, String> {
    let player = premium_player(shared).await?;
    match premium::resolve(video_id, Some(cookie), visitor, &player, true, max_kbps).await? {
        premium::Outcome::Stream(stream) => {
            premium::diag(format!(
                "{video_id}: no client gave a plain URL; playing through the web client"
            ));
            Ok(stream)
        }
        premium::Outcome::NoPremiumFormats => Err("no formats".into()),
    }
}

impl App {
    /// Resolves a stream: Premium when signed in with Premium, otherwise the
    /// first plain-URL client that answers, and signed in, the web client as
    /// a last resort. Reuses a prefetched one.
    pub(super) fn resolve_future(
        &self,
        video_id: String,
        prefetched: Option<ResolvedStream>,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        let api = self.api.clone();
        let premium = Arc::clone(&self.premium);
        let (want_premium, max_kbps) = {
            let state = self.state.borrow();
            let want_premium = state.signed_in
                && state.settings.premium_audio
                && state.playback.force_free.as_deref() != Some(video_id.as_str());
            (want_premium, state.settings.quality.max_kbps())
        };
        let dry_run = !want_premium && premium_dry_run();
        async move {
            if let Some(stream) = prefetched {
                return Ok(stream);
            }
            let started = Instant::now();
            let visitor = api.visitor_data(false).await?;
            let cookie = premium
                .cookie
                .read()
                .unwrap()
                .clone()
                .filter(|_| want_premium);
            if let Some(cookie) = cookie.as_deref().filter(|_| !premium.skipped()) {
                match premium_player(&premium).await {
                    Ok(player) => {
                        match premium::resolve(
                            &video_id,
                            Some(cookie),
                            &visitor,
                            &player,
                            false,
                            None,
                        )
                        .await
                        {
                            Ok(premium::Outcome::Stream(stream)) => {
                                premium.record_premium();
                                log::info!(
                                    "{video_id}: Premium stream resolved in {} ms",
                                    started.elapsed().as_millis()
                                );
                                return Ok(stream);
                            }
                            Ok(premium::Outcome::NoPremiumFormats) => premium.record_miss(),
                            Err(e) => {
                                premium::diag(format!(
                                    "{video_id}: Premium path failed, using the free stream: {e}"
                                ));
                                premium.record_error();
                            }
                        }
                    }
                    Err(e) => premium::diag(format!("could not prepare the player: {e}")),
                }
            } else if let Some(cookie) = cookie.filter(|_| premium.take_recheck()) {
                // Checked off the playback path: this song plays the free
                // stream at once, and the next one gets Premium if it is back.
                let premium = Arc::clone(&premium);
                let (video_id, visitor) = (video_id.clone(), visitor.clone());
                tokio::spawn(async move {
                    let Ok(player) = premium_player(&premium).await else {
                        return;
                    };
                    match premium::resolve(&video_id, Some(&cookie), &visitor, &player, false, None)
                        .await
                    {
                        Ok(premium::Outcome::Stream(_)) => premium.record_premium(),
                        Ok(premium::Outcome::NoPremiumFormats) => premium.record_miss(),
                        Err(e) => premium::diag(format!("{video_id}: Premium check failed: {e}")),
                    }
                });
            } else if dry_run && let Ok(player) = premium_player(&premium).await {
                // Times the Premium path without an account; the result is
                // discarded as YouTube won't serve it anonymously.
                let dry = Instant::now();
                let result =
                    premium::resolve(&video_id, None, &visitor, &player, false, None).await;
                premium::diag(format!(
                    "{video_id}: dry run {} in {} ms",
                    if result.is_ok() { "resolved" } else { "failed" },
                    dry.elapsed().as_millis()
                ));
            }
            let free = Instant::now();
            let result = match streams::resolve(&video_id, &visitor, max_kbps).await {
                Ok(stream) => Ok(stream),
                // YouTube sometimes bot-checks a visitor ID; retry with a fresh one.
                Err(e) => {
                    log::debug!("retrying with a new visitor ID: {e}");
                    let visitor = api.visitor_data(true).await?;
                    streams::resolve(&video_id, &visitor, max_kbps).await
                }
            };
            log::info!(
                "{video_id}: free stream resolved in {} ms",
                free.elapsed().as_millis()
            );
            match result {
                Ok(stream) => Ok(stream),
                // Signed in, the web client's scrambled URLs are a last
                // resort, read by the Premium path's solver whether or not
                // the account has Premium.
                Err(e) => {
                    let cookie = premium.cookie.read().unwrap().clone();
                    match cookie {
                        Some(cookie) => {
                            web_fallback(&premium, &video_id, &cookie, &visitor, max_kbps)
                                .await
                                .map_err(|web| {
                                    log::debug!("{video_id}: web client fallback failed: {web}");
                                    e
                                })
                        }
                        None => Err(e),
                    }
                }
            }
        }
    }

    /// Prepares the Premium player script in the background, so the first
    /// song does not wait for it. Only runs when Premium audio can apply.
    pub(super) fn warm_premium(self: &Rc<Self>) {
        let wanted = {
            let state = self.state.borrow();
            state.signed_in
                && state.settings.premium_audio
                && self.premium.cookie.read().unwrap().is_some()
        };
        if !wanted && !premium_dry_run() {
            return;
        }
        let shared = Arc::clone(&self.premium);
        self.rt.spawn(async move {
            if let Err(e) = premium_player(&shared).await {
                premium::diag(format!("could not prepare the player: {e}"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_videos_never_turn_off_a_known_premium_account() {
        let shared = PremiumShared::new(Some("SID=a".into()), true);
        for _ in 0..10 {
            shared.record_miss();
        }
        assert_eq!(shared.status(), PREMIUM_YES);
        assert!(!shared.skipped());
    }

    #[test]
    fn checks_again_in_the_background_after_taking_an_account_as_without_premium() {
        let shared = PremiumShared::new(Some("SID=a".into()), false);
        shared.record_miss();
        shared.record_miss();
        assert!(!shared.skipped(), "two songs say nothing yet");
        assert!(!shared.take_recheck(), "nothing to check while unknown");
        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_NO);
        assert!(shared.skipped(), "songs go straight to the free stream");
        assert!(!shared.take_recheck(), "not due yet");

        *shared.recheck_at.lock().unwrap() = Some(Instant::now());
        assert!(shared.take_recheck(), "due once the cool-off ends");
        assert!(!shared.take_recheck(), "only one check at a time");
        assert!(shared.skipped(), "songs still play free meanwhile");

        shared.record_premium();
        assert_eq!(shared.status(), PREMIUM_YES);
        assert!(!shared.skipped());
        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_YES);
    }

    #[test]
    fn a_new_sign_in_starts_unknown() {
        let shared = PremiumShared::new(Some("SID=a".into()), false);
        for _ in 0..3 {
            shared.record_miss();
        }
        shared.set_cookie(Some("SID=b".into()));
        assert_eq!(shared.status(), PREMIUM_UNKNOWN);
        assert!(!shared.skipped());
    }
}
