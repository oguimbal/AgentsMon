//! Update check, download and hand-over to the system installer.
//!
//! The latest version is read from the GitHub release of the repository, the
//! same place the release workflow publishes to. No manifest is signed: on
//! Windows a downloaded setup runs only if it carries the release signature
//! (`authenticode`), and on macOS the DMG is opened in Finder, where
//! Gatekeeper checks its notarisation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

const LATEST_RELEASE: &str = "https://api.github.com/repos/oguimbal/agentsmon/releases/latest";
/// Assets are only downloaded from the repository's own releases.
const DOWNLOAD_PREFIX: &str = "https://github.com/oguimbal/agentsmon/releases/download/";
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The release asset for this platform. File names carry no version, so
/// `releases/latest/download/<name>` is also a stable manual download link.
const ASSET: Option<&str> = if cfg!(target_os = "macos") {
    Some("AgentsMon.dmg")
} else if cfg!(target_os = "windows") {
    Some("AgentsMon-setup.exe")
} else {
    None
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: semver::Version,
    pub url: String,
}

/// Ask GitHub for the latest release. Blocking; `None` on any failure, when
/// this version is current, or when the release has no file for this platform.
pub fn check() -> Option<Update> {
    let asset = ASSET?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(8))
        .build();
    let release: Value = match agent
        .get(LATEST_RELEASE)
        .set("Accept", "application/vnd.github+json")
        .set(
            "User-Agent",
            concat!("AgentsMon/", env!("CARGO_PKG_VERSION")),
        )
        .call()
    {
        Ok(response) => response.into_json().ok()?,
        Err(error) => {
            tracing::debug!(%error, "update check failed");
            return None;
        }
    };
    newer_release(&release, asset, &current)
}

/// The update described by a GitHub release, if it is newer than `current`
/// and already carries `asset`. The macOS and Windows jobs publish to the same
/// release one after the other: a release without this platform's file is not
/// announced.
fn newer_release(release: &Value, asset: &str, current: &semver::Version) -> Option<Update> {
    if release.get("draft")?.as_bool()? || release.get("prerelease")?.as_bool()? {
        return None;
    }
    let tag = release.get("tag_name")?.as_str()?;
    let version = semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    if version <= *current {
        return None;
    }
    let url = release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|candidate| candidate.get("name").and_then(Value::as_str) == Some(asset))?
        .get("browser_download_url")?
        .as_str()?;
    url.starts_with(DOWNLOAD_PREFIX).then(|| Update {
        version,
        url: url.to_owned(),
    })
}

/// Download the release file to `~/Downloads/AgentsMon-<version>.<ext>`.
/// Blocking.
pub fn download(update: &Update) -> Option<PathBuf> {
    let directory = directories::UserDirs::new()?.download_dir()?.to_path_buf();
    if let Err(error) = std::fs::create_dir_all(&directory) {
        tracing::warn!(%error, "cannot create the download directory");
        return None;
    }
    let extension = if cfg!(target_os = "windows") {
        "exe"
    } else {
        "dmg"
    };
    let destination = directory.join(format!("AgentsMon-{}.{extension}", update.version));
    let partial = destination.with_extension(format!("{extension}.part"));

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(120))
        .build();
    let response = match agent.get(&update.url).call() {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "update download failed");
            return None;
        }
    };
    let written = std::fs::File::create(&partial)
        .and_then(|mut file| std::io::copy(&mut response.into_reader(), &mut file))
        .and_then(|_| std::fs::rename(&partial, &destination));
    if let Err(error) = written {
        tracing::warn!(%error, "cannot save the update");
        let _ = std::fs::remove_file(&partial);
        return None;
    }
    tracing::info!(path = %destination.display(), "update downloaded");
    Some(destination)
}

/// Hand the downloaded file to the system. Returns whether it was accepted.
///
/// macOS: Finder mounts the DMG and the user drags the application over the
/// installed one; `replaced_bundle` then relaunches it.
#[cfg(target_os = "macos")]
pub fn apply(path: &Path) -> bool {
    match std::process::Command::new("/usr/bin/open")
        .arg(path)
        .status()
    {
        Ok(status) => status.success(),
        Err(error) => {
            tracing::warn!(%error, "cannot open the update");
            false
        }
    }
}

