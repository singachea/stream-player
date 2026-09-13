use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::sync::LazyLock;

use regex::Regex;

use crate::error::{Error, Result};
use crate::fetch::{http_get, http_get_retry, http_get_timeout};
use crate::urls::join_url;

static STREAM_INF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)#EXT-X-STREAM-INF:.*?BANDWIDTH=(\d+)(?:.*?RESOLUTION=\d+x(\d+))?")
        .expect("regex")
});
static URI_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"URI="([^"]+)""#).expect("regex"));
static CODECS_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"CODECS="([^"]+)""#).expect("regex"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub bandwidth: u64,
    pub height: u32,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleTrack {
    pub name: String,
    pub language: String,
    pub url: String,
    pub default: bool,
}

pub struct ResolvedHls {
    pub media_url: String,
    pub media_text: String,
    pub subtitles: Vec<SubtitleTrack>,
}

fn hls_attr(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let i = line
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())?;
    let rest = &line[i + needle.len()..];
    if rest.starts_with('"') {
        let end = rest[1..].find('"')?;
        Some(rest[1..1 + end].to_string())
    } else {
        let end = rest.find(',').unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

pub fn parse_subtitle_tracks(playlist: &str, base_url: &str) -> Vec<SubtitleTrack> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in playlist.lines() {
        let line = raw.trim();
        if !line.starts_with("#EXT-X-MEDIA:") {
            continue;
        }
        let ty = hls_attr(line, "TYPE").unwrap_or_default();
        if !ty.eq_ignore_ascii_case("SUBTITLES") {
            continue;
        }
        let Some(uri) = hls_attr(line, "URI") else {
            continue;
        };
        let url = join_url(base_url, &uri);
        if !seen.insert(url.clone()) {
            continue;
        }
        let name = hls_attr(line, "NAME")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "subtitles".into());
        let language = hls_attr(line, "LANGUAGE").unwrap_or_default();
        let default = hls_attr(line, "DEFAULT").is_some_and(|s| s.eq_ignore_ascii_case("YES"));
        out.push(SubtitleTrack {
            name,
            language,
            url,
            default,
        });
    }
    out.sort_by_key(|t| !t.default);
    out
}

fn subtitle_path(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_ascii_lowercase();
    };
    let inner = parsed
        .query_pairs()
        .find(|(k, _)| k == "url")
        .map(|(_, v)| v.into_owned());
    if let Some(inner) = inner {
        if let Ok(u) = url::Url::parse(&inner) {
            return u.path().to_ascii_lowercase();
        }
    }
    parsed.path().to_ascii_lowercase()
}

pub fn is_subtitle_url(url: &str) -> bool {
    let path = subtitle_path(url);
    path.ends_with(".vtt")
        || path.ends_with(".webvtt")
        || path.ends_with(".srt")
        || path.ends_with(".ass")
        || path.ends_with(".ssa")
}

pub fn subtitle_ext(url: &str) -> &'static str {
    let p = url.to_ascii_lowercase();
    if p.contains(".srt") {
        "srt"
    } else if p.contains(".ass") || p.contains(".ssa") {
        "ass"
    } else {
        "vtt"
    }
}

pub fn subtitle_content_type(buf: &[u8]) -> Option<&'static str> {
    let n = buf.len().min(96);
    let s = std::str::from_utf8(&buf[..n]).ok()?.trim_start();
    if s.starts_with("WEBVTT") {
        return Some("text/vtt");
    }
    if s.starts_with("[Script Info]") {
        return Some("text/x-ssa");
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) && s.contains("-->") {
        return Some("application/x-subrip");
    }
    None
}

