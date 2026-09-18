use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tiny_http::{Header, Response, Server, StatusCode};

use crate::error::{Error, Result};
use crate::fetch::{curl_push_url, current_cancel};
use crate::hls::{fetch_segment_body, subtitle_content_type, unwrap_media};

pub struct ProxyState {
    pub playlist: Vec<u8>,
    pub headers: HashMap<String, String>,
    pub segments: Vec<String>,
    pub files: HashMap<String, Vec<u8>>,
    /// Prefetched segment bodies by index. Filled before VLC launches when
    /// buffer-first is on, so slow remotes start with a cushion.
    pub seg_cache: HashMap<usize, Vec<u8>>,
    /// Last segment index VLC requested. Tracks the current position;
    /// seeking back moves this backward so the fetch window follows.
    pub served_cur: Option<usize>,
    /// Highest segment index VLC has requested. Kept for stats only.
    pub served_max: Option<usize>,
    /// Total segment requests served to VLC.
    pub served_count: u64,
    /// Cached bytes currently held.
    pub cached_bytes: u64,
    pub verbose: bool,
    pub source: Option<String>,
    pub on_log: Option<Arc<dyn Fn(&str, String) + Send + Sync>>,
    pub pending_log: Vec<(String, String)>,
    log_fail: u32,
}

pub struct ProxyHandle {
    stop: Arc<AtomicBool>,
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
    pub state: Arc<Mutex<ProxyState>>,
    pub base: String,
}

impl Clone for ProxyHandle {
    fn clone(&self) -> Self {
        Self {
            stop: self.stop.clone(),
            server: self.server.clone(),
            thread: None,
            state: self.state.clone(),
            base: self.base.clone(),
        }
    }
}

impl ProxyHandle {
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn playlist_url(&self) -> String {
        format!("{}/index.m3u8", self.base)
    }

    pub fn master_url(&self) -> String {
        format!("{}/master.m3u8", self.base)
    }

    pub fn file_url(&self) -> String {
        format!("{}/file.mp4", self.base)
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
    }
}

pub fn start_proxy(
    headers: HashMap<String, String>,
    segments: Vec<String>,
    playlist: Vec<u8>,
    verbose: bool,
) -> Result<ProxyHandle> {
    let server =
        Server::http("127.0.0.1:0").map_err(|e| Error::msg(format!("play: proxy: {e}")))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| Error::msg("play: proxy: no ip addr"))?
        .port();
    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(ProxyState {
        playlist,
        headers,
        segments,
        files: HashMap::new(),
        seg_cache: HashMap::new(),
        served_cur: None,
        served_max: None,
        served_count: 0,
        cached_bytes: 0,
        verbose,
        source: None,
        on_log: None,
        pending_log: Vec::new(),
        log_fail: 0,
    }));
    let s = server.clone();
    let st = state.clone();
    let stop2 = stop.clone();
    let thread = thread::spawn(move || {
        for request in s.incoming_requests() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let st = st.clone();
            let stop3 = stop2.clone();
            thread::spawn(move || handle(request, &st, &stop3));
        }
    });
    Ok(ProxyHandle {
        stop,
        server,
        thread: Some(thread),
        state,
        base: format!("http://127.0.0.1:{port}"),
    })
}

pub fn start_file_proxy(
    url: String,
    headers: HashMap<String, String>,
    verbose: bool,
) -> Result<ProxyHandle> {
    let handle = start_proxy(headers, Vec::new(), Vec::new(), verbose)?;
    handle.state.lock().unwrap().source = Some(url);
    Ok(handle)
}

/// Download the first `count` media segments into the proxy cache so VLC
/// starts with buffered data on slow remotes. Reports (done, total) per
/// segment. Stops at the first failure; uncached segments stream on demand.
pub fn prefetch_segments(
    handle: &ProxyHandle,
    count: usize,
    mut on_step: impl FnMut(usize, usize),
) {
    fill_cache(handle, 0, count, &mut on_step);
}

