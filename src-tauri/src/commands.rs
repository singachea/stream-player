use std::backtrace::Backtrace;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{MutexGuard, TryLockError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::download::{clamp_workers, download_stream, DEFAULT_WORKERS, MAX_WORKERS, MIN_WORKERS};
use crate::error::Error;
use crate::fetch::{
    http_get_maybe_playlist, http_headers, with_cancel, CancelCtx, MaybePlaylist,
};
use crate::hls::{
    parse_subtitle_tracks, parse_variants, ranked_variants, subtitle_label, variant_label, Variant,
};
use crate::player::{
    rc_length, rc_seek, spawn_vlc, start_hls_playback, start_http_playback, vlc_path,
    vlc_stderr_level, wait_for_vlc,
};
use crate::prefs::{
    host_of, load_prefs, normalize_referer, prefs_path, remember_prefs, save_prefs, settings_for,
    Prefs,
};
use crate::proxy::{port_of, ProxyHandle, ProxyState};
use crate::urls::{
    auto_filename, detect_kind, origin_of, parse_curl, strip_proto, Kind, ParsedCurl, UA_DEFAULT,
};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct GuiConfig {
    workdir: Option<String>,
    workers: Option<u32>,
}

pub struct AppState {
    pub primary: Mutex<Option<u32>>,
    pub extras: Mutex<Vec<u32>>,
    pub workdir: Mutex<Option<PathBuf>>,
    pub workers: Mutex<Option<usize>>,
    pub jobs: Mutex<BTreeMap<u64, Job>>,
    pub job_seq: AtomicU64,
    pub pending_capture: Mutex<Option<ParsedCurl>>,
    pub streams: TrackedMutex<BTreeMap<u64, StreamSession>>,
    pub stream_seq: AtomicU64,
    /// Cancel context of the current resolve/play op. A new op stands down
    /// the previous one; `cancel_op_cmd` trips it from the UI.
    pub op_cancel: Mutex<CancelCtx>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            primary: Mutex::new(None),
            extras: Mutex::new(Vec::new()),
            workdir: Mutex::new(None),
            workers: Mutex::new(None),
            jobs: Mutex::new(BTreeMap::new()),
            job_seq: AtomicU64::new(1),
            pending_capture: Mutex::new(None),
            streams: TrackedMutex::new(BTreeMap::new()),
            stream_seq: AtomicU64::new(1),
            op_cancel: Mutex::new(CancelCtx::new()),
        }
    }
}

/// One playing stream: VLC process plus its proxy cache. Snapshots
/// (`StreamSnap`) carry facts out from under the streams guard; the handle
/// itself is never cloned (dropping a handle clone stops its dispatch loop).
pub struct StreamSession {
    id: u64,
    url: String,
    quality: String,
    pid: Option<u32>,
    status: String,
    proxy: Option<ProxyHandle>,
    /// Local proxy URL VLC opened (`<title>.m3u8` / `<title>.mp4`).
    play_url: String,
    /// VLC subtitle args from launch, reused by seek relaunches.
    sub_args: Vec<String>,
    /// Remote-control port of this session's VLC, if allocated.
    rc_port: Option<u16>,
    /// Container page title from the extension, when provided. Names the
    /// local proxy file VLC displays.
    title: Option<String>,
    /// Staged subtitle files VLC reads by local path. Dropped when the
    /// session ends so temp dirs are cleaned up after VLC exits.
    staged_subs: Vec<std::path::PathBuf>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDto {
    pub id: u64,
    pub url: String,
    pub host: String,
    pub quality: String,
    pub status: String,
    pub title: Option<String>,
    /// Local proxy URL VLC opened.
    pub local_url: String,
    /// Loopback port VLC connects to.
    pub port: Option<u16>,
    /// Upstream fetch state: `downloading` | `buffered` | `error` | `idle`.
    pub fetch_status: String,
    /// Trailing-10s download speed in bytes/sec, when recently active.
    pub speed_bps: Option<u64>,
    /// Cumulative upstream bytes fetched for this session.
    pub fetched_bytes: u64,
    /// Latest upstream failure diagnosis, if the last result was an error.
    pub last_error: Option<String>,
    /// Sorted indexes of segments currently held in cache.
    pub cached: Vec<usize>,
    pub pid: Option<u32>,
    /// Last segment index VLC requested (playhead). Seeks move it.
    pub playhead: Option<usize>,
    /// Contiguous cached segments right after the playhead.
    pub buffered: usize,
    /// Cached/uncached flags for the next 10% window after the playhead,
    /// sampled into 20 pieces (each piece is 0.5% of the playlist).
    pub window: Vec<bool>,
    pub window_start: Option<usize>,
    pub window_len: usize,
    pub total: usize,
    pub cached_bytes: u64,
    pub served: u64,
}

/// Owned session facts safe to carry past the streams guard: proxy state by
/// Arc and base URL by value, never the handle itself.
#[derive(Clone)]
struct StreamSnap {
    id: u64,
    url: String,
    quality: String,
    status: String,
    title: Option<String>,
    play_url: String,
    pid: Option<u32>,
    proxy_state: Option<Arc<Mutex<ProxyState>>>,
    proxy_base: Option<String>,
}

impl StreamSession {
    fn snapshot(&self) -> StreamSnap {
        let (proxy_state, proxy_base) = match &self.proxy {
            Some(p) => (Some(p.state.clone()), Some(p.base.clone())),
            None => (None, None),
        };
        StreamSnap {
            id: self.id,
            url: self.url.clone(),
            quality: self.quality.clone(),
            status: self.status.clone(),
            title: self.title.clone(),
            play_url: self.play_url.clone(),
            pid: self.pid,
            proxy_state,
            proxy_base,
        }
    }
}

/// Non-blocking proxy read for UI-thread DTOs. A wedged proxy degrades to
/// a retryable miss instead of beachballing the poll: the next tick retries.
/// Contention is reported on the terminal (captured, not shown, under
/// `cargo test`), and poisoned state is recovered like the tracked lock.
fn proxy_guard(snap: &StreamSnap) -> Option<std::sync::MutexGuard<'_, ProxyState>> {
    let state = snap.proxy_state.as_ref()?;
    match state.try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => {
            eprintln!("play: proxy busy, retrying stream {}", snap.id);
            None
        }
    }
}

/// DTO for progressive-file sessions: the grid renders served spans, so
/// green means streamed to VLC before. Files stream without a disk cache.
/// One brief proxy section; never nested under streams.
fn file_session_dto(snap: &StreamSnap, now: u64) -> Option<StreamDto> {
    let st = proxy_guard(snap)?;
    let total = st.total_bytes.unwrap_or(0);
    let playhead = st.file_seek_chunk.or_else(|| {
        st.served_cur_byte
            .and_then(|off| crate::proxy::file_playhead_chunk(total, off))
    });
    let window = crate::proxy::served_cells(total, &st.served_ranges);
    let mut buffered = 0;
    while buffered < window.len() && window[buffered] {
        buffered += 1;
    }
    let cached: Vec<usize> = window
        .iter()
        .enumerate()
        .filter(|(_, &c)| c)
        .map(|(i, _)| i)
        .collect();
    let fetch_status = crate::proxy::fetch_status(
        crate::proxy::FILE_CHUNKS,
        buffered,
        crate::proxy::FILE_CHUNKS,
        st.fetch_inflight,
        st.last_ok_ms,
        st.last_error_ms,
        now,
    );
    Some(StreamDto {
        id: snap.id,
        url: snap.url.clone(),
        host: host_of(&snap.url),
        quality: snap.quality.clone(),
        status: snap.status.clone(),
        title: snap.title.clone(),
        local_url: snap.play_url.clone(),
        port: snap.proxy_base.as_deref().and_then(port_of),
        fetch_status: fetch_status.to_string(),
        speed_bps: crate::proxy::recent_speed_bps(&st.recent, now),
        fetched_bytes: st.fetched_bytes,
        last_error: st.last_error.clone(),
        cached,
        pid: snap.pid,
        playhead,
        buffered,
        window,
        window_start: Some(0),
        window_len: crate::proxy::FILE_CHUNKS,
        total: crate::proxy::FILE_CHUNKS,
        cached_bytes: st.cached_bytes,
        served: st.served_count,
    })
}