pub fn local_master_playlist(video_url: &str, tracks: &[(SubtitleTrack, String)]) -> String {
    let mut out = vec!["#EXTM3U".into(), "#EXT-X-INDEPENDENT-SEGMENTS".into()];
    for (t, uri) in tracks {
        let name = t.name.replace('"', "'");
        let lang = if t.language.is_empty() {
            "und"
        } else {
            t.language.as_str()
        };
        let def = if t.default { "YES" } else { "NO" };
        out.push(format!(
            r#"#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="{name}",LANGUAGE="{lang}",DEFAULT={def},AUTOSELECT={def},URI="{uri}""#
        ));
    }
    out.push(r#"#EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES="subs""#.into());
    out.push(video_url.to_string());
    out.push(String::new());
    out.join("\n")
}

fn mpegts_offset(buf: &[u8], limit: usize) -> Option<usize> {
    let end = buf.len().min(limit);
    for off in 0..end {
        if buf[off] != b'G' {
            continue;
        }
        if off + 188 * 3 > buf.len() {
            return None;
        }
        if (0..3).all(|i| buf[off + i * 188] == b'G') {
            return Some(off);
        }
    }
    None
}

fn looks_like_fmp4(buf: &[u8]) -> bool {
    let head = &buf[..buf.len().min(64)];
    head.windows(4)
        .any(|w| w == b"ftyp" || w == b"moof" || w == b"sidx")
}

fn is_ts_at(buf: &[u8], off: usize) -> bool {
    if buf.get(off) != Some(&b'G') {
        return false;
    }
    if off + 188 * 3 > buf.len() {
        return true;
    }
    (0..3).all(|i| buf[off + i * 188] == b'G')
}

fn image_prefix_len(buf: &[u8]) -> usize {
    if buf.starts_with(b"\x89PNG\r\n\x1a\n") {
        if let Some(iend) = buf.windows(4).position(|w| w == b"IEND") {
            return (iend + 8).min(buf.len());
        }
    } else if buf.starts_with(b"\xff\xd8\xff") {
        if let Some(eoi) = buf.windows(2).position(|w| w == b"\xff\xd9") {
            return eoi + 2;
        }
    } else if buf.starts_with(b"GIF87a") || buf.starts_with(b"GIF89a") {
        if let Some(trailer) = buf.iter().position(|&b| b == 0x3b) {
            if trailer > 0 && trailer < 512 {
                return trailer + 1;
            }
        }
    }
    0
}

/// Skip a decoy image header. Return (prefix_len, "ts"|"m4s").
pub fn unwrap_media(buf: &[u8]) -> (usize, &'static str) {
    // fMP4/CMAF before MPEG-TS: 0x47 ('G') every 188 bytes is a common false sync.
    if looks_like_fmp4(buf) {
        return (0, "m4s");
    }
    if is_ts_at(buf, 0) {
        return (0, "ts");
    }
    let skip = image_prefix_len(buf);
    let rest = &buf[skip.min(buf.len())..];
    if looks_like_fmp4(rest) {
        return (skip, "m4s");
    }
    if is_ts_at(rest, 0) {
        return (skip, "ts");
    }
    if let Some(ts_off) = mpegts_offset(rest, 4096) {
        return (skip + ts_off, "ts");
    }
    (skip, "ts")
}

/// Fetch one media segment and return the playable bytes (unwrapped,
/// non-subtitle). Returns `None` for keys, subtitles, or error bodies.
pub fn fetch_segment_body(
    src: &str,
    headers: &mut HashMap<String, String>,
    rest: &str,
) -> Option<Vec<u8>> {
    let (_, body) = http_get_timeout(src, headers, 60).ok()?;
    let body = unwrap_media_body(&body);
    if subtitle_content_type(&body).is_some() {
        return None;
    }
    if !rest.ends_with(".key")
        && !rest.ends_with(".vtt")
        && !rest.ends_with(".srt")
        && looks_like_error_body(&body)
    {
        return None;
    }
    Some(body)
}

fn unwrap_media_body(body: &[u8]) -> Vec<u8> {
    let (skip, _) = unwrap_media(&body[..body.len().min(65536)]);
    let skip = skip.min(body.len());
    body[skip..].to_vec()
}

fn looks_like_error_body(buf: &[u8]) -> bool {
    let start = buf
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .unwrap_or(0);
    let head = buf.get(start..buf.len().min(start + 96)).unwrap_or(&[]);
    head.starts_with(b"<") || head.starts_with(b"{") || head.starts_with(b"[")
}

pub fn sniff_segment_kind(url: &str, headers: &mut HashMap<String, String>) -> &'static str {
    match http_get(url, headers) {
        Ok((_, buf)) => unwrap_media(&buf[..buf.len().min(65536)]).1,
        Err(_) => "ts",
    }
}

