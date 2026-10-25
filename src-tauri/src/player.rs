use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::fetch::http_get;
use crate::hls::{
    filter_subtitles, is_subtitle_url, local_master_playlist, map_or_first_segment_url,
    playlist_hint, resolve_media_playlist, rewrite_media_playlist, sniff_segment_kind,
    subtitle_content_type, subtitle_ext, unwrap_media, SubtitleTrack,
};
use crate::proxy::{start_file_proxy, start_proxy, ProxyHandle};

pub const VLC_BIN: &str = "/Applications/VLC.app/Contents/MacOS/VLC";

pub fn vlc_path() -> PathBuf {
    std::env::var("VLC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(VLC_BIN))
}

/// A free loopback port for this VLC instance's remote-control channel.
fn free_loopback_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()
        .map(|a| a.port())
}

/// Remote-control listener args for one VLC instance. `--extraintf` keeps
/// the normal GUI while adding the line-based remote-control interface
/// (module `oldrc` in VLC 3.x builds: `seek X` seeks X seconds), so the
/// app can seek the running player instead of relaunching it.
/// `--rc-fake-tty` is required: without a TTY on stdin `oldrc` refuses to
/// start at all (verified live against VLC 3.0.23: the TCP listener only
/// comes up with this flag, and stays up on a DEVNULL stdin).
pub fn rc_args(port: u16) -> Vec<String> {
    vec![
        "--extraintf=oldrc".into(),
        format!("--rc-host=127.0.0.1:{port}"),
        "--rc-fake-tty".into(),
    ]
}

/// Absolute `seek <secs>` on a running instance's RC channel, with short
/// retries for a listener that is still coming up. A successful write is
/// success: rc replies vary by VLC version, so nothing is parsed. Worst
/// case ~1s, and the caller runs off the main thread regardless.
pub fn rc_seek(port: u16, secs: u64) -> Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| Error::msg(format!("play: bad rc addr: {e}")))?;
    let mut last = "unreachable".to_string();
    for _ in 0..3 {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
            Ok(mut stream) => {
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                match stream.write_all(format!("seek {secs}\n").as_bytes()) {
                    Ok(()) => {
                        let _ = stream.flush();
                        return Ok(());
                    }
                    Err(e) => last = e.to_string(),
                }
            }
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(Error::msg(format!("play: rc seek 127.0.0.1:{port}: {last}")))
}

/// Media duration in seconds via `get_length`. Drains pre-command chatter,
/// then takes the first bare-number reply line. Bounded (~3s per attempt);
/// callers run off the main thread.
pub fn rc_length(port: u16) -> Result<u64> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| Error::msg(format!("play: bad rc addr: {e}")))?;
    let mut last = "unreachable".to_string();
    for _ in 0..2 {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
            Ok(mut stream) => {
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                let mut rd = BufReader::new(stream.try_clone().map_err(|e| {
                    Error::msg(format!("play: rc length 127.0.0.1:{port}: {e}"))
                })?);
                // Drain status broadcasts so the reply is unambiguous.
                let mut line = String::new();
                loop {
                    line.clear();
                    match rd.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                if stream.write_all(b"get_length\n").is_err() {
                    last = "write failed".to_string();
                } else {
                    let _ = stream.flush();
                    line.clear();
                    loop {
                        line.clear();
                        match rd.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if let Ok(v) = line.trim().parse::<u64>() {
                                    return Ok(v);
                                }
                            }
                        }
                    }
                    last = "no length reply".to_string();
                }
            }
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(Error::msg(format!(
        "play: rc length 127.0.0.1:{port}: {last}"
    )))
}

pub fn vlc_launch_args(vlc: &Path, media_args: &[&str]) -> Vec<String> {
    let mut cmd = vec![
        vlc.to_string_lossy().into_owned(),
        "--play-and-exit".into(),
        "--network-caching=8000".into(),
        // VideoToolbox often plays AAC and deadlocks the video decoder (black
        // picture, "buffer deadlock prevented"). Software avcodec is fine at
        // typical HLS resolutions.
        "--no-videotoolbox".into(),
        "--avcodec-hw=none".into(),
    ];
    cmd.extend(media_args.iter().map(|s| (*s).to_string()));
    cmd
}

