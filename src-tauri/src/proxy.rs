use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Byte spans served to VLC by the progressive-file proxy (start
    /// inclusive, end exclusive). The file grid renders these cells, so
    /// green means streamed before. Bounded.
    pub served_ranges: Vec<(u64, u64)>,
    /// Unused by the streaming file proxy; kept so the file DTO shape and
    /// HLS grid rendering stay unchanged.
    pub cached_cells: [bool; FILE_CHUNKS],
    /// Start offset of the most recently served file span: fallback MP4
    /// playhead before any seek (e.g. initial playback from byte 0).
    pub served_cur_byte: Option<u64>,
    /// Chunk index of the last successful file seek: the MP4 playhead.
    /// The seek knows its fraction exactly; serve traffic only
    /// approximates it (VBR skew, metadata probes), so this wins.
    pub file_seek_chunk: Option<usize>,
    /// Upstream file size in bytes, when response headers revealed it.
    pub total_bytes: Option<u64>,
    /// Total segment requests served to VLC.
    pub served_count: u64,
    /// Cached bytes currently held.
    pub cached_bytes: u64,
    /// Cumulative upstream segment bytes fetched (cache + on-demand).
    pub fetched_bytes: u64,
    /// Upstream fetches currently in flight.
    pub fetch_inflight: usize,
    /// Epoch ms of the last successful upstream fetch/request.
    pub last_ok_ms: u64,
    /// Epoch ms of the last failed upstream fetch, 0 when never failed.
    pub last_error_ms: u64,
    /// Last upstream failure message, for the Diagnostics view.
    pub last_error: Option<String>,
    /// (epoch ms, bytes) of recent successful fetches, pruned to ~10s.
    /// Drives the download-speed readout; zero-byte marks are not stored.
    pub recent: VecDeque<(u64, u64)>,
    /// Readahead generation. Each serving trigger bumps it and passes the
    /// value to its fill thread; a thread whose epoch is stale (the user
    /// sought to a new window) stops before its next segment, so only the
    /// current seek position +10% is fetched.
    pub fetch_epoch: u64,
    pub verbose: bool,
    pub source: Option<String>,
    pub on_log: Option<Arc<dyn Fn(&str, String) + Send + Sync>>,
    pub pending_log: Vec<(String, String)>,
    log_fail: u32,
}

/// Immutable-after-setup routing facts for one progressive-file session.
pub struct FileRoute {
    pub source: Option<String>,
    pub files: HashMap<String, Vec<u8>>,
    pub headers: HashMap<String, String>,
}

pub struct ProxyHandle {
    stop: Arc<AtomicBool>,
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
    pub state: Arc<Mutex<ProxyState>>,
    pub base: String,
    /// Write-once routing snapshot for progressive-file sessions. `source`,
    /// `files`, and `headers` never change after setup, so serving reads
    /// them here without the mutex: a wedged proxy can never stall VLC
    /// routing. HLS sessions never set this and keep the locked path.
    pub file_route: Arc<OnceLock<FileRoute>>,
}

/// Shares state but NOT lifetime: dropping ANY clone (including this one)
/// sets the shared stop flag and unblocks the dispatch loop. Only clone a
/// handle for session-scoped ownership; short-lived snapshots must carry
/// `state`/`base` instead, never the handle.
impl Clone for ProxyHandle {
    fn clone(&self) -> Self {
        Self {
            stop: self.stop.clone(),
            server: self.server.clone(),
            thread: None,
            state: self.state.clone(),
            base: self.base.clone(),
            file_route: self.file_route.clone(),
        }
    }
}

impl ProxyHandle {
    /// Local playlist URL named after the stream title when available, so
    /// VLC shows `<title>.m3u8` instead of `index.m3u8`.
    pub fn playlist_url_with(&self, title: Option<&str>) -> String {
        format!("{}/{}", self.base, crate::urls::playlist_filename(title))
    }

    /// Local master URL for subtitle-bundled sessions, title-named likewise.
    pub fn master_url_with(&self, title: Option<&str>) -> String {
        format!("{}/{}", self.base, crate::urls::master_filename(title))
    }

    /// Local progressive-file URL: `<title>.mp4` instead of `file.mp4`.
    pub fn file_url_with(&self, title: Option<&str>) -> String {
        format!("{}/{}", self.base, crate::urls::media_filename(title))
    }

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

    /// Loopback port VLC connects to, for the Diagnostics view.
    pub fn port(&self) -> Option<u16> {
        port_of(&self.base)
    }
}

/// Loopback port parsed from a proxy base URL, without needing the handle.
pub fn port_of(base: &str) -> Option<u16> {
    base.rsplit(':').next()?.parse().ok()
}

/// Virtual time-chunk count for progressive-file sessions: the detail grid
/// renders this many cells with no frontend changes.
pub(crate) const FILE_CHUNKS: usize = 100;

/// Resolve a served response to a `(start, end_exclusive, total)` byte span
/// for chunk display. `range` is the request's Range value, `code`/`clen`
/// the upstream status and length, `crange` its Content-Range value.
/// Unparseable spans return None and are simply not displayed.
pub(crate) fn resolve_serve_span(
    range: Option<&str>,
    code: u16,
    clen: Option<usize>,
    crange: Option<&str>,
) -> Option<(u64, u64, Option<u64>)> {
    fn crange_total(crange: Option<&str>) -> Option<u64> {
        let cr = crange?;
        let (_, total) = cr.split_once('/')?;
        total.trim().parse().ok()
    }
    let total = crange_total(crange).or(if code == 200 { clen.map(|n| n as u64) } else { None });
    match range.and_then(|r| r.strip_prefix("bytes=")) {
        Some(rest) => {
            let (a, b) = rest.split_once('-')?;
            let start: u64 = a.trim().parse().ok()?;
            let end = if b.trim().is_empty() {
                total.unwrap_or(start.saturating_add(1))
            } else {
                b.trim().parse::<u64>().ok()?.saturating_add(1)
            };
            if end <= start {
                return None;
            }
            Some((start, end, total))
        }
        None => {
            // Full-file response: only honest when the size is known.
            let total = total?;
            (total > 0).then_some((0, total, Some(total)))
        }
    }
}