pub fn parse_variants(playlist: &str, base_url: &str) -> Vec<Variant> {
    let lines: Vec<&str> = playlist.lines().collect();
    let mut variants = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let (bw, height) = STREAM_INF
                .captures(line)
                .map(|c| {
                    let bw = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                    let height = c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                    (bw, height)
                })
                .unwrap_or((0, 0));
            let uri = if let Some(c) = URI_ATTR.captures(line) {
                Some(c.get(1).unwrap().as_str().to_string())
            } else if i + 1 < lines.len() && !lines[i + 1].starts_with('#') {
                i += 1;
                Some(lines[i].trim().to_string())
            } else {
                i += 1;
                continue;
            };
            if let Some(uri) = uri {
                variants.push(Variant {
                    bandwidth: bw,
                    height,
                    url: join_url(base_url, &uri),
                });
            }
        }
        i += 1;
    }
    variants
}

pub fn pick_variant(variants: &[Variant], quality: &str) -> Result<Variant> {
    if variants.is_empty() {
        return Err(Error::msg("play: no HLS variants in playlist"));
    }
    let q = quality.to_lowercase();
    if q == "best" || q == "live" {
        return Ok(variants
            .iter()
            .max_by_key(|v| (v.bandwidth, v.height))
            .unwrap()
            .clone());
    }
    if q == "worst" {
        return Ok(variants
            .iter()
            .min_by_key(|v| {
                (
                    if v.bandwidth == 0 {
                        u64::MAX
                    } else {
                        v.bandwidth
                    },
                    v.height,
                )
            })
            .unwrap()
            .clone());
    }
    if q.ends_with('p') {
        if let Ok(want) = q[..q.len() - 1].parse::<u32>() {
            return Ok(variants
                .iter()
                .min_by_key(|v| {
                    let h = if v.height == 0 { want } else { v.height };
                    h.abs_diff(want)
                })
                .unwrap()
                .clone());
        }
    }
    Ok(variants
        .iter()
        .max_by_key(|v| (v.bandwidth, v.height))
        .unwrap()
        .clone())
}

pub fn ranked_variants(variants: &[Variant]) -> Vec<Variant> {
    let mut v = variants.to_vec();
    v.sort_by(|a, b| (b.bandwidth, b.height).cmp(&(a.bandwidth, a.height)));
    v
}

pub fn variant_label(v: &Variant) -> String {
    if v.height > 0 {
        format!("{}p", v.height)
    } else if v.bandwidth > 0 {
        format!("{}bps", v.bandwidth)
    } else {
        "unknown".into()
    }
}

pub fn format_quality_menu(variants: &[Variant]) -> String {
    let mut lines = Vec::new();
    for (i, v) in ranked_variants(variants).iter().enumerate() {
        let n = i + 1;
        let bw = if v.bandwidth >= 1000 {
            format!("{} kbps", v.bandwidth / 1000)
        } else {
            String::new()
        };
        let mark = if n == 1 { "  [default]" } else { "" };
        let label = variant_label(v);
        if !bw.is_empty() {
            lines.push(format!("  {n}) {label:<8} {bw}{mark}"));
        } else {
            lines.push(format!("  {n}) {label}{mark}"));
        }
    }
    lines.join("\n")
}

