use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::{Error, Result};
use crate::urls::{curl_url_args, restore_effective_url};

pub(crate) fn curl_push_url(cmd: &mut Command, url: &str) {
    for a in curl_url_args(url) {
        cmd.arg(a);
    }
}

/// Cooperative cancel plus a list of child PIDs (curl, ffmpeg) to kill immediately.
#[derive(Clone, Default)]
pub struct CancelCtx {
    flag: Arc<AtomicBool>,
    pids: Arc<Mutex<HashSet<u32>>>,
}

impl CancelCtx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::Relaxed);
        let pids: Vec<u32> = self.pids.lock().unwrap().drain().collect();
        for pid in pids {
            kill_pid(pid);
        }
    }

    pub(crate) fn track(&self, pid: u32) {
        if self.is_cancelled() {
            kill_pid(pid);
            return;
        }
        self.pids.lock().unwrap().insert(pid);
        if self.is_cancelled() {
            self.pids.lock().unwrap().remove(&pid);
            kill_pid(pid);
        }
    }

    pub(crate) fn untrack(&self, pid: u32) {
        self.pids.lock().unwrap().remove(&pid);
    }
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

thread_local! {
    static CURRENT_CANCEL: RefCell<Option<CancelCtx>> = const { RefCell::new(None) };
}

pub fn with_cancel<R>(ctx: Option<&CancelCtx>, f: impl FnOnce() -> R) -> R {
    CURRENT_CANCEL.with(|slot| {
        let prev = slot.replace(ctx.cloned());
        let out = f();
        slot.replace(prev);
        out
    })
}

pub(crate) fn current_cancel() -> Option<CancelCtx> {
    CURRENT_CANCEL.with(|slot| slot.borrow().clone())
}

pub(crate) fn cancelled_error() -> Error {
    Error::msg("play: download cancelled")
}

pub fn http_headers(ua: &str, referer: &str, origin: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("User-Agent".into(), ua.into());
    h.insert("Referer".into(), referer.into());
    h.insert("Origin".into(), origin.into());
    h.insert("Accept".into(), "*/*".into());
    h
}

/// True when Referer is from a different host than the stream (not the CDN default).
fn is_embed_referer(stream_url: &str, referer: Option<&str>) -> bool {
    let Some(referer) = referer.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let stream_host = host_of_url(stream_url);
    let ref_host = host_of_url(referer);
    !stream_host.is_empty() && !ref_host.is_empty() && stream_host != ref_host
}

/// Same-origin GET (browser omits Origin). Cross-origin CORS Origin is sent.
/// First-party Cookie is not sent to another host (the CDN).
pub(crate) fn omit_curl_header(
    name: &str,
    value: &str,
    url: &str,
    headers: &HashMap<String, String>,
) -> bool {
    let v = sanitize_header_value(value);
    if v.is_empty() {
        return true;
    }
    if name.eq_ignore_ascii_case("origin") && !is_embed_referer(url, Some(&v)) {
        return true;
    }
    if name.eq_ignore_ascii_case("cookie") {
        let referer = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("referer"))
            .map(|(_, val)| val.as_str());
        if let Some(r) = referer {
            let uh = host_of_url(url);
            let rh = host_of_url(r);
            if !uh.is_empty() && !rh.is_empty() && uh != rh {
                return true;
            }
        }
    }
    false
}

fn path_looks_like_playlist(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".m3u8") || p.ends_with(".m3u") || p.ends_with(".mpd") || p.contains(".m3u8")
}

/// Playlist URLs must not be Range-probed: some WAFs 403 `Range` on .m3u8.
pub(crate) fn looks_like_playlist_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return path_looks_like_playlist(url);
    };
    if path_looks_like_playlist(u.path()) {
        return true;
    }
    if let Some(inner) = u
        .query_pairs()
        .find(|(k, _)| k == "url")
        .map(|(_, v)| v.into_owned())
    {
        if inner.starts_with("http://") || inner.starts_with("https://") {
            return looks_like_playlist_url(&inner);
        }
    }
    false
}

