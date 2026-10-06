//! Updating the app from its own GitHub releases.
//!
//! On launch the app asks GitHub for the latest release. If it is newer, a
//! banner offers it; "Update now" downloads the archive for this platform,
//! checks it, and swaps it in for the running executable. Nothing installs
//! without being asked for, and the check can be turned off.
//!
//! What makes this safe is the signature, not GitHub. Every release carries
//! `SHA256SUMS.txt.minisig`, made on the maintainer's own machine by
//! `scripts/sign-release.sh` with a key that never goes near CI or GitHub. The
//! app trusts only the public keys below: it checks the signature over the
//! checksum list, then the archive against that list. A release someone
//! slipped in through a stolen account or a poisoned workflow has no valid
//! signature, and is only ever offered as a link to the release page.
//!
//! The signature's trusted comment names the release (`mnm-cartographer
//! v0.0.5`), and the archive name carries the version too, so an old signed
//! release cannot be passed off as a new one.

use crate::VERSION;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const REPO: &str = "Noodle-face/mnm-cartographer";

/// Public keys a release may be signed with. The first signs releases; the
/// second is a spare kept offline, so a lost or leaked main key can be
/// retired without stranding everyone on a version that trusts only it.
const KEYS: [&str; 2] = [
    "RWSQKY1DixXTwTuqmzG0lTEUsrR10OYr3mYrUoZ3sc9eDjnI09cJJCZA", // main, C1D3158B438D2990
    "RWRwj8vLQxPD7FKKX6ph8G9KRwJOFOgFODAjOFg2IQmKkYDrdGsC5Ac4", // spare, ECC31343CBCB8F70
];

const SUMS: &str = "SHA256SUMS.txt";
const SIG: &str = "SHA256SUMS.txt.minisig";
/// A release archive is about 4 MB. Anything far bigger is not one.
const MAX_ARCHIVE: u64 = 64 << 20;

/// The archive suffix for this platform, matching the release workflow.
fn platform_suffix() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("-linux-x86_64.tar.gz")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("-windows-x64.zip")
    } else {
        None
    }
}

/// The executable's path inside the archive.
const EXE_IN_ARCHIVE: &str = if cfg!(windows) {
    "mnm-cartographer/mnm-cartographer.exe"
} else {
    "mnm-cartographer/mnm-cartographer"
};

/// `v1.2.3` or `1.2.3` as a comparable triple. Anything else -- a pre-release
/// suffix, a fourth part -- is not something to offer.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.strip_prefix('v').unwrap_or(s).split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

#[derive(Clone, Debug)]
struct Asset {
    url: String,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    /// The release page, for "What's new" and as the fallback when the app
    /// cannot install the release itself.
    pub page: String,
    archive: Option<(String, Asset)>,
    sums: Option<Asset>,
    sig: Option<Asset>,
}

impl Release {
    /// Whether "Update now" can be offered: there is an archive for this
    /// platform and a signature to check it with. Whether the signature is
    /// any good is only known once it is downloaded.
    pub fn installable(&self) -> bool {
        self.archive.is_some() && self.sums.is_some() && self.sig.is_some()
    }

    fn from_json(v: &serde_json::Value) -> Option<Self> {
        let tag = v["tag_name"].as_str()?.to_string();
        let page = v["html_url"].as_str()?.to_string();
        let mut r = Release { tag, page, archive: None, sums: None, sig: None };
        let want = platform_suffix().map(|s| format!("mnm-cartographer-{}{s}", r.tag));
        for a in v["assets"].as_array().into_iter().flatten() {
            let (Some(name), Some(url)) =
                (a["name"].as_str(), a["browser_download_url"].as_str()) else { continue };
            let asset = Asset { url: url.to_string() };
            match name {
                SUMS => r.sums = Some(asset),
                SIG => r.sig = Some(asset),
                n if Some(n) == want.as_deref() => r.archive = Some((n.to_string(), asset)),
                _ => {}
            }
        }
        Some(r)
    }
}

#[derive(Clone, Debug)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading { tag: String, done: u64, total: Option<u64> },
    /// Swapped in; takes effect on the next start.
    Installed { tag: String },
    Failed { msg: String, page: Option<String> },
}

pub struct Updater {
    state: Arc<Mutex<State>>,
    /// The executable as it was at startup. On Linux, once the file has been
    /// replaced, `current_exe` names the deleted original; this does not.
    exe: Option<PathBuf>,
}

