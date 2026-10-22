//! Updates from GitHub releases: finds a newer version, downloads the
//! installer or the portable exe, checks it against the release's SHA-256
//! sums, and hands over to it once the app has closed.

use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST: &str = "https://api.github.com/repos/Visxoz/ytm-native/releases/latest";
/// GitHub's API and the hosts its release downloads redirect to.
const DOMAINS: &[&str] = &["api.github.com", "github.com", "githubusercontent.com"];
const SUMS: &str = "SHA256SUMS";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// How this copy was installed, which decides what an update replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Installed,
    Portable,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub version: String,
    kind: Kind,
    asset: Asset,
    sums: Asset,
}

/// The command that takes over once the app has closed.
static PENDING: Mutex<Option<Command>> = Mutex::new(None);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // GitHub's API refuses requests without one.
            .user_agent(format!("ytm-native/{CURRENT}"))
            .timeout(Duration::from_secs(120))
            .redirect(crate::http::redirects(DOMAINS))
            .build()
            .expect("HTTP client builds")
    })
}

/// An installed copy has Inno Setup's uninstaller beside it.
pub fn kind() -> Kind {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("unins000.exe")))
        .is_some_and(|uninstaller| uninstaller.exists());
    if installed {
        Kind::Installed
    } else {
        Kind::Portable
    }
}

/// `x.y.z`, with or without a leading `v`; anything else, such as a
/// pre-release suffix, is no version to update to.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
    let mut next = || parts.next()?.parse().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// The latest release, if newer than this build. Fails while the repo is
/// private, as GitHub then answers anonymous requests with not found.
pub async fn check() -> Result<Option<Update>, String> {
    let release: Release = client()
        .get(LATEST)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("could not check for updates: {e}"))?
        .json()
        .await
        .map_err(|e| format!("unexpected release data: {e}"))?;
    newer(release, CURRENT, kind())
}

fn newer(release: Release, current: &str, kind: Kind) -> Result<Option<Update>, String> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let (Some(latest), Some(current)) = (version(&release.tag_name), version(current)) else {
        return Ok(None);
    };
    if latest <= current {
        return Ok(None);
    }
    let suffix = match kind {
        Kind::Installed => "_x64-setup.exe",
        Kind::Portable => "_x64-portable.exe",
    };
    let find =
        |matches: &dyn Fn(&str) -> bool| release.assets.iter().find(|a| matches(&a.name)).cloned();
    let asset =
        find(&|name| name.ends_with(suffix)).ok_or("the release has no build for this copy")?;
    let sums = find(&|name| name == SUMS).ok_or("the release has no checksums")?;
    Ok(Some(Update {
        version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        kind,
        asset,
        sums,
    }))
}

/// Where the download goes: beside the exe for a portable copy, so it can
/// be renamed into place, or the temp folder for an installer.
fn download_path(update: &Update) -> Result<PathBuf, String> {
    match update.kind {
        Kind::Installed => Ok(std::env::temp_dir().join(&update.asset.name)),
        Kind::Portable => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            Ok(exe.with_extension("exe.new"))
        }
    }
}

fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

/// Downloads the update and checks it against the release's checksum and
/// size. `progress` gets the fraction done.
pub async fn download(update: &Update, progress: impl Fn(f32)) -> Result<PathBuf, String> {
    let fail = |e: reqwest::Error| format!("the download failed: {e}");
    let sums = client()
        .get(&update.sums.browser_download_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(fail)?
        .text()
        .await
        .map_err(fail)?;
    let expected =
        expected_hash(&sums, &update.asset.name).ok_or("the release has no checksum for it")?;

    let path = download_path(update)?;
    let mut response = client()
        .get(&update.asset.browser_download_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(fail)?;
    let mut file =
        fs::File::create(&path).map_err(|e| format!("could not save the update: {e}"))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(fail)? {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| format!("could not save the update: {e}"))?;
        done += chunk.len() as u64;
        progress((done as f32 / update.asset.size.max(1) as f32).min(1.0));
    }
    drop(file);

    let actual = format!("{:x}", hasher.finalize());
    if actual != expected || done != update.asset.size {
        let _ = fs::remove_file(&path);
        return Err("the download did not match its checksum".into());
    }
    Ok(path)
}

/// Readies a downloaded update to take over once the app closes.
pub fn stage(update: &Update, downloaded: PathBuf) -> Result<(), String> {
    let command = match update.kind {
        Kind::Installed => {
            let mut command = Command::new(&downloaded);
            command.args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/relaunch=1",
            ]);
            command
        }
        Kind::Portable => {
            // A running exe can be renamed, though not overwritten.
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let old = exe.with_extension("exe.old");
            let _ = fs::remove_file(&old);
            fs::rename(&exe, &old).map_err(|e| format!("could not replace the app: {e}"))?;
            if let Err(e) = fs::rename(&downloaded, &exe) {
                let _ = fs::rename(&old, &exe);
                return Err(format!("could not replace the app: {e}"));
            }
            Command::new(&exe)
        }
    };
    *PENDING.lock().unwrap() = Some(command);
    Ok(())
}