pub fn variant_from_choice(variants: &[Variant], raw: &str) -> Result<Variant> {
    if variants.is_empty() {
        return Err(Error::msg("play: no HLS variants in playlist"));
    }
    let ranked = ranked_variants(variants);
    let s = raw.trim().to_lowercase();
    if s.is_empty() || s == "best" || s == "live" {
        return Ok(ranked[0].clone());
    }
    if let Ok(n) = s.parse::<usize>() {
        if (1..=ranked.len()).contains(&n) {
            return Ok(ranked[n - 1].clone());
        }
        return Err(Error::msg(format!(
            "play: quality {n} not in 1..{}",
            ranked.len()
        )));
    }
    pick_variant(variants, &s)
}

pub fn choose_variant(
    variants: &[Variant],
    quality: Option<&str>,
    interactive: bool,
    mut input_fn: Option<&mut dyn FnMut(&str) -> String>,
) -> Result<Variant> {
    if variants.is_empty() {
        return Err(Error::msg("play: no HLS variants in playlist"));
    }
    if let Some(q) = quality {
        return pick_variant(variants, q);
    }
    if variants.len() == 1 || !interactive {
        return pick_variant(variants, "best");
    }
    eprintln!("play: qualities");
    eprintln!("{}", format_quality_menu(variants));
    let raw = if let Some(f) = input_fn.as_mut() {
        f("quality [1]: ")
    } else {
        eprint!("quality [1]: ");
        let _ = io::stderr().flush();
        let mut s = String::new();
        io::stdin().read_line(&mut s).ok();
        s
    };
    let chosen = variant_from_choice(variants, &raw)?;
    eprintln!("play: {}", variant_label(&chosen));
    Ok(chosen)
}