impl Updater {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(State::Idle)), exe: std::env::current_exe().ok() }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn busy(&self) -> bool {
        matches!(self.state(), State::Checking | State::Downloading { .. })
    }

    /// Look for a newer release in the background. A `quiet` check -- the one
    /// on launch -- says nothing if it fails: being offline is not news.
    pub fn check(&self, ctx: &egui::Context, quiet: bool) {
        if self.busy() { return }
        *self.state.lock().unwrap() = State::Checking;
        let (state, ctx) = (self.state.clone(), ctx.clone());
        std::thread::spawn(move || {
            let s = match latest() {
                Ok(r) if newer(&r.tag) => State::Available(r),
                Ok(_) => State::UpToDate,
                Err(_) if quiet => State::Idle,
                Err(e) => State::Failed { msg: format!("update check failed: {e}"), page: None },
            };
            *state.lock().unwrap() = s;
            ctx.request_repaint();
        });
    }

    /// Download, verify and swap in `r`, in the background.
    pub fn install(&self, r: Release, ctx: &egui::Context) {
        if self.busy() { return }
        let Some(exe) = self.exe.clone() else {
            *self.state.lock().unwrap() = State::Failed {
                msg: "cannot tell where this program is running from".into(),
                page: Some(r.page),
            };
            return;
        };
        *self.state.lock().unwrap() =
            State::Downloading { tag: r.tag.clone(), done: 0, total: None };
        let (state, ctx) = (self.state.clone(), ctx.clone());
        std::thread::spawn(move || {
            let progress = {
                let (state, ctx, tag) = (state.clone(), ctx.clone(), r.tag.clone());
                move |done, total| {
                    *state.lock().unwrap() = State::Downloading { tag: tag.clone(), done, total };
                    ctx.request_repaint();
                }
            };
            let s = match install(&r, &exe, &keys(), progress) {
                Ok(()) => State::Installed { tag: r.tag.clone() },
                Err(e) => State::Failed { msg: format!("{e}"), page: Some(r.page.clone()) },
            };
            *state.lock().unwrap() = s;
            ctx.request_repaint();
        });
    }

    /// Start the new executable with the same arguments. The caller then
    /// closes this one.
    pub fn restart(&self) -> anyhow::Result<()> {
        let exe = self.exe.as_ref().ok_or_else(|| anyhow::anyhow!("no executable path"))?;
        std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn()?;
        Ok(())
    }
}