fn host_of_url(url: &str) -> String {
    let parsed = url::Url::parse(url).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or("")
        .to_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

pub fn http_403_hint(url: &str, body: &[u8]) -> String {
    http_403_hint_for(url, body, None)
}

pub fn http_403_hint_for(url: &str, body: &[u8], referer: Option<&str>) -> String {
    let n = body.len().min(8192);
    let text = String::from_utf8_lossy(&body[..n]).to_lowercase();
    if text.contains("website access blocked")
        || (text.contains("affected zone") && text.contains("violations"))
    {
        return " (Cloudflare disabled this CDN zone for ToS violations; \
a Referer will not help — copy a fresh playlist URL from the player)"
            .into();
    }
    if text.contains("cloudflare")
        || text.contains("attention required")
        || text.contains("cf-error-details")
    {
        return " (Cloudflare blocked this; pass --referer from the page that embeds the stream)"
            .into();
    }
    if text.contains("nginx") && text.contains("403 forbidden") {
        if is_embed_referer(url, referer) {
            return " (nginx denied the playlist. Use the Referer from the m3u8 \
request in DevTools — the player iframe, not the outer page. Replay that \
request in the browser; if it 403s there too, master.m3u8 is spent — copy a \
quality playlist that still returns 200, or Copy as cURL)"
                .into();
        }
        return " (nginx denied the playlist; pass --referer from the player \
iframe that loads this stream, not the outer page)"
            .into();
    }
    if url.contains("?t=") || url.contains("&t=") || url.contains("&e=") || url.contains("&s=") {
        return " (stale signed token, or quote the URL so zsh does not split on &)".into();
    }
    if url.contains('&') {
        return " (quote the URL so zsh does not split on &)".into();
    }
    String::new()
}

pub fn referer_needed(url: &str, body: &[u8]) -> bool {
    referer_needed_for(url, body, None)
}

pub fn referer_needed_for(url: &str, body: &[u8], referer: Option<&str>) -> bool {
    http_403_hint_for(url, body, referer).contains("--referer")
}

fn sanitize_header_value(v: &str) -> String {
    v.chars()
        .filter(|c| *c != '\r' && *c != '\n' && !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn header_map(url: &str, headers: &HashMap<String, String>) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if omit_curl_header(k, v, url, headers) {
            continue;
        }
        let v = sanitize_header_value(v);
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            map.insert(name, val);
        }
    }
    map
}

fn client(timeout_secs: u64) -> Result<Client> {
    // urllib was HTTP/1.1. HTTP/2 to some CDNs 403s even with a valid Referer.
    let mut b = Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::limited(10));
    if timeout_secs > 0 {
        b = b.timeout(Duration::from_secs(timeout_secs));
    }
    b.build()
        .map_err(|e| Error::msg(format!("play: http client: {e}")))
}

pub fn http_get(url: &str, headers: &HashMap<String, String>) -> Result<(String, Vec<u8>)> {
    http_get_timeout(url, headers, 30)
}