fn dto_from_snap(snap: &StreamSnap) -> Option<StreamDto> {
    let now = now_ms();
    if proxy_guard(snap).is_some_and(|st| st.source.is_some()) {
        return file_session_dto(snap, now);
    }
    let (playhead, buffered, window, window_start, window_len, total, cached_bytes, served) =
        match &snap.proxy_state {
            Some(_) => {
                let st = proxy_guard(snap)?;
                let total = st.segments.len();
                let playhead = st.served_cur;
                // Contiguous cached run ahead of the playhead. Sparse
                // entries past a gap do not count: VLC cannot play them yet.
                let start = playhead.map_or(0, |h| h + 1);
                let mut buffered = 0;
                for n in start..total {
                    if st.seg_cache.contains_key(&n) {
                        buffered += 1;
                    } else {
                        break;
                    }
                }
                // Next 10% window after the playhead, sampled into 20
                // pieces (0.5% each). A piece is filled when any segment
                // in its slice is cached. Gaps stay visible until the
                // fetcher fills them in order.
                let want = crate::proxy::readahead_window(total, start);
                let window_start = (want > 0).then_some(start);
                let mut window = Vec::new();
                for i in 0..20 {
                    let lo = start + (want * i) / 20;
                    let hi = start + (want * (i + 1)) / 20;
                    window.push(lo < hi && (lo..hi).any(|n| st.seg_cache.contains_key(&n)));
                }
                (
                    playhead,
                    buffered,
                    window,
                    window_start,
                    want,
                    total,
                    st.cached_bytes,
                    st.served_count,
                )
            }
            None => (None, 0, Vec::new(), None, 0, 0, 0, 0),
        };
    let (port, fetch_status, speed_bps, fetched_bytes, last_error, cached) =
        match &snap.proxy_state {
            Some(_) => {
                let st = proxy_guard(snap)?;
                let status = crate::proxy::fetch_status(
                    total,
                    buffered,
                    window_len,
                    st.fetch_inflight,
                    st.last_ok_ms,
                    st.last_error_ms,
                    now,
                );
                let mut cached: Vec<usize> =
                    st.seg_cache.keys().copied().collect();
                cached.sort_unstable();
                (
                    snap.proxy_base.as_deref().and_then(port_of),
                    status.to_string(),
                    crate::proxy::recent_speed_bps(&st.recent, now),
                    st.fetched_bytes,
                    st.last_error.clone(),
                    cached,
                )
            }
            None => (None, "idle".to_string(), None, 0, None, Vec::new()),
        };
    Some(StreamDto {
        id: snap.id,
        url: snap.url.clone(),
        host: host_of(&snap.url),
        quality: snap.quality.clone(),
        status: snap.status.clone(),
        title: snap.title.clone(),
        local_url: snap.play_url.clone(),
        port,
        fetch_status,
        speed_bps,
        fetched_bytes,
        last_error,
        cached,
        pid: snap.pid,
        playhead,
        buffered,
        window,
        window_start,
        window_len,
        total,
        cached_bytes,
        served,
    })
}

fn emit_stream(app: &AppHandle, dto: &StreamDto) {
    let _ = app.emit("stream-updated", dto);
}

/// Call sites that take the streams lock, so a stuck holder can be named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamsSite {
    List,
    Patch,
    SpawnInsert,
    SpawnLog,
    Reap,
    Stop,
    SeekSnapshot,
    SeekUpdate,
    SeekLog,
}

impl StreamsSite {
    const FREE: u8 = u8::MAX;

    fn id(self) -> u8 {
        self as u8
    }

    fn name(id: u8) -> &'static str {
        match id {
            x if x == Self::List as u8 => "List",
            x if x == Self::Patch as u8 => "Patch",
            x if x == Self::SpawnInsert as u8 => "SpawnInsert",
            x if x == Self::SpawnLog as u8 => "SpawnLog",
            x if x == Self::Reap as u8 => "Reap",
            x if x == Self::Stop as u8 => "Stop",
            x if x == Self::SeekSnapshot as u8 => "SeekSnapshot",
            x if x == Self::SeekUpdate as u8 => "SeekUpdate",
            x if x == Self::SeekLog as u8 => "SeekLog",
            _ => "unknown",
        }
    }
}

/// A Mutex that records who holds it. The UI thread must never wedge on a
/// backend lock again: main-thread commands use `try_lock` and back off,
/// while `lock` reports the recorded holder when it has to wait, so a
/// stuck holder names itself instead of freezing the app silently.
struct TrackedMutex<T> {
    inner: Mutex<T>,
    site: AtomicU8,
    since_ms: AtomicU64,
    /// Owning stack of the current holder, captured at acquire time so a
    /// stuck holder names its own call path even if its thread is gone.
    trace: Mutex<String>,
    waiters: AtomicU64,
}

struct TrackedGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    meta: &'a TrackedMutex<T>,
}

impl<T> Deref for TrackedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for TrackedGuard<'_, T> {
    fn drop(&mut self) {
        self.meta.site.store(StreamsSite::FREE, Ordering::SeqCst);
    }
}

impl<T> TrackedMutex<T> {
    fn new(v: T) -> Self {
        Self {
            inner: Mutex::new(v),
            site: AtomicU8::new(StreamsSite::FREE),
            since_ms: AtomicU64::new(0),
            trace: Mutex::new(String::new()),
            waiters: AtomicU64::new(0),
        }
    }

    fn record(&self, site: StreamsSite) {
        self.since_ms.store(now_ms(), Ordering::SeqCst);
        self.site.store(site.id(), Ordering::SeqCst);
        // Short private section, always taken streams-then-trace and read
        // alone in `holder_trace`, so it cannot join a lock cycle.
        // A poisoned trace is skipped, never fatal.
        if let Ok(mut t) = self.trace.lock() {
            *t = format!("{:?}", Backtrace::force_capture());
        }
    }

    /// Who claims to hold the lock, if anyone, with hold time in ms.
    pub(crate) fn holder(&self) -> Option<(&'static str, u64)> {
        let id = self.site.load(Ordering::SeqCst);
        if id == StreamsSite::FREE {
            return None;
        }
        let age = now_ms().saturating_sub(self.since_ms.load(Ordering::SeqCst));
        Some((StreamsSite::name(id), age))
    }

    pub(crate) fn holder_report(&self) -> String {
        match self.holder() {
            Some((site, age)) => format!("held by {site} for {age}ms"),
            None => "holder unknown (already released)".into(),
        }
    }

    pub(crate) fn holder_trace(&self) -> String {
        self.trace.lock().map(|t| t.clone()).unwrap_or_default()
    }

    pub(crate) fn waiter_count(&self) -> u64 {
        self.waiters.load(Ordering::SeqCst)
    }

    fn wrap<'a>(&'a self, guard: MutexGuard<'a, T>, site: StreamsSite) -> TrackedGuard<'a, T> {
        self.record(site);
        TrackedGuard { guard, meta: self }
    }

