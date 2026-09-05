use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use aes::Aes128;
use cbc::Decryptor;
use cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use regex::Regex;
use std::sync::LazyLock;

use crate::error::{Error, Result};
use crate::fetch::{cancelled_error, current_cancel, http_get, with_cancel, CancelCtx};
use crate::hls::{resolve_media_playlist, unwrap_media};
use crate::urls::join_url;

static MEDIA_SEQ: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#EXT-X-MEDIA-SEQUENCE:(\d+)").expect("regex"));
static KEY_METHOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"METHOD=([^,]+)").expect("regex"));
static KEY_URI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"URI="([^"]+)""#).expect("regex"));
static KEY_IV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"IV=0x([0-9A-Fa-f]+)").expect("regex"));

const TS_EXTS: &[&str] = &[".ts", ".mts", ".m2ts", ".mpeg"];
const MP4_EXTS: &[&str] = &[".mp4", ".m4v", ".mov"];

pub fn plan_download(out_file: &str, has_ffmpeg: bool) -> (String, Option<String>) {
    let path = Path::new(out_file);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}").to_lowercase())
        .unwrap_or_default();
    if TS_EXTS.contains(&ext.as_str()) {
        return (out_file.to_string(), None);
    }
    let final_path = if ext.is_empty() {
        path.with_extension("mp4")
    } else {
        path.to_path_buf()
    };
    let ts = final_path.with_extension("ts");
    if !has_ffmpeg {
        return (ts.to_string_lossy().into_owned(), None);
    }
    (
        ts.to_string_lossy().into_owned(),
        Some(final_path.to_string_lossy().into_owned()),
    )
}

pub fn remux_cmd(ffmpeg: &str, src: &str, dest: &str) -> Vec<String> {
    remux_cmd_opts(ffmpeg, src, dest, false)
}

fn remux_cmd_opts(ffmpeg: &str, src: &str, dest: &str, aac_bsf: bool) -> Vec<String> {
    let mut cmd = vec![
        ffmpeg.to_string(),
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        src.to_string(),
        "-c".into(),
        "copy".into(),
    ];
    if aac_bsf {
        cmd.push("-bsf:a".into());
        cmd.push("aac_adtstoasc".into());
    }
    let ext = Path::new(dest)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}").to_lowercase())
        .unwrap_or_default();
    if MP4_EXTS.contains(&ext.as_str()) {
        cmd.push("-movflags".into());
        cmd.push("+faststart".into());
    }
    cmd.push(dest.to_string());
    cmd
}

fn run_remux(cmd: &[String], dest: &str) -> Result<()> {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::msg(format!("play: remux failed: {e}")))?;
    let pid = child.id();
    if let Some(hook) = current_cancel() {
        hook.track(pid);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::msg(format!("play: remux failed: {e}")))?;
    if let Some(hook) = current_cancel() {
        hook.untrack(pid);
        if hook.is_cancelled() {
            let _ = fs::remove_file(dest);
            return Err(cancelled_error());
        }
    }
    if out.status.success() {
        return Ok(());
    }
    if current_cancel().is_some_and(|c| c.is_cancelled()) {
        let _ = fs::remove_file(dest);
        return Err(cancelled_error());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(Error::msg(format!(
        "play: remux failed: ffmpeg {}{}",
        out.status.code().unwrap_or(1),
        if err.is_empty() {
            String::new()
        } else {
            format!(" {err}")
        }
    )))
}

pub fn remux_copy(src: &str, dest: &str, ffmpeg: &str) -> Result<()> {
    let first = remux_cmd_opts(ffmpeg, src, dest, false);
    match run_remux(&first, dest) {
        Ok(()) => return Ok(()),
        Err(e) if e.to_string().contains("cancelled") => return Err(e),
        Err(first_err) => {
            let ext = Path::new(dest)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}").to_lowercase())
                .unwrap_or_default();
            if !MP4_EXTS.contains(&ext.as_str()) {
                return Err(first_err);
            }
            let retry = remux_cmd_opts(ffmpeg, src, dest, true);
            match run_remux(&retry, dest) {
                Ok(()) => Ok(()),
                Err(e) if e.to_string().contains("cancelled") => Err(e),
                Err(_) => Err(first_err),
            }
        }
    }
}

