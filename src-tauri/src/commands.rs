use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::download::{clamp_workers, download_stream, DEFAULT_WORKERS, MAX_WORKERS, MIN_WORKERS};
use crate::error::Error;
use crate::fetch::{http_get_maybe_playlist, http_headers, CancelCtx, MaybePlaylist};
use crate::hls::{
    parse_subtitle_tracks, parse_variants, ranked_variants, subtitle_label, variant_label, Variant,
};
use crate::player::{
    start_hls_playback, start_http_playback, vlc_path, vlc_stderr_level, wait_for_vlc,
};
use crate::prefs::{
    host_of, load_prefs, normalize_referer, prefs_path, remember_prefs, save_prefs, settings_for,
    Prefs,
};
use crate::proxy::ProxyHandle;
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
    pub streams: Mutex<BTreeMap<u64, StreamSession>>,
    pub stream_seq: AtomicU64,
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
            streams: Mutex::new(BTreeMap::new()),
            stream_seq: AtomicU64::new(1),
        }
    }
}

/// One playing stream: VLC process plus its proxy cache.
pub struct StreamSession {
    id: u64,
    url: String,
    quality: String,
    pid: Option<u32>,
    status: String,
    proxy: Option<ProxyHandle>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDto {
    pub id: u64,
    pub url: String,
    pub host: String,
    pub quality: String,
    pub status: String,
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

impl StreamSession {
    fn dto(&self) -> StreamDto {
        let (playhead, buffered, window, window_start, window_len, total, cached_bytes, served) =
            match &self.proxy {
                Some(p) => {
                    let st = p.state.lock().unwrap();
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
        StreamDto {
            id: self.id,
            url: self.url.clone(),
            host: host_of(&self.url),
            quality: self.quality.clone(),
            status: self.status.clone(),
            pid: self.pid,
            playhead,
            buffered,
            window,
            window_start,
            window_len,
            total,
            cached_bytes,
            served,
        }
    }
}

fn emit_stream(app: &AppHandle, dto: &StreamDto) {
    let _ = app.emit("stream-updated", dto);
}

fn patch_stream(app: &AppHandle, id: u64, f: impl FnOnce(&mut StreamSession)) -> Option<StreamDto> {
    let st = app.state::<AppState>();
    let mut streams = st.streams.lock().unwrap();
    let s = streams.get_mut(&id)?;
    f(s);
    Some(s.dto())
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
    log(
        app,
        "info",
        format!("capture {}{extra}{cookie}{subs}", capture.url),
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
    tauri::async_runtime::spawn_blocking(move || {
        resolve_blocking(&app2, &dir, &url, referer, origin, user_agent, cookie, cookie_host)
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
) -> Result<ResolveResponse, String> {
    let app2 = app.clone();
    let dir = workdir(&app, &state);
    tauri::async_runtime::spawn_blocking(move || {
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
        )
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
            "play {} {url} quality={q}{}  Referer={}  Origin={}{}",
            kind.as_str(),
            if extra { " (another instance)" } else { "" },
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
            spawn_session(app, state, pb.child, Some(pb.proxy), extra, url, q);
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
                    spawn_session(app, state, pb.child, Some(pb.proxy), extra, url, q);
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
                    )
                    .map_err(|e| {
                        log(app, "error", e.to_string());
                        err_str(e)
                    })?;
                    let local = pb.play_url.clone();
                    let pid = pb.child.id();
                    log_vlc(app, "debug", pid, format!("local file {local}"));
                    spawn_session(app, state, pb.child, Some(pb.proxy), extra, url, q);
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

fn spawn_session(
    app: &AppHandle,
    state: &AppState,
    mut child: Child,
    proxy: Option<ProxyHandle>,
    extra: bool,
    url: &str,
    quality: &str,
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
    state.streams.lock().unwrap().insert(
        stream_id,
        StreamSession {
            id: stream_id,
            url: url.to_string(),
            quality: quality.to_string(),
            pid: Some(pid),
            status: "playing".into(),
            proxy,
        },
    );
    if let Some(dto) = patch_stream(app, stream_id, |_| {}) {
        emit_stream(app, &dto);
    }
    if let Some(s) = state.streams.lock().unwrap().get_mut(&stream_id) {
        if let Some(ref proxy) = s.proxy {
            let app_log = app.clone();
            let cb: Arc<dyn Fn(&str, String) + Send + Sync> = Arc::new(move |level, msg| {
                log_vlc(&app_log, level, pid, msg);
            });
            let pending = {
                let mut st = proxy.state.lock().unwrap();
                st.on_log = Some(cb.clone());
                std::mem::take(&mut st.pending_log)
            };
            for (level, msg) in pending {
                cb(&level, msg);
            }
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
    if let Some(stderr) = child.stderr.take() {
        let app_err = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if let Some(level) = vlc_stderr_level(&line) {
                    log_vlc(&app_err, level, pid, line);
                }
            }
        });
    }
    let app = app.clone();
    thread::spawn(move || {
        let code = wait_for_vlc(&mut child, false);
        let _ = child.kill();
        let st = app.state::<AppState>();
        {
            let mut primary = st.primary.lock().unwrap();
            if *primary == Some(pid) {
                *primary = None;
            }
        }
        st.extras.lock().unwrap().retain(|p| *p != pid);
        let removed = st.streams.lock().unwrap().remove(&stream_id);
        if removed.is_some() {
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
pub fn list_streams_cmd(state: State<AppState>) -> Vec<StreamDto> {
    state
        .streams
        .lock()
        .unwrap()
        .values()
        .map(|s| s.dto())
        .collect()
}

#[tauri::command]
pub fn stop_stream_cmd(app: AppHandle, id: u64) -> Result<(), String> {
    let removed = app
        .state::<AppState>()
        .streams
        .lock()
        .unwrap()
        .remove(&id);
    let Some(s) = removed else {
        return Err(format!("play: no stream {id}"));
    };
    if let Some(pid) = s.pid {
        log_vlc(&app, "info", pid, format!("stop stream {id} pid={pid}"));
        kill_pid(pid);
    }
    let st = app.state::<AppState>();
    {
        let mut primary = st.primary.lock().unwrap();
        if *primary == s.pid {
            *primary = None;
        }
    }
    if let Some(pid) = s.pid {
        st.extras.lock().unwrap().retain(|p| *p != pid);
    }
    let _ = app.emit("stream-removed", id);
    Ok(())
}

#[tauri::command]
pub fn stream_stats_cmd(state: State<AppState>) -> Vec<StreamDto> {
    list_streams_cmd(state)
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