/// `--update`: check, and with `apply` install, from the command line.
pub fn cli(apply: bool) -> anyhow::Result<()> {
    let r = latest()?;
    if !newer(&r.tag) {
        println!("up to date: {VERSION} (latest release {})", r.tag);
        return Ok(());
    }
    println!("{} is available (you have {VERSION}): {}", r.tag, r.page);
    if !r.installable() {
        println!("it has no signed download for this platform; get it from the page above");
        return Ok(());
    }
    if !apply {
        println!("run with --update to install it");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    install(&r, &exe, &keys(), |_, _| {})?;
    println!("installed {} over {}", r.tag, exe.display());
    Ok(())
}

fn newer(tag: &str) -> bool {
    match (parse_version(tag), parse_version(VERSION)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The keys to trust. A debug build also takes one from the environment, so
/// the whole path can be tried against a throwaway key; a release build never
/// does.
fn keys() -> Vec<String> {
    let mut k: Vec<String> = KEYS.iter().map(|s| s.to_string()).collect();
    if cfg!(debug_assertions) {
        if let Ok(t) = std::env::var("MNM_UPDATE_TEST_KEY") { k.push(t) }
    }
    k
}

pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(timeout)
        .user_agent(&format!("mnm-cartographer/{VERSION}"))
        .build()
}

fn latest() -> anyhow::Result<Release> {
    let mut url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    if cfg!(debug_assertions) {
        if let Ok(u) = std::env::var("MNM_UPDATE_URL") { url = u }
    }
    let body = agent(Duration::from_secs(20))
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()?
        .into_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    Release::from_json(&v).ok_or_else(|| anyhow::anyhow!("unexpected reply from GitHub"))
}

fn fetch(url: &str, limit: u64, mut progress: impl FnMut(u64, Option<u64>)) -> anyhow::Result<Vec<u8>> {
    let resp = agent(Duration::from_secs(300)).get(url).call()?;
    let total = resp.header("Content-Length").and_then(|v| v.parse().ok());
    if total.is_some_and(|t| t > limit) {
        anyhow::bail!("download is larger than expected");
    }
    let mut reader = resp.into_reader().take(limit + 1);
    let mut out = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 { break }
        out.extend_from_slice(&buf[..n]);
        progress(out.len() as u64, total);
    }
    if out.len() as u64 > limit {
        anyhow::bail!("download is larger than expected");
    }
    Ok(out)
}

fn install(r: &Release, exe: &Path, keys: &[String], progress: impl FnMut(u64, Option<u64>))
    -> anyhow::Result<()>
{
    let (Some((name, archive)), Some(sums), Some(sig)) = (&r.archive, &r.sums, &r.sig) else {
        anyhow::bail!("this release has no signed download for this platform");
    };
    let sums = fetch(&sums.url, 1 << 16, |_, _| {})?;
    let sig = String::from_utf8(fetch(&sig.url, 1 << 12, |_, _| {})?)?;
    verify_sums(&sums, &sig, &r.tag, keys)?;
    let want = expected_hash(&sums, name)
        .ok_or_else(|| anyhow::anyhow!("{name} is not in the signed checksum list"))?;
    let data = fetch(&archive.url, MAX_ARCHIVE, progress)?;
    if hex(&Sha256::digest(&data)) != want {
        anyhow::bail!("the download does not match its signed checksum");
    }
    let bin = unpack(&data)?;
    replace(exe, &bin)
}

/// Check `sig` is a signature over `sums` by one of `keys`, made for `tag`.
pub fn verify_sums(sums: &[u8], sig: &str, tag: &str, keys: &[String]) -> anyhow::Result<()> {
    let sig = minisign_verify::Signature::decode(sig)
        .map_err(|e| anyhow::anyhow!("unreadable signature: {e}"))?;
    let ok = keys.iter()
        .filter_map(|k| minisign_verify::PublicKey::from_base64(k).ok())
        .any(|k| k.verify(sums, &sig, false).is_ok());
    if !ok {
        anyhow::bail!("this release is not signed by the maintainer's key; not installing it");
    }
    // The trusted comment is covered by the signature, so it really does say
    // which release was signed.
    let want = format!("mnm-cartographer {tag}");
    if sig.trusted_comment() != want {
        anyhow::bail!("signature is for \"{}\", not {tag}", sig.trusted_comment());
    }
    Ok(())
}

/// The hash `sha256sum` recorded for `name`.
pub fn expected_hash(sums: &[u8], name: &str) -> Option<String> {
    std::str::from_utf8(sums).ok()?.lines().find_map(|l| {
        let (hash, file) = l.split_once(' ')?;
        // `sha256sum` writes "hash  name", or "hash *name" in binary mode.
        let file = file.trim_start_matches([' ', '*']);
        (file == name && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The executable out of a release archive.
fn unpack(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if cfg!(windows) { unpack_zip(data, EXE_IN_ARCHIVE) } else { unpack_tar_gz(data, EXE_IN_ARCHIVE) }
}

fn unpack_zip(data: &[u8], want: &str) -> anyhow::Result<Vec<u8>> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data))?;
    // Compress-Archive writes backslashes on some PowerShell versions.
    let idx = (0..z.len())
        .find(|&i| z.name_for_index(i).is_some_and(|n| n.replace('\\', "/") == want))
        .ok_or_else(|| anyhow::anyhow!("no {want} in the download"))?;
    let mut out = Vec::new();
    z.by_index(idx)?.read_to_end(&mut out)?;
    Ok(out)
}

fn unpack_tar_gz(data: &[u8], want: &str) -> anyhow::Result<Vec<u8>> {
    let mut t = tar::Archive::new(flate2::read::GzDecoder::new(data));
    for e in t.entries()? {
        let mut e = e?;
        if e.path()?.to_str() == Some(want) {
            let mut out = Vec::new();
            e.read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    anyhow::bail!("no {want} in the download")
}

/// Put `bin` in place of the running executable at `exe`. Staged beside it
/// first, so a directory we cannot write to fails before anything changes.
fn replace(exe: &Path, bin: &[u8]) -> anyhow::Result<()> {
    let dir = exe.parent().ok_or_else(|| anyhow::anyhow!("no folder for {}", exe.display()))?;
    let staged = dir.join(if cfg!(windows) { ".mnm-cartographer-new.exe" } else { ".mnm-cartographer-new" });
    std::fs::write(&staged, bin).map_err(|e| anyhow::anyhow!(
        "cannot write to {}: {e}. Download the new version from the release page instead.",
        dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let res = self_replace::self_replace(&staged);
    std::fs::remove_file(&staged).ok();
    res.map_err(|e| anyhow::anyhow!("could not replace {}: {e}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with a throwaway key (`minisign -G -W`) that signs nothing else:
    // a SHA256SUMS.txt for v9.9.9, and its signature.
    const TEST_KEY: &str = include_str!("../tests/update/test.pub.b64");
    const TEST_SUMS: &[u8] = include_bytes!("../tests/update/SHA256SUMS.txt");
    const TEST_SIG: &str = include_str!("../tests/update/SHA256SUMS.txt.minisig");

    fn test_keys() -> Vec<String> { vec![TEST_KEY.trim().to_string()] }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.0.4"), Some((0, 0, 4)));
        assert_eq!(parse_version("1.10.0"), Some((1, 10, 0)));
        assert_eq!(parse_version("v1.0.0-rc1"), None);
        assert_eq!(parse_version("v1.0"), None);
        assert_eq!(parse_version("v1.0.0.1"), None);
        assert!(parse_version("v0.0.10") > parse_version("v0.0.9"));
    }

    #[test]
    fn good_signature() {
        verify_sums(TEST_SUMS, TEST_SIG, "v9.9.9", &test_keys()).unwrap();
    }

    #[test]
    fn real_keys_reject_test_signature() {
        let real: Vec<String> = KEYS.iter().map(|s| s.to_string()).collect();
        assert!(verify_sums(TEST_SUMS, TEST_SIG, "v9.9.9", &real).is_err());
    }

    #[test]
    fn tampered_sums_rejected() {
        let mut s = TEST_SUMS.to_vec();
        s[0] = if s[0] == b'0' { b'1' } else { b'0' };
        assert!(verify_sums(&s, TEST_SIG, "v9.9.9", &test_keys()).is_err());
    }

    #[test]
    fn signature_for_another_release_rejected() {
        assert!(verify_sums(TEST_SUMS, TEST_SIG, "v10.0.0", &test_keys()).is_err());
    }

    #[test]
    fn hash_lookup() {
        let h = expected_hash(TEST_SUMS, "mnm-cartographer-v9.9.9-windows-x64.zip").unwrap();
        assert_eq!(h.len(), 64);
        assert!(expected_hash(TEST_SUMS, "windows-x64.zip").is_none());
        let bin = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *a.zip\n";
        assert!(expected_hash(bin, "a.zip").is_some());
    }

    // Laid out as the release workflow packs them, the exe holding "exe".
    #[test]
    fn unpacks_windows_zip() {
        let z = include_bytes!("../tests/update/release.zip");
        assert_eq!(unpack_zip(z, "mnm-cartographer/mnm-cartographer.exe").unwrap(), b"exe");
        assert!(unpack_zip(z, "mnm-cartographer/missing.exe").is_err());
    }

    #[test]
    fn unpacks_linux_tar_gz() {
        let t = include_bytes!("../tests/update/release.tar.gz");
        assert_eq!(unpack_tar_gz(t, "mnm-cartographer/mnm-cartographer").unwrap(), b"exe");
        assert!(unpack_tar_gz(t, "mnm-cartographer/missing").is_err());
    }

    #[test]
    fn picks_this_platforms_assets() {
        let v = serde_json::json!({
            "tag_name": "v9.9.9",
            "html_url": "https://example/release",
            "assets": [
                {"name": "mnm-cartographer-v9.9.9-linux-x86_64.tar.gz", "browser_download_url": "L"},
                {"name": "mnm-cartographer-v9.9.9-windows-x64.zip", "browser_download_url": "W"},
                {"name": "SHA256SUMS.txt", "browser_download_url": "S"},
            ]
        });
        let r = Release::from_json(&v).unwrap();
        assert!(r.archive.is_some() == platform_suffix().is_some());
        assert!(!r.installable(), "no signature, so not installable");
    }
}
