//! The Switch build's updates (docs/switch-updates.md). Once a launch the NRO asks GitHub for
//! [`REPO`]'s releases; a newer Switch build turns the title screen's Realms button into an update
//! button. Pressing it downloads the release's `horizon.nro` beside the running one, checks it
//! against the release's SHA-256 and GitHub's own digest of the file, keeps the running NRO as
//! `<name>.nro.old` and puts the new one in its place; quitting then starts it through the
//! homebrew loader where the loader can, and on the next launch otherwise.
//!
//! Skate 3's release updater protocol 1, for one file: releases come only from [`REPO`]'s API,
//! over HTTPS (redirects too), downloads only from its release downloads and within size limits,
//! each release names its repository, target, tag and build in a `release.json`, and no token is
//! ever sent. Off the Switch nothing here starts: the desktop game has the launcher.
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The only repository updates come from.
pub const REPO: &str = "JohnUzoka/horizon";
/// What a Switch release is tagged: `switch-<build>`.
const TAG_PREFIX: &str = "switch-";
/// `release.json`'s target.
const TARGET: &str = "switch";
/// The release's assets: the program, its `sha256sum` line, and the release's identity.
const PACKAGE: &str = "horizon.nro";
const CHECKSUM: &str = "horizon.nro.sha256";
const MANIFEST: &str = "release.json";
/// The largest NRO taken (the game's is ~50 MB).
const MAX_PACKAGE: u64 = 256 << 20;
/// Release lists are read 100 releases a page, up to this many pages.
const MAX_PAGES: u32 = 10;

/// This NRO's build: `HORIZON_SWITCH_BUILD` when it was built (`scripts/hx-release.sh` in the
/// port's workshop), 0 for a development build, which any release updates.
pub fn current_build() -> u32 {
    option_env!("HORIZON_SWITCH_BUILD")
        .and_then(|build| build.parse().ok())
        .unwrap_or(0)
}

/// Where the update stands, for the title's button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not checked yet, checking, or nothing newer: the title keeps its Realms button.
    None,
    /// A newer build is published.
    Available { build: u32 },
    /// Its NRO is downloading.
    Downloading { build: u32, percent: u8 },
    /// The new NRO is in place; the next start runs it.
    Installed { build: u32 },
    /// The download or install failed; pressing again retries.
    Failed { build: u32 },
}

/// What the title shows as a toast, once each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Available { build: u32 },
    Installed { build: u32 },
    /// Why, in a few words; the log has the details.
    Failed { reason: &'static str },
}

struct State {
    status: Status,
    release: Option<Release>,
    event: Option<Event>,
}
static STATE: Mutex<State> = Mutex::new(State {
    status: Status::None,
    release: None,
    event: None,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn status() -> Status {
    state().status.clone()
}

/// The newest event the title hasn't shown yet.
pub fn take_event() -> Option<Event> {
    state().event.take()
}

/// Starts the launch's one check on a thread of its own: on the Switch, the first time the title
/// screen is made.
pub fn start_check() {
    #[cfg(target_os = "horizon")]
    {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| spawn("update check", check));
    }
}

/// The title's update button: starts the download (again, after a failure), or once the new NRO
/// is in place asks the loader to start it after this one and returns true: the game quits.
pub fn press() -> bool {
    let mut state = state();
    match state.status {
        Status::Available { build } | Status::Failed { build } => {
            let Some(release) = state.release.clone() else {
                return false;
            };
            state.status = Status::Downloading { build, percent: 0 };
            drop(state);
            spawn("update download", move || install(release));
            false
        }
        Status::Installed { .. } => {
            drop(state);
            relaunch();
            true
        }
        Status::None | Status::Downloading { .. } => false,
    }
}

fn spawn(name: &str, job: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(job) {
        log::warn!("Horizon update: no {name} thread: {e}");
    }
}

/// The running NRO's path (its `argv[0]`); none off the Switch.
fn nro_path() -> Option<PathBuf> {
    #[cfg(target_os = "horizon")]
    let path = horizon_nx::nro_path().map(PathBuf::from);
    #[cfg(not(target_os = "horizon"))]
    let path = None;
    path
}

fn relaunch() {
    #[cfg(target_os = "horizon")]
    {
        let Some(path) = horizon_nx::nro_path() else {
            return;
        };
        if horizon_nx::relaunch_on_exit(path) {
            log::info!("Horizon update: the loader starts {path} again once this one quits");
        } else {
            log::info!(
                "Horizon update: this loader can't start {path} after quitting; start it again from hbmenu"
            );
        }
    }
}

/// `<path>.<suffix>`: `horizon.nro` -> `horizon.nro.old`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// The check: the newest Switch build above this one becomes `Available`.
fn check() {
    let current = current_build();
    // A download a quit cut short.
    if let Some(nro) = nro_path() {
        let _ = std::fs::remove_file(sibling(&nro, "download"));
    }
    match newest(&agent(), current) {
        Ok(Some(release)) => {
            log::info!(
                "Horizon update: {REPO} has build {} ({}), newer than this build {current}",
                release.build,
                release.tag
            );
            let build = release.build;
            let mut state = state();
            state.status = Status::Available { build };
            state.release = Some(release);
            state.event = Some(Event::Available { build });
        }
        Ok(None) => log::info!("Horizon update: {REPO} has no Switch build newer than this build {current}"),
        Err(e) => log::warn!("Horizon update: the check failed: {e}"),
    }
}

/// A release asset: where it downloads from, its size and GitHub's digest of it (`sha256:<hex>`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Asset {
    url: String,
    size: u64,
    digest: Option<String>,
}

