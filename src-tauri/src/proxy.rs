use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::fs::File;
use std::io::{Cursor, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// inclusive, end exclusive). Kept for diagnostics; the grid renders
    /// `cached_cells` instead so green means on-disk like HLS. Bounded.
    pub served_ranges: Vec<(u64, u64)>,
    /// File grid cells whose bytes sit on disk (fetched, warmed, or
    /// pre-read). Rendered directly by the file DTO: green means available
    /// locally, matching cached HLS segments.
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

/// Mark one disk-complete piece visible on the file grid: only cells whose
/// first byte (the seek landing point) the piece contains. A cell is green
/// only when seeking there starts instantly, matching the HLS grid where
/// green means cached segments. In particular a trailing `moov` probe piece
/// lights nothing: seeking the last cell still needs its first bytes.
/// Best-effort: a wedged proxy only dims the grid, never stalls.
pub(crate) fn record_piece_cell(state: &Mutex<ProxyState>, total: u64, idx: u64) {
    if total == 0 {
        return;
    }
    let (pstart, plen) = file_piece_span(total, idx);
    if plen == 0 {
        return;
    }
    // First cell whose first byte sits at or after `off`, unclamped so a
    // past-the-end piece compares empty instead of clamping onto cell 99.
    let first_cell_from = |off: u64| {
        (off.saturating_mul(FILE_CHUNKS as u64).saturating_add(total - 1) / total) as usize
    };
    let (lo, hi) = (
        first_cell_from(pstart),
        first_cell_from(pstart + plen).saturating_sub(1),
    );
    if lo <= hi {
        if let Ok(mut st) = state.try_lock() {
            for c in lo.min(FILE_CHUNKS - 1)..=hi.min(FILE_CHUNKS - 1) {
                st.cached_cells[c] = true;
            }
        }
    }
}

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

/// Read-through extent cache for progressive files: 1 MiB pieces as files,
/// complete via atomic rename (presence = complete, crash-safe by
/// construction). Concurrent same-piece fetches may duplicate work but
/// never corrupt: temp names are unique, renames last-wins identical bytes.
pub(crate) const FILE_PIECE: u64 = 1024 * 1024;
/// How many content dirs the cache keeps; oldest beyond this is pruned.
pub(crate) const FILE_CACHE_MAX_CONTENTS: usize = 4;

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stable cache key for a (possibly signed, rotating-token) file URL:
/// host plus path with the query and any per-issue `~signature` tail
/// stripped. Degenerate paths fall back to the full query-less URL so the
/// key never collapses to host-only. Misses are the safe direction.
pub(crate) fn file_cache_key(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or(url);
    let after_scheme = no_query.split("://").nth(1).unwrap_or(no_query);
    let slash = after_scheme.find('/').unwrap_or(after_scheme.len());
    let (host, path) = after_scheme.split_at(slash);
    let stable = path.split('~').next().unwrap_or(path);
    let keyed = if stable.len() < 8 { no_query } else { stable };
    format!("{:016x}", fnv1a64(&format!("{host}{keyed}")))
}

pub(crate) fn file_cache_root() -> PathBuf {
    std::env::temp_dir().join("play-file-cache")
}

/// Piece index holding a byte offset; (start, expected_len) of a piece.
pub(crate) fn file_piece_index(offset: u64) -> u64 {
    offset / FILE_PIECE
}

pub(crate) fn file_piece_span(total: u64, idx: u64) -> (u64, u64) {
    let start = idx.saturating_mul(FILE_PIECE);
    let len = total.saturating_sub(start).min(FILE_PIECE);
    (start, len)
}

/// Requested (start, end-exclusive) byte span from a Range value against a
/// known total, clamped into range. Full requests map the whole file.
/// Unparseable or out-of-range spans return None: serve streaming instead.
pub(crate) fn parse_request_span(range: Option<&str>, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    match range.and_then(|r| r.strip_prefix("bytes=")) {
        Some(rest) => {
            let (a, b) = rest.split_once('-')?;
            let start: u64 = a.trim().parse().ok()?;
            let mut end = if b.trim().is_empty() {
                total
            } else {
                b.trim().parse::<u64>().ok()?.saturating_add(1)
            };
            end = end.min(total);
            if end <= start || start >= total {
                return None;
            }
            Some((start, end))
        }
        None => Some((0, total)),
    }
}

/// Pure LRU choice: which cached contents to drop, oldest first, never the
/// live one. Tested; the fs walk feeding it is not.
pub(crate) fn dirs_to_prune(
    mut entries: Vec<(PathBuf, std::time::SystemTime)>,
    live: &Path,
    cap: usize,
) -> Vec<PathBuf> {
    entries.retain(|(p, _)| p != live);
    entries.sort_by_key(|(_, t)| *t);
    let drop = entries.len().saturating_sub(cap.saturating_sub(1));
    entries.into_iter().take(drop).map(|(p, _)| p).collect()
}

#[derive(Clone)]
pub(crate) struct FileCache {
    dir: PathBuf,
    total: u64,
    ctype: String,
}

impl FileCache {
    /// Open (creating) the cache for one content id. Records the content
    /// type when known, prunes other contents past the cap. None on fs
    /// failure: callers fall back to pass-through streaming.
    pub(crate) fn open(url: &str, total: u64, ctype: Option<&str>) -> Option<Self> {
        let dir = file_cache_root().join(file_cache_key(url));
        if fs::create_dir_all(&dir).is_err() {
            return None;
        }
        if let Some(ct) = ctype {
            let marker = dir.join("ctype");
            if !marker.exists() {
                let _ = fs::write(&marker, ct.as_bytes().get(..128).unwrap_or(ct.as_bytes()));
            }
        }
        // Best-effort LRU prune of other contents.
        if let Ok(rd) = fs::read_dir(file_cache_root()) {
            let mut entries = Vec::new();
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if let Ok(m) = fs::metadata(&p).and_then(|m| m.modified()) {
                        entries.push((p, m));
                    }
                }
            }
            for old in dirs_to_prune(entries, &dir, FILE_CACHE_MAX_CONTENTS) {
                let _ = fs::remove_dir_all(old);
            }
        }
        let ctype = fs::read(dir.join("ctype"))
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "video/mp4".to_string());
        Some(Self { dir, total, ctype })
    }

    fn part_path(&self, idx: u64) -> PathBuf {
        self.dir.join(format!("{idx:08}.part"))
    }
}