/// Fill cache for segment range [start, start+count). Reports (done, total)
/// per segment. Skips already cached. Stops at the first failure.
fn fill_cache(
    handle: &ProxyHandle,
    start: usize,
    count: usize,
    on_step: &mut impl FnMut(usize, usize),
) {
    let (segments, headers) = {
        let st = handle.state.lock().unwrap();
        (st.segments.clone(), st.headers.clone())
    };
    let end = (start + count).min(segments.len());
    let total = end.saturating_sub(start);
    if total == 0 {
        return;
    }
    let rest_ext = "ts";
    for (n, src) in segments.iter().enumerate().take(end).skip(start) {
        if handle.state.lock().unwrap().seg_cache.contains_key(&n) {
            on_step(n - start + 1, total);
            continue;
        }
        let mut hdrs = headers.clone();
        match fetch_segment_body(src, &mut hdrs, rest_ext) {
            Some(body) => {
                let mut st = handle.state.lock().unwrap();
                st.cached_bytes += body.len() as u64;
                st.seg_cache.insert(n, body);
                drop(st);
                on_step(n - start + 1, total);
            }
            None => {
                on_step(n - start, total);
                break;
            }
        }
    }
}

/// Keep downloading segments ahead of `from` in the background so the
/// buffer stays ahead of VLC on slow remotes. Runs until `count` ahead
/// are cached, a fetch fails, or the proxy shuts down.
#[allow(dead_code)]
pub fn readahead(handle: &ProxyHandle, from: usize, count: usize) {
    let (segments, headers) = {
        let st = handle.state.lock().unwrap();
        (st.segments.clone(), st.headers.clone())
    };
    let rest_ext = "ts";
    let end = (from + count).min(segments.len());
    for n in from..end {
        if stop_flag(handle) {
            break;
        }
        if handle.state.lock().unwrap().seg_cache.contains_key(&n) {
            continue;
        }
        let Some(src) = segments.get(n) else {
            break;
        };
        let mut hdrs = headers.clone();
        match fetch_segment_body(src, &mut hdrs, rest_ext) {
            Some(body) => {
                let mut st = handle.state.lock().unwrap();
                st.cached_bytes += body.len() as u64;
                st.seg_cache.insert(n, body);
            }
            None => break,
        }
    }
}

fn stop_flag(handle: &ProxyHandle) -> bool {
    handle.stop.load(Ordering::SeqCst)
}

/// After serving a segment, keep 10% of the whole playlist cached ahead
/// of the current position. A 789-segment playlist keeps ~79 ahead from
/// any seek point. Runs in the background; each request re-anchors the
/// window at the latest segment VLC asked for, so seeks move the window.
fn trigger_readahead(state: Arc<Mutex<ProxyState>>, stop: Arc<AtomicBool>) {
    let (segments, headers, start, total) = {
        let st = state.lock().unwrap();
        let total = st.segments.len();
        if total == 0 {
            return;
        }
        let start = st.served_cur.map_or(0, |c| c + 1);
        (st.segments.clone(), st.headers.clone(), start, total)
    };
    let want = readahead_window(total, start);
    if want == 0 {
        return;
    }
    let have = {
        let st = state.lock().unwrap();
        (start..total)
            .take_while(|n| st.seg_cache.contains_key(n))
            .count()
    };
    if have >= want {
        return;
    }
    thread::spawn(move || {
        fill_range(&segments, &headers, &state, &stop, start, want);
    });
}

/// Segments to keep cached ahead: 10% of the whole playlist from the
/// playhead, capped by what remains. At least 1 while anything remains.
pub(crate) fn readahead_window(total: usize, start: usize) -> usize {
    let remaining = total.saturating_sub(start);
    if remaining == 0 {
        return 0;
    }
    (total / 10).max(1).min(remaining)
}

/// Download [start, start+count) into the cache. Skips cached entries,
/// stops at the first failure or when the proxy shuts down.
fn fill_range(
    segments: &[String],
    headers: &HashMap<String, String>,
    state: &Mutex<ProxyState>,
    stop: &AtomicBool,
    start: usize,
    count: usize,
) {
    let rest_ext = "ts";
    let end = (start + count).min(segments.len());
    for n in start..end {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if state.lock().unwrap().seg_cache.contains_key(&n) {
            continue;
        }
        let Some(src) = segments.get(n) else {
            break;
        };
        let mut hdrs = headers.clone();
        match fetch_segment_body(src, &mut hdrs, rest_ext) {
            Some(body) => {
                let mut st = state.lock().unwrap();
                st.cached_bytes += body.len() as u64;
                st.seg_cache.insert(n, body);
            }
            None => break,
        }
    }
}