/// Windows: the Inno Setup installer closes the running application, replaces
/// its files and starts it again (`installer.iss`). The download address comes
/// from the network, so only a setup with the release signature is run.
#[cfg(target_os = "windows")]
pub fn apply(path: &Path) -> bool {
    if let Err(error) = crate::authenticode::signed_by_oganbo(path) {
        tracing::warn!(%error, "refusing the downloaded installer");
        let _ = std::fs::remove_file(path);
        return false;
    }
    match std::process::Command::new(path)
        .args(["/SILENT", "/CLOSEAPPLICATIONS"])
        .spawn()
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, "cannot start the installer");
            false
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn apply(_path: &Path) -> bool {
    false
}

/// The `.app` bundle this process was started from, if any.
#[cfg(target_os = "macos")]
pub fn running_bundle() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let bundle = executable.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Whether the bundle on disk is now a newer version than this process: the
/// user dragged an update over the running application.
#[cfg(target_os = "macos")]
pub fn bundle_was_updated(bundle: &Path) -> bool {
    let Ok(current) = semver::Version::parse(env!("CARGO_PKG_VERSION")) else {
        return false;
    };
    std::fs::read_to_string(bundle.join("Contents/Info.plist"))
        .ok()
        .and_then(|plist| plist_version(&plist))
        .is_some_and(|installed| installed > current)
}

/// Start the bundle again once this process has exited.
#[cfg(target_os = "macos")]
pub fn relaunch_after_exit(bundle: &Path) {
    let result = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 1; exec /usr/bin/open \"$0\""])
        .arg(bundle)
        .spawn();
    if let Err(error) = result {
        tracing::warn!(%error, "cannot schedule the relaunch");
    }
}

/// `CFBundleShortVersionString` of an XML Info.plist.
#[cfg(any(target_os = "macos", test))]
fn plist_version(plist: &str) -> Option<semver::Version> {
    let after_key = plist.split_once("<key>CFBundleShortVersionString</key>")?.1;
    let value = after_key
        .split_once("<string>")?
        .1
        .split_once("</string>")?
        .0;
    semver::Version::parse(value.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(tag: &str, assets: &[&str]) -> Value {
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": assets.iter().map(|name| json!({
                "name": name,
                "browser_download_url": format!("{DOWNLOAD_PREFIX}{tag}/{name}"),
            })).collect::<Vec<_>>(),
        })
    }

    fn current() -> semver::Version {
        semver::Version::new(0, 1, 9)
    }

    #[test]
    fn announces_a_newer_release_that_has_this_platform_file() {
        let found = newer_release(
            &release("v0.1.10", &["AgentsMon.dmg", "AgentsMon-setup.exe"]),
            "AgentsMon.dmg",
            &current(),
        )
        .unwrap();
        assert_eq!(found.version, semver::Version::new(0, 1, 10));
        assert_eq!(
            found.url,
            "https://github.com/oguimbal/agentsmon/releases/download/v0.1.10/AgentsMon.dmg"
        );
    }

    #[test]
    fn ignores_current_older_and_incomplete_releases() {
        let dmg = "AgentsMon.dmg";
        assert_eq!(
            newer_release(&release("v0.1.9", &[dmg]), dmg, &current()),
            None
        );
        assert_eq!(
            newer_release(&release("v0.1.2", &[dmg]), dmg, &current()),
            None
        );
        // The Windows job has not published yet.
        assert_eq!(
            newer_release(
                &release("v0.2.0", &[dmg]),
                "AgentsMon-setup.exe",
                &current()
            ),
            None
        );
        let mut draft = release("v0.2.0", &[dmg]);
        draft["prerelease"] = json!(true);
        assert_eq!(newer_release(&draft, dmg, &current()), None);
        assert_eq!(
            newer_release(&json!({ "message": "Not Found" }), dmg, &current()),
            None
        );
    }

    #[test]
    fn refuses_a_download_address_outside_the_repository() {
        let mut foreign = release("v0.2.0", &["AgentsMon.dmg"]);
        foreign["assets"][0]["browser_download_url"] = json!("https://example.com/AgentsMon.dmg");
        assert_eq!(newer_release(&foreign, "AgentsMon.dmg", &current()), None);
    }

    #[test]
    fn reads_the_bundle_version() {
        let plist = include_str!("../../../bundle/Info.plist");
        assert_eq!(
            plist_version(plist),
            semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()
        );
        assert_eq!(plist_version("<dict></dict>"), None);
    }
}