fn bin_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join("bin"));
        dirs.push(home.join(".local/bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

fn lookup_bin(name: &str) -> Option<PathBuf> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    for dir in bin_search_dirs() {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    let out = Command::new("/bin/zsh")
        .args(["-lc", &format!("command -v {name}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    p.is_file().then_some(p)
}

pub fn find_bin(name: &str) -> Option<PathBuf> {
    if name == "ffmpeg" {
        static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
        return FFMPEG.get_or_init(|| lookup_bin("ffmpeg")).clone();
    }
    lookup_bin(name)
}

#[derive(Clone)]
enum Crypto {
    None,
    Aes128 { uri: String, iv: Option<[u8; 16]> },
}

struct Seg {
    url: String,
    crypto: Crypto,
}

fn parse_iv(hex: &str) -> Option<[u8; 16]> {
    let h = hex.trim();
    if h.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn seq_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn parse_key_line(line: &str, base: &str) -> Result<Crypto> {
    let method = KEY_METHOD
        .captures(line)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_uppercase())
        .unwrap_or_default();
    if method == "NONE" {
        return Ok(Crypto::None);
    }
    if method != "AES-128" {
        return Err(Error::msg(format!(
            "play: unsupported HLS key method {method}"
        )));
    }
    let uri = KEY_URI
        .captures(line)
        .and_then(|c| c.get(1))
        .map(|m| join_url(base, m.as_str()))
        .ok_or_else(|| Error::msg("play: AES-128 key missing URI"))?;
    let iv = KEY_IV
        .captures(line)
        .and_then(|c| c.get(1))
        .and_then(|m| parse_iv(m.as_str()));
    Ok(Crypto::Aes128 { uri, iv })
}

fn decrypt_aes128(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    type Aes128CbcDec = Decryptor<Aes128>;
    let dec =
        Aes128CbcDec::new_from_slices(key, iv).map_err(|_| Error::msg("play: bad AES key/iv"))?;
    dec.decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Error::msg("play: AES decrypt failed"))
}

fn parse_playlist(media_url: &str, media_text: &str) -> Result<(u64, Vec<String>, Vec<Seg>)> {
    let mut seq = 0u64;
    let mut crypto = Crypto::None;
    let mut maps = Vec::new();
    let mut segs = Vec::new();
    for raw in media_text.lines() {
        let line = raw.trim();
        if line.starts_with("#EXT-X-KEY:") {
            crypto = parse_key_line(line, media_url)?;
        } else if line.starts_with("#EXT-X-MAP:") {
            if let Some(c) = KEY_URI.captures(line) {
                maps.push(join_url(media_url, c.get(1).unwrap().as_str()));
            }
        } else if line.starts_with("#EXT-X-MEDIA-SEQUENCE:") {
            if let Some(c) = MEDIA_SEQ.captures(line) {
                seq = c.get(1).unwrap().as_str().parse().unwrap_or(seq);
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            segs.push(Seg {
                url: join_url(media_url, line),
                crypto: crypto.clone(),
            });
        }
    }
    Ok((seq, maps, segs))
}

pub const DEFAULT_WORKERS: usize = 6;
pub const MIN_WORKERS: usize = 1;
pub const MAX_WORKERS: usize = 16;

pub fn clamp_workers(n: usize) -> usize {
    n.clamp(MIN_WORKERS, MAX_WORKERS)
}

pub fn worker_count() -> usize {
    DEFAULT_WORKERS
}

fn cancelled(cancel: Option<&CancelCtx>) -> bool {
    cancel.is_some_and(|c| c.is_cancelled())
}

fn unwrap_payload(body: &[u8]) -> Vec<u8> {
    let (skip, _) = unwrap_media(body);
    body[skip.min(body.len())..].to_vec()
}

fn fetch_one_seg(
    seg: &Seg,
    seq: u64,
    headers: &HashMap<String, String>,
    keys: &HashMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let (_, body) = http_get(&seg.url, headers)?;
    let mut payload = unwrap_payload(&body);
    if let Crypto::Aes128 { uri, iv } = &seg.crypto {
        let key = keys
            .get(uri)
            .ok_or_else(|| Error::msg("play: missing AES key"))?;
        let iv = iv.unwrap_or_else(|| seq_iv(seq));
        payload = decrypt_aes128(key, &iv, &payload)?;
    }
    Ok(payload)
}

/// Fetch segments in parallel, then invoke `on_seg` in playlist order.
fn fetch_segs_parallel(
    segs: &[Seg],
    start_seq: u64,
    headers: &HashMap<String, String>,
    keys: &HashMap<String, Vec<u8>>,
    workers: usize,
    cancel: Option<&CancelCtx>,
    mut on_seg: impl FnMut(usize, Vec<u8>) -> Result<()>,
) -> Result<()> {
    let n = segs.len();
    if n == 0 {
        return Ok(());
    }
    let workers = workers.clamp(1, n);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let slots: Vec<Mutex<Option<Result<Vec<u8>>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let hook = cancel.cloned();

    thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            scope.spawn(|| {
                with_cancel(hook.as_ref(), || loop {
                    if stop.load(Ordering::Relaxed) || cancelled(hook.as_ref()) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let result = fetch_one_seg(&segs[i], start_seq + i as u64, headers, keys);
                    if result.is_err() {
                        stop.store(true, Ordering::Relaxed);
                    }
                    *slots[i].lock().unwrap() = Some(result);
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                })
            });
        }

        let mut written = 0usize;
        while written < n {
            if cancelled(cancel) {
                stop.store(true, Ordering::Relaxed);
                return Err(cancelled_error());
            }
            let taken = slots[written].lock().unwrap().take();
            match taken {
                Some(Ok(data)) => {
                    if let Err(e) = on_seg(written, data) {
                        stop.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                    written += 1;
                }
                Some(Err(e)) => {
                    stop.store(true, Ordering::Relaxed);
                    return Err(e);
                }
                None => thread::sleep(Duration::from_millis(4)),
            }
        }
        Ok(())
    })
}

pub fn download_media(
    media_url: &str,
    media_text: &str,
    headers: &mut HashMap<String, String>,
    out_file: &str,
    verbose: bool,
    progress: impl FnMut(usize, usize, u64, &str),
    cancel: Option<&CancelCtx>,
    workers: usize,
) -> Result<String> {
    download_media_inner(
        media_url,
        media_text,
        headers,
        out_file,
        verbose,
        progress,
        cancel,
        clamp_workers(workers),
    )
}

fn download_media_inner(
    media_url: &str,
    media_text: &str,
    headers: &mut HashMap<String, String>,
    out_file: &str,
    verbose: bool,
    progress: impl FnMut(usize, usize, u64, &str),
    cancel: Option<&CancelCtx>,
    workers: usize,
) -> Result<String> {
    with_cancel(cancel, || {
        download_media_body(
            media_url, media_text, headers, out_file, verbose, progress, cancel, workers,
        )
    })
}

fn download_media_body(
    media_url: &str,
    media_text: &str,
    headers: &mut HashMap<String, String>,
    out_file: &str,
    _verbose: bool,
    mut progress: impl FnMut(usize, usize, u64, &str),
    cancel: Option<&CancelCtx>,
    workers: usize,
) -> Result<String> {
    if cancelled(cancel) {
        return Err(cancelled_error());
    }
    let ffmpeg = find_bin("ffmpeg");
    let (ts_out, remux_to) = plan_download(out_file, ffmpeg.is_some());
    let wanted_ts = Path::new(out_file)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| TS_EXTS.contains(&format!(".{e}").to_lowercase().as_str()))
        .unwrap_or(false);
    let final_path = remux_to.clone().unwrap_or_else(|| ts_out.clone());
    if remux_to.is_none() && !wanted_ts {
        return Err(Error::msg(
            "play: ffmpeg not found; install it with `brew install ffmpeg` to save mp4",
        ));
    }
    if let Some(ff) = ffmpeg.as_ref() {
        eprintln!("play: writing {final_path} via {}", ff.display());
    } else {
        eprintln!("play: writing {final_path}");
    }

    let (seq, maps, segs) = parse_playlist(media_url, media_text)?;
    if segs.is_empty() {
        return Err(Error::msg("play: no segments in playlist"));
    }
    let total = maps.len() + segs.len();
    let mut done = 0usize;
    let mut bytes = 0u64;
    let mut file = File::create(&ts_out)?;
    struct CancelCleanup {
        ts_out: String,
        extra: Option<String>,
        keep: bool,
    }
    impl Drop for CancelCleanup {
        fn drop(&mut self) {
            if self.keep || !current_cancel().is_some_and(|c| c.is_cancelled()) {
                return;
            }
            let _ = fs::remove_file(&self.ts_out);
            if let Some(p) = &self.extra {
                let _ = fs::remove_file(p);
            }
        }
    }
    let mut cleanup = CancelCleanup {
        ts_out: ts_out.clone(),
        extra: remux_to.clone(),
        keep: false,
    };

    let abort = |file: File, ts_out: &str, remux_to: &Option<String>| -> Result<String> {
        drop(file);
        let _ = fs::remove_file(ts_out);
        if let Some(d) = remux_to {
            let _ = fs::remove_file(d);
        }
        Err(cancelled_error())
    };

    for map_url in &maps {
        if cancelled(cancel) {
            return abort(file, &ts_out, &remux_to);
        }
        let (_, body) = http_get(map_url, headers)?;
        let payload = unwrap_payload(&body);
        file.write_all(&payload)?;
        done += 1;
        bytes += payload.len() as u64;
        progress(done, total, bytes, &ts_out);
    }

    let mut keys: HashMap<String, Vec<u8>> = HashMap::new();
    for seg in &segs {
        if let Crypto::Aes128 { uri, .. } = &seg.crypto {
            if keys.contains_key(uri) {
                continue;
            }
            if cancelled(cancel) {
                return abort(file, &ts_out, &remux_to);
            }
            let (_, key) = http_get(uri, headers)?;
            keys.insert(uri.clone(), key);
        }
    }

    let mut write_err: Option<Error> = None;
    fetch_segs_parallel(&segs, seq, headers, &keys, workers, cancel, |_, payload| {
        if cancelled(cancel) {
            return Err(cancelled_error());
        }
        file.write_all(&payload)?;
        done += 1;
        bytes += payload.len() as u64;
        progress(done, total, bytes, &ts_out);
        Ok(())
    })
    .unwrap_or_else(|e| write_err = Some(e));
    if let Some(e) = write_err {
        if cancelled(cancel) || e.to_string().contains("cancelled") {
            return abort(file, &ts_out, &remux_to);
        }
        return Err(e);
    }
    drop(file);

    if cancelled(cancel) {
        let _ = fs::remove_file(&ts_out);
        if let Some(d) = &remux_to {
            let _ = fs::remove_file(d);
        }
        return Err(cancelled_error());
    }

    if let (Some(dest), Some(ff)) = (remux_to.as_ref(), ffmpeg.as_ref()) {
        eprintln!("play: remux {ts_out} -> {dest} with {}", ff.display());
        match remux_copy(&ts_out, dest, &ff.to_string_lossy()) {
            Ok(()) => {
                let _ = fs::remove_file(&ts_out);
                cleanup.keep = true;
                progress(total, total, bytes, dest);
                return Ok(dest.clone());
            }
            Err(e) => {
                if cancelled(cancel) || e.to_string().contains("cancelled") {
                    let _ = fs::remove_file(&ts_out);
                    let _ = fs::remove_file(dest);
                    return Err(cancelled_error());
                }
                eprintln!("{e}");
                return Err(e);
            }
        }
    }
    cleanup.keep = true;
    Ok(ts_out)
}

pub fn download_http(
    url: &str,
    headers: &HashMap<String, String>,
    out_file: &str,
    mut progress: impl FnMut(usize, usize, u64, &str),
    cancel: Option<&CancelCtx>,
) -> Result<String> {
    with_cancel(cancel, || {
        if cancelled(cancel) {
            return Err(cancelled_error());
        }
        eprintln!("play: writing {out_file}");
        crate::fetch::http_get_to_file(url, headers, out_file)?;
        if cancelled(cancel) {
            let _ = fs::remove_file(out_file);
            return Err(cancelled_error());
        }
        let bytes = fs::metadata(out_file).map(|m| m.len()).unwrap_or(0);
        progress(1, 1, bytes, out_file);
        Ok(out_file.to_string())
    })
}

pub fn download_stream<F>(
    url: &str,
    headers: &mut HashMap<String, String>,
    quality: Option<&str>,
    out_file: &str,
    verbose: bool,
    interactive: bool,
    on_403: Option<&mut F>,
    progress: impl FnMut(usize, usize, u64, &str),
    cancel: Option<&CancelCtx>,
    workers: usize,
) -> Result<String>
where
    F: FnMut(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>> + ?Sized,
{
    match crate::urls::detect_kind(url) {
        crate::urls::Kind::Http => download_http(url, headers, out_file, progress, cancel),
        crate::urls::Kind::Dash => Err(Error::msg("play: DASH download is not supported")),
        crate::urls::Kind::Hls => match resolve_media_playlist(
            crate::urls::strip_proto(url),
            headers,
            quality,
            verbose,
            Some(interactive),
            on_403,
        ) {
            Ok(resolved) => with_cancel(cancel, || {
                download_media(
                    &resolved.media_url,
                    &resolved.media_text,
                    headers,
                    out_file,
                    verbose,
                    progress,
                    cancel,
                    workers,
                )
            }),
            Err(Error::Progressive { .. }) => {
                download_http(url, headers, out_file, progress, cancel)
            }
            Err(e) => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_workers_is_six() {
        assert_eq!(DEFAULT_WORKERS, 6);
        assert_eq!(worker_count(), 6);
        assert_eq!(clamp_workers(0), 1);
        assert_eq!(clamp_workers(6), 6);
        assert_eq!(clamp_workers(99), 16);
    }

    #[test]
    fn test_explicit_ts_is_not_remuxed() {
        let (sl_out, remux_to) = plan_download("episode.ts", true);
        assert_eq!(sl_out, "episode.ts");
        assert!(remux_to.is_none());
    }

    #[test]
    fn test_mp4_with_ffmpeg_dumps_ts_then_remuxes() {
        let (sl_out, remux_to) = plan_download("episode.mp4", true);
        assert_eq!(Path::new(&sl_out).extension().unwrap(), "ts");
        assert_eq!(remux_to.as_deref(), Some("episode.mp4"));
        assert_ne!(sl_out, remux_to.unwrap());
    }

    #[test]
    fn test_mp4_without_ffmpeg_keeps_ts() {
        let (sl_out, remux_to) = plan_download("episode.mp4", false);
        assert_eq!(sl_out, "episode.ts");
        assert!(remux_to.is_none());
    }

    #[test]
    fn test_explicit_mkv_remuxes_when_ffmpeg_exists() {
        let (sl_out, remux_to) = plan_download("episode.mkv", true);
        assert_eq!(Path::new(&sl_out).extension().unwrap(), "ts");
        assert_eq!(remux_to.as_deref(), Some("episode.mkv"));
    }

    #[test]
    fn test_mp4_copy_uses_faststart() {
        let cmd = remux_cmd("/usr/bin/ffmpeg", "a.ts", "a.mp4");
        assert_eq!(cmd[0], "/usr/bin/ffmpeg");
        assert!(cmd.iter().any(|a| a == "-c"));
        assert!(cmd.iter().any(|a| a == "copy"));
        assert!(cmd.iter().any(|a| a == "+faststart"));
        assert_eq!(cmd.last().unwrap(), "a.mp4");
    }

    #[test]
    fn test_mkv_copy_skips_faststart() {
        let cmd = remux_cmd("/usr/bin/ffmpeg", "a.ts", "a.mkv");
        assert!(!cmd.iter().any(|a| a == "+faststart"));
        assert_eq!(cmd.last().unwrap(), "a.mkv");
    }

    #[test]
    fn test_mp4_retry_cmd_adds_aac_bitstream_filter() {
        let cmd = remux_cmd_opts("/usr/bin/ffmpeg", "a.ts", "a.mp4", true);
        assert!(cmd
            .windows(2)
            .any(|w| w[0] == "-bsf:a" && w[1] == "aac_adtstoasc"));
        assert!(cmd.iter().any(|a| a == "+faststart"));
    }

    #[test]
    fn test_bin_search_includes_homebrew_and_usr_local() {
        let dirs = bin_search_dirs();
        assert!(dirs
            .iter()
            .any(|p| p.ends_with("homebrew/bin") || p == Path::new("/usr/local/bin")));
    }

    #[test]
    fn test_mpegts_becomes_mp4() {
        let Some(ffmpeg) = find_bin("ffmpeg") else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("clip.ts");
        let dest = tmp.path().join("clip.mp4");
        let st = Command::new(&ffmpeg)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=0.4:size=64x64:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.4",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-shortest",
                src.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(st.success());
        remux_copy(
            src.to_str().unwrap(),
            dest.to_str().unwrap(),
            ffmpeg.to_str().unwrap(),
        )
        .unwrap();
        assert!(dest.is_file());
        assert!(dest.metadata().unwrap().len() > 0);
        let out = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-i",
                dest.to_str().unwrap(),
                "-f",
                "null",
                "-",
            ])
            .output()
            .unwrap();
        let info = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(Regex::new(r"Audio:\s*aac").unwrap().is_match(&info));
        assert!(Regex::new(r"Video:\s*h264").unwrap().is_match(&info));
    }

    #[test]
    fn test_cancel_before_fetch_errors() {
        let cancel = CancelCtx::new();
        cancel.request();
        let mut headers = HashMap::new();
        let err = download_media(
            "http://127.0.0.1/pl.m3u8",
            "#EXTM3U\n#EXTINF:1,\nseg.ts\n",
            &mut headers,
            "out.ts",
            false,
            |_, _, _, _| {},
            Some(&cancel),
            worker_count(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn test_cancel_kills_in_flight_fetch() {
        use std::io::Cursor;
        use std::sync::mpsc;
        use tiny_http::{Header, Response, Server, StatusCode};

        let curl_ok = Command::new("curl")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !curl_ok {
            return;
        }

        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let _ = started_tx.send(());
                std::thread::sleep(std::time::Duration::from_secs(8));
                let body = b"XXXX".repeat(32);
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

        let base = format!("http://127.0.0.1:{port}/pl.m3u8");
        let media = format!("#EXTM3U\n#EXTINF:1,\nhttp://127.0.0.1:{port}/seg/0\n");
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("clip.ts");
        let cancel = CancelCtx::new();
        let cancel2 = cancel.clone();
        let dest_s = dest.to_string_lossy().into_owned();
        let handle = std::thread::spawn(move || {
            let mut headers = HashMap::from([("User-Agent".into(), "t".into())]);
            download_media(
                &base,
                &media,
                &mut headers,
                &dest_s,
                false,
                |_, _, _, _| {},
                Some(&cancel2),
                worker_count(),
            )
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("server should see the fetch");
        let t0 = std::time::Instant::now();
        cancel.request();
        let err = handle.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("cancelled"));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "cancel should abort the in-flight curl, took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn test_parallel_writes_segments_in_playlist_order() {
        use std::io::Cursor;
        use tiny_http::{Header, Response, Server, StatusCode};

        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let url = request.url().to_string();
                if url.ends_with("/0") {
                    std::thread::sleep(std::time::Duration::from_millis(120));
                }
                let body = if url.ends_with("/0") {
                    b"AAAA".repeat(48)
                } else if url.ends_with("/1") {
                    b"BBBB".repeat(48)
                } else {
                    b"CCCC".repeat(48)
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

        let base = format!("http://127.0.0.1:{port}/pl.m3u8");
        let media = format!(
            "#EXTM3U\n#EXTINF:1,\nhttp://127.0.0.1:{port}/seg/0\n\
             #EXTINF:1,\nhttp://127.0.0.1:{port}/seg/1\n\
             #EXTINF:1,\nhttp://127.0.0.1:{port}/seg/2\n"
        );
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("clip.ts");
        let mut headers = HashMap::from([("User-Agent".into(), "t".into())]);
        let mut ticks = Vec::new();
        download_media_inner(
            &base,
            &media,
            &mut headers,
            dest.to_str().unwrap(),
            false,
            |done, total, _bytes, _path| ticks.push((done, total)),
            None,
            4,
        )
        .unwrap();
        let got = fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend(b"AAAA".repeat(48));
        expect.extend(b"BBBB".repeat(48));
        expect.extend(b"CCCC".repeat(48));
        assert_eq!(got, expect, "segments must be written in playlist order");
        assert_eq!(ticks.last().copied(), Some((3, 3)));
    }
}
