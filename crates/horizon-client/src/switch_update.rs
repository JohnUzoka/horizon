//! The Switch build's updates (docs/switch-updates.md). Once a launch the NRO asks GitHub for
//! [`REPO`]'s releases; a newer Switch release turns the title screen's Realms button into an
//! update button. Pressing it asks first, then downloads the release's `horizon.nro` beside the
//! running one, checks it against the release's SHA-256 and GitHub's own digest of the file, keeps
//! the running NRO as `<name>.nro.old` and puts the new one in its place; quitting then starts it
//! through the homebrew loader where the loader can, and on the next launch otherwise. That launch
//! says it was updated (a `.updated` marker beside the NRO).
//!
//! Skate 3's release updater protocol, for one file and with versions where it has build numbers
//! (`release.json` schema 2): releases come only from [`REPO`]'s API, over HTTPS (redirects too),
//! downloads only from its release downloads and within size limits, each release names its
//! repository, target, tag and version in a `release.json`, and no token is ever sent. Off the
//! Switch nothing here starts: the desktop game has the launcher.
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The only repository updates come from.
pub const REPO: &str = "JohnUzoka/horizon";
/// What a Switch release is tagged: `v<version>`, as `v0.1.0`.
const TAG_PREFIX: &str = "v";
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

/// A release's version, `major.minor.patch`, compared part by part (0.10.0 is above 0.9.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// A development build's, below every release.
    pub const DEVELOPMENT: Version = Version(0, 0, 0);

    /// `major.minor.patch`: three numbers without leading zeros, and nothing after them.
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.split('.').map(|part| {
            (!part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')))
            .then(|| part.parse().ok())
            .flatten()
        });
        let version = Version(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// This NRO's version: `HORIZON_SWITCH_VERSION` when it was built (`scripts/hx-release.sh` in the
/// port's workshop); a development build has none, and any release updates it.
pub fn current_version() -> Version {
    option_env!("HORIZON_SWITCH_VERSION")
        .and_then(Version::parse)
        .unwrap_or(Version::DEVELOPMENT)
}

/// Where the update stands, for the title's button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not checked yet, checking, or nothing newer: the title keeps its Realms button.
    None,
    /// A newer release is published.
    Available { version: Version },
    /// Its NRO is downloading.
    Downloading { version: Version, percent: u8 },
    /// The new NRO is in place; the next start runs it.
    Installed { version: Version },
    /// The download or install failed; pressing again retries.
    Failed { version: Version },
}

/// What the title shows as a toast, each once, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Available { version: Version },
    Installed { version: Version },
    /// Why, in a few words; the log has the details.
    Failed { reason: &'static str },
    /// The first launch of the version an update installed.
    Updated { version: Version },
}

struct State {
    status: Status,
    release: Option<Release>,
    events: VecDeque<Event>,
}
static STATE: Mutex<State> = Mutex::new(State {
    status: Status::None,
    release: None,
    events: VecDeque::new(),
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn status() -> Status {
    state().status.clone()
}

/// The oldest event the title hasn't shown yet.
pub fn take_event() -> Option<Event> {
    state().events.pop_front()
}

/// The available release's NRO size in bytes, for the title's question.
pub fn download_size() -> Option<u64> {
    state().release.as_ref().map(|release| release.package.size)
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

/// Starts the download of the available release (again, after a failure).
pub fn start_download() {
    let mut state = state();
    let (Status::Available { version } | Status::Failed { version }) = state.status else {
        return;
    };
    let Some(release) = state.release.clone() else {
        return;
    };
    state.status = Status::Downloading { version, percent: 0 };
    drop(state);
    spawn("update download", move || install(release));
}

/// Once the new NRO is in place: asks the loader to start it after this one, as the game quits.
pub fn quit_to_update() {
    if matches!(status(), Status::Installed { .. }) {
        relaunch();
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

/// The version a finished update put in place, if it's this one: the marker [`install`] leaves
/// beside the NRO, read once and removed. None for any other version (the old NRO started again).
fn take_update_marker(nro: &Path, current: Version) -> Option<Version> {
    let marker = sibling(nro, "updated");
    let text = std::fs::read_to_string(&marker).ok()?;
    let _ = std::fs::remove_file(&marker);
    let version = Version::parse(text.trim())?;
    (version == current).then_some(version)
}

/// The check: the newest Switch release above this one becomes `Available`. First, the first
/// launch after an update says so.
fn check() {
    let current = current_version();
    if let Some(nro) = nro_path() {
        // A download a quit cut short.
        let _ = std::fs::remove_file(sibling(&nro, "download"));
        if let Some(version) = take_update_marker(&nro, current) {
            log::info!("Horizon update: this is {version}, which the last launch installed");
            state().events.push_back(Event::Updated { version });
        }
    }
    match newest(&agent(), current) {
        Ok(Some(release)) => {
            log::info!(
                "Horizon update: {REPO} has {} ({}), newer than this version {current}",
                release.version,
                release.tag
            );
            let version = release.version;
            let mut state = state();
            state.status = Status::Available { version };
            state.release = Some(release);
            state.events.push_back(Event::Available { version });
        }
        Ok(None) => log::info!("Horizon update: {REPO} has no Switch release newer than this version {current}"),
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
    version: Version,
    tag: String,
    package: Asset,
    checksum: Asset,
    manifest: Asset,
}

/// `v<version>`, never the development build's 0.0.0.
fn tag_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix(TAG_PREFIX)?).filter(|&version| version > Version::DEVELOPMENT)
}

/// The Switch releases on one page of GitHub's release list with a version above `current`:
/// published (not drafts, not pre-releases), tagged `v<version>`, with all three assets uploaded,
/// each downloading from this repository's release downloads.
fn candidates(page: &Value, current: Version) -> Vec<Release> {
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
            let version = tag_version(tag).filter(|&version| version > current)?;
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
                version,
                tag: tag.to_owned(),
                package: asset(PACKAGE)?,
                checksum: asset(CHECKSUM)?,
                manifest: asset(MANIFEST)?,
            })
        })
        .collect()
}

/// A release's `release.json` must name it: schema 2, the Switch, this repository, its own tag
/// and version, and the 40-digit commit it was built from.
fn check_manifest(text: &[u8], release: &Release) -> Result<(), String> {
    let manifest: Value =
        serde_json::from_slice(text).map_err(|e| format!("{}'s {MANIFEST}: {e}", release.tag))?;
    let revision = manifest["revision"].as_str().unwrap_or_default();
    let matches = manifest["schema"] == 2
        && manifest["target"] == TARGET
        && manifest["repository"] == REPO
        && manifest["tag"] == release.tag.as_str()
        && manifest["version"] == release.version.to_string().as_str()
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
        .user_agent(&format!("horizon-switch/{}", current_version()))
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
fn newest(agent: &ureq::Agent, current: Version) -> Result<Option<Release>, String> {
    let mut found = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100&page={page}");
        let list: Value = serde_json::from_slice(&fetch(agent, &url, true, 8 << 20)?)
            .map_err(|e| format!("{url}: {e}"))?;
        found.extend(candidates(&list, current));
        // A short page is the last; never settle on a release from a cut-off list.
        if list.as_array().is_some_and(|releases| releases.len() < 100) {
            found.sort_by(|a, b| b.version.cmp(&a.version));
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
    let version = release.version;
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
    match result {
        Ok(nro) => {
            log::info!("Horizon update: {version} is in place at {}; the old one is its .old", nro.display());
            // The next launch's confirmation (take_update_marker); without it the update still works.
            if let Err(e) = std::fs::write(sibling(&nro, "updated"), version.to_string()) {
                log::warn!("Horizon update: no .updated marker: {e}");
            }
            let mut state = state();
            state.status = Status::Installed { version };
            state.events.push_back(Event::Installed { version });
        }
        Err(Failure { reason, detail }) => {
            log::warn!("Horizon update: installing {version} failed: {reason}: {detail}");
            let mut state = state();
            state.status = Status::Failed { version };
            state.events.push_back(Event::Failed { reason });
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
            state().status = Status::Downloading { version: release.version, percent };
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
    fn versions(page: Value, current: Version) -> Vec<String> {
        candidates(&page, current).iter().map(|r| r.version.to_string()).collect()
    }

    #[test]
    fn only_published_switch_releases_above_this_version() {
        let mut draft = release("v0.9.0");
        draft["draft"] = json!(true);
        let mut pre = release("v0.8.0");
        pre["prerelease"] = json!(true);
        let mut unpublished = release("v0.7.0");
        unpublished["published_at"] = Value::Null;
        let page = json!([
            release("v0.2.0"),
            draft,
            pre,
            unpublished,
            release("v0.10.0"),
            // The old scheme, and versions written any other way.
            release("switch-3"),
            release("v0.03.0"),
            release("v0.0.0"),
            release("v+0.4.0"),
            release("v1.0.0-rc1"),
            release("v0.4"),
            release("0.4.0"),
            release("v0.1.0"),
        ]);
        let dev = Version::DEVELOPMENT;
        assert_eq!(versions(page.clone(), dev), ["0.2.0", "0.10.0", "0.1.0"]);
        // Part by part: 0.10.0 is above 0.9.0 and 0.2.0, which comparing text would get wrong.
        assert_eq!(versions(page.clone(), Version(0, 9, 0)), ["0.10.0"]);
        assert_eq!(versions(page.clone(), Version(0, 2, 0)), ["0.10.0"]);
        assert_eq!(versions(page, Version(0, 10, 0)), Vec::<String>::new());
    }

    #[test]
    fn a_version_is_three_plain_numbers() {
        assert_eq!(Version::parse("0.1.0"), Some(Version(0, 1, 0)));
        assert_eq!(Version::parse("12.0.3"), Some(Version(12, 0, 3)));
        for bad in [
            "", "0.1", "0.1.0.0", "0.01.0", "0.1.0-rc1", "0.1.+0", "0..1", "v0.1.0",
            "4294967296.0.0",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert_eq!(Version(0, 1, 0).to_string(), "0.1.0");
    }

    #[test]
    fn a_release_needs_all_three_assets_uploaded_from_its_own_downloads() {
        let mut missing = release("v0.3.0");
        missing["assets"].as_array_mut().unwrap().remove(1);
        let mut uploading = release("v0.4.0");
        uploading["assets"][0]["state"] = json!("starter");
        let mut elsewhere = release("v0.5.0");
        elsewhere["assets"][0]["browser_download_url"] =
            json!("https://example.com/JohnUzoka/horizon/releases/download/v0.5.0/horizon.nro");
        let mut other_tag = release("v0.6.0");
        other_tag["assets"][2]["browser_download_url"] = json!(format!(
            "https://github.com/{REPO}/releases/download/v0.1.0/{MANIFEST}"
        ));
        let page = json!([missing, uploading, elsewhere, other_tag, release("v0.2.0")]);
        assert_eq!(versions(page, Version::DEVELOPMENT), ["0.2.0"]);
    }

    #[test]
    fn the_manifest_must_name_its_own_release() {
        let release = candidates(&json!([release("v0.4.0")]), Version::DEVELOPMENT).remove(0);
        let good = json!({
            "schema": 2, "target": "switch", "repository": REPO,
            "version": "0.4.0", "tag": "v0.4.0",
            "revision": "0123456789abcdef0123456789abcdef01234567",
        });
        assert!(check_manifest(good.to_string().as_bytes(), &release).is_ok());
        for (key, value) in [
            ("schema", json!(1)),
            ("target", json!("windows-x64")),
            ("repository", json!("horizon-rust/horizon")),
            ("version", json!("0.5.0")),
            ("version", json!("v0.4.0")),
            ("version", json!(4)),
            ("tag", json!("v0.5.0")),
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

    #[test]
    fn only_the_installed_version_says_it_was_updated_and_only_once() {
        let dir = std::env::temp_dir().join(format!("horizon-switch-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let nro = dir.join("horizon.nro");
        let marker = sibling(&nro, "updated");
        std::fs::write(&marker, "0.3.0").unwrap();
        assert_eq!(take_update_marker(&nro, Version(0, 3, 0)), Some(Version(0, 3, 0)));
        assert!(!marker.exists());
        assert_eq!(take_update_marker(&nro, Version(0, 3, 0)), None);
        // The old NRO, started again after an update, doesn't claim it.
        std::fs::write(&marker, "0.3.0").unwrap();
        assert_eq!(take_update_marker(&nro, Version(0, 2, 0)), None);
        assert!(!marker.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