    /// Blocking acquire with progress reports: never waits silently.
    fn lock(&self, site: StreamsSite) -> TrackedGuard<'_, T> {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let start = Instant::now();
        let mut next_report = start + Duration::from_secs(2);
        loop {
            match self.inner.try_lock() {
                Ok(guard) => {
                    self.waiters.fetch_sub(1, Ordering::SeqCst);
                    return self.wrap(guard, site);
                }
                Err(TryLockError::Poisoned(e)) => {
                    // A panicked holder already dropped its guard: recover
                    // instead of wedging every future caller on its behalf.
                    self.waiters.fetch_sub(1, Ordering::SeqCst);
                    return self.wrap(e.into_inner(), site);
                }
                Err(TryLockError::WouldBlock) => {}
            }
            if Instant::now() >= next_report {
                let msg = format!(
                    "play: streams lock waited {}ms ({})",
                    start.elapsed().as_millis(),
                    self.holder_report()
                );
                eprintln!("{msg}");
                wedge_log(&msg);
                next_report = Instant::now() + Duration::from_secs(5);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Non-blocking acquire for main-thread commands: back off instead of
    /// beachballing when a worker is mid-update.
    fn try_lock(&self, site: StreamsSite) -> Option<TrackedGuard<'_, T>> {
        match self.inner.try_lock() {
            Ok(guard) => Some(self.wrap(guard, site)),
            Err(TryLockError::Poisoned(e)) => Some(self.wrap(e.into_inner(), site)),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// File holding wedge diagnostics; readable without the dev terminal.
pub(crate) fn wedge_path() -> std::path::PathBuf {
    std::env::temp_dir().join("play-wedge.log")
}

/// Append one wedge line, restarting the file past 512 KiB. Never panics,
/// never blocks on backend locks: diagnostics must not join a wedge.
pub(crate) fn wedge_log(msg: &str) {
    let path = wedge_path();
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 512 * 1024 {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = writeln!(f, "{} {msg}", now_ms());
    }
}

/// Background watchdog: while the streams map stays held, record the
/// holder's own acquire-time stack to the wedge file every 5 s.
pub(crate) fn spawn_wedge_watchdog(app: &AppHandle) {
    eprintln!("play: wedge log at {}", wedge_path().display());
    let app = app.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        let st = app.state::<AppState>();
        if st.streams.holder().is_some_and(|(_, age)| age > 15_000) {
            wedge_log(&format!(
                "WEDGE streams {} waiters={} trace:\n{}",
                st.streams.holder_report(),
                st.streams.waiter_count(),
                st.streams.holder_trace()
            ));
        }
    });
}

fn patch_stream(app: &AppHandle, id: u64, f: impl FnOnce(&mut StreamSession)) -> Option<StreamDto> {
    let st = app.state::<AppState>();
    // Brief map-only section: clone under the guard, build the DTO after it
    // drops, so a slow proxy can never wedge the streams map through here.
    let snap = {
        let mut streams = st.streams.lock(StreamsSite::Patch);
        let s = streams.get_mut(&id)?;
        f(s);
        s.snapshot()
    };
    dto_from_snap(&snap)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobDto {
    pub id: u64,
    pub url: String,
    pub host: String,
    pub dest: String,
    pub quality: String,
    pub status: String,
    pub done: usize,
    pub total: usize,
    pub bytes: u64,
    pub error: Option<String>,
    pub hint: Option<String>,
}

pub struct Job {
    id: u64,
    url: String,
    dest: String,
    quality: String,
    status: String,
    done: usize,
    total: usize,
    bytes: u64,
    error: Option<String>,
    hint: Option<String>,
    cancel: CancelCtx,
    last_emit: Instant,
}

impl Job {
    fn dto(&self) -> JobDto {
        JobDto {
            id: self.id,
            url: self.url.clone(),
            host: host_of(&self.url),
            dest: self.dest.clone(),
            quality: self.quality.clone(),
            status: self.status.clone(),
            done: self.done,
            total: self.total,
            bytes: self.bytes,
            error: self.error.clone(),
            hint: self.hint.clone(),
        }
    }

    fn is_active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "queued" | "running" | "remuxing" | "cancelling"
        )
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEvent {
    ms: u64,
    level: String,
    msg: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn log(app: &AppHandle, level: &str, msg: impl AsRef<str>) {
    emit_log(app, level, "play", None, msg);
}

pub fn ingest_capture(app: &AppHandle, capture: ParsedCurl) {
    if let Ok(mut slot) = app.state::<AppState>().pending_capture.lock() {
        *slot = Some(capture.clone());
    }
    let extra = capture
        .referer
        .as_deref()
        .map(|r| format!(" Referer={r}"))
        .unwrap_or_default();
    let cookie = if capture.cookie.as_deref().is_some_and(|c| !c.is_empty()) {
        " cookie=yes"
    } else {
        ""
    };
    let subs = capture
        .subtitles
        .as_ref()
        .map(|s| format!(" subs={}", s.len()))
        .unwrap_or_default();
    let title = capture
        .title
        .as_deref()
        .map(|t| format!(" title={t:?}"))
        .unwrap_or_default();
    log(
        app,
        "info",
        format!("capture {}{extra}{cookie}{subs}{title}", capture.url),
    );
    let _ = app.emit("play-open", &capture);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn log_vlc(app: &AppHandle, level: &str, pid: u32, msg: impl AsRef<str>) {
    emit_log(app, level, "vlc", Some(pid as u64), msg);
}

fn log_job(app: &AppHandle, level: &str, id: u64, msg: impl AsRef<str>) {
    emit_log(app, level, "job", Some(id), msg);
}

fn emit_log(app: &AppHandle, level: &str, source: &str, id: Option<u64>, msg: impl AsRef<str>) {
    let _ = app.emit(
        "play-log",
        LogEvent {
            ms: now_ms(),
            level: level.to_string(),
            msg: msg.as_ref().to_string(),
            source: source.to_string(),
            id,
        },
    );
}

/// Flood gate for machine-generated log lines (VLC stderr). VLC repeats
/// the same decoder error hundreds of times per second when it struggles
/// — notably right after a seek into an uncached region — and every line
/// would otherwise become its own UI event, freezing the log view.
/// At most BUDGET lines pass per WINDOW; the rest are counted and reported
/// as a single notice.
#[derive(Debug)]
struct LogGate {
    window_start: Instant,
    allowed: u32,
    suppressed: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum GateAction {
    Emit,
    EmitWithNotice(u32),
    Suppress,
}

impl LogGate {
    const BUDGET: u32 = 20;
    const WINDOW: Duration = Duration::from_secs(1);

    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            allowed: 0,
            suppressed: 0,
        }
    }

    fn check(&mut self, now: Instant) -> GateAction {
        if now.duration_since(self.window_start) >= Self::WINDOW {
            let n = self.suppressed;
            self.window_start = now;
            self.allowed = 0;
            self.suppressed = 0;
            if n > 0 {
                // The boundary line still counts against the new window.
                self.allowed = 1;
                return GateAction::EmitWithNotice(n);
            }
        }
        if self.allowed < Self::BUDGET {
            self.allowed += 1;
            GateAction::Emit
        } else {
            self.suppressed += 1;
            GateAction::Suppress
        }
    }

    fn suppressed(&self) -> u32 {
        self.suppressed
    }
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

fn config_file(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("config.json")
}

fn load_gui_config(app: &AppHandle) -> GuiConfig {
    let p = config_file(app);
    fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_gui_config(app: &AppHandle, cfg: &GuiConfig) {
    let p = config_file(app);
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(t) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(p, format!("{t}\n"));
    }
}

fn default_workdir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn patch_gui_config(app: &AppHandle, f: impl FnOnce(&mut GuiConfig)) {
    let mut cfg = load_gui_config(app);
    f(&mut cfg);
    save_gui_config(app, &cfg);
}

fn workers(app: &AppHandle, state: &AppState) -> usize {
    let mut slot = state.workers.lock().unwrap();
    if let Some(n) = *slot {
        return n;
    }
    let n = load_gui_config(app)
        .workers
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_WORKERS);
    let n = clamp_workers(n);
    *slot = Some(n);
    n
}

fn workdir(app: &AppHandle, state: &AppState) -> PathBuf {
    let mut slot = state.workdir.lock().unwrap();
    if let Some(p) = slot.as_ref() {
        return p.clone();
    }
    let cfg = load_gui_config(app);
    let p = cfg
        .workdir
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(default_workdir);
    *slot = Some(p.clone());
    p
}

fn err_str(e: Error) -> String {
    e.to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantDto {
    pub label: String,
    pub quality: String,
    pub height: u32,
    pub bandwidth: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleDto {
    pub label: String,
    pub name: String,
    pub language: String,
    pub default: bool,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ResolveResponse {
    Ok {
        kind: String,
        host: String,
        variants: Vec<VariantDto>,
        subtitles: Vec<SubtitleDto>,
        referer: String,
        origin: String,
    },
    NeedReferer {
        url: String,
        hint: String,
        host: String,
    },
}

fn subtitles_dto(tracks: &[crate::hls::SubtitleTrack]) -> Vec<SubtitleDto> {
    tracks
        .iter()
        .map(|t| SubtitleDto {
            label: subtitle_label(t),
            name: t.name.clone(),
            language: t.language.clone(),
            default: t.default,
        })
        .collect()
}

fn variants_dto(v: &[Variant]) -> Vec<VariantDto> {
    ranked_variants(v)
        .into_iter()
        .map(|x| {
            let label = variant_label(&x);
            VariantDto {
                quality: label.clone(),
                label,
                height: x.height,
                bandwidth: x.bandwidth,
            }
        })
        .collect()
}

fn load_headers(
    url: &str,
    dir: &Path,
    referer: Option<&str>,
    origin: Option<&str>,
    user_agent: Option<&str>,
    cookie: Option<&str>,
    cookie_host: Option<&str>,
) -> Result<(HashMap<String, String>, Prefs, PathBuf), Error> {
    let path = prefs_path(dir);
    let prefs = load_prefs(&path)?;
    let ua = user_agent.filter(|s| !s.is_empty()).unwrap_or(UA_DEFAULT);
    let referer = referer.and_then(normalize_referer);
    let origin = origin
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let cfg = settings_for(url, &prefs, referer.as_deref(), origin.as_deref(), ua)?;
    let mut headers = http_headers(&cfg["user_agent"], &cfg["referer"], &cfg["origin"]);
    if let Some(c) = cookie.map(str::trim).filter(|s| !s.is_empty()) {
        headers.insert("Cookie".into(), c.to_string());
        // Exact-request cookies (read from the player's own request
        // headers) must be replayed even cross-site; the cookie jar
        // fallback stays same-site only.
        if let Some(h) = cookie_host.map(str::trim).filter(|s| !s.is_empty()) {
            headers.insert("X-Play-Cookie-Host".into(), h.to_lowercase());
        }
    }
    Ok((headers, prefs, path))
}

fn map_403(url: &str, e: Error) -> Result<ResolveResponse, String> {
    match e {
        Error::Http {
            code: 403,
            url: u,
            hint,
            ..
        } if hint.contains("--referer") => Ok(ResolveResponse::NeedReferer {
            url: u,
            hint,
            host: host_of(url),
        }),
        other => Err(err_str(other)),
    }
}

#[tauri::command]
pub fn get_workdir_cmd(app: AppHandle, state: State<AppState>) -> String {
    workdir(&app, &state).to_string_lossy().into_owned()
}

#[tauri::command]
pub fn set_workdir_cmd(
    app: AppHandle,
    state: State<AppState>,
    path: String,
) -> Result<String, String> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("play: not a directory: {path}"));
    }
    *state.workdir.lock().unwrap() = Some(p.clone());
    patch_gui_config(&app, |cfg| {
        cfg.workdir = Some(p.to_string_lossy().into_owned());
    });
    Ok(p.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn list_hosts_cmd(app: AppHandle, state: State<AppState>) -> Result<Vec<HostRow>, String> {
    let dir = workdir(&app, &state);
    let prefs = load_prefs(&prefs_path(&dir)).map_err(err_str)?;
    Ok(prefs
        .into_iter()
        .map(|(host, e)| HostRow {
            host,
            referer: e.get("referer").cloned().unwrap_or_default(),
            origin: e.get("origin").cloned().unwrap_or_default(),
        })
        .collect())
}

#[derive(Serialize, Deserialize)]
pub struct HostRow {
    pub host: String,
    pub referer: String,
    pub origin: String,
}

#[tauri::command]
pub fn save_host_cmd(
    app: AppHandle,
    state: State<AppState>,
    url: String,
    referer: String,
    origin: Option<String>,
) -> Result<Vec<HostRow>, String> {
    let dir = workdir(&app, &state);
    let path = prefs_path(&dir);
    let mut prefs = load_prefs(&path).map_err(err_str)?;
    let orig = match origin.filter(|s| !s.is_empty()) {
        Some(o) => o,
        None => origin_of(&referer).map_err(err_str)?,
    };
    remember_prefs(
        &mut prefs,
        &url,
        &BTreeMap::from([("referer".into(), referer), ("origin".into(), orig)]),
    );
    save_prefs(&path, &prefs).map_err(err_str)?;
    list_hosts_cmd(app, state)
}

#[tauri::command]
pub fn delete_host_cmd(
    app: AppHandle,
    state: State<AppState>,
    host: String,
) -> Result<Vec<HostRow>, String> {
    let dir = workdir(&app, &state);
    let path = prefs_path(&dir);
    let mut prefs = load_prefs(&path).map_err(err_str)?;
    prefs.remove(&host);
    save_prefs(&path, &prefs).map_err(err_str)?;
    list_hosts_cmd(app, state)
}

/// Start a UI-cancellable op (resolve/play): stand down any previous op,
/// then hand out a fresh context the new op installs with `with_cancel`.
/// Fetch stages already honor the thread-local context (tracked curl PIDs
/// die on request), and the frontend discards late results by generation.
fn begin_op(state: &AppState) -> CancelCtx {
    let mut slot = state.op_cancel.lock().unwrap();
    slot.request();
    *slot = CancelCtx::new();
    slot.clone()
}

/// Trip the current resolve/play op from the UI cancel button. In-flight
/// curls die at once; staged work aborts at its next cancel check.
#[tauri::command]
pub fn cancel_op_cmd(state: State<'_, AppState>) -> Result<(), String> {
    state.op_cancel.lock().unwrap().request();
    Ok(())
}

#[tauri::command(async)]
pub async fn resolve_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    referer: Option<String>,
    origin: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    cookie_host: Option<String>,
) -> Result<ResolveResponse, String> {
    let app2 = app.clone();
    let dir = workdir(&app, &state);
    let ctx = begin_op(&state);
    tauri::async_runtime::spawn_blocking(move || {
        with_cancel(Some(&ctx), || {
            resolve_blocking(&app2, &dir, &url, referer, origin, user_agent, cookie, cookie_host)
        })
    })
    .await
    .map_err(|e| format!("play: resolve task failed: {e}"))?
}

fn resolve_blocking(
    app: &AppHandle,
    dir: &std::path::Path,
    url: &str,
    referer: Option<String>,
    origin: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    cookie_host: Option<String>,
) -> Result<ResolveResponse, String> {
    let (headers, _prefs, _path) = load_headers(
        url,
        dir,
        referer.as_deref(),
        origin.as_deref(),
        user_agent.as_deref(),
        cookie.as_deref(),
        cookie_host.as_deref(),
    )
    .map_err(err_str)?;
    let kind = detect_kind(url);
    let host = host_of(url);
    let referer_s = headers.get("Referer").cloned().unwrap_or_default();
    let origin_s = headers.get("Origin").cloned().unwrap_or_default();
    log(
        app,
        "info",
        format!(
            "resolve {} {url}  Referer={}  Origin={}{}",
            kind.as_str(),
            referer_s,
            origin_s,
            if headers.contains_key("Cookie") {
                "  cookie=yes"
            } else {
                ""
            }
        ),
    );
    if kind != Kind::Hls {
        log(
            app,
            "debug",
            format!("not HLS ({}), no variant list", kind.as_str()),
        );
        return Ok(ResolveResponse::Ok {
            kind: kind.as_str().into(),
            host,
            variants: vec![],
            subtitles: vec![],
            referer: referer_s,
            origin: origin_s,
        });
    }
    let fetch_url = strip_proto(url);
    log(app, "debug", format!("GET {fetch_url}"));
    let got = http_get_maybe_playlist(fetch_url, &headers);
    match got {
        Ok(MaybePlaylist::Progressive { url: final_url }) => {
            log(
                app,
                "info",
                format!("not a playlist, treating as a file ({final_url})"),
            );
            Ok(ResolveResponse::Ok {
                kind: "http".into(),
                host,
                variants: vec![],
                subtitles: vec![],
                referer: referer_s,
                origin: origin_s,
            })
        }
        Ok(MaybePlaylist::Playlist {
            url: final_url,
            body,
        }) => {
            log(
                app,
                "debug",
                format!("GET {} → {} bytes", final_url, body.len()),
            );
            let text = String::from_utf8_lossy(&body);
            let variants = if text.contains("#EXT-X-STREAM-INF") {
                variants_dto(&parse_variants(&text, &final_url))
            } else {
                vec![VariantDto {
                    label: "best".into(),
                    quality: "best".into(),
                    height: 0,
                    bandwidth: 0,
                }]
            };
            let labels: Vec<_> = variants.iter().map(|v| v.label.as_str()).collect();
            log(app, "info", format!("qualities: {}", labels.join(", ")));
            let subtitles = parse_subtitle_tracks(&text, &final_url);
            if !subtitles.is_empty() {
                let names: Vec<_> = subtitles.iter().map(|t| subtitle_label(t)).collect();
                log(app, "info", format!("subtitles: {}", names.join(", ")));
            }
            Ok(ResolveResponse::Ok {
                kind: "hls".into(),
                host,
                variants,
                subtitles: subtitles_dto(&subtitles),
                referer: headers.get("Referer").cloned().unwrap_or(referer_s),
                origin: headers.get("Origin").cloned().unwrap_or(origin_s),
            })
        }
        Err(e) => {
            log(app, "error", e.to_string());
            map_403(url, e)
        }
    }
}

#[tauri::command(async)]
pub async fn play_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    quality: Option<String>,
    referer: Option<String>,
    origin: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    cookie_host: Option<String>,
    extra: Option<bool>,
    subtitles: Option<Vec<String>>,
    subtitle: Option<String>,
    prefetch_count: Option<usize>,
    title: Option<String>,
) -> Result<ResolveResponse, String> {
    let app2 = app.clone();
    let dir = workdir(&app, &state);
    let ctx = begin_op(&state);
    tauri::async_runtime::spawn_blocking(move || {
        with_cancel(Some(&ctx), || {
            play_blocking(
                &app2,
                &dir,
                &url,
                quality,
                referer,
                origin,
                user_agent,
                cookie,
                cookie_host,
                extra,
                subtitles,
                subtitle,
                prefetch_count,
                title,
            )
        })
    })
    .await
    .map_err(|e| format!("play: play task failed: {e}"))?
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayProgress {
    stage: String,
    stage_key: String,
    done: usize,
    total: usize,
}

fn emit_progress(
    app: &AppHandle,
    stage_key: &str,
    label: &str,
    done: usize,
    total: usize,
) {
    let _ = app.emit(
        "play-progress",
        PlayProgress {
            stage: label.to_string(),
            stage_key: stage_key.to_string(),
            done,
            total,
        },
    );
}

fn emit_stage(
    app: &AppHandle,
    stage: &str,
    done: usize,
    total: usize,
    detail: Option<String>,
) {
    let total = total.max(1);
    let done = done.min(total);
    match stage {
        "master" => emit_progress(app, "playlist", "Fetching master playlist", done, total),
        "variant" => emit_progress(app, "playlist", "Fetching quality playlist", done, total),
        "playlist" => emit_progress(app, "playlist", "Fetching playlist", done, total),
        "sniff" => emit_progress(app, "sniff", "Sniffing first segment", done, total),
        "proxy" => emit_progress(app, "proxy", "Starting local proxy", done, total),
        "subtitles" => {
            let label = match detail {
                Some(name) if !name.is_empty() => format!("Fetching subtitle {name}"),
                _ if total > 1 => format!("Fetching subtitles {done}/{total}"),
                _ => "Fetching subtitles".into(),
            };
            emit_progress(app, "subtitles", &label, done, total);
        }
        "launch" => emit_progress(app, "launch", "Launching VLC", done, total),
        "buffer" => {
            let label = match detail {
                Some(d) if !d.is_empty() => format!("Buffering {d}"),
                _ if total > 1 => format!("Buffering {done}/{total}"),
                _ => "Buffering".into(),
            };
            emit_progress(app, "buffer", &label, done, total);
        }
        other => emit_progress(app, "playlist", other, done, total),
    }
}

#[allow(clippy::too_many_arguments)]
fn play_blocking(
    app: &AppHandle,
    dir: &std::path::Path,
    url: &str,
    quality: Option<String>,
    referer: Option<String>,
    origin: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    cookie_host: Option<String>,
    extra: Option<bool>,
    subtitles: Option<Vec<String>>,
    subtitle: Option<String>,
    prefetch_count: Option<usize>,
    title: Option<String>,
) -> Result<ResolveResponse, String> {
    let extra = extra.unwrap_or(false);
    let prefetch_count = prefetch_count.unwrap_or(3).min(20);
    let extra_subs: Vec<String> = subtitles
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .collect();
    let state = app.state::<AppState>();
    let state = state.inner();
    let (mut headers, _prefs, _path) = load_headers(
        url,
        &dir,
        referer.as_deref(),
        origin.as_deref(),
        user_agent.as_deref(),
        cookie.as_deref(),
        cookie_host.as_deref(),
    )
    .map_err(err_str)?;
    let kind = detect_kind(url);
    let vlc = vlc_path();
    let q = quality.as_deref().unwrap_or("best");
    log(
        app,
        "info",
        format!(
            "play {} {url} quality={q}{}{}  Referer={}  Origin={}{}",
            kind.as_str(),
            if extra { " (another instance)" } else { "" },
            title
                .as_deref()
                .map(|t| format!(" title={t:?}"))
                .unwrap_or_default(),
            headers.get("Referer").cloned().unwrap_or_default(),
            headers.get("Origin").cloned().unwrap_or_default(),
            if headers.contains_key("Cookie") {
                "  cookie=yes"
            } else {
                ""
            }
        ),
    );
    match kind {
        Kind::Http | Kind::Dash => {
            let referer = headers.get("Referer").cloned().unwrap_or_default();
            let origin = headers.get("Origin").cloned().unwrap_or_default();
            let app_stage = app.clone();
            let mut on_stage = move |stage: &str, done: usize, total: usize, detail: Option<String>| {
                emit_stage(&app_stage, stage, done, total, detail);
            };
            let pb = start_http_playback(
                url,
                &headers,
                &vlc,
                false,
                &extra_subs,
                Some(&mut on_stage),
                title.as_deref(),
            )
            .map_err(|e| {
                log(app, "error", e.to_string());
                err_str(e)
            })?;
            let local = pb.play_url.clone();
            let pid = pb.child.id();
            log_vlc(app, "debug", pid, format!("local file {local}"));
            if !pb.hint.is_empty() {
                log_vlc(app, "debug", pid, pb.hint.clone());
            }
            let mut pb = pb;
            let staged = pb.take_staged_subs();
            spawn_session(
                app,
                state,
                pb.child,
                Some(pb.proxy),
                extra,
                url,
                q,
                staged,
                title.clone(),
                local,
                pb.sub_args,
                pb.rc_port,
            );
            Ok(ResolveResponse::Ok {
                kind: kind.as_str().into(),
                host: host_of(url),
                variants: vec![],
                subtitles: vec![],
                referer,
                origin,
            })
        }
        Kind::Hls => {
            let app_stage = app.clone();
            let mut on_stage = move |stage: &str, done: usize, total: usize, detail: Option<String>| {
                emit_stage(&app_stage, stage, done, total, detail);
            };
            match start_hls_playback::<
                fn(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>>,
            >(
                strip_proto(url),
                &mut headers,
                quality.as_deref(),
                &vlc,
                false,
                false,
                &extra_subs,
                subtitle.as_deref(),
                None,
                Some(&mut on_stage),
                prefetch_count,
                title.as_deref(),
            ) {
                Ok(pb) => {
                    let referer = headers.get("Referer").cloned().unwrap_or_default();
                    let origin = headers.get("Origin").cloned().unwrap_or_default();
                    let local = pb.play_url.clone();
                    let pid = pb.child.id();
                    log_vlc(app, "debug", pid, format!("local playlist {local}"));
                    if !pb.hint.is_empty() {
                        log_vlc(app, "debug", pid, pb.hint.clone());
                    }
                    let mut pb = pb;
                    let staged = pb.take_staged_subs();
                    spawn_session(
                        app,
                        state,
                        pb.child,
                        Some(pb.proxy),
                        extra,
                        url,
                        q,
                        staged,
                        title.clone(),
                        local,
                        pb.sub_args,
                        pb.rc_port,
                    );
                    Ok(ResolveResponse::Ok {
                        kind: "hls".into(),
                        host: host_of(url),
                        variants: vec![],
                        subtitles: vec![],
                        referer,
                        origin,
                    })
                }
                Err(Error::Progressive { .. }) => {
                    log(
                        app,
                        "info",
                        format!("not a playlist, playing as a file ({url})"),
                    );
                    let referer = headers.get("Referer").cloned().unwrap_or_default();
                    let origin = headers.get("Origin").cloned().unwrap_or_default();
                    let pb = start_http_playback(
                        url,
                        &headers,
                        &vlc,
                        false,
                        &extra_subs,
                        Some(&mut on_stage),
                        title.as_deref(),
                    )
                    .map_err(|e| {
                        log(app, "error", e.to_string());
                        err_str(e)
                    })?;
                    let local = pb.play_url.clone();
                    let pid = pb.child.id();
                    log_vlc(app, "debug", pid, format!("local file {local}"));
                    let mut pb = pb;
                    let staged = pb.take_staged_subs();
                    spawn_session(
                        app,
                        state,
                        pb.child,
                        Some(pb.proxy),
                        extra,
                        url,
                        q,
                        staged,
                        title.clone(),
                        local,
                        pb.sub_args,
                        pb.rc_port,
                    );
                    Ok(ResolveResponse::Ok {
                        kind: "http".into(),
                        host: host_of(url),
                        variants: vec![],
                        subtitles: vec![],
                        referer,
                        origin,
                    })
                }
                Err(e) => {
                    log(app, "error", e.to_string());
                    map_403(url, e)
                }
            }
        }
    }
}

fn remove_staged_subs(paths: Vec<std::path::PathBuf>) {
    for path in paths {
        let dir = path.parent().map(std::path::Path::to_path_buf);
        let _ = std::fs::remove_file(&path);
        if let Some(dir) = dir {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn spawn_session(
    app: &AppHandle,
    state: &AppState,
    child: Child,
    proxy: Option<ProxyHandle>,
    extra: bool,
    url: &str,
    quality: &str,
    staged_subs: Vec<std::path::PathBuf>,
    title: Option<String>,
    play_url: String,
    sub_args: Vec<String>,
    rc_port: Option<u16>,
) {
    let pid = child.id();
    if extra {
        state.extras.lock().unwrap().push(pid);
    } else if let Some(old) = state.primary.lock().unwrap().replace(pid) {
        log_vlc(
            app,
            "debug",
            old,
            format!("stopping previous VLC pid={old}"),
        );
        kill_pid(old);
    }
    let stream_id = state.stream_seq.fetch_add(1, Ordering::SeqCst);
    state.streams.lock(StreamsSite::SpawnInsert).insert(
        stream_id,
        StreamSession {
            id: stream_id,
            url: url.to_string(),
            quality: quality.to_string(),
            pid: Some(pid),
            status: "playing".into(),
            proxy,
            title: title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
            play_url,
            sub_args,
            rc_port,
            staged_subs,
        },
    );
    if let Some(dto) = patch_stream(app, stream_id, |_| {}) {
        emit_stream(app, &dto);
    }
    // Snapshot the proxy first: the pending-log flush emits to the UI and
    // must never run under the streams guard.
    // Snapshot proxy state by Arc: cloning the handle would stop its
    // dispatch loop when the clone drops.
    let proxy_state = {
        let streams = state.streams.lock(StreamsSite::SpawnLog);
        streams
            .get(&stream_id)
            .and_then(|s| s.proxy.as_ref().map(|p| p.state.clone()))
    };
    if let Some(ref proxy_state) = proxy_state {
        let app_log = app.clone();
        let cb: Arc<dyn Fn(&str, String) + Send + Sync> = Arc::new(move |level, msg| {
            log_vlc(&app_log, level, pid, msg);
        });
        let pending = {
            let mut st = proxy_state.lock().unwrap();
            st.on_log = Some(cb.clone());
            std::mem::take(&mut st.pending_log)
        };
        for (level, msg) in pending {
            cb(&level, msg);
        }
    }
    log_vlc(
        app,
        "info",
        pid,
        format!(
            "VLC pid={pid}{}",
            if extra { " extra instance" } else { "" }
        ),
    );
    watch_child(app.clone(), stream_id, pid, child);
}

/// Drain one VLC child's stderr into the log, then reap it. When a seek
/// replaced the session pid first, the old child's exit tears nothing down.
fn watch_child(app: AppHandle, stream_id: u64, pid: u32, mut child: Child) {
    if let Some(stderr) = child.stderr.take() {
        let app_err = app.clone();
        thread::spawn(move || {
            let mut gate = LogGate::new(Instant::now());
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let Some(level) = vlc_stderr_level(&line) else {
                    continue;
                };
                match gate.check(Instant::now()) {
                    GateAction::Emit => log_vlc(&app_err, level, pid, line),
                    GateAction::EmitWithNotice(n) => {
                        log_vlc(
                            &app_err,
                            "warn",
                            pid,
                            format!("…{n} repeated VLC lines suppressed"),
                        );
                        log_vlc(&app_err, level, pid, line);
                    }
                    GateAction::Suppress => {}
                }
            }
            if gate.suppressed() > 0 {
                log_vlc(
                    &app_err,
                    "warn",
                    pid,
                    format!("…{} repeated VLC lines suppressed", gate.suppressed()),
                );
            }
        });
    }
    thread::spawn(move || {
        let code = wait_for_vlc(&mut child, false);
        let _ = child.kill();
        let st = app.state::<AppState>();
        if st.streams.lock(StreamsSite::Reap).get(&stream_id).and_then(|s| s.pid) != Some(pid) {
            // Superseded by a seek relaunch: the session lives on.
            return;
        }
        {
            let mut primary = st.primary.lock().unwrap();
            if *primary == Some(pid) {
                *primary = None;
            }
        }
        st.extras.lock().unwrap().retain(|p| *p != pid);
        let removed = st.streams.lock(StreamsSite::Reap).remove(&stream_id);
        if let Some(s) = removed {
            // Temp subtitle dirs die with the session so VLC can read them
            // for the whole playback and nothing lingers after.
            remove_staged_subs(s.staged_subs);
            let _ = app.emit("stream-removed", stream_id);
        }
        log_vlc(&app, "debug", pid, format!("VLC pid={pid} exited {code}"));
        let _ = app.emit("player-stopped", pid);
    });
}

#[tauri::command]
pub fn stop_cmd(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if let Some(pid) = state.primary.lock().unwrap().take() {
        log_vlc(&app, "info", pid, format!("stop VLC pid={pid}"));
        kill_pid(pid);
    }
    Ok(())
}

#[tauri::command]
pub fn list_streams_cmd(state: State<AppState>) -> Result<Vec<StreamDto>, String> {
    // try_lock: this runs on the UI thread (poll + mount), so back off
    // instead of beachballing when a worker is mid-update.
    // Brief map-only section: snapshot sessions, assemble DTOs (which take
    // the proxy lock) after the guard drops. Never nests locks.
    let snaps: Vec<StreamSnap> = {
        let streams = state.streams.try_lock(StreamsSite::List).ok_or_else(|| {
            format!(
                "play: streams busy ({}), retrying",
                state.streams.holder_report()
            )
        })?;
        streams.values().map(|s| s.snapshot()).collect()
    };
    Ok(snaps.iter().filter_map(dto_from_snap).collect())
}

#[tauri::command]
pub fn stop_stream_cmd(app: AppHandle, id: u64) -> Result<(), String> {
    // try_lock: UI-thread command; back off with a retryable error instead
    // of wedging when a worker is mid-update.
    let st = app.state::<AppState>();
    let removed = st
        .streams
        .try_lock(StreamsSite::Stop)
        .ok_or_else(|| format!("play: streams busy ({}), retry", st.streams.holder_report()))?
        .remove(&id);
    let Some(s) = removed else {
        return Err(format!("play: no stream {id}"));
    };
    if let Some(pid) = s.pid {
        log_vlc(&app, "info", pid, format!("stop stream {id} pid={pid}"));
        kill_pid(pid);
    }
    {
        let mut primary = st.primary.lock().unwrap();
        if *primary == s.pid {
            *primary = None;
        }
    }
    if let Some(pid) = s.pid {
        st.extras.lock().unwrap().retain(|p| *p != pid);
    }
    remove_staged_subs(s.staged_subs);
    let _ = app.emit("stream-removed", id);
    Ok(())
}

#[tauri::command]
pub fn stream_stats_cmd(state: State<AppState>) -> Result<Vec<StreamDto>, String> {
    list_streams_cmd(state)
}

/// Jump VLC to segment `seg` without interrupting playback: an absolute
/// `seek` on the instance's remote-control channel, positioned from the
/// summed `#EXTINF` durations before the segment. When the control channel
/// is unavailable, falls back to relaunching VLC on the same local URL
/// with `?from=seg` (the proxy serves it trimmed). Either way the session
/// and its cache survive.
///
/// Async with blocking work off the main thread: the control-channel
/// retries alone can take seconds, which would beachball the UI as a sync
/// command.
#[tauri::command(async)]
pub async fn seek_stream_cmd(app: AppHandle, id: u64, seg: usize) -> Result<StreamDto, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || seek_blocking(&app2, id, seg))
        .await
        .map_err(|e| format!("play: seek task failed: {e}"))?
}

fn seek_blocking(app: &AppHandle, id: u64, seg: usize) -> Result<StreamDto, String> {
    let st = app.state::<AppState>();
    // Flat snapshot: map fields first, proxy state after the guard drops.
    // No streams section below touches the proxy lock anymore.
    // Snapshot proxy state by Arc, never the handle: dropping a handle
    // clone stops its dispatch loop.
    let (play_url, sub_args, old_pid, rc_port, proxy_state) = {
        let streams = st.streams.lock(StreamsSite::SeekSnapshot);
        let s = streams
            .get(&id)
            .ok_or_else(|| format!("play: no stream {id}"))?;
        (
            s.play_url.clone(),
            s.sub_args.clone(),
            s.pid,
            s.rc_port,
            s.proxy.as_ref().map(|p| p.state.clone()),
        )
    };
    let proxy_state = proxy_state
        .as_ref()
        .ok_or_else(|| format!("play: stream {id} has no proxy"))?;
    let (is_file, playlist) = {
        let pst = proxy_state
            .try_lock()
            .map_err(|_| format!("play: proxy busy, retrying seek {id}"))?;
        if pst.source.is_some() {
            // Progressive file: fixed virtual chunks, no playlist math.
            if seg >= crate::proxy::FILE_CHUNKS {
                return Err(format!(
                    "play: chunk {seg} out of range (total {})",
                    crate::proxy::FILE_CHUNKS
                ));
            }
            (true, Vec::new())
        } else {
            if seg >= pst.segments.len() {
                return Err(format!(
                    "play: segment {seg} out of range (total {})",
                    pst.segments.len()
                ));
            }
            (false, pst.playlist.clone())
        }
    };
    if let Some(port) = rc_port {
        // File sessions seek by time fraction: chunk N of 20 across the
        // RC-reported duration. HLS sums EXTINF durations instead.
        let secs = if is_file {
            let dur = rc_length(port).map_err(err_str)?;
            seg as u64 * dur / crate::proxy::FILE_CHUNKS as u64
        } else {
            crate::proxy::seg_offset_secs(&playlist, seg) as u64
        };
        match rc_seek(port, secs) {
            Ok(()) => {
                // Stand down any stale fill window at once; the new window
                // starts filling on the next serve. Show the target until
                // serving confirms arrival. Proxy-only section (the map
                // lookup updated nothing): no streams guard, so a slow
                // proxy cannot wedge the map from the seek path. File
                // sessions record the sought chunk itself: the seek knows
                // its fraction exactly, serve traffic only approximates it.
                if is_file {
                    {
                        let mut pst = proxy_state
                            .try_lock()
                            .map_err(|_| format!("play: proxy busy, retrying seek {id}"))?;
                        pst.file_seek_chunk = Some(seg);
                    }
                } else {
                    let mut pst = proxy_state
                        .try_lock()
                        .map_err(|_| format!("play: proxy busy, retrying seek {id}"))?;
                    pst.fetch_epoch += 1;
                    pst.served_cur = seg.checked_sub(1);
                }
                log_vlc(&app, "info", old_pid.unwrap_or(0), format!("seek stream {id} → seg {seg} ({secs}s)"));
                let dto = patch_stream(&app, id, |_| {})
                    .ok_or_else(|| format!("play: no stream {id}"))?;
                emit_stream(&app, &dto);
                return Ok(dto);
            }
            Err(e) => {
                log_vlc(
                    &app,
                    "warn",
                    old_pid.unwrap_or(0),
                    format!("seek stream {id}: rc unavailable ({e}), relaunching"),
                );
            }
        }
    }
    // Progressive files have no relaunch seek target: restarting at 0
    // would lie about position, so a missing control channel is an error.
    if is_file {
        return Err(format!(
            "play: seek stream {id}: remote control unavailable"
        ));
    }
    let sep = if play_url.contains('?') { '&' } else { '?' };
    let target = format!("{play_url}{sep}from={seg}");
    let mut args = sub_args;
    args.push(target.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    // Spawn first: if VLC fails to launch, the old instance keeps playing.
    let child = spawn_vlc(&vlc_path(), &arg_refs).map_err(err_str)?;
    let pid = child.id();
    log_vlc(&app, "info", pid, format!("seek stream {id} → seg {seg}"));
    // Proxy first, map second: flat sections, never nested. Show the
    // target immediately; serving updates it as VLC arrives.
    {
        let mut pst = proxy_state
            .try_lock()
            .map_err(|_| format!("play: proxy busy, retrying seek {id}"))?;
        pst.served_cur = seg.checked_sub(1);
    }
    {
        let mut streams = st.streams.lock(StreamsSite::SeekUpdate);
        let s = streams
            .get_mut(&id)
            .ok_or_else(|| format!("play: no stream {id}"))?;
        s.pid = Some(pid);
    }
    if let Some(old) = old_pid {
        kill_pid(old);
        {
            let mut primary = st.primary.lock().unwrap();
            if *primary == Some(old) {
                *primary = Some(pid);
            }
        }
        if let Some(slot) = st.extras.lock().unwrap().iter_mut().find(|p| **p == old) {
            *slot = pid;
        }
    }
    // Snapshot proxy was taken above: install the log hook with no streams
    // section at all.
    {
        let app_log = app.clone();
        let cb: Arc<dyn Fn(&str, String) + Send + Sync> = Arc::new(move |level, msg| {
            log_vlc(&app_log, level, pid, msg);
        });
        proxy_state.lock().unwrap().on_log = Some(cb);
    }
    let dto = patch_stream(&app, id, |_| {})
        .ok_or_else(|| format!("play: no stream {id}"))?;
    emit_stream(&app, &dto);
    watch_child(app.clone(), id, pid, child);
    Ok(dto)
}

#[tauri::command]
pub fn read_clipboard_cmd() -> Result<String, String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_text())
        .map_err(|e| format!("play: clipboard: {e}"))
}

#[tauri::command]
pub fn write_clipboard_cmd(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| format!("play: clipboard: {e}"))
}

#[tauri::command]
pub fn parse_curl_cmd(text: String) -> Option<ParsedCurl> {
    parse_curl(&text)
}

#[tauri::command]
pub fn take_capture_cmd(state: State<AppState>) -> Option<ParsedCurl> {
    state
        .pending_capture
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

fn emit_job(app: &AppHandle, job: &JobDto) {
    let _ = app.emit("job-updated", job);
}

fn patch_job(app: &AppHandle, id: u64, f: impl FnOnce(&mut Job)) -> Option<JobDto> {
    let st = app.state::<AppState>();
    let mut jobs = st.jobs.lock().unwrap();
    let job = jobs.get_mut(&id)?;
    f(job);
    Some(job.dto())
}

fn dest_in_use(jobs: &BTreeMap<u64, Job>, dest: &str, except: Option<u64>) -> bool {
    jobs.values()
        .any(|j| j.is_active() && j.dest == dest && except != Some(j.id))
}

fn spawn_job_thread(
    app: AppHandle,
    id: u64,
    url: String,
    dest: String,
    quality: Option<String>,
    mut headers: HashMap<String, String>,
    cancel: CancelCtx,
    workers: usize,
) {
    thread::spawn(move || {
        if cancel.is_cancelled() {
            if let Some(dto) = patch_job(&app, id, |j| {
                j.status = "cancelled".into();
            }) {
                emit_job(&app, &dto);
            }
            return;
        }
        if let Some(dto) = patch_job(&app, id, |j| {
            if j.status == "cancelling" || cancel.is_cancelled() {
                j.status = "cancelled".into();
                return;
            }
            j.status = "running".into();
            j.error = None;
            j.hint = None;
            j.done = 0;
            j.bytes = 0;
            j.last_emit = Instant::now() - Duration::from_secs(1);
        }) {
            emit_job(&app, &dto);
            if dto.status == "cancelled" {
                return;
            }
        }
        log_job(
            &app,
            "info",
            id,
            format!("job {id} download {url} → {dest} workers={workers}"),
        );
        let app_prog = app.clone();
        let cancel_prog = cancel.clone();
        let result = download_stream::<
            fn(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>>,
        >(
            &url,
            &mut headers,
            quality.as_deref(),
            &dest,
            false,
            false,
            None,
            move |done, total, bytes, _path| {
                let mut dto = None;
                {
                    let st = app_prog.state::<AppState>();
                    let mut jobs = st.jobs.lock().unwrap();
                    if let Some(j) = jobs.get_mut(&id) {
                        if j.status == "cancelling"
                            || j.status == "cancelled"
                            || cancel_prog.is_cancelled()
                        {
                            return;
                        }
                        j.done = done;
                        j.total = total.max(1);
                        j.bytes = bytes;
                        if done >= total && total > 0 {
                            j.status = "remuxing".into();
                        }
                        if j.status == "remuxing"
                            || j.last_emit.elapsed() >= Duration::from_millis(80)
                        {
                            j.last_emit = Instant::now();
                            dto = Some(j.dto());
                        }
                    }
                }
                if let Some(dto) = dto {
                    emit_job(&app_prog, &dto);
                }
            },
            Some(&cancel),
            workers,
        );
        if cancel.is_cancelled() {
            let _ = fs::remove_file(&dest);
            let ts = Path::new(&dest).with_extension("ts");
            let _ = fs::remove_file(&ts);
            if let Some(dto) = patch_job(&app, id, |j| {
                j.status = "cancelled".into();
                j.error = None;
            }) {
                emit_job(&app, &dto);
            }
            log_job(&app, "info", id, format!("job {id} cancelled"));
            return;
        }
        match result {
            Ok(path) => {
                if let Some(dto) = patch_job(&app, id, |j| {
                    j.status = "done".into();
                    j.dest = path.clone();
                    j.done = j.total.max(j.done).max(1);
                    j.total = j.total.max(j.done).max(1);
                }) {
                    emit_job(&app, &dto);
                }
                log_job(&app, "info", id, format!("job {id} saved {path}"));
            }
            Err(e) => {
                let cancelled = cancel.is_cancelled() || e.to_string().contains("cancelled");
                if cancelled {
                    if let Some(dto) = patch_job(&app, id, |j| {
                        j.status = "cancelled".into();
                        j.error = None;
                    }) {
                        emit_job(&app, &dto);
                    }
                    log_job(&app, "info", id, format!("job {id} cancelled"));
                    return;
                }
                let (status, hint, msg) = match &e {
                    Error::Http {
                        code: 403,
                        url: u,
                        hint,
                        ..
                    } if hint.contains("--referer") => (
                        "needReferer",
                        Some(hint.clone()),
                        format!("play: HTTP 403 fetching {u}{hint}"),
                    ),
                    _ => ("error", None, e.to_string()),
                };
                if let Some(dto) = patch_job(&app, id, |j| {
                    j.status = status.into();
                    j.hint = hint;
                    j.error = Some(msg.clone());
                }) {
                    emit_job(&app, &dto);
                }
                log_job(&app, "error", id, format!("job {id} {msg}"));
            }
        }
    });
}

fn enqueue_job(
    app: &AppHandle,
    state: &AppState,
    url: String,
    dest: String,
    quality: Option<String>,
    headers: HashMap<String, String>,
    reuse: Option<u64>,
    workers: usize,
) -> Result<JobDto, String> {
    let quality_s = quality.clone().unwrap_or_else(|| "best".into());
    let (id, cancel, dto) = {
        let mut jobs = state.jobs.lock().unwrap();
        if dest_in_use(&jobs, &dest, reuse) {
            return Err(format!("play: already downloading {dest}"));
        }
        if let Some(id) = reuse {
            let job = jobs
                .get_mut(&id)
                .ok_or_else(|| "play: no such job".to_string())?;
            if job.is_active() {
                return Err("play: job is still running".into());
            }
            job.url = url.clone();
            job.dest = dest.clone();
            job.quality = quality_s;
            job.status = "queued".into();
            job.done = 0;
            job.total = 1;
            job.bytes = 0;
            job.error = None;
            job.hint = None;
            job.cancel = CancelCtx::new();
            job.last_emit = Instant::now();
            (id, job.cancel.clone(), job.dto())
        } else {
            let id = state.job_seq.fetch_add(1, Ordering::Relaxed);
            let cancel = CancelCtx::new();
            let job = Job {
                id,
                url: url.clone(),
                dest: dest.clone(),
                quality: quality_s,
                status: "queued".into(),
                done: 0,
                total: 1,
                bytes: 0,
                error: None,
                hint: None,
                cancel: cancel.clone(),
                last_emit: Instant::now(),
            };
            let dto = job.dto();
            jobs.insert(id, job);
            (id, cancel, dto)
        }
    };
    emit_job(app, &dto);
    spawn_job_thread(
        app.clone(),
        id,
        url,
        dest,
        quality,
        headers,
        cancel,
        workers,
    );
    Ok(dto)
}

#[tauri::command]
pub fn download_cmd(
    app: AppHandle,
    state: State<AppState>,
    url: String,
    quality: Option<String>,
    dest: String,
    referer: Option<String>,
    origin: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    cookie_host: Option<String>,
) -> Result<JobDto, String> {
    let dir = workdir(&app, &state);
    let (headers, _prefs, _path) = load_headers(
        &url,
        &dir,
        referer.as_deref(),
        origin.as_deref(),
        user_agent.as_deref(),
        cookie.as_deref(),
        cookie_host.as_deref(),
    )
    .map_err(err_str)?;
    let n = workers(&app, &state);
    enqueue_job(&app, &state, url, dest, quality, headers, None, n)
}

#[tauri::command]
pub fn list_jobs_cmd(state: State<AppState>) -> Vec<JobDto> {
    let jobs = state.jobs.lock().unwrap();
    jobs.values().rev().map(|j| j.dto()).collect()
}

#[tauri::command]
pub fn cancel_job_cmd(app: AppHandle, state: State<AppState>, id: u64) -> Result<JobDto, String> {
    let dto = {
        let mut jobs = state.jobs.lock().unwrap();
        let job = jobs
            .get_mut(&id)
            .ok_or_else(|| "play: no such job".to_string())?;
        if job.is_active() {
            job.status = "cancelling".into();
            job.cancel.request();
        }
        job.dto()
    };
    emit_job(&app, &dto);
    log_job(&app, "info", id, format!("job {id} cancel requested"));
    Ok(dto)
}

#[tauri::command]
pub fn retry_job_cmd(
    app: AppHandle,
    state: State<AppState>,
    id: u64,
    referer: Option<String>,
    origin: Option<String>,
    user_agent: Option<String>,
    cookie: Option<String>,
    cookie_host: Option<String>,
) -> Result<JobDto, String> {
    let (url, dest, quality) = {
        let jobs = state.jobs.lock().unwrap();
        let job = jobs
            .get(&id)
            .ok_or_else(|| "play: no such job".to_string())?;
        if job.is_active() {
            return Err("play: job is still running".into());
        }
        (job.url.clone(), job.dest.clone(), Some(job.quality.clone()))
    };
    let dir = workdir(&app, &state);
    let (headers, _prefs, _path) = load_headers(
        &url,
        &dir,
        referer.as_deref(),
        origin.as_deref(),
        user_agent.as_deref(),
        cookie.as_deref(),
        cookie_host.as_deref(),
    )
    .map_err(err_str)?;
    let n = workers(&app, &state);
    enqueue_job(&app, &state, url, dest, quality, headers, Some(id), n)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadConfigDto {
    pub workers: u32,
    pub min_workers: u32,
    pub max_workers: u32,
}

#[tauri::command]
pub fn get_download_config_cmd(app: AppHandle, state: State<AppState>) -> DownloadConfigDto {
    DownloadConfigDto {
        workers: workers(&app, &state) as u32,
        min_workers: MIN_WORKERS as u32,
        max_workers: MAX_WORKERS as u32,
    }
}

#[tauri::command]
pub fn set_download_config_cmd(
    app: AppHandle,
    state: State<AppState>,
    workers: u32,
) -> Result<DownloadConfigDto, String> {
    let n = clamp_workers(workers as usize);
    *state.workers.lock().unwrap() = Some(n);
    patch_gui_config(&app, |cfg| {
        cfg.workers = Some(n as u32);
    });
    log(&app, "info", format!("download workers={n}"));
    Ok(DownloadConfigDto {
        workers: n as u32,
        min_workers: MIN_WORKERS as u32,
        max_workers: MAX_WORKERS as u32,
    })
}

#[tauri::command]
pub fn clear_jobs_cmd(app: AppHandle, state: State<AppState>) -> Vec<JobDto> {
    {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.retain(|_, j| j.is_active());
    }
    let _ = app.emit("jobs-cleared", ());
    list_jobs_cmd(state)
}

#[tauri::command]
pub fn auto_filename_cmd(url: String) -> String {
    auto_filename(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_gate_passes_budget_then_suppresses() {
        let t0 = Instant::now();
        let mut gate = LogGate::new(t0);
        for _ in 0..LogGate::BUDGET {
            assert_eq!(gate.check(t0), GateAction::Emit);
        }
        assert_eq!(gate.check(t0), GateAction::Suppress);
        assert_eq!(gate.suppressed(), 1);
    }

    #[test]
    fn test_log_gate_rollover_reports_suppressed_once() {
        let t0 = Instant::now();
        let mut gate = LogGate::new(t0);
        for _ in 0..LogGate::BUDGET {
            gate.check(t0);
        }
        gate.check(t0);
        gate.check(t0);
        assert_eq!(gate.suppressed(), 2);
        let later = t0 + LogGate::WINDOW;
        assert_eq!(gate.check(later), GateAction::EmitWithNotice(2));
        // New window starts clean: budget restored, count reset.
        assert_eq!(gate.check(later), GateAction::Emit);
        assert_eq!(gate.suppressed(), 0);
    }

    #[test]
    fn test_log_gate_quiet_rollover_has_no_notice() {
        let t0 = Instant::now();
        let mut gate = LogGate::new(t0);
        gate.check(t0);
        assert_eq!(gate.check(t0 + LogGate::WINDOW), GateAction::Emit);
    }

    #[test]
    fn test_tracked_mutex_records_and_clears_holder() {
        let m = TrackedMutex::new(1u32);
        assert_eq!(m.holder(), None);
        {
            let g = m.lock(StreamsSite::Patch);
            assert_eq!(*g, 1);
            let (site, _) = m.holder().expect("holder recorded");
            assert_eq!(site, "Patch");
        }
        assert_eq!(m.holder(), None);
    }

    #[test]
    fn test_tracked_mutex_try_lock_backs_off_and_names_holder() {
        let m = TrackedMutex::new(1u32);
        let _held = m.lock(StreamsSite::SpawnInsert);
        // Same-thread try on a held non-recursive mutex must not block.
        assert!(m.try_lock(StreamsSite::List).is_none());
        let (site, _) = m.holder().expect("holder recorded");
        assert_eq!(site, "SpawnInsert");
    }

    #[test]
    fn test_tracked_mutex_recovers_poison() {
        let m = TrackedMutex::new(1u32);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _g = m.lock(StreamsSite::Patch);
                    panic!("boom");
                });
            });
        }));
        assert!(r.is_err());
        let g = m.lock(StreamsSite::List);
        assert_eq!(*g, 1);
    }

    #[test]
    fn test_tracked_mutex_holder_visible_across_threads() {
        let m = std::sync::Arc::new(TrackedMutex::new(1u32));
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|s| {
            let (m2, entered2) = (m.clone(), entered.clone());
            s.spawn(move || {
                let _g = m2.lock(StreamsSite::Reap);
                entered2.wait();
                entered2.wait();
            });
            entered.wait();
            let (site, _) = m.holder().expect("holder visible");
            assert_eq!(site, "Reap");
            entered.wait();
        });
    }

