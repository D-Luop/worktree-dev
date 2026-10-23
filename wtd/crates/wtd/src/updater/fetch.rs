//! Download (http(s) or file://, resumable, retried), verify and unpack a release payload.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::{read_version, sign, Layout, Manifest};

pub struct Opts {
    pub attempts: u32,
    pub backoff: Duration,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { attempts: 4, backoff: Duration::from_secs(5) }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("WorkTreeDev/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:///").or_else(|| url.strip_prefix("file://"))?;
    Some(crate::paths::normalize(&rest.replace("%20", " ")))
}

pub fn get_text(url: &str) -> Result<String> {
    if let Some(p) = file_url(url) {
        return std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()));
    }
    let r = agent().get(url).call().map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
    let mut s = String::new();
    r.into_reader().take(4 << 20).read_to_string(&mut s)?;
    Ok(s)
}

/// Download `url` to `dest`, resuming a partial `dest.part` with an HTTP Range request and retrying
/// transient failures. `progress(done, total)` is called as bytes arrive.
pub fn download(url: &str, dest: &Path, size: u64, opts: &Opts, progress: &dyn Fn(u64, u64)) -> Result<()> {
    let part = dest.with_extension("part");
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(src) = file_url(url) {
        std::fs::copy(&src, &part).with_context(|| format!("copying {}", src.display()))?;
        std::fs::rename(&part, dest)?;
        let n = std::fs::metadata(dest)?.len();
        progress(n, n);
        return Ok(());
    }
    let mut last = None;
    for attempt in 0..opts.attempts.max(1) {
        if attempt > 0 {
            std::thread::sleep(opts.backoff * attempt);
        }
        match download_once(url, &part, size, progress) {
            Ok(()) => {
                std::fs::rename(&part, dest)?;
                return Ok(());
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap()).with_context(|| format!("downloading {url}"))
}

fn download_once(url: &str, part: &Path, size: u64, progress: &dyn Fn(u64, u64)) -> Result<()> {
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if size > 0 && have == size {
        return Ok(());
    }
    let mut req = agent().get(url);
    if have > 0 && (size == 0 || have < size) {
        req = req.set("Range", &format!("bytes={have}-"));
    }
    let r = match req.call() {
        Ok(r) => r,
        // the partial file is already complete, or stale: start over
        Err(ureq::Error::Status(416, _)) => {
            std::fs::remove_file(part)?;
            bail!("range not satisfiable; restarting");
        }
        Err(e) => bail!("GET {url}: {e}"),
    };
    let resumed = r.status() == 206;
    let total = if size > 0 { size } else { r.header("Content-Length").and_then(|l| l.parse::<u64>().ok()).map_or(0, |l| if resumed { l + have } else { l }) };
    let mut f = if resumed { OpenOptions::new().append(true).open(part)? } else { File::create(part)? };
    let mut done = if resumed { have } else { 0 };
    let mut rd = r.into_reader();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = rd.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    f.flush()?;
    if total > 0 && done != total {
        bail!("short download: {done} of {total} bytes");
    }
    Ok(())
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = File::open(p).with_context(|| format!("opening {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn verify_sha256(p: &Path, expected: &str) -> Result<()> {
    let got = sha256_file(p)?;
    if !got.eq_ignore_ascii_case(expected) {
        bail!("sha256 mismatch for {}: expected {expected}, got {got}", p.display());
    }
    Ok(())
}

/// Unpack a payload zip (in `current\` shape; one wrapping top-level folder is tolerated) into `dest`.
pub fn unzip(zip_path: &Path, dest: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(File::open(zip_path)?).context("reading the payload zip")?;
    let names: Vec<PathBuf> = (0..z.len()).filter_map(|i| z.by_index(i).ok()?.enclosed_name()).collect();
    let prefix = common_root(&names);
    std::fs::create_dir_all(dest)?;
    for i in 0..z.len() {
        let mut e = z.by_index(i)?;
        let Some(rel) = e.enclosed_name() else { bail!("unsafe path in payload: {}", e.name()) };
        let rel = match &prefix {
            Some(p) => rel.strip_prefix(p).map(Path::to_path_buf).unwrap_or(rel),
            None => rel,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut f = File::create(&out).with_context(|| format!("writing {}", out.display()))?;
        std::io::copy(&mut e, &mut f)?;
    }
    Ok(())
}

/// Every entry under one folder that isn't itself the payload (no `bin/` at the root): strip it.
fn common_root(names: &[PathBuf]) -> Option<PathBuf> {
    if names.iter().any(|n| n.components().next().is_some_and(|c| c.as_os_str() == "bin")) {
        return None;
    }
    let first = names.first()?.components().next()?.as_os_str().to_owned();
    names.iter().all(|n| n.components().next().is_some_and(|c| c.as_os_str() == first) && n.components().count() >= 1).then(|| PathBuf::from(first))
}

/// Download, verify and unpack `m`'s payload to `versions\<ver>\`.
pub fn stage(l: &Layout, m: &Manifest, opts: &Opts, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let v = m.version();
    let pending = l.pending_dir(&v);
    let zip = pending.join("payload.zip");
    if zip.is_file() && verify_sha256(&zip, &m.payload.sha256).is_err() {
        std::fs::remove_file(&zip)?;
    }
    if !zip.is_file() {
        download(&m.payload.url, &zip, m.payload.size, opts, progress)?;
    }
    if let Err(e) = verify_sha256(&zip, &m.payload.sha256) {
        let _ = std::fs::remove_file(&zip);
        return Err(e);
    }
    let tmp = l.versions().join(format!("{v}.tmp"));
    if tmp.exists() {
        crate::settings::remove_tree(&tmp)?;
    }
    unzip(&zip, &tmp)?;
    check_payload(&tmp, &v)?;
    if sign::is_signed(&std::env::current_exe()?) {
        for exe in ["wtd.exe", "wtd-tray.exe"] {
            let p = tmp.join("bin").join(exe);
            if p.is_file() {
                sign::verify(&p).with_context(|| format!("{exe} in {v}: this build is signed, the update isn't"))?;
            }
        }
    }
    let dest = l.version_dir(&v);
    if dest.exists() {
        crate::settings::remove_tree(&dest)?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn check_payload(dir: &Path, v: &str) -> Result<()> {
    if !dir.join("bin").join("wtd.exe").is_file() {
        bail!("payload has no bin\\wtd.exe");
    }
    match read_version(dir) {
        Some(got) if got != v => bail!("payload version.txt says {got}, latest.json says {v}"),
        Some(_) => Ok(()),
        None => Ok(std::fs::write(dir.join("version.txt"), format!("{v}\n"))?),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::tests::temp;
    use super::super::{Asset, State};
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A zip in `current\` shape: bin\wtd.exe (+ tray), version.txt, a template and a vsix.
    pub fn fake_payload(dir: &Path, v: &str, wrap: Option<&str>) -> PathBuf {
        let p = dir.join(format!("payload-{v}.zip"));
        let mut z = zip::ZipWriter::new(File::create(&p).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        let pre = wrap.map(|w| format!("{w}/")).unwrap_or_default();
        for (name, body) in [
            ("bin/wtd.exe", format!("wtd {v}")),
            ("bin/wtd-tray.exe", format!("tray {v}")),
            ("version.txt", format!("{v}\n")),
            ("templates/x.md", "t".into()),
            ("claude-status.vsix", format!("vsix {v}")),
        ] {
            z.start_file(format!("{pre}{name}"), o).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        p
    }

    pub fn file_uri(p: &Path) -> String {
        format!("file:///{}", p.to_string_lossy().replace('\\', "/"))
    }

    fn manifest(v: &str, url: String, sha: String, size: u64) -> Manifest {
        Manifest { version: v.into(), channel: "stable".into(), published: String::new(), payload: Asset { url, sha256: sha, size }, setup: None, min_daemon_protocol: 1, notes_url: None }
    }

    /// One-file HTTP server honouring `Range`. The first `fail_first` requests send half the body
    /// and hang up, so the client has to resume.
    fn serve(body: Vec<u8>, fail_first: usize) -> (String, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/payload.zip", l.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let ranges = Arc::new(std::sync::Mutex::new(vec![]));
        let (h, rg) = (hits.clone(), ranges.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let n = h.fetch_add(1, Ordering::SeqCst);
                let mut rd = BufReader::new(s.try_clone().unwrap());
                let mut from = 0usize;
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(r) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        from = r.trim().trim_end_matches('-').parse().unwrap();
                        rg.lock().unwrap().push(r.trim().to_string());
                    }
                }
                let rest = &body[from..];
                let head = if from > 0 {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n", rest.len(), body.len() - 1, body.len())
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", rest.len())
                };
                let _ = s.write_all(head.as_bytes());
                let send = if n < fail_first { &rest[..rest.len() / 2] } else { rest };
                let _ = s.write_all(send);
                let _ = s.flush();
            }
        });
        (url, hits, ranges)
    }

    #[test]
    fn stages_from_a_file_url_and_strips_a_wrapping_folder() {
        let t = temp("file");
        let zip = fake_payload(&t, "1.2.0", Some("worktree-dev-1.2.0"));
        let l = Layout::new(t.join("prog"));
        let m = manifest("1.2.0", file_uri(&zip), sha256_file(&zip).unwrap(), 0);
        let dir = stage(&l, &m, &Opts::default(), &|_, _| {}).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("bin").join("wtd.exe")).unwrap(), "wtd 1.2.0");
        assert_eq!(read_version(&dir).as_deref(), Some("1.2.0"));
        assert!(dir.join("templates").join("x.md").is_file());
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn sha_mismatch_is_rejected_and_nothing_is_staged() {
        let t = temp("sha");
        let zip = fake_payload(&t, "1.2.0", None);
        let l = Layout::new(t.join("prog"));
        let m = manifest("1.2.0", file_uri(&zip), "00".repeat(32), 0);
        let e = stage(&l, &m, &Opts::default(), &|_, _| {}).unwrap_err();
        assert!(format!("{e:#}").contains("sha256 mismatch"), "{e:#}");
        assert!(!l.version_dir("1.2.0").exists());
        assert!(!l.pending_dir("1.2.0").join("payload.zip").exists(), "a bad download is discarded");
        assert_eq!(State::load(&l).staged, None);
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn payload_version_must_match_the_manifest() {
        let t = temp("ver");
        let zip = fake_payload(&t, "1.3.0", None);
        let l = Layout::new(t.join("prog"));
        let m = manifest("1.2.0", file_uri(&zip), sha256_file(&zip).unwrap(), 0);
        assert!(format!("{:#}", stage(&l, &m, &Opts::default(), &|_, _| {}).unwrap_err()).contains("version.txt says 1.3.0"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn http_download_resumes_after_a_dropped_connection() {
        let t = temp("http");
        let zip = fake_payload(&t, "1.2.0", None);
        let body = std::fs::read(&zip).unwrap();
        let (url, hits, ranges) = serve(body.clone(), 1);
        let dest = t.join("dl").join("payload.zip");
        let seen = std::sync::Mutex::new(0u64);
        download(&url, &dest, body.len() as u64, &Opts { attempts: 3, backoff: Duration::from_millis(10) }, &|d, _| *seen.lock().unwrap() = d).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(ranges.lock().unwrap().as_slice(), [format!("{}-", body.len() / 2)]);
        assert_eq!(*seen.lock().unwrap(), body.len() as u64);
        assert!(!dest.with_extension("part").exists());
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn http_download_gives_up_after_its_attempts() {
        let t = temp("http-fail");
        let (url, hits, _) = serve(vec![7u8; 4096], usize::MAX);
        let dest = t.join("payload.zip");
        assert!(download(&url, &dest, 4096, &Opts { attempts: 2, backoff: Duration::from_millis(5) }, &|_, _| {}).is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(!dest.exists());
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn check_and_stage_reads_the_manifest_url() {
        let t = temp("check");
        let zip = fake_payload(&t, "9.0.0", None);
        let mf = t.join("latest.json");
        std::fs::write(&mf, serde_json::to_string(&manifest("9.0.0", file_uri(&zip), sha256_file(&zip).unwrap(), 0)).unwrap()).unwrap();
        let l = Layout::new(t.join("prog"));
        std::fs::create_dir_all(l.current()).unwrap();
        std::fs::write(l.current().join("version.txt"), "1.0.0").unwrap();
        let _env = super::super::apply::tests::ENV.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("WTD_UPDATE_URL", file_uri(&mf));
        let r = super::super::check_and_stage(&l, super::super::Channel::Stable, &|_, _| {});
        std::env::remove_var("WTD_UPDATE_URL");
        assert_eq!(r.unwrap().as_deref(), Some("9.0.0"));
        let st = State::load(&l);
        assert_eq!((st.latest.as_deref(), st.staged.as_deref(), st.error), (Some("9.0.0"), Some("9.0.0"), None));
        assert_eq!(l.staged().as_deref(), Some("9.0.0"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn file_urls() {
        assert_eq!(file_url("file:///D:/x/latest.json"), Some(PathBuf::from(r"D:\x\latest.json")));
        assert_eq!(file_url("file://D:/x%20y/a.zip"), Some(PathBuf::from(r"D:\x y\a.zip")));
        assert_eq!(file_url("https://x"), None);
    }
}