/// Starts a staged update. Called as the app exits, after it has let go of
/// its single-instance lock, which the new copy needs.
pub fn launch_pending() {
    if let Some(mut command) = PENDING.lock().unwrap().take()
        && let Err(e) = command.spawn()
    {
        log::warn!("could not start the update: {e}");
    }
}

/// Removes the exe a portable update replaced.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(exe.with_extension("exe.old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, names: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: names
                .iter()
                .map(|name| Asset {
                    name: (*name).into(),
                    browser_download_url: format!("https://github.com/x/{name}"),
                    size: 1,
                })
                .collect(),
        }
    }

    const ASSETS: &[&str] = &[
        "ytm-native_0.2.0_x64-portable.exe",
        "ytm-native_0.2.0_x64-setup.exe",
        "SHA256SUMS",
    ];

    #[test]
    fn reads_versions() {
        assert_eq!(version("v0.1.2"), Some((0, 1, 2)));
        assert_eq!(version("1.10.0"), Some((1, 10, 0)));
        assert_eq!(version("v1.3.0-rc.8"), None);
        assert_eq!(version("v1.2"), None);
        assert_eq!(version("v1.2.3.4"), None);
    }

    #[test]
    fn offers_only_newer_releases() {
        assert!(
            newer(release("v0.1.2", ASSETS), "0.1.2", Kind::Portable)
                .unwrap()
                .is_none()
        );
        assert!(
            newer(release("v0.1.1", ASSETS), "0.1.2", Kind::Portable)
                .unwrap()
                .is_none()
        );
        let update = newer(release("v0.2.0", ASSETS), "0.1.2", Kind::Portable)
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "0.2.0");
        assert_eq!(update.asset.name, "ytm-native_0.2.0_x64-portable.exe");
        let mut draft = release("v0.2.0", ASSETS);
        draft.prerelease = true;
        assert!(newer(draft, "0.1.2", Kind::Portable).unwrap().is_none());
    }

    #[test]
    fn picks_the_build_for_this_copy() {
        let update = newer(release("v0.2.0", ASSETS), "0.1.2", Kind::Installed)
            .unwrap()
            .unwrap();
        assert_eq!(update.asset.name, "ytm-native_0.2.0_x64-setup.exe");
        assert!(
            newer(release("v0.2.0", &ASSETS[..2]), "0.1.2", Kind::Installed).is_err(),
            "no checksums, no update"
        );
    }

    #[test]
    fn finds_the_checksum() {
        let sums =
            "ABC123  ytm-native_0.2.0_x64-setup.exe\ndef456 *ytm-native_0.2.0_x64-portable.exe\n";
        assert_eq!(
            expected_hash(sums, "ytm-native_0.2.0_x64-setup.exe").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            expected_hash(sums, "ytm-native_0.2.0_x64-portable.exe").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_hash(sums, "other.exe"), None);
    }

    #[test]
    fn only_talks_to_github() {
        let allowed = |url: &str| crate::http::within(&url::Url::parse(url).unwrap(), DOMAINS);
        assert!(allowed(LATEST));
        assert!(allowed(
            "https://github.com/Visxoz/ytm-native/releases/download/v1/a.exe"
        ));
        assert!(allowed("https://release-assets.githubusercontent.com/x"));
        assert!(!allowed("https://github.com.evil.com/"));
        assert!(!allowed("https://www.youtube.com/"));
        assert!(!allowed("http://github.com/"));
    }
}