/// A Switch release on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Release {
    build: u32,
    tag: String,
    package: Asset,
    checksum: Asset,
    manifest: Asset,
}

/// `switch-<build>`, with a build from 1 written without leading zeros.
fn tag_build(tag: &str) -> Option<u32> {
    let digits = tag.strip_prefix(TAG_PREFIX)?;
    let build: u32 = digits.parse().ok()?;
    (build > 0 && digits.bytes().all(|b| b.is_ascii_digit()) && !digits.starts_with('0'))
        .then_some(build)
}

/// The Switch releases on one page of GitHub's release list with a build above `current`:
/// published (not drafts, not pre-releases), tagged `switch-<build>`, with all three assets
/// uploaded, each downloading from this repository's release downloads.
fn candidates(page: &Value, current: u32) -> Vec<Release> {
    let Some(releases) = page.as_array() else {
        return Vec::new();
    };
    releases
        .iter()
        .filter_map(|release| {
            if release["draft"] != false
                || release["prerelease"] != false
                || !release["published_at"].is_string()
            {
                return None;
            }
            let tag = release["tag_name"].as_str()?;
            let build = tag_build(tag).filter(|&build| build > current)?;
            let assets = release["assets"].as_array()?;
            let asset = |name: &str| {
                let asset = assets
                    .iter()
                    .find(|a| a["name"] == name && a["state"] == "uploaded")?;
                let url = asset["browser_download_url"].as_str()?;
                (url == format!("https://github.com/{REPO}/releases/download/{tag}/{name}")).then(|| {
                    Asset {
                        url: url.to_owned(),
                        size: asset["size"].as_u64().unwrap_or(0),
                        digest: asset["digest"].as_str().map(str::to_owned),
                    }
                })
            };
            Some(Release {
                build,
                tag: tag.to_owned(),
                package: asset(PACKAGE)?,
                checksum: asset(CHECKSUM)?,
                manifest: asset(MANIFEST)?,
            })
        })
        .collect()
}

/// A release's `release.json` must name it: schema 1, the Switch, this repository, its own tag
/// and build, and the 40-digit commit it was built from.
fn check_manifest(text: &[u8], release: &Release) -> Result<(), String> {
    let manifest: Value =
        serde_json::from_slice(text).map_err(|e| format!("{}'s {MANIFEST}: {e}", release.tag))?;
    let revision = manifest["revision"].as_str().unwrap_or_default();
    let matches = manifest["schema"] == 1
        && manifest["target"] == TARGET
        && manifest["repository"] == REPO
        && manifest["tag"] == release.tag.as_str()
        && manifest["build"].as_u64() == Some(u64::from(release.build))
        && revision.len() == 40
        && revision.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if matches {
        Ok(())
    } else {
        Err(format!("{}'s {MANIFEST} doesn't describe it", release.tag))
    }
}

/// `sha256sum`'s line for the package: 64 hex digits, then its name.
fn parse_checksum(text: &[u8]) -> Option<[u8; 32]> {
    let mut words = std::str::from_utf8(text).ok()?.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some(hex), Some(PACKAGE), None) => decode_sha256(hex),
        _ => None,
    }
}

fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(digest)
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .https_only(true)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .user_agent(&format!("horizon-switch/{}", current_build()))
        .build()
}

/// GET `url`, which must end up on HTTPS.
fn get(agent: &ureq::Agent, url: &str, api: bool) -> Result<ureq::Response, String> {
    let mut request = agent.get(url);
    if api {
        request = request
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(60));
    }
    let response = request.call().map_err(|e| match e {
        ureq::Error::Status(403 | 429, _) => format!("{url}: GitHub's rate limit; try later"),
        ureq::Error::Status(code, _) => format!("{url}: HTTP {code}"),
        // ureq's message starts with the URL.
        ureq::Error::Transport(e) => e.to_string(),
    })?;
    if !response.get_url().starts_with("https://") {
        return Err(format!("{url}: redirected off HTTPS"));
    }
    Ok(response)
}

/// A small body: at most `limit` bytes.
fn fetch(agent: &ureq::Agent, url: &str, api: bool, limit: u64) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    get(agent, url, api)?
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("{url}: {e}"))?;
    if body.len() as u64 > limit {
        return Err(format!("{url}: larger than {limit} bytes"));
    }
    Ok(body)
}

/// The newest Switch release above `current` whose `release.json` checks out.
fn newest(agent: &ureq::Agent, current: u32) -> Result<Option<Release>, String> {
    let mut found = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100&page={page}");
        let list: Value = serde_json::from_slice(&fetch(agent, &url, true, 8 << 20)?)
            .map_err(|e| format!("{url}: {e}"))?;
        found.extend(candidates(&list, current));
        // A short page is the last; never settle on a release from a cut-off list.
        if list.as_array().is_some_and(|releases| releases.len() < 100) {
            found.sort_by(|a, b| b.build.cmp(&a.build));
            for release in found {
                match fetch(agent, &release.manifest.url, false, 64 << 10)
                    .and_then(|text| check_manifest(&text, &release))
                {
                    Ok(()) => return Ok(Some(release)),
                    Err(e) => log::warn!("Horizon update: skipping {}: {e}", release.tag),
                }
            }
            return Ok(None);
        }
    }
    Err(format!("{REPO} has more than {} releases", MAX_PAGES * 100))
}

/// A failed install: a few words for the toast, the details for the log.
struct Failure {
    reason: &'static str,
    detail: String,
}
fn failure(reason: &'static str) -> impl FnOnce(String) -> Failure {
    move |detail| Failure { reason, detail }
}

fn install(release: Release) {
    let build = release.build;
    let result = (|| {
        let nro = nro_path().ok_or_else(|| Failure {
            reason: "this NRO's path is unknown",
            detail: "no argv[0]".into(),
        })?;
        let new = sibling(&nro, "download");
        let installed = download_and_replace(&release, &nro, &new);
        if installed.is_err() {
            let _ = std::fs::remove_file(&new);
        }
        installed.map(|()| nro)
    })();
    let mut state = state();
    match result {
        Ok(nro) => {
            log::info!("Horizon update: build {build} is in place at {}; the old one is its .old", nro.display());
            state.status = Status::Installed { build };
            state.event = Some(Event::Installed { build });
        }
        Err(Failure { reason, detail }) => {
            log::warn!("Horizon update: installing build {build} failed: {reason}: {detail}");
            state.status = Status::Failed { build };
            state.event = Some(Event::Failed { reason });
        }
    }
}

fn download_and_replace(release: &Release, nro: &Path, new: &Path) -> Result<(), Failure> {
    let agent = agent();
    let checksum = fetch(&agent, &release.checksum.url, false, 1024)
        .map_err(failure("the download failed"))?;
    let expected = parse_checksum(&checksum).ok_or_else(|| Failure {
        reason: "the release's checksum is unreadable",
        detail: String::from_utf8_lossy(&checksum).into_owned(),
    })?;
    let response = get(&agent, &release.package.url, false).map_err(failure("the download failed"))?;
    let total = response
        .header("Content-Length")
        .and_then(|length| length.parse().ok())
        .unwrap_or(release.package.size)
        .max(1);
    let file = std::fs::File::create(new)
        .map_err(|e| format!("{}: {e}", new.display()))
        .map_err(failure("the SD card can't be written"))?;
    let mut shown = 0;
    let digest = copy_hashed(response.into_reader(), &file, MAX_PACKAGE, |done| {
        let percent = (done.min(total) * 100 / total) as u8;
        if percent != shown {
            shown = percent;
            state().status = Status::Downloading { build: release.build, percent };
        }
    })
    .map_err(|e| format!("{}: {e}", release.package.url))
    .map_err(failure("the download failed"))?;
    file.sync_all()
        .map_err(|e| format!("{}: {e}", new.display()))
        .map_err(failure("the SD card can't be written"))?;
    drop(file);
    verify(&digest, &expected, release.package.digest.as_deref())
        .map_err(failure("the download doesn't match its checksum"))?;
    if !is_nro(new) {
        return Err(Failure {
            reason: "the download isn't an NRO",
            detail: new.display().to_string(),
        });
    }
    replace(nro, new).map_err(failure("the SD card can't be written"))
}

