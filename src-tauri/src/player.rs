use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::fetch::http_get;
use crate::hls::{
    is_subtitle_url, local_master_playlist, map_or_first_segment_url, playlist_hint,
    resolve_media_playlist, rewrite_media_playlist, sniff_segment_kind, subtitle_content_type,
    subtitle_ext, unwrap_media, SubtitleTrack,
};
use crate::proxy::{start_file_proxy, start_proxy, ProxyHandle};

pub const VLC_BIN: &str = "/Applications/VLC.app/Contents/MacOS/VLC";

pub fn vlc_path() -> PathBuf {
    std::env::var("VLC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(VLC_BIN))
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

fn merge_extra_subs(tracks: &mut Vec<SubtitleTrack>, extra: &[String]) {
    for u in extra {
        let u = u.trim();
        if u.is_empty() || tracks.iter().any(|t| t.url == u) {
            continue;
        }
        let name = url::Url::parse(u)
            .ok()
            .and_then(|p| {
                p.path_segments()
                    .and_then(|s| s.last())
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

pub fn start_hls_playback<F>(
    url: &str,
    headers: &mut HashMap<String, String>,
    quality: Option<&str>,
    vlc: &Path,
    verbose: bool,
    interactive: bool,
    extra_subs: &[String],
    mut on_403: Option<&mut F>,
) -> Result<Playback>
where
    F: FnMut(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>> + ?Sized,
{
    let resolved = resolve_media_playlist(
        url,
        headers,
        quality,
        verbose,
        Some(interactive),
        on_403.as_deref_mut(),
    )?;
    let media_url = resolved.media_url;
    let media_text = resolved.media_text;
    let mut subs = resolved.subtitles;
    merge_extra_subs(&mut subs, extra_subs);
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
    let proxy = start_proxy(headers.clone(), Vec::new(), Vec::new(), verbose)?;
    let rewritten =
        rewrite_media_playlist(&media_text, &media_url, &proxy.base, &mut segments, ext);
    let mut files = HashMap::new();
    let mut hls_subs: Vec<(SubtitleTrack, String)> = Vec::new();
    let mut sidecar: Vec<String> = Vec::new();
    for (i, track) in subs.iter().enumerate() {
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
            files.insert(path.clone(), body);
            sidecar.push(format!("{}{path}", proxy.base));
        }
    }
    let video_url = proxy.playlist_url();
    let (play_url, embedded) = if !hls_subs.is_empty() {
        files.insert(
            "/master.m3u8".into(),
            local_master_playlist(&video_url, &hls_subs).into_bytes(),
        );
        (proxy.master_url(), true)
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
    args.push(play_url.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let child = spawn_vlc(vlc, &arg_refs)?;
    Ok(Playback {
        proxy,
        child,
        hint,
        play_url,
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
    let resolved = resolve_media_playlist(url, headers, quality, verbose, Some(false), on_403)?;
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
) -> Result<Playback> {
    let proxy = start_file_proxy(url.to_string(), headers.clone(), verbose)?;
    let local = proxy.file_url();
    let mut sidecar = Vec::new();
    if !extra_subs.is_empty() {
        let mut files = HashMap::new();
        let mut hdrs = headers.clone();
        for (i, u) in extra_subs.iter().enumerate() {
            let Ok((_, body)) = http_get(u, &mut hdrs) else {
                continue;
            };
            if subtitle_content_type(&body).is_none() && !is_subtitle_url(u) {
                continue;
            }
            let path = format!("/sub/{i}.{}", subtitle_ext(u));
            files.insert(path.clone(), body);
            sidecar.push(format!("{}{path}", proxy.base));
        }
        proxy.state.lock().unwrap().files = files;
    }
    if verbose {
        eprintln!("play: local file {local}");
    }
    let mut args = vlc_sub_options(&sidecar, false);
    args.push(local.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let child = spawn_vlc(vlc, &arg_refs)?;
    Ok(Playback {
        proxy,
        child,
        hint: if sidecar.is_empty() {
            "http".into()
        } else {
            format!("http subs={}", sidecar.len())
        },
        play_url: local,
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