fn proxy_log(state: &Mutex<ProxyState>, level: &str, msg: String) {
    let (cb, out) = {
        let mut st = state.lock().unwrap();
        if st.log_fail >= 8 {
            return;
        }
        st.log_fail += 1;
        let out = if st.log_fail >= 8 {
            format!("{msg} (further segment errors omitted)")
        } else {
            msg
        };
        (st.on_log.clone(), out)
    };
    if let Some(cb) = cb {
        cb(level, out);
    } else {
        state
            .lock()
            .unwrap()
            .pending_log
            .push((level.to_string(), out));
    }
}

/// Refetch a failed segment once for a one-line diagnosis: HTTP status or
/// transport error plus a short body preview.
fn segment_fetch_detail(src: &str, headers: &mut HashMap<String, String>) -> (u16, String) {
    match crate::fetch::http_get_timeout(src, headers, 20) {
        Ok((_, body)) => {
            let preview: String = String::from_utf8_lossy(&body[..body.len().min(80)])
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .take(80)
                .collect();
            (200, format!(" (HTTP 200, {} bytes, rejected: {preview:?})", body.len()))
        }
        Err(crate::error::Error::Http { code, hint, body, .. }) => {
            let preview: String = String::from_utf8_lossy(&body[..body.len().min(80)])
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .take(80)
                .collect();
            (code, format!(" (HTTP {code}{hint}: {preview:?})"))
        }
        Err(e) => (0, format!(" ({e})")),
    }
}

fn file_content_type(path: &str) -> &'static [u8] {
    if path.ends_with(".m3u8") {
        b"application/vnd.apple.mpegurl"
    } else if path.ends_with(".vtt") || path.ends_with(".webvtt") {
        b"text/vtt"
    } else if path.ends_with(".srt") {
        b"application/x-subrip"
    } else if path.ends_with(".ass") || path.ends_with(".ssa") {
        b"text/x-ssa"
    } else {
        b"application/octet-stream"
    }
}

fn respond_bytes(request: tiny_http::Request, body: Vec<u8>, ctype: &[u8]) {
    let resp = Response::new(
        StatusCode(200),
        vec![
            Header::from_bytes(&b"Content-Type"[..], ctype).unwrap(),
            Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
            Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        ],
        Cursor::new(body.clone()),
        Some(body.len()),
        None,
    );
    let _ = request.respond(resp);
}

fn respond_segment(request: tiny_http::Request, body: &[u8], rest: &str) {
    if let Some(ctype) = subtitle_content_type(body) {
        respond_bytes(request, body.to_vec(), ctype.as_bytes());
        return;
    }
    let (skip, kind) = unwrap_media(&body[..body.len().min(65536)]);
    let skip = skip.min(body.len());
    let out = body[skip..].to_vec();
    let ctype: &[u8] = if rest.ends_with(".vtt") || rest.ends_with(".srt") {
        file_content_type(rest)
    } else if kind == "m4s" {
        b"video/mp4"
    } else {
        b"video/MP2T"
    };
    let resp = Response::new(
        StatusCode(200),
        vec![
            Header::from_bytes(&b"Content-Type"[..], ctype).unwrap(),
            Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        ],
        Cursor::new(out.clone()),
        Some(out.len()),
        None,
    );
    let _ = request.respond(resp);
}