/// Chunk holding a byte offset, for the MP4 playhead. Unknown totals map
/// to None rather than a dishonest cell.
pub(crate) fn file_playhead_chunk(total: u64, offset: u64) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let idx = offset.saturating_mul(FILE_CHUNKS as u64) / total;
    Some((idx as usize).min(FILE_CHUNKS - 1))
}

/// Record a successfully served progressive-file span. Brief map-adjacent
/// section only; never called holding another lock.
/// Best-effort variant for the serve data plane: a wedged proxy must skip
/// a progress mark, never stall VLC's bytes. Returns whether it marked.
pub(crate) fn try_note_file_served(
    state: &Mutex<ProxyState>,
    start: u64,
    end: u64,
    total: Option<u64>,
) -> bool {
    let mut st = match state.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    if total.is_some() {
        st.total_bytes = total;
    }
    // One span per requested start: extend it as bytes flow so aborts and
    // seeks leave honest, bounded history instead of span confetti.
    match st.served_ranges.iter_mut().find(|sp| sp.0 == start) {
        Some(sp) => sp.1 = sp.1.max(end),
        None => st.served_ranges.push((start, end)),
    }
    while st.served_ranges.len() > 512 {
        st.served_ranges.remove(0);
    }
    true
}

/// Map served byte spans onto file grid cells. Lit cells were streamed
/// to VLC before; with no disk cache this is the honest file-grid
/// signal. Best-effort like every other progress mark.
pub(crate) fn served_cells(total: u64, spans: &[(u64, u64)]) -> Vec<bool> {
    let mut cells = vec![false; FILE_CHUNKS];
    if total == 0 {
        return cells;
    }
    for &(s, e) in spans {
        if e <= s {
            continue;
        }
        let lo = (s.saturating_mul(FILE_CHUNKS as u64) / total) as usize;
        let hi =
            (e.saturating_sub(1).saturating_mul(FILE_CHUNKS as u64) / total) as usize;
        for c in lo.min(FILE_CHUNKS - 1)..=hi.min(FILE_CHUNKS - 1) {
            cells[c] = true;
        }
    }
    cells
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mark an upstream fetch start. Callers must pair with exactly one of
/// `note_fetch_ok` / `note_fetch_err`. Best-effort: activity counters never
/// park the serve path, so a wedged proxy only skews status, never stalls.
pub(crate) fn note_fetch_begin(state: &Mutex<ProxyState>) {
    if let Ok(mut st) = state.try_lock() {
        st.fetch_inflight += 1;
    }
}

/// Mark a successful upstream fetch of `bytes` bytes. Zero-byte marks
/// (progressive-file requests, whose bodies stream through curl) refresh
/// recency but do not feed the speed average.
pub(crate) fn note_fetch_ok(state: &Mutex<ProxyState>, bytes: u64) {
    let Ok(mut st) = state.try_lock() else {
        return;
    };
    st.fetch_inflight = st.fetch_inflight.saturating_sub(1);
    let now = now_ms();
    st.last_ok_ms = now;
    st.fetched_bytes += bytes;
    if bytes > 0 {
        st.recent.push_back((now, bytes));
        while st.recent.len() > 256 {
            st.recent.pop_front();
        }
    }
}

/// Mark a failed upstream fetch with a one-line diagnosis.
pub(crate) fn note_fetch_err(state: &Mutex<ProxyState>, msg: String) {
    let Ok(mut st) = state.try_lock() else {
        eprintln!("play: fetch error [{msg}]");
        return;
    };
    st.fetch_inflight = st.fetch_inflight.saturating_sub(1);
    st.last_error_ms = now_ms();
    st.last_error = Some(msg);
}

/// Recent download speed in bytes/sec over the trailing ~10s window.
/// Returns None when nothing was fetched recently.
pub(crate) fn recent_speed_bps(recent: &VecDeque<(u64, u64)>, now_ms: u64) -> Option<u64> {
    const WINDOW_MS: u64 = 10_000;
    let mut bytes = 0u64;
    let mut oldest = now_ms;
    let mut n = 0usize;
    for (ms, b) in recent.iter() {
        if now_ms.saturating_sub(*ms) <= WINDOW_MS {
            bytes += b;
            oldest = oldest.min(*ms);
            n += 1;
        }
    }
    if n == 0 || bytes == 0 {
        return None;
    }
    let span_ms = now_ms.saturating_sub(oldest).max(1);
    Some(bytes.saturating_mul(1000) / span_ms)
}

/// Numeric `from` query on a served playlist/master URL (`?from=N`),
/// used by click-to-seek relaunches.
pub(crate) fn query_from(url: &str) -> Option<usize> {
    url.split_once('?')?.1.split('&').find_map(|p| {
        p.split_once('=')
            .filter(|(k, _)| *k == "from")
            .and_then(|(_, v)| v.parse().ok())
    })
}

/// `/seg/` slot number embedded in a rewritten media URI, if any.
fn slot_of(uri: &str) -> Option<usize> {
    uri.split("/seg/").nth(1)?.split('.').next()?.parse().ok()
}

/// Media time in seconds at the start of segment slot `slot`: the summed
/// `#EXTINF` durations of earlier entries. Feeds absolute `seek` commands
/// on the player's remote-control channel. Missing durations count as 0.
pub(crate) fn seg_offset_secs(body: &[u8], slot: usize) -> f64 {
    let Ok(text) = std::str::from_utf8(body) else {
        return 0.0;
    };
    let mut total = 0.0;
    let mut pending = 0.0;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("#EXTINF:") {
            pending = v.split(',').next().unwrap_or("").trim().parse().unwrap_or(0.0);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if slot_of(line).is_some_and(|s| s < slot) {
            total += pending;
        }
        pending = 0.0;
    }
    total
}

/// Serve the stored media playlist starting at segment slot `from`
/// (click-to-seek): drop earlier segment entries and advance
/// `#EXT-X-MEDIA-SEQUENCE` by the dropped count. KEY/MAP/EXTINF tags are
/// carried onto the first kept segment. Returns None when `from` is past
/// every segment, in which case the caller serves the full playlist.
pub(crate) fn trim_playlist(body: &[u8], from: usize) -> Option<Vec<u8>> {
    if from == 0 {
        return Some(body.to_vec());
    }
    let text = std::str::from_utf8(body).ok()?;
    // Pass 1: `from` is valid only when some media entry survives it.
    // Unparseable URI lines count as kept (never silently dropped).
    let mut media_count = 0usize;
    let mut dropped = 0usize;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        media_count += 1;
        if slot_of(line).is_some_and(|s| s < from) {
            dropped += 1;
        }
    }
    if dropped >= media_count {
        return None;
    }
    // Pass 2: rewrite.
    let mut out: Vec<String> = Vec::new();
    let mut key_stash: Option<String> = None;
    let mut map_stash: Option<String> = None;
    let mut extinf_stash: Option<String> = None;
    let mut saw_seq = false;
    let mut media_seen = 0usize;
    let mut dropped_seen = 0usize;
    let mut started = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            let orig: u64 = v.trim().parse().unwrap_or(0);
            saw_seq = true;
            out.push(format!("#EXT-X-MEDIA-SEQUENCE:{}", orig + dropped as u64));
            continue;
        }
        if line.starts_with("#EXT-X-KEY") {
            key_stash = Some(line.to_string());
            continue;
        }
        if line.starts_with("#EXT-X-MAP") {
            map_stash = Some(line.to_string());
            continue;
        }
        if line.starts_with("#EXTINF") {
            extinf_stash = Some(line.to_string());
            continue;
        }
        if line.starts_with('#') {
            // Header tags always stay; mid-playlist tags stay once every
            // dropped entry has been passed.
            if media_seen == 0 || dropped_seen >= dropped {
                out.push(line.to_string());
            }
            continue;
        }
        media_seen += 1;
        if slot_of(line).is_some_and(|s| s < from) {
            dropped_seen += 1;
            extinf_stash = None;
            continue;
        }
        if !started {
            started = true;
            if !saw_seq {
                out.push(format!("#EXT-X-MEDIA-SEQUENCE:{dropped}"));
            }
            out.extend(key_stash.take());
            out.extend(map_stash.take());
        }
        out.extend(extinf_stash.take());
        out.push(line.to_string());
    }
    let mut s = out.join("\n");
    s.push('\n');
    Some(s.into_bytes())
}