pub fn require_vlc(vlc: &Path) -> Result<()> {
    if !vlc.is_file() {
        return Err(Error::msg(format!(
            "play: VLC not found at {} (set VLC=...)",
            vlc.display()
        )));
    }
    Ok(())
}

fn vlc_window_count(pid: u32) -> Option<i32> {
    let script = format!(
        r#"
    tell application "System Events"
      if not (exists (first process whose unix id is {pid})) then
        return -1
      end if
      tell (first process whose unix id is {pid})
        return count of windows
      end tell
    end tell
    "#
    );
    let out = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// VLC module logs look like `[ptr] name error: message`. Info noise is ignored.
pub fn vlc_stderr_level(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if lower.contains(" error:") {
        Some("error")
    } else if lower.contains(" warning:") {
        Some("warn")
    } else {
        None
    }
}

pub fn wait_for_vlc(proc: &mut Child, verbose: bool) -> i32 {
    if let Some(stderr) = proc.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if verbose || vlc_stderr_level(&line).is_some() {
                    eprintln!("{line}");
                }
            }
        });
    }
    let pid = proc.id();
    let mut seen_window = false;
    loop {
        match proc.try_wait() {
            Ok(Some(st)) => return st.code().unwrap_or(1),
            Ok(None) => {}
            Err(_) => return 1,
        }
        match vlc_window_count(pid) {
            None => {
                return proc.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            }
            Some(n) if n < 0 => {
                return proc.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            }
            Some(n) if n > 0 => seen_window = true,
            Some(_) if seen_window => {
                if verbose {
                    eprintln!("play: VLC window closed, stopping");
                }
                let _ = proc.kill();
                return proc.wait().ok().and_then(|s| s.code()).unwrap_or(1);
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(400));
    }
}

pub struct Playback {
    pub proxy: ProxyHandle,
    pub child: Child,
    pub hint: String,
    pub play_url: String,
    /// This instance's remote-control port, if one was allocated.
    pub rc_port: Option<u16>,
    /// VLC args (subtitle options) excluding the play URL, kept so a
    /// seek relaunch preserves subtitles.
    pub sub_args: Vec<String>,
    /// Staged subtitle files handed to VLC by local path. The caller moves
    /// these into the stream session; `stop()` removes any left behind.
    pub staged_subs: Vec<std::path::PathBuf>,
}

fn vlc_sub_options(sidecar: &[String], enable_embedded: bool) -> Vec<String> {
    let mut o = Vec::new();
    if let Some(first) = sidecar.first() {
        o.push(format!("--sub-file={first}"));
        o.push("--sub-track=0".into());
    } else if enable_embedded {
        o.push("--sub-track=0".into());
    }
    if sidecar.len() > 1 {
        o.push(format!("--input-slave={}", sidecar[1..].join("#")));
    }
    o
}

/// VLC resolves `--sub-file` against the working directory: a plain
/// `http://...` value is parsed as a file path, which produces errors like
/// `file:///.../http%3A//127.0.0.1%3A.../sub/0.vtt`. Stage the fetched body
/// to a temp file instead and hand VLC the local path.
fn stage_sub(body: &[u8], ext: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "play-subs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let path = dir.join(format!("sub.{}", ext.trim_start_matches('.')));
    if fs::write(&path, body).is_err() {
        let _ = fs::remove_dir_all(&dir);
        return None;
    }
    Some(path)
}