/// Stream a URL to a file. No size cap and no short timeout (progressive downloads).
pub fn http_get_to_file(url: &str, headers: &HashMap<String, String>, dest: &str) -> Result<()> {
    if current_cancel().is_some_and(|c| c.is_cancelled()) {
        return Err(cancelled_error());
    }
    if !has_curl() {
        let (_, body) = http_get_timeout(url, headers, 0)?;
        fs::write(dest, body)
            .map_err(|e| Error::msg(format!("play: failed writing {dest}: {e}")))?;
        return Ok(());
    }
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--http1.1",
        "-L",
        "--max-redirs",
        "10",
        "-o",
        dest,
        "-w",
        "%{http_code}",
    ]);
    for (k, v) in headers {
        if omit_curl_header(k, v, url, headers) {
            continue;
        }
        let v = sanitize_header_value(v);
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    curl_push_url(&mut cmd, url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?;
    let pid = child.id();
    if let Some(hook) = current_cancel() {
        hook.track(pid);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?;
    if let Some(hook) = current_cancel() {
        hook.untrack(pid);
        if hook.is_cancelled() {
            let _ = fs::remove_file(dest);
            return Err(cancelled_error());
        }
    }
    if !out.status.success() {
        let _ = fs::remove_file(dest);
        if current_cancel().is_some_and(|c| c.is_cancelled()) {
            return Err(cancelled_error());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(Error::msg(format!(
            "play: failed fetching {url}: {}",
            if err.is_empty() {
                "curl failed".into()
            } else {
                err.to_string()
            }
        )));
    }
    let code: u16 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if !(200..300).contains(&code) {
        let _ = fs::remove_file(dest);
        return Err(Error::Http {
            code,
            url: url.to_string(),
            hint: http_error_hint(code, url, &[], headers),
            body: Vec::new(),
        });
    }
    Ok(())
}

pub fn http_get_timeout(
    url: &str,
    headers: &HashMap<String, String>,
    timeout_secs: u64,
) -> Result<(String, Vec<u8>)> {
    http_get_limited(url, headers, timeout_secs, None).map(|g| (g.url, g.body))
}

const PLAYLIST_MAX_BYTES: u64 = 2 * 1024 * 1024;
const PLAYLIST_TIMEOUT_SECS: u64 = 20;

struct LimitedGet {
    url: String,
    body: Vec<u8>,
}

pub enum MaybePlaylist {
    Playlist { url: String, body: Vec<u8> },
    Progressive { url: String },
}

pub fn is_hls_body(body: &[u8]) -> bool {
    let start = body
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .unwrap_or(0);
    body.get(start..)
        .is_some_and(|rest| rest.starts_with(b"#EXTM3U"))
}

pub fn looks_like_media_body(body: &[u8]) -> bool {
    if body.len() >= 12 && &body[4..8] == b"ftyp" {
        return true;
    }
    if body.starts_with(b"\x1aE\xdf\xa3") {
        return true;
    }
    if body.starts_with(b"RIFF") {
        return true;
    }
    if body.starts_with(b"ID3") {
        return true;
    }
    if body.len() >= 188 && body[0] == 0x47 {
        return true;
    }
    false
}

fn curl_aborted_as_media(status: &std::process::ExitStatus, stderr: &str) -> bool {
    matches!(status.code(), Some(28) | Some(63))
        || stderr.contains("Maximum file size exceeded")
        || stderr.contains("timed out")
}

/// Fetch a playlist without pulling a multi-GB progressive file.
pub fn http_get_maybe_playlist(
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<MaybePlaylist> {
    match http_get_limited(
        url,
        headers,
        PLAYLIST_TIMEOUT_SECS,
        Some(PLAYLIST_MAX_BYTES),
    ) {
        Ok(g) => {
            if is_hls_body(&g.body) {
                Ok(MaybePlaylist::Playlist {
                    url: g.url,
                    body: g.body,
                })
            } else {
                Ok(MaybePlaylist::Progressive { url: g.url })
            }
        }
        Err(e) => {
            let s = e.to_string();
            if s.contains("Maximum file size exceeded")
                || s.contains("timed out")
                || s.contains("curl: (28)")
                || s.contains("curl: (63)")
            {
                return Ok(MaybePlaylist::Progressive {
                    url: url.to_string(),
                });
            }
            Err(e)
        }
    }
}

fn http_get_limited(
    url: &str,
    headers: &HashMap<String, String>,
    timeout_secs: u64,
    max_bytes: Option<u64>,
) -> Result<LimitedGet> {
    if current_cancel().is_some_and(|c| c.is_cancelled()) {
        return Err(cancelled_error());
    }
    if has_curl() {
        curl_get(url, headers, timeout_secs, max_bytes)
    } else {
        reqwest_get(url, headers, timeout_secs, max_bytes)
    }
}

fn has_curl() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        Command::new("curl")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

static CURL_SEQ: AtomicU64 = AtomicU64::new(0);

fn curl_get(
    url: &str,
    headers: &HashMap<String, String>,
    timeout_secs: u64,
    max_bytes: Option<u64>,
) -> Result<LimitedGet> {
    let n = CURL_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("play-curl-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir)
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?;
    let body_path = dir.join("body");
    let result = curl_get_inner(url, headers, timeout_secs, max_bytes, &body_path);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn curl_get_inner(
    url: &str,
    headers: &HashMap<String, String>,
    timeout_secs: u64,
    max_bytes: Option<u64>,
    body_path: &PathBuf,
) -> Result<LimitedGet> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--http1.1",
        "-L",
        "--max-redirs",
        "10",
        "-o",
        body_path.to_str().unwrap_or("body"),
        "-w",
        "%{http_code}\n%{url_effective}",
    ]);
    if timeout_secs > 0 {
        cmd.args(["--max-time", &timeout_secs.to_string()]);
    }
    if let Some(n) = max_bytes {
        cmd.args(["--max-filesize", &n.to_string()]);
        if !looks_like_playlist_url(url) {
            let last = n.saturating_sub(1);
            cmd.args(["-r", &format!("0-{last}")]);
        }
    }
    for (k, v) in headers {
        if omit_curl_header(k, v, url, headers) {
            continue;
        }
        let v = sanitize_header_value(v);
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    curl_push_url(&mut cmd, url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?;
    let pid = child.id();
    if let Some(hook) = current_cancel() {
        hook.track(pid);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?;
    if let Some(hook) = current_cancel() {
        hook.untrack(pid);
        if hook.is_cancelled() {
            return Err(cancelled_error());
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if current_cancel().is_some_and(|c| c.is_cancelled()) {
            return Err(cancelled_error());
        }
        if max_bytes.is_some() && curl_aborted_as_media(&out.status, &stderr) {
            let body = fs::read(body_path).unwrap_or_default();
            if !is_hls_body(&body) {
                return Ok(LimitedGet {
                    url: url.to_string(),
                    body,
                });
            }
        }
        let err = stderr.trim();
        return Err(Error::msg(format!(
            "play: failed fetching {url}: {}",
            if err.is_empty() {
                "curl failed".into()
            } else {
                err.to_string()
            }
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let code: u16 = lines.next().unwrap_or("0").parse().unwrap_or(0);
    let final_url = restore_effective_url(url, lines.next().unwrap_or(url));
    let body = fs::read(body_path).unwrap_or_default();
    if (200..300).contains(&code) {
        return Ok(LimitedGet {
            url: final_url,
            body,
        });
    }
    let hint = http_error_hint(code, url, &body, headers);
    Err(Error::Http {
        code,
        url: url.to_string(),
        hint,
        body,
    })
}

fn http_error_hint(code: u16, url: &str, body: &[u8], headers: &HashMap<String, String>) -> String {
    if code == 403 {
        return http_403_hint_for(url, body, headers.get("Referer").map(|s| s.as_str()));
    }
    if code == 401 {
        return " (server requires auth — Copy as cURL from a 200 request in the browser, including Cookie)"
            .into();
    }
    if code == 400 {
        return " (nginx rejected the request; Copy as cURL from a 200 in DevTools)".into();
    }
    String::new()
}

fn reqwest_get(
    url: &str,
    headers: &HashMap<String, String>,
    timeout_secs: u64,
    max_bytes: Option<u64>,
) -> Result<LimitedGet> {
    let client = client(timeout_secs)?;
    let mut req = client.get(url).headers(header_map(url, headers));
    if let Some(n) = max_bytes {
        if !looks_like_playlist_url(url) {
            req = req.header(
                reqwest::header::RANGE,
                format!("bytes=0-{}", n.saturating_sub(1)),
            );
        }
    }
    let resp = req
        .send()
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?;
    let final_url = resp.url().to_string();
    let status = resp.status();
    let code = status.as_u16();
    if let Some(n) = max_bytes {
        if let Some(len) = resp.content_length() {
            if len > n && !is_mpegurl_type(resp.headers().get("content-type")) {
                return Ok(LimitedGet {
                    url: final_url,
                    body: Vec::new(),
                });
            }
        }
    }
    let mut body = resp
        .bytes()
        .map_err(|e| Error::msg(format!("play: failed fetching {url}: {e}")))?
        .to_vec();
    if let Some(n) = max_bytes {
        let n = n as usize;
        if body.len() > n {
            body.truncate(n);
        }
    }
    if status.is_success() {
        return Ok(LimitedGet {
            url: final_url,
            body,
        });
    }
    let hint = http_error_hint(code, url, &body, headers);
    Err(Error::Http {
        code,
        url: url.to_string(),
        hint,
        body,
    })
}

fn is_mpegurl_type(val: Option<&HeaderValue>) -> bool {
    val.and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().contains("mpegurl"))
}

fn maybe_playlist_to_result(got: MaybePlaylist) -> Result<(String, Vec<u8>)> {
    match got {
        MaybePlaylist::Playlist { url, body } => Ok((url, body)),
        MaybePlaylist::Progressive { url } => Err(Error::Progressive { url }),
    }
}

pub fn http_get_retry<F>(
    url: &str,
    headers: &mut HashMap<String, String>,
    on_403: Option<&mut F>,
) -> Result<(String, Vec<u8>)>
where
    F: FnMut(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>> + ?Sized,
{
    match http_get_maybe_playlist(url, headers) {
        Ok(got) => maybe_playlist_to_result(got),
        Err(Error::Http {
            code: 403,
            url: u,
            hint,
            body,
        }) => {
            let extra = on_403.and_then(|cb| cb(&u, headers, &body));
            if let Some(extra) = extra {
                for (k, v) in extra {
                    headers.insert(k, v);
                }
                return http_get_maybe_playlist(url, headers).and_then(maybe_playlist_to_result);
            }
            Err(Error::Http {
                code: 403,
                url: u,
                hint,
                body,
            })
        }
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::thread;
    use tiny_http::{Header, Response, Server, StatusCode};

    #[test]
    fn test_cloudflare_challenge_suggests_embed_referer() {
        let body = b"<title>Attention Required! | Cloudflare</title>\n<div id='cf-error-details'>";
        let hint = http_403_hint("https://cdn.example/abc/playlist.m3u8", body);
        assert!(hint.contains("Cloudflare"));
        assert!(hint.contains("--referer"));
        assert!(!hint.contains("stale signed token"));
    }

    #[test]
    fn test_cloudflare_tos_zone_block_does_not_suggest_referer() {
        let mut body = b"<title>Website Access Blocked</title><div id='cf-error-details'>".to_vec();
        body.extend(std::iter::repeat(b'x').take(2500));
        body.extend(
            b"Cloudflare has restricted access to this website due to \
Terms of Service violations.The affected zone is cdn.example.",
        );
        let hint = http_403_hint("https://cdn.example/hls/a/master.txt", &body);
        assert!(hint.contains("ToS"));
        assert!(hint.contains("zone"));
        assert!(!hint.contains("--referer"));
    }

    #[test]
    fn test_signed_url_suggests_token_or_quoting() {
        let url = "https://cdn.example/hls/show/index.m3u8?t=abc&s=1&e=43200";
        let hint = http_403_hint(url, b"");
        assert!(hint.contains("stale signed token"));
    }

    #[test]
    fn test_nginx_forbidden_suggests_expired_path_or_referer() {
        let body = b"<html>\r\n<head><title>403 Forbidden</title></head>\r\n\
<body>\r\n<center><h1>403 Forbidden</h1></center>\r\n\
<hr><center>nginx</center>\r\n</body>\r\n</html>";
        let hint = http_403_hint("https://cdn.example/hls/show/id/master.m3u8", body);
        assert!(hint.to_lowercase().contains("nginx"));
        assert!(hint.contains("player iframe"));
        assert!(hint.contains("--referer"));
    }

    #[test]
    fn test_nginx_forbidden_with_embed_referer_is_expired_path() {
        let body = b"<html>\r\n<head><title>403 Forbidden</title></head>\r\n\
<body>\r\n<center><h1>403 Forbidden</h1></center>\r\n\
<hr><center>nginx</center>\r\n</body>\r\n</html>";
        let url = "https://cdn.example/hls/show/id/master.m3u8";
        let hint = http_403_hint_for(url, body, Some("https://embed.example/watch/1/"));
        assert!(hint.to_lowercase().contains("nginx"));
        assert!(hint.contains("player iframe"));
        assert!(hint.contains("Copy as cURL"));
        assert!(!hint.contains("--referer"));
        assert!(!referer_needed_for(
            url,
            body,
            Some("https://embed.example/watch/1/")
        ));
        // Same-host Referer is the CDN default, still ask for the embed page.
        assert!(referer_needed_for(url, body, Some("https://cdn.example/")));
    }

    #[test]
    fn test_plain_403_has_no_hint() {
        assert_eq!(http_403_hint("https://cdn.example/a.m3u8", b"denied"), "");
    }

    #[test]
    fn test_referer_needed_only_when_hint_asks_for_it() {
        let cf = b"<title>Attention Required! | Cloudflare</title>";
        assert!(referer_needed("https://cdn.example/a.m3u8", cf));
        let nginx = b"<title>403 Forbidden</title><center>nginx</center>";
        assert!(referer_needed("https://cdn.example/a.m3u8", nginx));
        let tos = b"<title>Website Access Blocked</title>affected zone violations";
        assert!(!referer_needed("https://cdn.example/a.m3u8", tos));
        assert!(!referer_needed(
            "https://cdn.example/a.m3u8?t=x&s=1&e=2",
            b""
        ));
    }

    #[test]
    fn test_on_403_retry_succeeds() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let hits = Arc::new(AtomicU32::new(0));
        let hits2 = hits.clone();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                hits2.fetch_add(1, Ordering::SeqCst);
                let referer = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Referer"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                if referer == "https://embed.example/" {
                    let body = b"#EXTM3U\n".to_vec();
                    let _ = request.respond(Response::new(
                        StatusCode(200),
                        vec![Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap()],
                        Cursor::new(body.clone()),
                        Some(body.len()),
                        None,
                    ));
                } else {
                    let body = b"<title>403 Forbidden</title><center>nginx</center>".to_vec();
                    let _ = request.respond(Response::new(
                        StatusCode(403),
                        vec![Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap()],
                        Cursor::new(body.clone()),
                        Some(body.len()),
                        None,
                    ));
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}/master.m3u8");
        let mut headers = HashMap::from([
            ("User-Agent".into(), "t".into()),
            ("Referer".into(), "http://127.0.0.1/".into()),
            ("Accept".into(), "*/*".into()),
        ]);
        let mut on_403 = |u: &str, _h: &mut HashMap<String, String>, body: &[u8]| {
            assert!(referer_needed(u, body));
            Some(HashMap::from([(
                "Referer".into(),
                "https://embed.example/".into(),
            )]))
        };
        let (_got, body) = http_get_retry(&url, &mut headers, Some(&mut on_403)).unwrap();
        assert!(body.starts_with(b"#EXTM3U"));
        assert_eq!(headers.get("Referer").unwrap(), "https://embed.example/");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_referer_newline_is_stripped_and_sent() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let referer = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Referer"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                let ok = referer == "https://embed.example/";
                let body = if ok {
                    b"#EXTM3U\n".to_vec()
                } else {
                    referer.into_bytes()
                };
                let code = if ok { 200 } else { 400 };
                let _ = request.respond(Response::new(
                    StatusCode(code),
                    vec![Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                ));
            }
        });
        let url = format!("http://127.0.0.1:{port}/master.m3u8");
        let mut headers = HashMap::from([
            ("User-Agent".into(), "t".into()),
            ("Referer".into(), "https://embed.example/\r\n".into()),
            ("Accept".into(), "*/*".into()),
        ]);
        let (_got, body) = http_get(&url, &mut headers).unwrap();
        assert!(body.starts_with(b"#EXTM3U"));
    }

    #[test]
    fn test_hls_body_sniff() {
        assert!(is_hls_body(b"#EXTM3U\n#EXT-X-STREAM-INF\n"));
        assert!(is_hls_body(b"\n  #EXTM3U\n"));
        assert!(!is_hls_body(b"\0\0\0\x20ftypisom"));
        assert!(!is_hls_body(b""));
        let mut mp4 = vec![0, 0, 0, 32];
        mp4.extend(b"ftypisom");
        assert!(looks_like_media_body(&mp4));
    }

    #[test]
    fn test_maybe_playlist_sniffs_hls_vs_mp4() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let path = request.url().to_string();
                let body = if path.contains("master") {
                    b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nindex.m3u8\n".to_vec()
                } else {
                    let mut b = vec![0, 0, 0, 32];
                    b.extend(b"ftypisom");
                    b.extend(vec![0u8; 64]);
                    b
                };
                let _ = request.respond(Response::new(
                    StatusCode(200),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/octet-stream"[..],
                    )
                    .unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                ));
            }
        });
        let headers = HashMap::from([("User-Agent".into(), "t".into())]);
        match http_get_maybe_playlist(&format!("http://127.0.0.1:{port}/master.m3u8"), &headers)
            .unwrap()
        {
            MaybePlaylist::Playlist { body, .. } => assert!(is_hls_body(&body)),
            MaybePlaylist::Progressive { .. } => panic!("expected playlist"),
        }
        match http_get_maybe_playlist(&format!("http://127.0.0.1:{port}/video"), &headers).unwrap()
        {
            MaybePlaylist::Progressive { .. } => {}
            MaybePlaylist::Playlist { .. } => panic!("expected progressive file"),
        }
    }

    #[test]
    fn test_looks_like_playlist_url() {
        assert!(looks_like_playlist_url(
            "https://embed.example/wrap/token.m3u8"
        ));
        assert!(looks_like_playlist_url(
            "https://cdn.example/show/id/master.m3u8?token=abc"
        ));
        assert!(looks_like_playlist_url(
            "https://embed.example/proxy?url=https%3A%2F%2Fcdn.example%2Fmaster.m3u8"
        ));
        assert!(!looks_like_playlist_url("https://cdn.example/video.mp4"));
    }

    #[test]
    fn test_omit_origin_only_when_same_host() {
        let url = "https://cdn.example/master.m3u8";
        let empty = HashMap::new();
        assert!(omit_curl_header(
            "Origin",
            "https://cdn.example",
            url,
            &empty
        ));
        assert!(omit_curl_header(
            "Origin",
            "https://cdn.example/",
            url,
            &empty
        ));
        assert!(!omit_curl_header(
            "Origin",
            "https://embed.example",
            url,
            &empty
        ));
        assert!(!omit_curl_header(
            "Referer",
            "https://cdn.example/",
            url,
            &empty
        ));
        assert!(omit_curl_header("Origin", "", url, &empty));
    }

    #[test]
    fn test_omit_cookie_on_other_host() {
        let headers = HashMap::from([
            ("Referer".into(), "https://embed.example/watch/1".into()),
            ("Cookie".into(), "sid=1".into()),
        ]);
        assert!(omit_curl_header(
            "Cookie",
            "sid=1",
            "https://cdn.example/master.m3u8",
            &headers
        ));
        assert!(!omit_curl_header(
            "Cookie",
            "sid=1",
            "https://embed.example/wrap/token.m3u8",
            &headers
        ));
    }

    #[test]
    fn test_playlist_get_does_not_send_range() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let range = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Range"))
                    .map(|h| h.value.as_str().to_string());
                let body = if range.is_some() {
                    b"range-not-allowed".to_vec()
                } else {
                    b"#EXTM3U\n#EXTINF:1,\nseg.ts\n".to_vec()
                };
                let code = if range.is_some() { 403 } else { 200 };
                let _ = request.respond(Response::new(
                    StatusCode(code),
                    vec![Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                ));
            }
        });
        let url = format!("http://127.0.0.1:{port}/wrap/token.m3u8");
        let headers = HashMap::from([("User-Agent".into(), "t".into())]);
        match http_get_maybe_playlist(&url, &headers).unwrap() {
            MaybePlaylist::Playlist { body, .. } => assert!(is_hls_body(&body)),
            MaybePlaylist::Progressive { .. } => panic!("range probe 403ed the playlist"),
        }
    }

    #[test]
    fn test_cross_origin_is_sent_same_origin_is_not() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let origin = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Origin"))
                    .map(|h| h.value.as_str().to_string());
                let path = request.url().to_string();
                let ok = if path.contains("cross") {
                    origin.as_deref() == Some("https://embed.example")
                } else {
                    origin.is_none()
                };
                let body = if ok {
                    b"#EXTM3U\n".to_vec()
                } else {
                    format!("origin={origin:?}").into_bytes()
                };
                let code = if ok { 200 } else { 400 };
                let _ = request.respond(Response::new(
                    StatusCode(code),
                    vec![Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                ));
            }
        });
        let cross = format!("http://127.0.0.1:{port}/cross/master.m3u8");
        let same = format!("http://127.0.0.1:{port}/same/master.m3u8");
        let mut cross_headers = HashMap::from([
            ("User-Agent".into(), "t".into()),
            ("Origin".into(), "https://embed.example".into()),
            ("Referer".into(), "https://embed.example/watch/1".into()),
        ]);
        let mut same_headers = HashMap::from([
            ("User-Agent".into(), "t".into()),
            ("Origin".into(), format!("http://127.0.0.1:{port}")),
            ("Referer".into(), format!("http://127.0.0.1:{port}/watch/1")),
        ]);
        let (_, body) = http_get(&cross, &mut cross_headers).unwrap();
        assert!(body.starts_with(b"#EXTM3U"));
        let (_, body) = http_get(&same, &mut same_headers).unwrap();
        assert!(body.starts_with(b"#EXTM3U"));
    }

    #[test]
    fn test_http_get_keeps_playlist_path_not_origin() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let path = request.url().to_string();
                let body = if path.contains("/hls/") {
                    b"#EXTM3U\n#EXTINF:1,\nseg.ts\n".to_vec()
                } else {
                    b"<html>home</html>".to_vec()
                };
                let _ = request.respond(Response::new(
                    StatusCode(200),
                    vec![Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                ));
            }
        });
        let url = format!("http://127.0.0.1:{port}/hls/vid/index_720p.m3u8?token=abc");
        let headers = HashMap::from([("User-Agent".into(), "t".into())]);
        match http_get_maybe_playlist(&url, &headers).unwrap() {
            MaybePlaylist::Playlist { url: got, body } => {
                assert!(is_hls_body(&body));
                assert!(
                    got.contains("/hls/vid/index_720p.m3u8"),
                    "playlist base lost the path: {got}"
                );
            }
            MaybePlaylist::Progressive { url: got } => {
                panic!("expected playlist, got progressive {got}")
            }
        }
    }
}