/// Copies `from` into `to`, at most `limit` bytes, hashing what passes; `progress` gets the bytes
/// so far.
fn copy_hashed(
    mut from: impl Read,
    mut to: impl Write,
    limit: u64,
    mut progress: impl FnMut(u64),
) -> std::io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 256 << 10];
    let mut done = 0;
    loop {
        let n = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        done += n as u64;
        if done > limit {
            return Err(std::io::Error::other(format!("larger than {limit} bytes")));
        }
        hash.update(&buffer[..n]);
        to.write_all(&buffer[..n])?;
        progress(done);
    }
    to.flush()?;
    Ok(hash.finalize().into())
}

/// The download against the release's checksum and GitHub's digest of the asset, if it has one.
fn verify(digest: &[u8; 32], expected: &[u8; 32], github: Option<&str>) -> Result<(), String> {
    if digest != expected {
        return Err(format!("SHA-256 {}, the release says {}", hex(digest), hex(expected)));
    }
    match github {
        Some(github) if github != format!("sha256:{}", hex(digest)) => {
            Err(format!("SHA-256 {}, GitHub says {github}", hex(digest)))
        }
        _ => Ok(()),
    }
}

/// An NRO has `NRO0` at 0x10.
fn is_nro(path: &Path) -> bool {
    let mut head = [0; 0x14];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| &head[0x10..] == b"NRO0")
}