fn merge_extra_subs(tracks: &mut Vec<SubtitleTrack>, extra: &[String]) {
    for u in extra {
        let u = u.trim();
        if u.is_empty() || tracks.iter().any(|t| t.url == u) {
            continue;
        }
        // Keep the raw file name (e.g. "English.vtt") so the in-app picker
        // can rank sidecars by name even when no LANGUAGE tag exists.
        let name = url::Url::parse(u)
            .ok()
            .and_then(|p| {
                p.path_segments()
                    .and_then(|s| s.last())
                    // path_segments yields percent-decoded segments already.
                    .map(|s| s.to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "subtitles".into());
        tracks.push(SubtitleTrack {
            name,
            language: String::new(),
            url: u.to_string(),
            default: tracks.is_empty(),
        });
    }
}

impl Playback {
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        Self::remove_staged(&mut self.staged_subs);
    }

    fn remove_staged(staged: &mut Vec<std::path::PathBuf>) {
        for path in std::mem::take(staged) {
            let dir = path.parent().map(Path::to_path_buf);
            let _ = fs::remove_file(&path);
            if let Some(dir) = dir {
                let _ = fs::remove_dir(dir);
            }
        }
    }

    /// Take staged subtitle paths for the session to own. Called when
    /// ownership moves into a stream session (commands.rs removes them
    /// when the session ends); `stop()` removes any left behind.
    pub fn take_staged_subs(&mut self) -> Vec<std::path::PathBuf> {
        std::mem::take(&mut self.staged_subs)
    }
}

pub fn spawn_vlc(vlc: &Path, media_args: &[&str]) -> Result<Child> {
    require_vlc(vlc)?;
    let args = vlc_launch_args(vlc, media_args);
    Command::new(&args[0])
        .args(&args[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::msg(format!("play: spawn VLC: {e}")))
}

/// Progress callback: (stage, done, total). Stages: playlist, sniff,
/// proxy, subtitles, launch. `None` disables reporting (CLI path).
pub type PlayStage = crate::hls::PlayStage;

fn emit_stage_opt(
    on_stage: &mut Option<&mut PlayStage>,
    name: &str,
    done: usize,
    total: usize,
    detail: Option<String>,
) {
    if let Some(cb) = on_stage.as_deref_mut() {
        cb(name, done, total, detail);
    }
}

pub fn start_hls_playback<F>(
    url: &str,
    headers: &mut HashMap<String, String>,
    quality: Option<&str>,
    vlc: &Path,
    verbose: bool,
    interactive: bool,
    extra_subs: &[String],
    subtitle: Option<&str>,
    mut on_403: Option<&mut F>,
    mut on_stage: Option<&mut PlayStage>,
    prefetch_count: usize,
    title: Option<&str>,
) -> Result<Playback>
where
    F: FnMut(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>> + ?Sized,
{
    emit_stage_opt(&mut on_stage, "playlist", 0, 5, None);
    let resolved = resolve_media_playlist(
        url,
        headers,
        quality,
        verbose,
        Some(interactive),
        on_403.as_deref_mut(),
        on_stage.as_deref_mut(),
    )?;
    let media_url = resolved.media_url;
    let media_text = resolved.media_text;
    let mut subs = resolved.subtitles;
    merge_extra_subs(&mut subs, extra_subs);
    let subs = filter_subtitles(subs, subtitle);
    emit_stage_opt(&mut on_stage, "sniff", 1, 5, None);
    let first = map_or_first_segment_url(&media_url, &media_text);
    let ext = first
        .as_deref()
        .map(|u| sniff_segment_kind(u, headers))
        .unwrap_or("ts");
    let mut hint = playlist_hint(&media_text, ext);
    if verbose {
        eprintln!("play: {hint}");
    }
    let mut segments = Vec::new();
    emit_stage_opt(&mut on_stage, "proxy", 2, 5, None);
    let proxy = start_proxy(headers.clone(), Vec::new(), Vec::new(), verbose)?;
    let rewritten =
        rewrite_media_playlist(&media_text, &media_url, &proxy.base, &mut segments, ext);
    let mut files = HashMap::new();
    let mut hls_subs: Vec<(SubtitleTrack, String)> = Vec::new();
    let mut sidecar: Vec<String> = Vec::new();
    let mut staged: Vec<std::path::PathBuf> = Vec::new();
    let total_subs = subs.len().max(1);
    for (i, track) in subs.iter().enumerate() {
        emit_stage_opt(&mut on_stage, "subtitles", i, total_subs, Some(track.name.clone()));
        let got = http_get(&track.url, headers);
        let Ok((final_u, body)) = got else {
            if verbose {
                eprintln!("play: skip subtitle {}: fetch failed", track.url);
            }
            continue;
        };
        let text = String::from_utf8_lossy(&body);
        if text.trim_start().starts_with("#EXTM3U") {
            let pl = rewrite_media_playlist(&text, &final_u, &proxy.base, &mut segments, "vtt");
            let path = format!("/sub/{i}.m3u8");
            files.insert(path.clone(), pl.into_bytes());
            hls_subs.push((track.clone(), format!("{}{path}", proxy.base)));
        } else if subtitle_content_type(&body).is_some() || is_subtitle_url(&track.url) {
            let path = format!("/sub/{i}.{}", subtitle_ext(&track.url));
            files.insert(path.clone(), body.clone());
            // Hand VLC a staged local file, not the proxy URL. VLC parses
            // `--sub-file=http://...` as a relative file path and fails with
            // `file:///.../http%3A//...`, so the fetched body is staged here.
            match stage_sub(&body, subtitle_ext(&track.url)) {
                Some(staged_path) => {
                    sidecar.push(staged_path.to_string_lossy().into_owned());
                    staged.push(staged_path);
                }
                None => sidecar.push(format!("{}{path}", proxy.base)),
            }
        }
    }
    let video_url = proxy.playlist_url_with(title);
    let (play_url, embedded) = if !hls_subs.is_empty() {
        let master_body = local_master_playlist(&video_url, &hls_subs).into_bytes();
        // Keep the legacy path working; VLC opens the title-named one.
        files.insert("/master.m3u8".into(), master_body.clone());
        let master_path = format!("/{}", crate::urls::master_filename(title));
        if master_path != "/master.m3u8" {
            files.insert(master_path, master_body);
        }
        (proxy.master_url_with(title), true)
    } else {
        (video_url, false)
    };
    {
        let mut st = proxy.state.lock().unwrap();
        st.playlist = rewritten.into_bytes();
        st.segments = segments;
        st.files = files;
    }
    if !hls_subs.is_empty() || !sidecar.is_empty() {
        hint = format!("{hint} subs={}", hls_subs.len() + sidecar.len());
    }
    if verbose {
        eprintln!("play: local playlist {play_url}");
        for (t, _) in &hls_subs {
            eprintln!("play: subtitle {} {}", t.language, t.url);
        }
    }
    let mut args = vlc_sub_options(&sidecar, embedded);
    let rc_port = free_loopback_port();
    if let Some(port) = rc_port {
        args.extend(rc_args(port));
    }
    let sub_args = args.clone();
    args.push(play_url.clone());
    if verbose {
        eprintln!("play: vlc sub args {}", sidecar.join(", "));
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if prefetch_count > 0 {
        let total = prefetch_count.min(proxy.state.lock().unwrap().segments.len());
        crate::proxy::prefetch_segments(&proxy, prefetch_count, |done, _| {
            emit_stage_opt(
                &mut on_stage,
                "buffer",
                done,
                total.max(1),
                Some(format!("{done}/{total} segments")),
            );
        });
        hint = format!("{hint} buffered={total}");
    }
    emit_stage_opt(&mut on_stage, "launch", 4, 5, None);
    let child = spawn_vlc(vlc, &arg_refs)?;
    emit_stage_opt(&mut on_stage, "launch", 5, 5, None);
    Ok(Playback {
        proxy,
        child,
        hint,
        play_url,
        rc_port,
        sub_args,
        staged_subs: staged,
    })
}

pub fn dry_run_hls<F>(
    url: &str,
    headers: &mut HashMap<String, String>,
    quality: Option<&str>,
    verbose: bool,
    on_403: Option<&mut F>,
) -> Result<()>
where
    F: FnMut(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>> + ?Sized,
{
    let resolved = resolve_media_playlist(url, headers, quality, verbose, Some(false), on_403, None)?;
    let media_url = resolved.media_url;
    let media_text = resolved.media_text;
    if !resolved.subtitles.is_empty() {
        eprintln!("play: {} subtitle track(s)", resolved.subtitles.len());
    }
    let first = map_or_first_segment_url(&media_url, &media_text);
    let ext = first
        .as_deref()
        .map(|u| sniff_segment_kind(u, headers))
        .unwrap_or("ts");
    if verbose {
        eprintln!("play: segment kind .{ext}");
    }
    let mut segments = Vec::new();
    let proxy = start_proxy(headers.clone(), Vec::new(), Vec::new(), verbose)?;
    let rewritten =
        rewrite_media_playlist(&media_text, &media_url, &proxy.base, &mut segments, ext);
    {
        let mut st = proxy.state.lock().unwrap();
        st.playlist = rewritten.clone().into_bytes();
        st.segments = segments;
    }
    let playlist_url = proxy.playlist_url();
    eprintln!("play: local playlist {playlist_url}");
    for ln in rewritten.lines().take(16) {
        eprintln!("{ln}");
    }
    let empty = HashMap::new();
    let mut probe_headers = empty;
    let (_, probe) = http_get(&playlist_url, &mut probe_headers)?;
    let text = String::from_utf8_lossy(&probe);
    let segs: Vec<&str> = text
        .lines()
        .filter(|ln| ln.starts_with("http://"))
        .collect();
    if segs.is_empty() {
        proxy.shutdown();
        return Err(Error::msg("play: rewrite produced no segments"));
    }
    let mut probe_headers = HashMap::new();
    let (_, seg_body) = http_get(segs[0], &mut probe_headers)?;
    let (skip, kind) = unwrap_media(&seg_body);
    let magic = seg_body.get(skip).copied();
    eprintln!(
        "play: proxied {} bytes kind={kind} magic={:?}",
        seg_body.len(),
        magic.map(|b| [b])
    );
    let encrypted = text.contains("#EXT-X-KEY");
    if kind == "ts" && magic != Some(b'G') && !encrypted {
        proxy.shutdown();
        return Err(Error::msg(
            "play: first segment is not MPEG-TS after unwrap",
        ));
    }
    eprintln!("play: dry-run ok");
    proxy.shutdown();
    Ok(())
}

pub fn start_http_playback(
    url: &str,
    headers: &HashMap<String, String>,
    vlc: &Path,
    verbose: bool,
    extra_subs: &[String],
    mut on_stage: Option<&mut PlayStage>,
    title: Option<&str>,
) -> Result<Playback> {
    emit_stage_opt(&mut on_stage, "proxy", 0, 2, None);
    let full = crate::urls::strip_range_query(url);
    let proxy = start_file_proxy(full, headers.clone(), verbose)?;
    let local = proxy.file_url_with(title);
    let mut sidecar = Vec::new();
    let mut staged: Vec<std::path::PathBuf> = Vec::new();
    if !extra_subs.is_empty() {
        let mut files = HashMap::new();
        let mut hdrs = headers.clone();
        let total_subs = extra_subs.len().max(1);
        for (i, u) in extra_subs.iter().enumerate() {
            emit_stage_opt(&mut on_stage, "subtitles", i, total_subs, Some(u.clone()));
            let Ok((_, body)) = http_get(u, &mut hdrs) else {
                continue;
            };
            if subtitle_content_type(&body).is_none() && !is_subtitle_url(u) {
                continue;
            }
            let path = format!("/sub/{i}.{}", subtitle_ext(u));
            files.insert(path.clone(), body.clone());
            match stage_sub(&body, subtitle_ext(u)) {
                Some(staged_path) => {
                    sidecar.push(staged_path.to_string_lossy().into_owned());
                    staged.push(staged_path);
                }
                None => sidecar.push(format!("{}{path}", proxy.base)),
            }
        }
        proxy.state.lock().unwrap().files = files;
    }
    // Publish the immutable routing snapshot: serving reads it without the
    // mutex from here on. Setup-time only; no request has arrived yet.
    {
        let st = proxy.state.lock().unwrap();
        let _ = proxy.file_route.set(crate::proxy::FileRoute {
            source: st.source.clone(),
            files: st.files.clone(),
            headers: headers.clone(),
        });
    }
    if verbose {
        eprintln!("play: local file {local}");
    }
    let mut args = vlc_sub_options(&sidecar, false);
    let rc_port = free_loopback_port();
    if let Some(port) = rc_port {
        args.extend(rc_args(port));
    }
    let sub_args = args.clone();
    args.push(local.clone());
    if verbose {
        eprintln!("play: vlc sub args {}", sidecar.join(", "));
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    emit_stage_opt(&mut on_stage, "launch", 1, 2, None);
    let child = spawn_vlc(vlc, &arg_refs)?;
    emit_stage_opt(&mut on_stage, "launch", 2, 2, None);
    Ok(Playback {
        proxy,
        child,
        hint: if sidecar.is_empty() {
            "http".into()
        } else {
            format!("http subs={}", sidecar.len())
        },
        play_url: local,
        rc_port,
        sub_args,
        staged_subs: staged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_launch_args_are_vlc_cli_options() {
        let cmd = vlc_launch_args(
            Path::new("/Applications/VLC.app/Contents/MacOS/VLC"),
            &["http://127.0.0.1/index.m3u8"],
        );
        assert_eq!(cmd[0], "/Applications/VLC.app/Contents/MacOS/VLC");
        assert!(cmd.iter().any(|a| a == "--play-and-exit"));
        assert!(cmd.iter().any(|a| a == "--network-caching=8000"));
        assert!(cmd.iter().any(|a| a == "--no-videotoolbox"));
        assert!(cmd.iter().any(|a| a == "--avcodec-hw=none"));
        assert!(!cmd.iter().any(|a| a.contains("one-instance")));
        assert_eq!(cmd.last().unwrap(), "http://127.0.0.1/index.m3u8");
        for a in &cmd[1..cmd.len() - 1] {
            assert!(
                a.starts_with("--"),
                "VLC short option would be parsed as -N: {a}"
            );
            assert!(!a.contains("NSQuit"));
        }
    }

    #[test]
    fn test_rc_args_keep_gui_and_listen_on_loopback() {
        // `oldrc` is the module name in VLC 3.x builds (verified against
        // the bundled liboldrc_plugin.dylib, which serves `seek X`).
        let args = rc_args(4212);
        assert!(args.iter().any(|a| a == "--extraintf=oldrc"));
        assert!(args.iter().any(|a| a == "--rc-host=127.0.0.1:4212"));
        assert!(args.iter().any(|a| a == "--rc-fake-tty"));
    }

    #[test]
    fn test_rc_seek_writes_seek_command() {
        // Stand-in for VLC's rc listener: assert the exact line protocol.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let got = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let got2 = got.clone();
        let t = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(&stream);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            *got2.lock().unwrap() = line;
            let _ = stream.write_all(b"ok\n");
        });
        rc_seek(port, 42).unwrap();
        t.join().unwrap();
        assert_eq!(got.lock().unwrap().as_str(), "seek 42\n");
    }

    #[test]
    fn test_rc_seek_refused_is_an_error() {
        // Bind-then-drop reserves a (virtually certainly) closed port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(rc_seek(port, 1).is_err());
    }

    #[test]
    fn test_vlc_stderr_level_picks_errors() {
        assert_eq!(
            vlc_stderr_level(
                "[000000010930af10] adaptive demux error: Failed to create demuxer 0x0 Unknown"
            ),
            Some("error")
        );
        assert_eq!(
            vlc_stderr_level("[0000000129805f40] main warning: late picture"),
            Some("warn")
        );
        assert!(vlc_stderr_level(
            "[0000000129805f40] main libvlc: Running vlc with the default interface."
        )
        .is_none());
        assert!(vlc_stderr_level("").is_none());
    }
}