    #[test]
    fn test_dto_skips_wedged_proxy_without_blocking() {
        use std::sync::Arc;
        let state = Arc::new(crate::proxy::tests::blank_state(Vec::new(), 0));
        let snap = StreamSnap {
            id: 7,
            url: "https://cdn.example/f".to_string(),
            quality: "best".to_string(),
            status: "playing".to_string(),
            title: None,
            play_url: "http://127.0.0.1:1/f.mp4".to_string(),
            pid: None,
            proxy_state: Some(state.clone()),
            proxy_base: None,
        };
        // Free proxy: DTO present on both branches (blank state starts
        // sourceless, so the first pass takes the HLS path).
        assert!(dto_from_snap(&snap).is_some());
        state.lock().unwrap().source = Some("file".to_string());
        assert!(file_session_dto(&snap, 0).is_some());
        // Held proxy: retryable miss on both branches, never parks.
        let _held = state.lock().unwrap();
        assert!(dto_from_snap(&snap).is_none());
        assert!(file_session_dto(&snap, 0).is_none());
    }

    #[test]
    fn test_begin_op_stands_down_previous() {
        // A new op cancels the previous context and installs a live one,
        // so stale curls die and only the latest op can open VLC.
        let state = AppState::new();
        let first = begin_op(&state);
        assert!(!first.is_cancelled());
        let second = begin_op(&state);
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
    }