/// Point a stored master playlist's video URI at `?from=N` so a seek
/// relaunch keeps its subtitle bundle. The subtitle (`/sub/`) URIs are
/// left untouched. Returns None when no video URI is found.
fn with_from_in_master(body: &[u8], from: usize) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    if !text.contains("#EXT-X-STREAM-INF") {
        return None;
    }
    let mut out = Vec::new();
    let mut patched = false;
    for raw in text.lines() {
        let line = raw.trim();
        if !patched
            && (line.starts_with("http://") || line.starts_with("https://"))
            && line.contains(".m3u8")
            && !line.contains("/sub/")
        {
            let sep = if line.contains('?') { '&' } else { '?' };
            out.push(format!("{line}{sep}from={from}"));
            patched = true;
        } else {
            out.push(raw.to_string());
        }
    }
    if !patched {
        return None;
    }
    let mut s = out.join("\n");
    s.push('\n');
    Some(s.into_bytes())
}

/// Session fetch status for the Diagnostics view: `downloading` (green,
/// upstream traffic in flight or within the last 5s), `buffered` (yellow,
/// idle with the readahead window full), `error` (red, the latest upstream
/// result within the last 30s was a failure), else `idle`.
pub(crate) fn fetch_status(
    total: usize,
    buffered: usize,
    want: usize,
    inflight: usize,
    last_ok_ms: u64,
    last_error_ms: u64,
    now_ms: u64,
) -> &'static str {
    if last_error_ms > last_ok_ms && now_ms.saturating_sub(last_error_ms) < 30_000 {
        return "error";
    }
    if inflight > 0 || (last_ok_ms > 0 && now_ms.saturating_sub(last_ok_ms) < 5_000) {
        return "downloading";
    }
    if total > 0 && buffered >= want {
        return "buffered";
    }
    "idle"
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
        served_ranges: Vec::new(),
        cached_cells: [false; FILE_CHUNKS],
        served_cur_byte: None,
        file_seek_chunk: None,
        total_bytes: None,
        served_count: 0,
        cached_bytes: 0,
        fetched_bytes: 0,
        fetch_inflight: 0,
        last_ok_ms: 0,
        last_error_ms: 0,
        last_error: None,
        recent: VecDeque::new(),
        fetch_epoch: 0,
        verbose,
        source: None,
        on_log: None,
        pending_log: Vec::new(),
        log_fail: 0,
    }));
    let s = server.clone();
    let st = state.clone();
    let stop2 = stop.clone();
    let route = Arc::new(OnceLock::new());
    let route2 = route.clone();
    let thread = thread::spawn(move || {
        for request in s.incoming_requests() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let st = st.clone();
            let stop3 = stop2.clone();
            let route3 = route2.clone();
            thread::spawn(move || handle(request, &st, &stop3, &route3));
        }
    });
    Ok(ProxyHandle {
        stop,
        server,
        thread: Some(thread),
        state,
        base: format!("http://127.0.0.1:{port}"),
        file_route: route,
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
        note_fetch_begin(&handle.state);
        match fetch_segment_body(src, &mut hdrs, rest_ext) {
            Some(body) => {
                let bytes = body.len() as u64;
                note_fetch_ok(&handle.state, bytes);
                let mut st = handle.state.lock().unwrap();
                st.cached_bytes += bytes;
                st.seg_cache.insert(n, body);
                drop(st);
                on_step(n - start + 1, total);
            }
            None => {
                note_fetch_err(&handle.state, format!("prefetch seg {n} {src} → upstream failed"));
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
        note_fetch_begin(&handle.state);
        match fetch_segment_body(src, &mut hdrs, rest_ext) {
            Some(body) => {
                let bytes = body.len() as u64;
                note_fetch_ok(&handle.state, bytes);
                let mut st = handle.state.lock().unwrap();
                st.cached_bytes += bytes;
                st.seg_cache.insert(n, body);
            }
            None => {
                note_fetch_err(&handle.state, format!("readahead seg {n} → upstream failed"));
                break;
            }
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
    // New generation: any fill thread from an older seek window is stale
    // and stops before its next segment, so only the current position +10%
    // is fetched even when the user keeps seeking. Bumped even when the
    // window is already full, so stragglers stand down and nothing runs.
    let epoch = {
        let mut st = state.lock().unwrap();
        st.fetch_epoch += 1;
        st.fetch_epoch
    };
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
        fill_range(&segments, &headers, &state, &stop, start, want, epoch);
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
/// stops at the first failure, when the proxy shuts down, or when a newer
/// seek bumped the epoch (this window is stale).
fn fill_range(
    segments: &[String],
    headers: &HashMap<String, String>,
    state: &Mutex<ProxyState>,
    stop: &AtomicBool,
    start: usize,
    count: usize,
    epoch: u64,
) {
    let rest_ext = "ts";
    let end = (start + count).min(segments.len());
    for n in start..end {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if state.lock().unwrap().fetch_epoch != epoch {
            break;
        }
        if state.lock().unwrap().seg_cache.contains_key(&n) {
            continue;
        }
        let Some(src) = segments.get(n) else {
            break;
        };
        let mut hdrs = headers.clone();
        note_fetch_begin(state);
        match fetch_segment_body(src, &mut hdrs, rest_ext) {
            Some(body) => {
                let bytes = body.len() as u64;
                note_fetch_ok(state, bytes);
                let mut st = state.lock().unwrap();
                st.cached_bytes += bytes;
                st.seg_cache.insert(n, body);
            }
            None => {
                note_fetch_err(state, format!("readahead seg {n} → upstream failed"));
                break;
            }
        }
    }
}

fn proxy_log(state: &Mutex<ProxyState>, level: &str, msg: String) {
    // Best-effort: error logging never parks the serve path. On contention
    // the line still reaches the terminal; only the UI queue is skipped.
    let Ok(mut st) = state.try_lock() else {
        eprintln!("play: log [{level}] {msg}");
        return;
    };
    if st.log_fail >= 8 {
        return;
    }
    st.log_fail += 1;
    let out = if st.log_fail >= 8 {
        format!("{msg} (further segment errors omitted)")
    } else {
        msg
    };
    let cb = st.on_log.clone();
    if cb.is_none() {
        st.pending_log.push((level.to_string(), out.clone()));
    }
    drop(st);
    if let Some(cb) = cb {
        cb(level, out);
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

fn handle(
    request: tiny_http::Request,
    state: &Arc<Mutex<ProxyState>>,
    stop: &Arc<AtomicBool>,
    route: &Arc<OnceLock<FileRoute>>,
) {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or(&url);
    // Lock-free fast path for progressive-file sessions: routing facts are
    // immutable after setup, so a wedged proxy can never stall VLC routing.
    // HLS sessions never set a route and keep the locked path below.
    if let Some(route) = route.get() {
        if let Some(body) = route.files.get(path) {
            if let Some(from) = query_from(&url) {
                if let Some(master) = with_from_in_master(body, from) {
                    respond_bytes(request, master, file_content_type(path));
                    return;
                }
            }
            respond_bytes(request, body.clone(), file_content_type(path));
            return;
        }
        if let Some(src) = route.source.as_ref() {
            stream_file(request, src, state, &route.headers);
            return;
        }
        let _ = request.respond(Response::from_string("no route").with_status_code(404));
        return;
    }
    let (file_hit, source) = {
        let st = state.lock().unwrap();
        (st.files.get(path).cloned(), st.source.clone())
    };
    if let Some(body) = file_hit {
        // Click-to-seek on a subtitle-bundled session opens the master with
        // `?from=N`: point its video URI at the trimmed media playlist so
        // the relaunch keeps subtitles.
        if let Some(from) = query_from(&url) {
            if let Some(master) = with_from_in_master(&body, from) {
                respond_bytes(request, master, file_content_type(path));
                return;
            }
        }
        respond_bytes(request, body, file_content_type(path));
        return;
    }
    if let Some(src) = source {
        let headers = state.lock().unwrap().headers.clone();
        stream_file(request, &src, state, &headers);
        return;
    }
    // The playlist may be served under a title-based name
    // (`/<title>.m3u8`); anything ending in .m3u8 outside /seg/ and /sub/
    // is the playlist. /master.m3u8 and /sub/*.m3u8 resolve from `files`
    // above first, so this branch never shadows them.
    if path == "/"
        || path == "/index.m3u8"
        || path == "/playlist.m3u8"
        || (path.ends_with(".m3u8")
            && !path.starts_with("/seg/")
            && !path.starts_with("/sub/"))
    {
        let body = state.lock().unwrap().playlist.clone();
        // Click-to-seek opens the media playlist with `?from=N`: serve it
        // trimmed so VLC starts there. Out-of-range values fall back to
        // the full playlist.
        let body = query_from(&url)
            .and_then(|from| trim_playlist(&body, from))
            .unwrap_or(body);
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
        note_fetch_begin(&**state);
        match fetch_segment_body(&src, &mut hdrs, rest) {
            Some(body) => {
                let bytes = body.len() as u64;
                note_fetch_ok(&**state, bytes);
                {
                    let mut st = state.lock().unwrap();
                    st.cached_bytes += bytes;
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
                let msg = format!("seg {idx} {src} → upstream failed{detail}");
                note_fetch_err(&**state, msg.clone());
                proxy_log(state, "error", msg);
                let _ =
                    request.respond(Response::from_string("upstream failed").with_status_code(502));
            }
        }
        return;
    }
    let _ = request.respond(Response::from_string("not found").with_status_code(404));
}

/// Counts bytes pulled through a progressive-file response so the chunk
/// grid lights up while VLC streams. Open-ended ranges may never complete,
/// so marks land every MiB and on drop — never only at close. Sections are
/// brief and flat; the reader is dropped by the response pump on completion
/// or abort alike.
struct ServeCounter<R> {
    inner: R,
    state: Arc<Mutex<ProxyState>>,
    span: Option<(u64, Option<u64>)>,
    delivered: u64,
    marked: u64,
}

impl<R: std::io::Read> ServeCounter<R> {
    fn mark(&mut self) {
        if let Some((start, total)) = self.span {
            if self.delivered > self.marked {
                self.marked = self.delivered;
                // Best-effort: the data plane never parks on a wedged proxy.
                let _ = try_note_file_served(&self.state, start, start + self.delivered, total);
            }
        }
    }
}

impl<R: std::io::Read> std::io::Read for ServeCounter<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.delivered += n as u64;
            if self.delivered - self.marked >= 1024 * 1024 {
                self.mark();
            }
        }
        Ok(n)
    }
}

impl<R> Drop for ServeCounter<R> {
    fn drop(&mut self) {
        // Inlined mark: Drop impls cannot carry the Read bound.
        // Best-effort: teardown never parks on a wedged proxy either.
        if let Some((start, total)) = self.span {
            if self.delivered > self.marked {
                let _ = try_note_file_served(&self.state, start, start + self.delivered, total);
            }
        }
    }
}

fn stream_file(
    request: tiny_http::Request,
    src: &str,
    state: &Arc<Mutex<ProxyState>>,
    headers: &HashMap<String, String>,
) {
    // Bodies stream through curl, so byte counts are unknown: mark activity
    // only, which keeps the session status truthful without skewing speed.
    // Headers arrive lock-free from the routing snapshot. Without a known
    // total the cache is skipped and the request streams, so a wedged proxy
    // degrades instead of stalling VLC's first byte.
    note_fetch_begin(state);
    let is_head = *request.method() == tiny_http::Method::Head;
    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());
    let fetch = crate::urls::strip_range_query(src);
    // Progressive files always stream straight through curl: VLC
    // reads each span once, so first-byte latency beats any disk
    // cache. Served spans still mark progress (see ServeCounter).
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
    for (k, v) in headers {
        if crate::fetch::omit_curl_header(k, v, &fetch, headers) {
            continue;
        }
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    curl_push_url(&mut cmd, &fetch);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // One transparent retry on throttling/overload codes: a lone 429 /
    // 502 / 503 / 504 must not fail playback when the next attempt
    // succeeds. The `-D` header file is overwritten per attempt.
    let mut attempt = 0;
    let (code, ctype, clen, crange, mut child, pid) = loop {
        attempt += 1;
        let mut spawned = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                let _ = fs::remove_dir_all(&dir);
                note_fetch_err(state, format!("file {src} → curl spawn failed"));
                let _ = request.respond(Response::from_string("curl").with_status_code(502));
                return;
            }
        };
        let attempt_pid = spawned.id();
        if let Some(hook) = current_cancel() {
            hook.track(attempt_pid);
        }
        let hdr_text = wait_http_headers(&hdr_path, Duration::from_secs(20));
        let (code, ctype, clen, crange) = parse_upstream_headers(&hdr_text);
        let retryable = matches!(code, 429 | 502 | 503 | 504);
        if !(200..300).contains(&code) && retryable && attempt == 1 {
            if let Some(hook) = current_cancel() {
                hook.untrack(attempt_pid);
            }
            let _ = spawned.kill();
            let _ = spawned.wait();
            if current_cancel().is_some_and(|c| c.is_cancelled()) {
                let _ = fs::remove_dir_all(&dir);
                let msg = format!("file {src} → HTTP {code}");
                note_fetch_err(state, msg.clone());
                proxy_log(state, "error", msg);
                let _ =
                    request.respond(Response::from_string(format!("upstream {code}"))
                        .with_status_code(code));
                return;
            }
            proxy_log(state, "error", format!("file {src} → HTTP {code}, retrying once"));
            thread::sleep(Duration::from_millis(1500));
            continue;
        }
        break (code, ctype, clen, crange, spawned, attempt_pid);
    };
    if !(200..300).contains(&code) {
        if let Some(hook) = current_cancel() {
            hook.untrack(pid);
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&dir);
        let msg = format!("file {src} → HTTP {}", if code == 0 { 502 } else { code });
        note_fetch_err(state, msg.clone());
        proxy_log(state, "error", msg);
        let _ = request.respond(
            Response::from_string(format!("upstream {code}")).with_status_code(if code == 0 {
                502
            } else {
                code
            }),
        );
        return;
    }
    note_fetch_ok(state, 0);
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
        // Giant responses must not hold the connection: a seek reuses a
        // fresh socket instead of queueing behind an abandoned pump.
        Header::from_bytes(&b"Connection"[..], &b"close"[..]).unwrap(),
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
    // Mark the request up front: open-ended streams may never complete, and
    // the grid must light while VLC pulls, not after it disconnects. The
    // counter below refines the span as bytes flow.
    let span = if is_head {
        None
    } else {
        resolve_serve_span(range.as_deref(), code, clen, crange.as_deref())
    };
    if let Some((start, _end, total)) = span {
        // Best-effort pre-mark: never stall the first byte.
        if let Ok(mut st) = state.try_lock() {
            if total.is_some() {
                st.total_bytes = total;
            }
            st.served_cur_byte = Some(start);
            st.served_count += 1;
        }
    }
    let counter = ServeCounter {
        inner: stdout,
        state: state.clone(),
        span: span.map(|(start, _, total)| (start, total)),
        delivered: 0,
        marked: 0,
    };
    let resp = Response::new(StatusCode(code), resp_hdrs, counter, clen, None);
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
pub(crate) mod tests {
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

    #[test]
    fn test_fetch_status_error_beats_everything() {
        let now = 1_000_000u64;
        // Latest result failed 1s ago: red even with traffic in flight.
        assert_eq!(fetch_status(100, 10, 10, 1, now - 2_000, now - 1_000, now), "error");
        // Stale failure (older than 30s) with a newer success: not red.
        assert_eq!(fetch_status(100, 10, 10, 0, now - 1_000, now - 60_000, now), "downloading");
        // Stale failure and idle: falls through to buffered.
        assert_eq!(fetch_status(100, 10, 10, 0, 0, now - 60_000, now), "buffered");
    }

    #[test]
    fn test_fetch_status_downloading_buffered_idle() {
        let now = 1_000_000u64;
        // In flight counts as downloading even with a full window.
        assert_eq!(fetch_status(100, 10, 10, 2, 0, 0, now), "downloading");
        // Success 1s ago counts as downloading.
        assert_eq!(fetch_status(100, 10, 10, 0, now - 1_000, 0, now), "downloading");
        // Idle with the readahead window full: yellow.
        assert_eq!(fetch_status(100, 10, 10, 0, now - 60_000, 0, now), "buffered");
        // Idle with a gap ahead: not buffered.
        assert_eq!(fetch_status(100, 3, 10, 0, now - 60_000, 0, now), "idle");
        // Fresh session, nothing yet.
        assert_eq!(fetch_status(0, 0, 0, 0, 0, 0, now), "idle");
    }

    pub(crate) fn blank_state(segments: Vec<String>, epoch: u64) -> Mutex<ProxyState> {
        Mutex::new(ProxyState {
            playlist: Vec::new(),
            headers: HashMap::new(),
            segments,
            files: HashMap::new(),
            seg_cache: HashMap::new(),
            served_cur: None,
            served_max: None,
            served_ranges: Vec::new(),
        cached_cells: [false; FILE_CHUNKS],
            served_cur_byte: None,
            file_seek_chunk: None,
            total_bytes: None,
            served_count: 0,
            cached_bytes: 0,
            fetched_bytes: 0,
            fetch_inflight: 0,
            last_ok_ms: 0,
            last_error_ms: 0,
            last_error: None,
            recent: VecDeque::new(),
            fetch_epoch: epoch,
            verbose: false,
            source: None,
            on_log: None,
            pending_log: Vec::new(),
            log_fail: 0,
        })
    }

    #[test]
    fn test_fill_range_stale_epoch_fetches_nothing() {
        // A seek moved the epoch on: the stale window must do zero work —
        // no step reports, no cache writes, and no fetch is even attempted
        // (an attempt would leave fetch marks behind).
        let segments = vec!["http://127.0.0.1:1/bogus-0.ts".to_string()];
        let headers = HashMap::new();
        let state = blank_state(segments.clone(), 7);
        let stop = AtomicBool::new(false);
        fill_range(&segments, &headers, &state, &stop, 0, 1, 6);
        let st = state.lock().unwrap();
        assert!(st.seg_cache.is_empty());
        assert_eq!(st.last_error_ms, 0);
        assert_eq!(st.last_ok_ms, 0);
        assert_eq!(st.fetch_inflight, 0);
    }

    #[test]
    fn test_fill_range_current_epoch_reports_cached_without_fetching() {
        // The live window proceeds: cached entries report steps, and with
        // everything cached no upstream fetch is attempted.
        let segments = vec!["http://127.0.0.1:1/x.ts".to_string()];
        let headers = HashMap::new();
        let state = blank_state(segments.clone(), 7);
        state.lock().unwrap().seg_cache.insert(0, vec![1, 2, 3]);
        let stop = AtomicBool::new(false);
        fill_range(&segments, &headers, &state, &stop, 0, 1, 7);
        let st = state.lock().unwrap();
        assert_eq!(st.seg_cache.len(), 1);
        assert_eq!(st.last_error_ms, 0);
        assert_eq!(st.last_ok_ms, 0);
    }

    #[test]
    fn test_trigger_readahead_bumps_epoch_without_spawning_when_full() {
        // Window already full: no fill thread spawns, but the epoch still
        // bumps so any straggler from an older seek stands down.
        let segments: Vec<String> =
            (0..10).map(|n| format!("http://127.0.0.1:1/s{n}.ts")).collect();
        let state = Arc::new(blank_state(segments, 0));
        {
            let mut st = state.lock().unwrap();
            for n in 0..10 {
                st.seg_cache.insert(n, vec![0]);
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        trigger_readahead(state.clone(), stop);
        assert_eq!(state.lock().unwrap().fetch_epoch, 1);
    }

    const TRIM_FIXTURE: &str = "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:6\n\
         #EXT-X-MEDIA-SEQUENCE:5\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"http://127.0.0.1:9/seg/00007.key\"\n\
         #EXTINF:6.0,\n\
         http://127.0.0.1:9/seg/00007.ts\n\
         #EXTINF:6.0,\n\
         http://127.0.0.1:9/seg/00008.ts\n\
         #EXT-X-KEY:METHOD=NONE\n\
         #EXTINF:6.0,\n\
         http://127.0.0.1:9/seg/00009.ts\n\
         #EXTINF:6.0,\n\
         http://127.0.0.1:9/seg/00010.ts\n\
         #EXT-X-ENDLIST\n";

    #[test]
    fn test_seg_offset_secs_sums_earlier_durations() {
        let body = "#EXTM3U\n\
             #EXT-X-TARGETDURATION:8\n\
             #EXTINF:8.0,\n\
             http://127.0.0.1:9/seg/00007.ts\n\
             #EXT-X-KEY:METHOD=NONE\n\
             #EXTINF:4.5,\n\
             http://127.0.0.1:9/seg/00008.ts\n\
             #EXTINF:6.0,\n\
             http://127.0.0.1:9/seg/00009.ts\n\
             #EXT-X-ENDLIST\n";
        assert_eq!(seg_offset_secs(body.as_bytes(), 7), 0.0);
        assert_eq!(seg_offset_secs(body.as_bytes(), 8), 8.0);
        assert_eq!(seg_offset_secs(body.as_bytes(), 9), 12.5);
        // Missing durations count as 0 rather than breaking the sum.
        let bare = "#EXTM3U\nhttp://127.0.0.1:9/seg/00000.ts\n";
        assert_eq!(seg_offset_secs(bare.as_bytes(), 1), 0.0);
        assert_eq!(seg_offset_secs(b"not utf8 \xff", 3), 0.0);
    }

    #[test]
    fn test_query_from_parses_seek_param() {
        assert_eq!(query_from("/x.m3u8?from=12"), Some(12));
        assert_eq!(query_from("/x.m3u8?foo=1&from=3"), Some(3));
        assert_eq!(query_from("/x.m3u8"), None);
        assert_eq!(query_from("/x.m3u8?from=abc"), None);
    }

    #[test]
    fn test_trim_playlist_zero_is_identity() {
        assert_eq!(
            trim_playlist(TRIM_FIXTURE.as_bytes(), 0),
            Some(TRIM_FIXTURE.as_bytes().to_vec())
        );
    }

    #[test]
    fn test_trim_playlist_drops_earlier_segments() {
        let out = String::from_utf8(trim_playlist(TRIM_FIXTURE.as_bytes(), 8).unwrap()).unwrap();
        assert!(
            out.contains("#EXT-X-MEDIA-SEQUENCE:6"),
            "sequence advances by dropped count, got:\n{out}"
        );
        assert!(!out.contains("00007.ts"), "earlier segment is gone:\n{out}");
        assert!(out.contains("00008.ts"));
        assert!(out.contains("00010.ts"));
        assert!(out.contains("#EXT-X-ENDLIST"));
        // The AES key from the dropped region rides along to the first kept
        // segment; the EXTINF orphan of the dropped segment does not.
        assert_eq!(out.matches("00007.key").count(), 1);
        assert_eq!(out.matches("#EXTINF").count(), 3);
    }

    #[test]
    fn test_trim_playlist_inserts_sequence_when_missing() {
        let no_seq = TRIM_FIXTURE.replace("#EXT-X-MEDIA-SEQUENCE:5\n", "");
        let out = String::from_utf8(trim_playlist(no_seq.as_bytes(), 9).unwrap()).unwrap();
        assert!(
            out.contains("#EXT-X-MEDIA-SEQUENCE:2"),
            "two dropped entries, got:\n{out}"
        );
        assert!(out.contains("00009.ts"));
        assert!(!out.contains("00008.ts"));
    }

    #[test]
    fn test_trim_playlist_past_end_is_none() {
        assert_eq!(trim_playlist(TRIM_FIXTURE.as_bytes(), 99), None);
    }

    #[test]
    fn test_with_from_in_master_points_video_at_trim() {
        let master = "#EXTM3U\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"http://127.0.0.1:9/sub/0.vtt\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES=\"subs\"\n\
             http://127.0.0.1:9/My_Show.m3u8\n";
        let out =
            String::from_utf8(with_from_in_master(master.as_bytes(), 4).unwrap()).unwrap();
        assert!(out.contains("http://127.0.0.1:9/My_Show.m3u8?from=4"), "got:\n{out}");
        assert!(
            out.contains("URI=\"http://127.0.0.1:9/sub/0.vtt\""),
            "sub URI untouched:\n{out}"
        );
        assert_eq!(with_from_in_master(b"#EXTM3U\n#EXT-X-VERSION:3\n", 4), None);
    }

    #[test]
    fn test_recent_speed_bps_uses_trailing_window() {
        let now = 1_000_000u64;
        let empty: VecDeque<(u64, u64)> = VecDeque::new();
        assert_eq!(recent_speed_bps(&empty, now), None);
        // 1 MB over the last 2s → ~500_000 B/s.
        let recent = VecDeque::from([(now - 2_000, 1_000_000u64)]);
        assert_eq!(recent_speed_bps(&recent, now), Some(500_000));
        // Samples older than 10s are ignored.
        let stale = VecDeque::from([(now - 60_000, 9_000_000u64)]);
        assert_eq!(recent_speed_bps(&stale, now), None);
        // Mixed: only the fresh byte counts contribute.
        let mixed = VecDeque::from([(now - 60_000, 9_000_000u64), (now - 1_000, 200_000u64)]);
        assert_eq!(recent_speed_bps(&mixed, now), Some(200_000));
    }

    #[test]
    fn test_resolve_serve_span_parses_ranges() {
        // Explicit range keeps its span; total stays unknown.
        assert_eq!(
            resolve_serve_span(Some("bytes=0-99"), 206, None, None),
            Some((0, 100, None))
        );
        // Open range resolves against the Content-Range total.
        assert_eq!(
            resolve_serve_span(Some("bytes=100-"), 206, None, Some("bytes 100-199/1000")),
            Some((100, 1000, Some(1000)))
        );
        // Full 200 response maps the whole known body.
        assert_eq!(
            resolve_serve_span(None, 200, Some(500), None),
            Some((0, 500, Some(500)))
        );
        // Full response of unknown size: nothing honest to mark.
        assert_eq!(resolve_serve_span(None, 200, None, None), None);
        // Backwards and garbage spans are skipped, not displayed.
        assert_eq!(resolve_serve_span(Some("bytes=100-50"), 206, None, None), None);
        assert_eq!(resolve_serve_span(Some("bytes=nope"), 206, None, None), None);
    }

    #[test]
    fn test_fetch_notes_skip_wedged_proxy() {
        let state = blank_state(Vec::new(), 0);
        {
            let _held = state.lock().unwrap();
            // None of these may park: wedged counters degrade silently, and
            // the error lines still reach the terminal (captured, not shown,
            // under cargo test) instead of the wedged UI queue.
            note_fetch_begin(&state);
            note_fetch_ok(&state, 10);
            note_fetch_err(&state, "boom".to_string());
            proxy_log(&state, "error", "boom".to_string());
        }
        let st = state.lock().unwrap();
        assert_eq!(st.fetch_inflight, 0);
        assert!(st.last_error.is_none());
        assert!(st.pending_log.is_empty());
    }

    #[test]
    fn test_try_note_file_served_skips_wedged_proxy() {
        let state = blank_state(Vec::new(), 0);
        assert!(try_note_file_served(&state, 5, 15, Some(100)));
        assert_eq!(state.lock().unwrap().served_ranges, vec![(5, 15)]);
        assert_eq!(state.lock().unwrap().total_bytes, Some(100));
        // Held proxy: miss, never parks the serve pump.
        let _held = state.lock().unwrap();
        assert!(!try_note_file_served(&state, 5, 20, Some(100)));
    }

    #[test]
    fn test_try_note_file_served_recovers_poisoned_proxy() {
        let state = blank_state(Vec::new(), 0);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = state.lock().unwrap();
            panic!("holder died");
        }));
        assert!(r.is_err());
        // Poisoned state is recovered, not skipped.
        assert!(try_note_file_served(&state, 0, 10, Some(50)));
    }

    #[test]
    fn test_serve_counter_marks_progress_while_streaming() {
        use std::io::Read;
        let state = Arc::new(blank_state(Vec::new(), 0));
        {
            let mut counter = ServeCounter {
                inner: Cursor::new(vec![0u8; 10]),
                state: state.clone(),
                span: Some((5, Some(100))),
                delivered: 0,
                marked: 0,
            };
            // Small reads stay below the MiB mark threshold: nothing yet.
            let mut buf = [0u8; 3];
            counter.read_exact(&mut buf).unwrap();
            assert!(state.lock().unwrap().served_ranges.is_empty());
            // Draining the rest then dropping flushes the final span.
            let mut rest = Vec::new();
            counter.read_to_end(&mut rest).unwrap();
            assert_eq!(rest.len(), 7);
        }
        let st = state.lock().unwrap();
        assert_eq!(st.served_ranges, vec![(5, 15)]);
        assert_eq!(st.total_bytes, Some(100));
    }

    #[test]
    fn test_serve_counter_extends_span_every_mib() {
        use std::io::Read;
        let state = Arc::new(blank_state(Vec::new(), 0));
        let body = vec![7u8; 3 * 1024 * 1024];
        let mut counter = ServeCounter {
            inner: Cursor::new(body),
            state: state.clone(),
            span: Some((100, Some(10_000_000))),
            delivered: 0,
            marked: 0,
        };
        // Pull ~1.5 MiB in small reads: the first MiB mark lands once the
        // threshold trips (11th read), extending the same span.
        let mut buf = [0u8; 100 * 1024];
        for _ in 0..15 {
            counter.read_exact(&mut buf).unwrap();
        }
        assert_eq!(
            state.lock().unwrap().served_ranges,
            vec![(100, 100 + 11 * 100 * 1024)]
        );
        // Drop flushes the pulled remainder into the same single span.
        drop(counter);
        let st = state.lock().unwrap();
        assert_eq!(st.served_ranges, vec![(100, 100 + 15 * 100 * 1024)]);
    }

    #[test]
    fn test_file_playhead_chunk_tracks_request_start() {
        assert_eq!(file_playhead_chunk(0, 100), None);
        // 2000 bytes → 20-byte cells: offset 150 lives in cell 7.
        assert_eq!(file_playhead_chunk(2000, 150), Some(7));
        // File start maps to the first cell.
        assert_eq!(file_playhead_chunk(2000, 0), Some(0));
        // Past-the-end offsets clamp to the final cell, never past it.
        assert_eq!(file_playhead_chunk(2000, 2000), Some(FILE_CHUNKS - 1));
        assert_eq!(file_playhead_chunk(2000, u64::MAX), Some(FILE_CHUNKS - 1));
    }

    #[test]
    fn test_served_cells_maps_spans_to_grid() {
        // Unknown total: nothing honest to display.
        assert!(served_cells(0, &[(0, 100)]).iter().all(|&c| !c));
        // 2000 bytes → 20-byte cells: span 140-160 lights cell 7 alone.
        let cells = served_cells(2000, &[(140, 160)]);
        assert!(cells[7]);
        assert!(!cells[6]);
        assert!(!cells[8]);
        // Overlapping spans merge; empty spans are skipped.
        let cells = served_cells(2000, &[(0, 20), (10, 40), (50, 50)]);
        assert!(cells[0]);
        assert!(cells[1]);
        assert!(!cells[2]);
    }
}