pub fn rewrite_media_playlist(
    playlist: &str,
    base_url: &str,
    proxy: &str,
    segments: &mut Vec<String>,
    ext: &str,
) -> String {
    let mut add = |uri: &str, file_ext: &str| -> String {
        segments.push(join_url(base_url, uri));
        format!("{proxy}/seg/{:05}.{}", segments.len() - 1, file_ext)
    };
    let mut out: Vec<String> = Vec::new();
    for raw_line in playlist.lines() {
        let mut line = raw_line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXT-X-I-FRAME-STREAM-INF") {
            continue;
        }
        if line.starts_with('#') {
            if line.contains("URI=") {
                let uri_ext = if line.starts_with("#EXT-X-KEY") {
                    "key"
                } else {
                    ext
                };
                line = URI_ATTR
                    .replace_all(&line, |caps: &regex::Captures| {
                        format!(r#"URI="{}""#, add(&caps[1], uri_ext))
                    })
                    .into_owned();
            }
            out.push(line);
            continue;
        }
        out.push(add(&line, ext));
    }
    if !out.iter().any(|l| l.starts_with("#EXT-X-ENDLIST")) {
        out.push("#EXT-X-ENDLIST".into());
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Progress callback: (stage, done, total, detail). `None` disables reporting.
pub type PlayStage = dyn FnMut(&str, usize, usize, Option<String>) + Send;

pub fn resolve_media_playlist<F>(
    url: &str,
    headers: &mut HashMap<String, String>,
    quality: Option<&str>,
    verbose: bool,
    interactive: Option<bool>,
    mut on_403: Option<&mut F>,
    mut on_stage: Option<&mut PlayStage>,
) -> Result<ResolvedHls>
where
    F: FnMut(&str, &mut HashMap<String, String>, &[u8]) -> Option<HashMap<String, String>> + ?Sized,
{
    let mut stage = |name: &str, done: usize, total: usize| {
        if let Some(cb) = on_stage.as_deref_mut() {
            cb(name, done, total, None);
        }
    };
    stage("master", 0, 100);
    let (mut final_url, body) = http_get_retry(url, headers, on_403.as_deref_mut())?;
    let mut text = String::from_utf8_lossy(&body).into_owned();
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(Error::msg(format!(
            "play: not an HLS playlist ({final_url}): {:?}",
            text.chars().take(80).collect::<String>()
        )));
    }
    let subtitles = parse_subtitle_tracks(&text, &final_url);
    stage("master", 25, 100);
    if text.contains("#EXT-X-STREAM-INF") {
        let variants = parse_variants(&text, &final_url);
        if verbose {
            for v in &variants {
                let h = if v.height == 0 {
                    "?".into()
                } else {
                    v.height.to_string()
                };
                eprintln!("  variant {h}p  {}  {}", v.bandwidth, v.url);
            }
        }
        let interactive = interactive.unwrap_or_else(|| io::stdin().is_terminal());
        let chosen = choose_variant(&variants, quality, interactive, None)?;
        if verbose {
            let h = if chosen.height == 0 {
                "?".into()
            } else {
                chosen.height.to_string()
            };
            eprintln!("play: chose {h}p {}", chosen.url);
        }
        let (u, body) = http_get_retry(&chosen.url, headers, on_403)?;
        stage("variant", 60, 100);
        final_url = u;
        text = String::from_utf8_lossy(&body).into_owned();
        if !text.trim_start().starts_with("#EXTM3U") {
            return Err(Error::msg(format!(
                "play: variant is not a playlist: {}",
                chosen.url
            )));
        }
        stage("variant", 100, 100);
    } else {
        stage("master", 100, 100);
    }
    Ok(ResolvedHls {
        media_url: final_url,
        media_text: text,
        subtitles,
    })
}

pub fn first_segment_url(media_url: &str, media_text: &str) -> Option<String> {
    for ln in media_text.lines() {
        let t = ln.trim();
        if !t.is_empty() && !t.starts_with('#') {
            return Some(join_url(media_url, t));
        }
    }
    None
}

/// Init segment if present, else first media URI. Used to sniff container kind.
pub fn map_or_first_segment_url(media_url: &str, media_text: &str) -> Option<String> {
    for ln in media_text.lines() {
        let t = ln.trim();
        if t.starts_with("#EXT-X-MAP:") {
            if let Some(c) = URI_ATTR.captures(t) {
                return Some(join_url(media_url, c.get(1).unwrap().as_str()));
            }
        }
    }
    first_segment_url(media_url, media_text)
}

pub fn playlist_hint(media_text: &str, ext: &str) -> String {
    let mut parts = vec![format!("segment={ext}")];
    if media_text
        .lines()
        .any(|l| l.trim().starts_with("#EXT-X-MAP:"))
    {
        parts.push("map=yes".into());
    }
    if let Some(c) = CODECS_ATTR.captures(media_text) {
        parts.push(format!("codecs={}", c.get(1).unwrap().as_str()));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_ts() {
        let buf = [b'G']
            .into_iter()
            .chain(std::iter::repeat(0u8).take(20))
            .collect::<Vec<_>>();
        let (skip, kind) = unwrap_media(&buf);
        assert_eq!(skip, 0);
        assert_eq!(kind, "ts");
    }

    #[test]
    fn test_png_wrapper() {
        let mut buf = b"\x89PNG\r\n\x1a\nxxxxIEND".to_vec();
        buf.extend_from_slice(&[0, 0, 0, 0, b'G']);
        buf.extend(std::iter::repeat(0u8).take(10));
        let (skip, kind) = unwrap_media(&buf);
        assert_eq!(kind, "ts");
        assert_eq!(&buf[skip..skip + 1], b"G");
    }

    #[test]
    fn test_fmp4_not_misread_as_ts() {
        let mut buf = vec![0, 0, 0, 24];
        buf.extend_from_slice(b"ftypisom");
        buf.resize(188 * 4, 0);
        buf[188] = b'G';
        buf[376] = b'G';
        buf[564] = b'G';
        let (skip, kind) = unwrap_media(&buf);
        assert_eq!(kind, "m4s");
        assert_eq!(skip, 0);
    }

    #[test]
    fn test_is_subtitle_url_unwraps_fetch_query() {
        let wrapped = "https://proxy.example/fetch?url=https%3A%2F%2Fcdn.example%2Fsubtitles%2Fen.vtt&ref=https%3A%2F%2Fembed.example%2F";
        assert!(is_subtitle_url(wrapped));
        assert!(!is_subtitle_url(
            "https://proxy.example/fetch?url=https%3A%2F%2Fcdn.example%2Findex.m3u8"
        ));
        assert!(is_subtitle_url("https://cdn.example/en.vtt"));
    }

    #[test]
    fn test_parse_subtitle_tracks_from_master() {
        let src = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="English",URI="audio.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",DEFAULT=YES,LANGUAGE="en",URI="en.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="Spanish",LANGUAGE="es",URI="https://cdn.example/es.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES="subs"
index.m3u8
"#;
        let tracks = parse_subtitle_tracks(src, "https://cdn.example/hls/master.m3u8");
        assert_eq!(tracks.len(), 2);
        assert!(tracks[0].default);
        assert_eq!(tracks[0].language, "en");
        assert_eq!(tracks[0].url, "https://cdn.example/hls/en.m3u8");
        assert_eq!(tracks[1].url, "https://cdn.example/es.m3u8");
        let master = local_master_playlist(
            "http://127.0.0.1/index.m3u8",
            &[
                (tracks[0].clone(), "http://127.0.0.1/sub/0.m3u8".into()),
                (tracks[1].clone(), "http://127.0.0.1/sub/1.m3u8".into()),
            ],
        );
        assert!(master.contains("TYPE=SUBTITLES"));
        assert!(master.contains(r#"URI="http://127.0.0.1/sub/0.m3u8""#));
        assert!(master.contains(r#"SUBTITLES="subs""#));
        assert!(master.contains("http://127.0.0.1/index.m3u8"));
    }

    #[test]
    fn test_variants_unwrap_proxy_query() {
        let src = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n/proxy?url=https%3A%2F%2Fcdn.example%2Findex.m3u8\n";
        let v = parse_variants(src, "https://embed.example/wrap/token.m3u8");
        assert_eq!(v[0].url, "https://cdn.example/index.m3u8");
    }

    #[test]
    fn test_map_or_first_prefers_init() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2,\nseg1.m4s\n";
        let u = map_or_first_segment_url("https://cdn.example/hls/a.m3u8", text).unwrap();
        assert_eq!(u, "https://cdn.example/hls/init.mp4");
    }

    #[test]
    fn test_png_wrapper_with_ff_padding() {
        let pkt: Vec<u8> = std::iter::once(b'G')
            .chain(std::iter::repeat(0u8).take(187))
            .collect();
        let mut buf = b"\x89PNG\r\n\x1a\nxxxxIEND".to_vec();
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.extend(std::iter::repeat(0xffu8).take(20));
        for _ in 0..4 {
            buf.extend_from_slice(&pkt);
        }
        let (skip, kind) = unwrap_media(&buf);
        assert_eq!(kind, "ts");
        assert_eq!(&buf[skip..skip + 1], b"G");
        assert_eq!(&buf[skip..skip + 188], pkt.as_slice());
    }

    fn sample_variants() -> Vec<Variant> {
        vec![
            Variant {
                bandwidth: 1,
                height: 360,
                url: "a".into(),
            },
            Variant {
                bandwidth: 5,
                height: 720,
                url: "b".into(),
            },
            Variant {
                bandwidth: 9,
                height: 1080,
                url: "c".into(),
            },
        ]
    }

    #[test]
    fn test_pick_best_and_720p() {
        let v = sample_variants();
        assert_eq!(pick_variant(&v, "best").unwrap().url, "c");
        assert_eq!(pick_variant(&v, "worst").unwrap().url, "a");
        assert_eq!(pick_variant(&v, "720p").unwrap().url, "b");
    }

    #[test]
    fn test_quality_menu_lists_best_first_as_default() {
        let variants = vec![
            Variant {
                bandwidth: 800_000,
                height: 360,
                url: "a".into(),
            },
            Variant {
                bandwidth: 5_000_000,
                height: 720,
                url: "b".into(),
            },
            Variant {
                bandwidth: 9_000_000,
                height: 1080,
                url: "c".into(),
            },
        ];
        let menu = format_quality_menu(&variants);
        let lines: Vec<&str> = menu.lines().filter(|ln| !ln.trim().is_empty()).collect();
        assert!(lines[0].contains("1)"));
        assert!(lines[0].contains("1080p"));
        assert!(lines[0].contains("[default]"));
        assert!(lines[1].contains("720p"));
        assert!(!lines[1].contains("[default]"));
        assert_eq!(variant_label(&variants[2]), "1080p");
    }

    #[test]
    fn test_choice_enter_number_or_height() {
        let v = sample_variants();
        assert_eq!(variant_from_choice(&v, "").unwrap().url, "c");
        assert_eq!(variant_from_choice(&v, "  ").unwrap().url, "c");
        assert_eq!(variant_from_choice(&v, "1").unwrap().url, "c");
        assert_eq!(variant_from_choice(&v, "2").unwrap().url, "b");
        assert_eq!(variant_from_choice(&v, "720p").unwrap().url, "b");
        assert_eq!(variant_from_choice(&v, "worst").unwrap().url, "a");
        assert!(variant_from_choice(&v, "9").is_err());
    }

    #[test]
    fn test_choose_variant_prompts_when_quality_omitted() {
        let v = vec![
            Variant {
                bandwidth: 1,
                height: 360,
                url: "a".into(),
            },
            Variant {
                bandwidth: 9,
                height: 1080,
                url: "c".into(),
            },
        ];
        let mut input = |_p: &str| "".to_string();
        let chosen = choose_variant(&v, None, true, Some(&mut input)).unwrap();
        assert_eq!(chosen.url, "c");
        let mut input = |_p: &str| "2".to_string();
        let chosen = choose_variant(&v, None, true, Some(&mut input)).unwrap();
        assert_eq!(chosen.url, "a");
    }

    #[test]
    fn test_choose_variant_skips_prompt_when_quality_given() {
        let v = sample_variants();
        let mut calls = 0;
        let mut input = |_p: &str| {
            calls += 1;
            "1".to_string()
        };
        let chosen = choose_variant(&v, Some("720p"), true, Some(&mut input)).unwrap();
        assert_eq!(chosen.url, "b");
        assert_eq!(calls, 0);
    }

    #[test]
    fn test_choose_variant_no_tty_uses_best() {
        let v = vec![
            Variant {
                bandwidth: 1,
                height: 360,
                url: "a".into(),
            },
            Variant {
                bandwidth: 9,
                height: 1080,
                url: "c".into(),
            },
        ];
        let chosen = choose_variant(&v, None, false, None).unwrap();
        assert_eq!(chosen.url, "c");
    }

    #[test]
    fn test_rewrite_relative_and_endlist() {
        let src = "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:10,\nseg-1.woff2\n#EXTINF:10,\nseg-2.woff2\n";
        let mut segs = Vec::new();
        let out = rewrite_media_playlist(
            src,
            "https://cdn.example/hls/vid/",
            "http://127.0.0.1:9",
            &mut segs,
            "ts",
        );
        assert!(out.contains("http://127.0.0.1:9/seg/00000.ts"));
        assert!(out.contains("#EXT-X-ENDLIST"));
        assert_eq!(segs[0], "https://cdn.example/hls/vid/seg-1.woff2");
    }

    #[test]
    fn test_rewrite_aes128_key_uses_key_ext() {
        let src = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://cdn.example/hls/vid/encryption.key?t=abc\"\n#EXTINF:10,\nseg-1.ts\n";
        let mut segs = Vec::new();
        let out = rewrite_media_playlist(
            src,
            "https://cdn.example/hls/vid/",
            "http://127.0.0.1:9",
            &mut segs,
            "ts",
        );
        assert!(out.contains(r#"URI="http://127.0.0.1:9/seg/00000.key""#));
        assert!(out.contains("http://127.0.0.1:9/seg/00001.ts"));
        assert_eq!(segs[0], "https://cdn.example/hls/vid/encryption.key?t=abc");
        assert_eq!(segs[1], "https://cdn.example/hls/vid/seg-1.ts");
    }
}