/// Fetch one exact byte span upstream into a temp file. Rejects short,
/// long, and non-2xx answers: a wrong-sized part must never complete.
fn fetch_piece_to_file(
    url: &str,
    headers: &HashMap<String, String>,
    start: u64,
    len: u64,
    dest_tmp: &Path,
) -> std::result::Result<(), String> {
    let end = start.saturating_add(len).saturating_sub(1);
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--http1.1",
        "-L",
        "--max-redirs",
        "10",
        "--max-time",
        "30",
        "--max-filesize",
        &len.to_string(),
        "-r",
        &format!("{start}-{end}"),
        "-o",
        dest_tmp.to_str().unwrap_or("part"),
        "-w",
        "%{http_code}",
    ]);
    for (k, v) in headers {
        if crate::fetch::omit_curl_header(k, v, url, headers) {
            continue;
        }
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    curl_push_url(&mut cmd, url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .map_err(|e| format!("play: cache fetch spawn failed: {e}"))?;
    if !out.status.success() {
        let _ = fs::remove_file(dest_tmp);
        return Err(format!("play: cache fetch curl failed"));
    }
    let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let size = fs::metadata(dest_tmp).map(|m| m.len()).unwrap_or(u64::MAX);
    if (code == "206" || code == "200") && size == len {
        Ok(())
    } else {
        let _ = fs::remove_file(dest_tmp);
        Err(format!("play: cache fetch HTTP {code} size {size}/{len}"))
    }
}

static PIECE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serves one requested span piece by piece: cache hits read from disk,
/// misses range-fetch upstream then complete atomically. Blocking fetches
/// run in serve threads (no locks held); VLC aborts orphan at most one
/// bounded fetch per in-flight piece.
pub(crate) struct CacheReader<F> {
    cache: FileCache,
    url: String,
    headers: HashMap<String, String>,
    fetch: F,
    cursor: u64,
    end: u64,
    cur: Option<(File, u64)>,
    /// Miss callback with the piece index, so the serve path can warm
    /// coming pieces. `None` in tests and anywhere without readahead.
    on_miss: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// Disk-complete callback with the piece index, so the file grid can
    /// light cached cells. Fires on hits and misses alike. `None` in tests.
    on_cached: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

impl<F> CacheReader<F>
where
    F: Fn(
        &str,
        &HashMap<String, String>,
        u64,
        u64,
        &Path,
    ) -> std::result::Result<(), String>,
{
    pub(crate) fn new(
        cache: FileCache,
        url: String,
        headers: HashMap<String, String>,
        fetch: F,
        start: u64,
        end: u64,
        on_miss: Option<Arc<dyn Fn(u64) + Send + Sync>>,
        on_cached: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    ) -> Self {
        Self {
            cache,
            url,
            headers,
            fetch,
            cursor: start,
            end,
            cur: None,
            on_miss,
            on_cached,
        }
    }

    fn record_cached(&self, idx: u64) {
        if let Some(cb) = &self.on_cached {
            cb(idx);
        }
    }

    fn ensure_piece(&self, idx: u64) -> std::io::Result<File> {
        let (pstart, plen) = file_piece_span(self.cache.total, idx);
        let path = self.cache.part_path(idx);
        match fs::metadata(&path) {
            Ok(m) if m.len() == plen => {
                self.record_cached(idx);
                return File::open(path).map_err(other_err);
            }
            Ok(_) => {
                let _ = fs::remove_file(&path);
            }
            Err(_) => {}
        }
        let tmp = self.cache.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            PIECE_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        (self.fetch)(&self.url, &self.headers, pstart, plen, &tmp)
            .map_err(|e| std::io::Error::other(e))?;
        fs::rename(&tmp, &path).map_err(other_err)?;
        self.record_cached(idx);
        File::open(&path).map_err(other_err)
    }

    /// Fetch the piece holding `offset` when missing. Serve paths call this
    /// before committing to a cache response: a range-ignoring or dead
    /// upstream must fall back to streaming, never truncate VLC mid-body.
    pub(crate) fn prewarm(&self, offset: u64) -> std::io::Result<()> {
        self.ensure_piece(file_piece_index(offset)).map(|_| ())
    }
}

/// A piece file is complete when present at exactly its expected length.
/// Zero-length tails are never complete: there is nothing to fetch.
pub(crate) fn piece_complete(cache: &FileCache, idx: u64) -> bool {
    let (_, plen) = file_piece_span(cache.total, idx);
    plen > 0 && fs::metadata(cache.part_path(idx)).map(|m| m.len() == plen).unwrap_or(false)
}

/// Warm explicit byte offsets through any fetch backend. Stops at the
/// total; per-piece completion is atomic, so interrupted warms simply miss
/// again later. Returns the warmed piece indexes so callers can light the
/// grid. Never touches the proxy mutex.
pub(crate) fn warm_pieces<F>(
    cache: &FileCache,
    url: &str,
    headers: &HashMap<String, String>,
    fetch: F,
    offsets: &[u64],
) -> Vec<u64>
where
    F: Fn(&str, &HashMap<String, String>, u64, u64, &Path) -> std::result::Result<(), String>,
{
    let reader = CacheReader::new(
        cache.clone(),
        url.to_string(),
        headers.clone(),
        &fetch,
        0,
        cache.total,
        None,
        None,
    );
    let mut done = Vec::new();
    for &off in offsets {
        if off >= cache.total {
            break;
        }
        if reader.prewarm(off).is_ok() {
            done.push(file_piece_index(off));
        }
    }
    done
}

/// Pieces currently warming in detached threads, by cache dir and index.
/// Dedupes overlapping readahead; entries are removed on completion.
static READAHEAD_INFLIGHT: std::sync::LazyLock<Mutex<HashSet<(PathBuf, u64)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Background warm of the pieces around a file seek target: the target
/// piece plus two ahead. Detached thread, epoch-guarded: a newer seek
/// abandons stale work before fetching. Piece bytes are content-addressed
/// by offset, so a stale warm can only waste bandwidth, never corrupt.
pub(crate) fn prefetch_file_around(state: &Arc<Mutex<ProxyState>>, seg: usize) {
    let (epoch, total, src, headers) = match state.try_lock() {
        Ok(st) => match (st.total_bytes, st.source.clone()) {
            (Some(total), Some(src)) if total > 0 => {
                let mut h = st.headers.clone();
                h.retain(|k, _| !k.eq_ignore_ascii_case("range"));
                (st.fetch_epoch, total, src, h)
            }
            _ => return,
        },
        Err(_) => return,
    };
    let fetch_url = crate::urls::strip_range_query(&src);
    let state = state.clone();
    thread::spawn(move || {
        let Some(cache) = FileCache::open(&src, total, None) else {
            return;
        };
        // A newer seek may have landed between snapshot and warm: skip.
        if state
            .try_lock()
            .map(|st| st.fetch_epoch != epoch)
            .unwrap_or(false)
        {
            return;
        }
        let center = (seg as u64).saturating_mul(total) / FILE_CHUNKS as u64;
        let mut offsets = Vec::new();
        for k in 0..3u64 {
            let off = center.saturating_add(k.saturating_mul(FILE_PIECE));
            if off < total {
                offsets.push(off);
            }
        }
        for idx in warm_pieces(&cache, &fetch_url, &headers, fetch_piece_to_file, &offsets) {
            record_piece_cell(&state, total, idx);
        }
    });
}

fn other_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl<F> std::io::Read for CacheReader<F>
where
    F: Fn(
        &str,
        &HashMap<String, String>,
        u64,
        u64,
        &Path,
    ) -> std::result::Result<(), String>,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some((f, rem)) = self.cur.as_mut() {
                if *rem == 0 {
                    self.cur = None;
                    continue;
                }
                let want = buf.len().min(*rem as usize);
                let n = f.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(other_err("play: cache piece truncated"));
                }
                self.cursor += n as u64;
                *rem -= n as u64;
                return Ok(n);
            }
            if self.cursor >= self.end {
                return Ok(0);
            }
            let idx = file_piece_index(self.cursor);
            let was_miss = !piece_complete(&self.cache, idx);
            let mut f = self.ensure_piece(idx)?;
            if was_miss {
                if let Some(cb) = &self.on_miss {
                    cb(idx);
                }
            }
            let (pstart, plen) = file_piece_span(self.cache.total, idx);
            let skip = self.cursor.saturating_sub(pstart);
            if skip > 0 {
                f.seek(std::io::SeekFrom::Start(skip)).map_err(other_err)?;
            }
            let remain = (pstart + plen).min(self.end).saturating_sub(self.cursor);
            self.cur = Some((f, remain));
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
    // Read-through extent cache: a previous request already revealed the
    // total, so serve from disk (fetching misses) with no curl child.
    // Any failure falls through to streaming below.
    if !is_head {
        if let Some(total) = state.try_lock().ok().and_then(|st| st.total_bytes) {
            if let Some((start, end)) = parse_request_span(range.as_deref(), total) {
                if let Some(cache) = FileCache::open(src, total, None) {
                    let mut piece_headers = headers.clone();
                    piece_headers.retain(|k, _| !k.eq_ignore_ascii_case("range"));
                    // Grid callback: every disk-complete piece lights its
                    // cells, so green means on disk like cached HLS.
                    let on_cached: Arc<dyn Fn(u64) + Send + Sync> = {
                        let state = state.clone();
                        Arc::new(move |idx: u64| {
                            if let Some(total) = state.try_lock().ok().and_then(|st| st.total_bytes)
                            {
                                record_piece_cell(&state, total, idx);
                            }
                        })
                    };
                    // Miss readahead: on each miss, warm the next two pieces
                    // in a detached thread. An in-flight set dedupes overlap;
                    // warmed fetches never cascade (they fetch directly).
                    // Warmed pieces light the grid through the same callback.
                    let readahead: Arc<dyn Fn(u64) + Send + Sync> = {
                        let cache = cache.clone();
                        let url = fetch.clone();
                        let hdrs = piece_headers.clone();
                        let cached_cb = on_cached.clone();
                        Arc::new(move |idx: u64| {
                            let mut want = Vec::new();
                            if let Ok(mut busy) = READAHEAD_INFLIGHT.try_lock() {
                                for n in [idx + 1, idx + 2] {
                                    let (_, plen) = file_piece_span(cache.total, n);
                                    if plen == 0 || piece_complete(&cache, n) {
                                        continue;
                                    }
                                    if busy.insert((cache.dir.clone(), n)) {
                                        want.push(n);
                                    }
                                }
                            }
                            for n in want {
                                let cache = cache.clone();
                                let url = url.clone();
                                let hdrs = hdrs.clone();
                                let cached_cb = cached_cb.clone();
                                thread::spawn(move || {
                                    let (pstart, plen) = file_piece_span(cache.total, n);
                                    let tmp = cache.dir.join(format!(
                                        ".tmp-{}-{}",
                                        std::process::id(),
                                        PIECE_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
                                    ));
                                    match fetch_piece_to_file(&url, &hdrs, pstart, plen, &tmp) {
                                        Ok(()) => {
                                            if fs::rename(&tmp, &cache.part_path(n)).is_ok() {
                                                cached_cb(n);
                                            } else {
                                                let _ = fs::remove_file(&tmp);
                                            }
                                        }
                                        Err(_) => {
                                            let _ = fs::remove_file(&tmp);
                                        }
                                    }
                                    if let Ok(mut busy) = READAHEAD_INFLIGHT.try_lock() {
                                        busy.remove(&(cache.dir.clone(), n));
                                    }
                                });
                            }
                        })
                    };
                    let reader = CacheReader::new(
                        cache.clone(),
                        fetch.clone(),
                        piece_headers,
                        fetch_piece_to_file,
                        start,
                        end,
                        Some(readahead),
                        Some(on_cached),
                    );
                    // Best-effort gate: prove the upstream serves ranges
                    // before VLC depends on it. A miss falls through to
                    // streaming below, so a range-ignoring or dead upstream
                    // can never truncate playback mid-body.
                    let cache_ready = match reader.prewarm(start) {
                        Ok(()) => true,
                        Err(e) => {
                            proxy_log(
                                state,
                                "error",
                                format!("file cache unavailable ({e}), streaming"),
                            );
                            false
                        }
                    };
                    if cache_ready {
                        let counter = ServeCounter {
                            inner: reader,
                            state: state.clone(),
                            span: Some((start, Some(total))),
                            delivered: 0,
                            marked: 0,
                        };
                        // Best-effort pre-mark: never stall the first byte.
                        if let Ok(mut st) = state.try_lock() {
                            st.served_cur_byte = Some(start);
                            st.served_count += 1;
                        }
                        let mut resp_hdrs = vec![
                            Header::from_bytes(&b"Content-Type"[..], cache.ctype.as_bytes())
                                .unwrap_or_else(|_| {
                                    Header::from_bytes(&b"Content-Type"[..], &b"video/mp4"[..])
                                        .unwrap()
                                }),
                            Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap(),
                            Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..])
                                .unwrap(),
                        ];
                        let code = if start == 0 && end == total {
                            StatusCode(200)
                        } else {
                            resp_hdrs.push(
                                Header::from_bytes(
                                    &b"Content-Range"[..],
                                    format!("bytes {}-{}/{}", start, end - 1, total).as_bytes(),
                                )
                                .unwrap(),
                            );
                            StatusCode(206)
                        };
                        let resp = Response::new(
                            code,
                            resp_hdrs,
                            counter,
                            Some((end - start) as usize),
                            None,
                        );
                        let _ = request.respond(resp);
                        note_fetch_ok(state, 0);
                        return;
                    }
                }
            }
        }
    }
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            let _ = fs::remove_dir_all(&dir);
            note_fetch_err(state, format!("file {src} → curl spawn failed"));
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
        if let Some(t) = total {
            // Seed the extent cache (dir + content-type marker) so later
            // requests serve from disk. Best-effort, outside the guard:
            // pruning walks the fs and must never stall polls.
            let _ = FileCache::open(src, t, Some(&ctype));
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
    fn test_record_piece_cell_maps_piece_bytes_to_cells() {
        let state = blank_state(Vec::new(), 0);
        // Unknown size: nothing honest to display.
        record_piece_cell(&state, 0, 3);
        assert!(state.lock().unwrap().cached_cells.iter().all(|&c| !c));
        // 100 pieces of 1 MiB: piece 50 lights cell 50 alone.
        let total = 100 * FILE_PIECE;
        record_piece_cell(&state, total, 50);
        let cells = state.lock().unwrap().cached_cells;
        assert!(cells[50]);
        assert!(!cells[49]);
        assert!(!cells[51]);
        // A piece spanning the whole small file lights every cell.
        let small = blank_state(Vec::new(), 0);
        record_piece_cell(&small, 2000, 0);
        assert!(small.lock().unwrap().cached_cells.iter().all(|&c| c));
        // A trailing `moov` probe piece lights nothing: seeking the last
        // cell lands at its first byte, which lives in an earlier piece.
        let total = 1485 * FILE_PIECE;
        let moov = blank_state(Vec::new(), 0);
        record_piece_cell(&moov, total, 1484);
        {
            let cells = moov.lock().unwrap().cached_cells;
            assert!(!cells[FILE_CHUNKS - 1]);
            assert!(!cells[50]);
        }
        // The piece holding the last cell's first byte lights it.
        let first_idx = (99 * total / 100) / FILE_PIECE;
        record_piece_cell(&moov, total, first_idx);
        {
            let cells = moov.lock().unwrap().cached_cells;
            assert!(cells[FILE_CHUNKS - 1]);
            assert!(!cells[50]);
        }
        // Past-the-end piece: nothing to fetch, nothing lit.
        record_piece_cell(&small, 2000, 5);
        assert_eq!(
            small
                .lock()
                .unwrap()
                .cached_cells
                .iter()
                .filter(|&&c| c)
                .count(),
            FILE_CHUNKS
        );
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
    fn test_file_cache_key_survives_token_rotation() {
        let a = "https://e1.cdn.example/v0d3ur3l/s0m3v1d30~AbC123?token=t0k3n000000000000000001&expiry=1000000000001";
        let b = "https://e1.cdn.example/v0d3ur3l/s0m3v1d30~OTHERsig?token=t0k3n000000000000000002&expiry=1000000000002";
        // Rotated query token and ~signature tail: same content, same key.
        assert_eq!(file_cache_key(a), file_cache_key(b));
        assert_eq!(file_cache_key(a).len(), 16);
        // Different content path: different key.
        let c = "https://e1.cdn.example/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/different~sig?token=x&expiry=1";
        assert_ne!(file_cache_key(a), file_cache_key(c));
    }

    #[test]
    fn test_file_piece_math() {
        assert_eq!(file_piece_index(0), 0);
        assert_eq!(file_piece_index(FILE_PIECE - 1), 0);
        assert_eq!(file_piece_index(FILE_PIECE), 1);
        // Full piece then a short tail.
        assert_eq!(file_piece_span(3 * FILE_PIECE, 1), (FILE_PIECE, FILE_PIECE));
        assert_eq!(file_piece_span(2 * FILE_PIECE + 7, 2), (2 * FILE_PIECE, 7));
    }

    #[test]
    fn test_parse_request_span_clamps() {
        assert_eq!(parse_request_span(Some("bytes=100-199"), 1000), Some((100, 200)));
        assert_eq!(parse_request_span(Some("bytes=100-"), 1000), Some((100, 1000)));
        // Past-the-end clamps into range; start past total is unservable.
        assert_eq!(parse_request_span(Some("bytes=900-9999"), 1000), Some((900, 1000)));
        assert_eq!(parse_request_span(Some("bytes=1000-"), 1000), None);
        assert_eq!(parse_request_span(None, 1000), Some((0, 1000)));
        assert_eq!(parse_request_span(Some("bytes=nope"), 1000), None);
        assert_eq!(parse_request_span(Some("bytes=0-99"), 0), None);
    }

    #[test]
    fn test_dirs_to_prune_keeps_live_and_newest() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        let live = PathBuf::from("/c/live");
        let entries = vec![
            (PathBuf::from("/c/old"), t(1)),
            (live.clone(), t(2)),
            (PathBuf::from("/c/new"), t(3)),
        ];
        // Cap 2: live plus newest survive; oldest drops.
        assert_eq!(
            dirs_to_prune(entries, &live, 2),
            vec![PathBuf::from("/c/old")]
        );
    }

    #[test]
    fn test_cache_reader_serves_hits_without_refetch() {
        use std::cell::Cell;
        use std::io::Read;
        use std::rc::Rc;
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache {
            dir: dir.path().to_path_buf(),
            total: 2 * FILE_PIECE + 10,
            ctype: "video/mp4".to_string(),
        };
        let calls = Rc::new(Cell::new(0u32));
        let fill = {
            let calls = calls.clone();
            move |_url: &str, _hd: &HashMap<String, String>, start: u64, len: u64, dest: &Path| {
                calls.set(calls.get() + 1);
                // Deterministic content: byte value = piece index.
                let v = (start / FILE_PIECE) as u8;
                fs::write(dest, vec![v; len as usize]).map(|_| ()).map_err(|e| e.to_string())
            }
        };
        // Span crossing three pieces: two full, one 10-byte tail.
        let span_start = FILE_PIECE - 5;
        let span_end = 2 * FILE_PIECE + 10;
        let mut out = Vec::new();
        CacheReader::new(
            cache.clone(),
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fill,
            span_start,
            span_end,
            None,
            None,
        )
        .read_to_end(&mut out)
        .unwrap();
        assert_eq!(out.len() as u64, span_end - span_start);
        assert_eq!(calls.get(), 3);
        assert_eq!(&out[..5], &[0u8; 5]);
        assert_eq!(&out[5..5 + FILE_PIECE as usize], &vec![1u8; FILE_PIECE as usize][..]);
        // Identical second pass: all hits, no refetch.
        let fill2 = {
            let calls = calls.clone();
            move |_url: &str, _hd: &HashMap<String, String>, _s: u64, _l: u64, _d: &Path| {
                calls.set(calls.get() + 1);
                Err("must not refetch".to_string())
            }
        };
        let mut out2 = Vec::new();
        CacheReader::new(
            cache,
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fill2,
            span_start,
            span_end,
            None,
            None,
        )
        .read_to_end(&mut out2)
        .unwrap();
        assert_eq!(out2, out);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn test_cache_reader_prewarm_fetches_first_piece_only() {
        use std::cell::Cell;
        use std::rc::Rc;
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache {
            dir: dir.path().to_path_buf(),
            total: 3 * FILE_PIECE,
            ctype: "video/mp4".to_string(),
        };
        let calls = Rc::new(Cell::new(0u32));
        let fill = {
            let calls = calls.clone();
            move |_url: &str,
                  _hd: &HashMap<String, String>,
                  _s: u64,
                  len: u64,
                  dest: &Path| {
                calls.set(calls.get() + 1);
                fs::write(dest, vec![9u8; len as usize])
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        };
        let reader = CacheReader::new(
            cache,
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fill,
            FILE_PIECE + 7,
            2 * FILE_PIECE,
            None,
            None,
        );
        reader.prewarm(FILE_PIECE + 7).unwrap();
        assert_eq!(calls.get(), 1);
        // Second prewarm is a hit: no refetch.
        reader.prewarm(FILE_PIECE + 7).unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn test_cache_reader_prewarm_reports_fetch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache {
            dir: dir.path().to_path_buf(),
            total: 3 * FILE_PIECE,
            ctype: "video/mp4".to_string(),
        };
        let fail =
            |_url: &str, _hd: &HashMap<String, String>, _s: u64, _l: u64, _d: &Path| {
                Err("upstream 403".to_string())
            };
        let reader = CacheReader::new(
            cache,
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fail,
            0,
            FILE_PIECE,
            None,
            None,
        );
        // The serve path treats this as streaming fallback, never truncation.
        assert!(reader.prewarm(0).is_err());
    }

    #[test]
    fn test_cache_reader_miss_callback_fires_once_per_miss() {
        use std::cell::Cell;
        use std::io::Read;
        use std::rc::Rc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache {
            dir: dir.path().to_path_buf(),
            total: 2 * FILE_PIECE + 10,
            ctype: "video/mp4".to_string(),
        };
        let fired = Arc::new(AtomicU64::new(0));
        let on_miss = {
            let fired = fired.clone();
            let cb: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |_idx: u64| {
                fired.fetch_add(1, Ordering::SeqCst);
            });
            cb
        };
        let calls = Rc::new(Cell::new(0u32));
        let fill = {
            let calls = calls.clone();
            move |_url: &str, _hd: &HashMap<String, String>, _s: u64, len: u64, dest: &Path| {
                calls.set(calls.get() + 1);
                fs::write(dest, vec![3u8; len as usize]).map(|_| ()).map_err(|e| e.to_string())
            }
        };
        // Span crossing three pieces: three misses, three callbacks.
        let mut out = Vec::new();
        CacheReader::new(
            cache,
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fill,
            FILE_PIECE - 5,
            2 * FILE_PIECE + 10,
            Some(on_miss),
            None,
        )
        .read_to_end(&mut out)
        .unwrap();
        assert_eq!(calls.get(), 3);
        assert_eq!(fired.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_warm_pieces_writes_missing_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache {
            dir: dir.path().to_path_buf(),
            total: 3 * FILE_PIECE,
            ctype: "video/mp4".to_string(),
        };
        use std::cell::Cell;
        use std::rc::Rc;
        let calls = Rc::new(Cell::new(0u32));
        let fill = {
            let calls = calls.clone();
            move |_url: &str, _hd: &HashMap<String, String>, _s: u64, len: u64, dest: &Path| {
                calls.set(calls.get() + 1);
                fs::write(dest, vec![5u8; len as usize]).map(|_| ()).map_err(|e| e.to_string())
            }
        };
        let done =
            warm_pieces(&cache, "https://cdn.example/f", &HashMap::new(), &fill, &[0, FILE_PIECE]);
        assert_eq!(done, vec![0, 1]);
        assert_eq!(calls.get(), 2);
        assert!(piece_complete(&cache, 0));
        assert!(piece_complete(&cache, 1));
        assert!(!piece_complete(&cache, 2));
        // Second warm is all hits: no refetch, stops past the total.
        let done = warm_pieces(
            &cache,
            "https://cdn.example/f",
            &HashMap::new(),
            &fill,
            &[0, FILE_PIECE, 2 * FILE_PIECE, 3 * FILE_PIECE],
        );
        assert_eq!(done, vec![0, 1, 2]);
        assert_eq!(calls.get(), 3);
        assert!(piece_complete(&cache, 2));
    }

    #[test]
    fn test_cache_reader_cached_callback_fires_on_hit_and_miss() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache {
            dir: dir.path().to_path_buf(),
            total: 2 * FILE_PIECE + 10,
            ctype: "video/mp4".to_string(),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let on_cached = {
            let seen = seen.clone();
            let cb: Arc<dyn Fn(u64) + Send + Sync> =
                Arc::new(move |idx: u64| seen.lock().unwrap().push(idx));
            cb
        };
        let fill =
            move |_url: &str, _hd: &HashMap<String, String>, _s: u64, len: u64, dest: &Path| {
                fs::write(dest, vec![3u8; len as usize]).map(|_| ()).map_err(|e| e.to_string())
            };
        // Three missing pieces report once each.
        let mut out = Vec::new();
        CacheReader::new(
            cache.clone(),
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fill,
            0,
            2 * FILE_PIECE + 10,
            None,
            Some(on_cached.clone()),
        )
        .read_to_end(&mut out)
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2]);
        // Identical second pass is all hits: reports again, no refetch.
        let fail = |_url: &str, _hd: &HashMap<String, String>, _s: u64, _l: u64, _d: &Path| {
            Err("must not refetch".to_string())
        };
        let mut out2 = Vec::new();
        CacheReader::new(
            cache,
            "https://cdn.example/f".to_string(),
            HashMap::new(),
            fail,
            0,
            2 * FILE_PIECE + 10,
            None,
            Some(on_cached),
        )
        .read_to_end(&mut out2)
        .unwrap();
        assert_eq!(out2, out);
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 0, 1, 2]);
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
}