fn handle(request: tiny_http::Request, state: &Arc<Mutex<ProxyState>>, stop: &Arc<AtomicBool>) {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or(&url);
    if let Some(body) = state.lock().unwrap().files.get(path).cloned() {
        respond_bytes(request, body, file_content_type(path));
        return;
    }
    let source = state.lock().unwrap().source.clone();
    if let Some(src) = source {
        stream_file(request, &src, state);
        return;
    }
    if path == "/" || path == "/index.m3u8" || path == "/playlist.m3u8" {
        let body = state.lock().unwrap().playlist.clone();
        let resp = Response::new(
            StatusCode(200),
            vec![
                Header::from_bytes(&b"Content-Type"[..], &b"application/vnd.apple.mpegurl"[..])
                    .unwrap(),
                Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
            ],
            Cursor::new(body.clone()),
            Some(body.len()),
            None,
        );
        let _ = request.respond(resp);
        return;
    }
    if let Some(rest) = path.strip_prefix("/seg/") {
        let stem = Path::new(rest)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let idx: usize = match stem.parse() {
            Ok(i) => i,
            Err(_) => {
                let _ = request.respond(Response::from_string("bad segment").with_status_code(404));
                return;
            }
        };
        let (src, headers, is_key) = {
            let mut st = state.lock().unwrap();
            if let Some(cached) = st.seg_cache.get(&idx).cloned() {
                st.served_count += 1;
                st.served_cur = Some(idx);
                st.served_max = Some(st.served_max.map_or(idx, |m| m.max(idx)));
                drop(st);
                respond_segment(request, &cached, rest);
                trigger_readahead(state.clone(), stop.clone());
                return;
            }
            let src = match st.segments.get(idx) {
                Some(s) => s.clone(),
                None => {
                    drop(st);
                    let _ =
                        request.respond(Response::from_string("bad segment").with_status_code(404));
                    return;
                }
            };
            (src, st.headers.clone(), rest.ends_with(".key"))
        };
        let mut hdrs = headers;
        if is_key {
            match crate::fetch::http_get(&src, &mut hdrs) {
                Ok((_, body)) => {
                    let resp = Response::new(
                        StatusCode(200),
                        vec![Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/octet-stream"[..],
                        )
                        .unwrap()],
                        Cursor::new(body.clone()),
                        Some(body.len()),
                        None,
                    );
                    let _ = request.respond(resp);
                }
                Err(Error::Http { code, .. }) => {
                    proxy_log(state, "error", format!("seg {idx} {src} → HTTP {code}"));
                    let _ = request.respond(
                        Response::from_string(format!("upstream {code}")).with_status_code(code),
                    );
                }
                Err(_) => {
                    proxy_log(state, "error", format!("seg {idx} {src} → upstream failed"));
                    let _ = request.respond(
                        Response::from_string("upstream failed").with_status_code(502),
                    );
                }
            }
            return;
        }
        match fetch_segment_body(&src, &mut hdrs, rest) {
            Some(body) => {
                {
                    let mut st = state.lock().unwrap();
                    st.cached_bytes += body.len() as u64;
                    st.seg_cache.insert(idx, body.clone());
                    st.served_count += 1;
                    st.served_cur = Some(idx);
                    st.served_max = Some(st.served_max.map_or(idx, |m| m.max(idx)));
                }
                respond_segment(request, &body, rest);
                trigger_readahead(state.clone(), stop.clone());
            }
            None => {
                let (_, detail) = segment_fetch_detail(&src, &mut hdrs);
                proxy_log(
                    state,
                    "error",
                    format!("seg {idx} {src} → upstream failed{detail}"),
                );
                let _ =
                    request.respond(Response::from_string("upstream failed").with_status_code(502));
            }
        }
        return;
    }
    let _ = request.respond(Response::from_string("not found").with_status_code(404));
}