/// Puts `new` in place of `nro`, keeping `nro` as `<nro>.old` (one step back, by renaming it
/// back). If `new` can't take its place, `nro` goes back.
fn replace(nro: &Path, new: &Path) -> Result<(), String> {
    let old = sibling(nro, "old");
    match std::fs::remove_file(&old) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("{}: {e}", old.display()));
        }
        _ => {}
    }
    std::fs::rename(nro, &old).map_err(|e| format!("{} -> {}: {e}", nro.display(), old.display()))?;
    if let Err(e) = std::fs::rename(new, nro) {
        let back = match std::fs::rename(&old, nro) {
            Ok(()) => "put back".to_owned(),
            Err(e) => format!("NOT put back ({e}): rename {} by hand", old.display()),
        };
        return Err(format!("{} -> {}: {e}; the running NRO {back}", new.display(), nro.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn asset(tag: &str, name: &str) -> Value {
        json!({
            "name": name,
            "state": "uploaded",
            "size": 10,
            "browser_download_url": format!("https://github.com/{REPO}/releases/download/{tag}/{name}"),
        })
    }
    fn release(tag: &str) -> Value {
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "published_at": "2026-11-03T00:00:00Z",
            "assets": [asset(tag, PACKAGE), asset(tag, CHECKSUM), asset(tag, MANIFEST)],
        })
    }
    fn builds(page: Value, current: u32) -> Vec<u32> {
        candidates(&page, current).iter().map(|r| r.build).collect()
    }

    #[test]
    fn only_published_switch_releases_above_this_build() {
        let mut draft = release("switch-9");
        draft["draft"] = json!(true);
        let mut pre = release("switch-8");
        pre["prerelease"] = json!(true);
        let mut unpublished = release("switch-7");
        unpublished["published_at"] = Value::Null;
        let page = json!([
            release("switch-2"),
            draft,
            pre,
            unpublished,
            release("switch-5"),
            release("v0.3.0"),
            release("switch-03"),
            release("switch-0"),
            release("switch-+4"),
            release("switch-1"),
        ]);
        assert_eq!(builds(page.clone(), 0), [2, 5, 1]);
        assert_eq!(builds(page.clone(), 2), [5]);
        assert_eq!(builds(page, 5), Vec::<u32>::new());
    }

    #[test]
    fn a_release_needs_all_three_assets_uploaded_from_its_own_downloads() {
        let mut missing = release("switch-3");
        missing["assets"].as_array_mut().unwrap().remove(1);
        let mut uploading = release("switch-4");
        uploading["assets"][0]["state"] = json!("starter");
        let mut elsewhere = release("switch-5");
        elsewhere["assets"][0]["browser_download_url"] =
            json!("https://example.com/JohnUzoka/horizon/releases/download/switch-5/horizon.nro");
        let mut other_tag = release("switch-6");
        other_tag["assets"][2]["browser_download_url"] = json!(format!(
            "https://github.com/{REPO}/releases/download/switch-1/{MANIFEST}"
        ));
        let page = json!([missing, uploading, elsewhere, other_tag, release("switch-2")]);
        assert_eq!(builds(page, 0), [2]);
    }

    #[test]
    fn the_manifest_must_name_its_own_release() {
        let release = candidates(&json!([release("switch-4")]), 0).remove(0);
        let good = json!({
            "schema": 1, "target": "switch", "repository": REPO, "build": 4, "tag": "switch-4",
            "revision": "0123456789abcdef0123456789abcdef01234567",
        });
        assert!(check_manifest(good.to_string().as_bytes(), &release).is_ok());
        for (key, value) in [
            ("schema", json!(2)),
            ("target", json!("windows-x64")),
            ("repository", json!("horizon-rust/horizon")),
            ("build", json!(5)),
            ("build", json!("4")),
            ("tag", json!("switch-5")),
            ("revision", json!("0123456789ABCDEF0123456789ABCDEF01234567")),
            ("revision", json!("0123456")),
        ] {
            let mut bad = good.clone();
            bad[key] = value.clone();
            assert!(
                check_manifest(bad.to_string().as_bytes(), &release).is_err(),
                "{key}: {value}"
            );
        }
        assert!(check_manifest(b"not json", &release).is_err());
    }

    #[test]
    fn the_checksum_is_sha256sums_line_for_the_nro() {
        let hex = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3";
        let digest = parse_checksum(format!("{hex}  horizon.nro\n").as_bytes()).unwrap();
        assert_eq!(super::hex(&digest), hex);
        assert_eq!(
            parse_checksum(format!("{}  horizon.nro", hex.to_uppercase()).as_bytes()),
            Some(digest)
        );
        for bad in [
            format!("{hex}  other.nro"),
            hex.to_owned(),
            format!("{hex}  horizon.nro extra"),
            format!("+{}  horizon.nro", &hex[1..]),
            format!("{}  horizon.nro", &hex[1..]),
        ] {
            assert_eq!(parse_checksum(bad.as_bytes()), None, "{bad}");
        }
    }

    #[test]
    fn a_download_must_match_both_digests_and_stay_under_the_limit() {
        let mut out = Vec::new();
        let digest = copy_hashed(&b"123"[..], &mut out, 3, |_| {}).unwrap();
        assert_eq!(out, b"123");
        let expected = decode_sha256("a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3");
        assert_eq!(Some(digest), expected);
        assert!(verify(&digest, &digest, None).is_ok());
        assert!(verify(&digest, &digest, Some(&format!("sha256:{}", hex(&digest)))).is_ok());
        assert!(verify(&digest, &[0; 32], None).is_err());
        assert!(verify(&digest, &digest, Some(&format!("sha256:{}", hex(&[0; 32])))).is_err());
        assert!(copy_hashed(&b"1234"[..], Vec::new(), 3, |_| {}).is_err());
    }

    #[test]
    fn replacing_keeps_one_old_nro_and_puts_it_back_on_failure() {
        let dir = std::env::temp_dir().join(format!("horizon-switch-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let nro = dir.join("horizon.nro");
        let (new, old) = (sibling(&nro, "download"), sibling(&nro, "old"));
        assert_eq!(old, dir.join("horizon.nro.old"));
        std::fs::write(&old, "older").unwrap();
        std::fs::write(&nro, "running").unwrap();
        std::fs::write(&new, "update").unwrap();
        replace(&nro, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&nro).unwrap(), "update");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "running");
        assert!(!new.exists());
        // No download to put in place: the running NRO stays where it was.
        assert!(replace(&nro, &new).is_err());
        assert_eq!(std::fs::read_to_string(&nro).unwrap(), "update");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