    #[test]
    fn test_file_dto_renders_cached_cells() {
        use std::sync::Arc;
        let state = Arc::new(crate::proxy::tests::blank_state(Vec::new(), 0));
        let snap = StreamSnap {
            id: 9,
            url: "https://cdn.example/f".to_string(),
            quality: "best".to_string(),
            status: "playing".to_string(),
            title: None,
            play_url: "http://127.0.0.1:1/f.mp4".to_string(),
            pid: None,
            proxy_state: Some(state.clone()),
            proxy_base: None,
        };
        // Sourceless sessions take the HLS branch; the file grid needs one.
        state.lock().unwrap().source = Some("file".to_string());
        // Served span 140-160 of 2000 bytes lands in cell 7 alone.
        {
            let mut st = state.lock().unwrap();
            st.total_bytes = Some(2000);
            st.served_ranges.push((140, 160));
        }
        let dto = file_session_dto(&snap, 0).expect("dto");
        // Green means streamed before: only the served cell lights.
        assert!(dto.window[7]);
        assert!(!dto.window[6]);
        assert!(!dto.window[8]);
        assert_eq!(dto.buffered, 0);
    }

    #[test]
    fn test_tracked_mutex_records_holder_trace() {
        let m = TrackedMutex::new(1u32);
        assert!(m.holder_trace().is_empty());
        assert_eq!(m.waiter_count(), 0);
        {
            let _g = m.lock(StreamsSite::SeekUpdate);
            assert_eq!(m.holder().expect("holder").0, "SeekUpdate");
            // The acquire-time stack must be captured: a stuck holder names
            // its own call path in the wedge log even if its thread is gone.
            assert!(!m.holder_trace().is_empty());
        }
        assert_eq!(m.holder(), None);
    }
}