fn stream_file(request: tiny_http::Request, src: &str, state: &Mutex<ProxyState>) {
    let headers = state.lock().unwrap().headers.clone();
    let is_head = *request.method() == tiny_http::Method::Head;
    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());
    let fetch = crate::urls::strip_range_query(src);
    let n = std::process::id();
    let dir = std::env::temp_dir().join(format!(
        "play-proxy-{}-{}",
        n,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if fs::create_dir_all(&dir).is_err() {
        let _ = request.respond(Response::from_string("proxy").with_status_code(500));
        return;
    }
    let hdr_path = dir.join("h");
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--http1.1",
        "-L",
        "--max-redirs",
        "10",
        "-D",
        hdr_path.to_str().unwrap_or("h"),
        "-o",
        if is_head { "/dev/null" } else { "-" },
    ]);
    if let Some(r) = range
        .as_deref()
        .and_then(|s| s.strip_prefix("bytes="))
        .filter(|s| !s.is_empty())
    {
        cmd.args(["-r", r]);
    } else if is_head {
        cmd.args(["-r", "0-0"]);
    }
    for (k, v) in &headers {
        if crate::fetch::omit_curl_header(k, v, &fetch, &headers) {
            continue;
        }
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    curl_push_url(&mut cmd, &fetch);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            let _ = fs::remove_dir_all(&dir);
            let _ = request.respond(Response::from_string("curl").with_status_code(502));
            return;
        }
    };
    let pid = child.id();
    if let Some(hook) = current_cancel() {
        hook.track(pid);
    }
    let hdr_text = wait_http_headers(&hdr_path, Duration::from_secs(20));
    let (code, ctype, clen, crange) = parse_upstream_headers(&hdr_text);
    if !(200..300).contains(&code) {
        if let Some(hook) = current_cancel() {
            hook.untrack(pid);
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&dir);
        proxy_log(
            state,
            "error",
            format!("file {src} → HTTP {}", if code == 0 { 502 } else { code }),
        );
        let _ = request.respond(
            Response::from_string(format!("upstream {code}")).with_status_code(if code == 0 {
                502
            } else {
                code
            }),
        );
        return;
    }
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            if let Some(hook) = current_cancel() {
                hook.untrack(pid);
            }
            let _ = child.kill();
            let _ = fs::remove_dir_all(&dir);
            let _ = request.respond(Response::from_string("curl").with_status_code(502));
            return;
        }
    };
    let mut resp_hdrs = vec![
        Header::from_bytes(
            &b"Content-Type"[..],
            ctype.as_bytes().get(..128).unwrap_or(b"video/mp4"),
        )
        .unwrap_or_else(|_| Header::from_bytes(&b"Content-Type"[..], &b"video/mp4"[..]).unwrap()),
        Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap(),
        Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
    ];
    if let Some(cr) = crange.as_ref() {
        if !is_head {
            if let Ok(h) = Header::from_bytes(&b"Content-Range"[..], cr.as_bytes()) {
                resp_hdrs.push(h);
            }
        }
    }
    if is_head {
        let size = crange
            .as_deref()
            .and_then(|cr| cr.rsplit('/').next())
            .and_then(|s| s.parse().ok())
            .or(clen);
        let _ = child.kill();
        let _ = child.wait();
        if let Some(hook) = current_cancel() {
            hook.untrack(pid);
        }
        let _ = fs::remove_dir_all(&dir);
        let resp = Response::new(
            StatusCode(200),
            resp_hdrs,
            Cursor::new(Vec::new()),
            size,
            None,
        );
        let _ = request.respond(resp);
        return;
    }
    let resp = Response::new(StatusCode(code), resp_hdrs, stdout, clen, None);
    let _ = request.respond(resp);
    let _ = child.wait();
    if let Some(hook) = current_cancel() {
        hook.untrack(pid);
    }
    let _ = fs::remove_dir_all(&dir);
}

fn wait_http_headers(path: &Path, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if text.contains("\r\n\r\n") || text.contains("\n\n") {
                return text;
            }
        }
        if start.elapsed() > timeout {
            return String::new();
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn parse_upstream_headers(text: &str) -> (u16, String, Option<usize>, Option<String>) {
    let mut code = 0u16;
    let mut ctype = "video/mp4".to_string();
    let mut clen = None;
    let mut crange = None;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if i == 0 {
            let p: Vec<_> = line.split_whitespace().collect();
            if p.len() >= 2 {
                code = p[1].parse().unwrap_or(0);
            }
            continue;
        }
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-type") {
                ctype = v.to_string();
            } else if k.eq_ignore_ascii_case("content-length") {
                clen = v.parse().ok();
            } else if k.eq_ignore_ascii_case("content-range") {
                crange = Some(v.to_string());
            }
        }
    }
    (code, ctype, clen, crange)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readahead_window_is_ten_percent() {
        // 789-segment playlist keeps ~79 ahead from any seek point.
        assert_eq!(readahead_window(789, 0), 78);
        assert_eq!(readahead_window(789, 400), 78);
        // Capped by what remains near the end.
        assert_eq!(readahead_window(789, 780), 9);
        assert_eq!(readahead_window(100, 90), 10);
        assert_eq!(readahead_window(5, 0), 1);
        assert_eq!(readahead_window(10, 10), 0);
        assert_eq!(readahead_window(0, 0), 0);
    }
}
