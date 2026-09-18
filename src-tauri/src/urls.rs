use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use url::Url;

use crate::error::{Error, Result};

pub const UA_DEFAULT: &str = concat!(
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) ",
    "AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/131.0.0.0 Safari/537.36"
);

static GENERIC_PLAYLIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(master|index|playlist|chunklist|manifest|stream|video)([._-].*)?$")
        .expect("regex")
});

const URL_PREFIXES: &[&str] = &[
    "http://",
    "https://",
    "hls://",
    "hlsvariant://",
    "httpstream://",
    "dash://",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hls,
    Http,
    Dash,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Hls => "hls",
            Kind::Http => "http",
            Kind::Dash => "dash",
        }
    }
}

pub fn is_url(s: &str) -> bool {
    URL_PREFIXES.iter().any(|p| s.starts_with(p))
}

pub fn strip_proto(url: &str) -> &str {
    for p in ["hlsvariant://", "hls://", "httpstream://", "dash://"] {
        if let Some(rest) = url.strip_prefix(p) {
            return rest;
        }
    }
    url
}

pub fn origin_of(url: &str) -> Result<String> {
    let u = Url::parse(strip_proto(url))
        .map_err(|_| Error::msg(format!("play: could not parse host from {url}")))?;
    if u.scheme().is_empty() || u.host_str().is_none() {
        return Err(Error::msg(format!("play: could not parse host from {url}")));
    }
    Ok(format!("{}://{}", u.scheme(), u.authority()))
}

pub fn detect_kind(url: &str) -> Kind {
    let raw = strip_proto(url);
    if url.starts_with("hls://") || url.starts_with("hlsvariant://") {
        return Kind::Hls;
    }
    if url.starts_with("httpstream://") {
        return Kind::Http;
    }
    if url.starts_with("dash://") {
        return Kind::Dash;
    }
    let path = Url::parse(raw)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    let base = Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if base.ends_with(".m3u8") || base.ends_with(".m3u") || base.ends_with(".txt") {
        Kind::Hls
    } else if base.ends_with(".mpd") {
        Kind::Dash
    } else if [".mp4", ".mkv", ".webm", ".m4v", ".mov", ".avi"]
        .iter()
        .any(|e| base.ends_with(e))
    {
        Kind::Http
    } else {
        Kind::Hls
    }
}

pub fn auto_filename(url: &str) -> String {
    let path = Url::parse(strip_proto(url))
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let suffix = parts
        .last()
        .map(|p| {
            Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default()
                .to_lowercase()
        })
        .unwrap_or_default();
    if [".mp4", ".mkv", ".webm", ".m4v", ".mov", ".avi"]
        .iter()
        .any(|e| suffix == *e)
    {
        return parts.last().unwrap().to_string();
    }
    let ext = ".mp4";
    if parts.is_empty() {
        return format!("stream{ext}");
    }
    let mut stem = Path::new(parts[parts.len() - 1])
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(parts[parts.len() - 1])
        .to_string();
    if GENERIC_PLAYLIST.is_match(&stem) && parts.len() >= 2 {
        stem = Path::new(parts[parts.len() - 2])
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(parts[parts.len() - 2])
            .to_string();
    }
    format!("{stem}{ext}")
}

/// Some CDNs encode a byte range in the query instead of a `Range` header:
/// okcdn uses `bytes=START-END`, Instagram/Facebook `bytestart`/`byteend`.
/// A captured URL like `...&bytestart=818&byteend=909` is only that slice:
/// VLC gets a truncated file and reports `avcodec demux error` / `cannot seek`.
/// Drop those params so playback and download fetch the whole file; VLC seeks
/// with Range headers. Remaining query pairs are kept byte-for-byte so CDN
/// signatures (`oh`, `efg`, …) are not re-encoded.
pub fn strip_range_query(url: &str) -> String {
    let Some((base, rest)) = strip_proto(url).split_once('?') else {
        return url.to_string();
    };
    let (query, frag) = match rest.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (rest, None),
    };
    let pairs: Vec<&str> = query.split('&').collect();
    let kept: Vec<&str> = pairs
        .iter()
        .copied()
        .filter(|pair| !is_byte_range_query_key(pair.split('=').next().unwrap_or("")))
        .collect();
    if kept.len() == pairs.len() {
        return url.to_string();
    }
    let mut out = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

fn is_byte_range_query_key(k: &str) -> bool {
    k.eq_ignore_ascii_case("bytes")
        || k.eq_ignore_ascii_case("bytestart")
        || k.eq_ignore_ascii_case("byteend")
}

/// nginx WAF 400s on `_` in the query when the path contains `=`.
fn waf_query(q: &str) -> String {
    q.replace('_', "%5F")
}

fn raw_request_target(u: &Url) -> String {
    match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    }
}

/// Origin URL plus a request-target with WAF-safe query encoding
/// (`_` → `%5F` when the path contains `=`). The path is left as parsed:
/// nginx-vod urlset paths use commas, and encoding them as `%2C` makes
/// signed `.ts` hashes fail with HTTP 471. Pass to curl as
/// `--request-target TARGET` plus the **full** URL (not `ORIGIN/`) so
/// `%{url_effective}` keeps the playlist path for relative segment joins.
pub fn curl_connect_and_target(raw: &str) -> (String, String) {
    let Ok(u) = Url::parse(strip_proto(raw)) else {
        return (raw.to_string(), String::new());
    };
    let connect = format!("{}://{}/", u.scheme(), u.authority());
    let path = u.path().to_string();
    let target = match u.query() {
        Some(q) => {
            let q = if u.path().contains('=') {
                waf_query(q)
            } else {
                q.to_string()
            };
            format!("{path}?{q}")
        }
        None => path,
    };
    (connect, target)
}

/// Curl URL arguments: the original URL, plus `--request-target` only when
/// WAF encoding actually changes the path/query. Always pass the full URL
/// so curl's effective URL still has the playlist directory.
pub fn curl_url_args(raw: &str) -> Vec<String> {
    let fetch = strip_proto(raw);
    let Ok(u) = Url::parse(fetch) else {
        return vec![raw.to_string()];
    };
    let (_, target) = curl_connect_and_target(fetch);
    if target.is_empty() || target == raw_request_target(&u) {
        vec![fetch.to_string()]
    } else {
        vec!["--request-target".into(), target, fetch.to_string()]
    }
}

/// curl `%{url_effective}` is the URL argument, which used to be `https://host/`
/// when using `--request-target`. Relative HLS segments must be joined against
/// the playlist path, not the origin.
pub fn restore_effective_url(original: &str, effective: &str) -> String {
    let orig_s = strip_proto(original);
    let Ok(orig) = Url::parse(orig_s) else {
        return effective.to_string();
    };
    let Ok(eff) = Url::parse(effective) else {
        return orig_s.to_string();
    };
    let orig_path = orig.path();
    let eff_path = eff.path();
    if (eff_path == "/" || eff_path.is_empty()) && orig_path != "/" && !orig_path.is_empty() {
        orig_s.to_string()
    } else {
        effective.to_string()
    }
}

/// Inner `url=` on `/proxy` and `/fetch` wrappers, else `url`.
pub fn unwrap_resource_url(url: &str) -> String {
    let Ok(u) = Url::parse(url) else {
        return url.to_string();
    };
    let Some(inner) = u
        .query_pairs()
        .find(|(k, _)| k == "url")
        .map(|(_, v)| v.into_owned())
    else {
        return url.to_string();
    };
    if inner.starts_with("http://") || inner.starts_with("https://") {
        inner
    } else {
        url.to_string()
    }
}

pub fn join_url(base: &str, rel: &str) -> String {
    let joined = if let Ok(b) = Url::parse(base) {
        b.join(rel)
            .map(|j| j.to_string())
            .unwrap_or_else(|_| rel.to_string())
    } else {
        rel.to_string()
    };
    unwrap_resource_url(&joined)
}

/// Headers extracted from a browser "Copy as cURL" command.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedCurl {
    pub url: String,
    pub referer: Option<String>,
    pub origin: Option<String>,
    pub user_agent: Option<String>,
    pub cookie: Option<String>,
    pub cookie_host: Option<String>,
    #[serde(default)]
    pub subtitles: Option<Vec<String>>,
}

fn looks_like_curl(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("curl ")
        || t.starts_with("curl\t")
        || t.starts_with("curl\n")
        || t.starts_with("curl.exe")
}

fn tokenize_shell(input: &str) -> Vec<String> {
    let s = input.replace("\\\r\n", " ").replace("\\\n", " ");
    let chars: Vec<char> = s.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] == '\'' || chars[i] == '"' {
            let q = chars[i];
            i += 1;
            let mut tok = String::new();
            while i < chars.len() && chars[i] != q {
                if chars[i] == '\\' && q == '"' && i + 1 < chars.len() {
                    i += 1;
                    tok.push(chars[i]);
                    i += 1;
                } else {
                    tok.push(chars[i]);
                    i += 1;
                }
            }
            if i < chars.len() {
                i += 1;
            }
            tokens.push(tok);
        } else {
            let mut tok = String::new();
            while i < chars.len() && !chars[i].is_whitespace() {
                tok.push(chars[i]);
                i += 1;
            }
            tokens.push(tok);
        }
    }
    tokens
}

fn header_pair(raw: &str) -> Option<(String, String)> {
    let (k, v) = raw.split_once(':')?;
    let k = k.trim();
    let v = v.trim();
    if k.is_empty() || v.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

/// Parse Chrome/Safari "Copy as cURL". Returns None if this is not a curl command.
pub fn parse_curl(text: &str) -> Option<ParsedCurl> {
    if !looks_like_curl(text) {
        return None;
    }
    let tokens = tokenize_shell(text);
    if tokens.is_empty() {
        return None;
    }
    let mut url = None;
    let mut referer = None;
    let mut origin = None;
    let mut user_agent = None;
    let mut cookie = None;
    let mut i = 1; // skip curl / curl.exe
    if tokens[0].eq_ignore_ascii_case("curl.exe") || tokens[0].eq_ignore_ascii_case("curl") {
        // ok
    } else {
        i = 0;
    }
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let next = tokens.get(i + 1).map(|s| s.as_str());
        if matches!(t, "-H" | "--header") {
            if let Some((k, v)) = next.and_then(header_pair) {
                match k.to_ascii_lowercase().as_str() {
                    "referer" => referer = Some(v),
                    "origin" => origin = Some(v),
                    "user-agent" => user_agent = Some(v),
                    "cookie" => cookie = Some(v),
                    _ => {}
                }
                i += 2;
                continue;
            }
        } else if matches!(t, "-e" | "--referer") {
            if let Some(v) = next {
                referer = Some(v.to_string());
                i += 2;
                continue;
            }
        } else if matches!(t, "-A" | "--user-agent") {
            if let Some(v) = next {
                user_agent = Some(v.to_string());
                i += 2;
                continue;
            }
        } else if matches!(t, "-b" | "--cookie") {
            if let Some(v) = next.filter(|s| !s.starts_with('-')) {
                cookie = Some(v.to_string());
                i += 2;
                continue;
            }
        } else if t == "--url" {
            if let Some(v) = next {
                url = Some(v.to_string());
                i += 2;
                continue;
            }
        } else if t.starts_with('-') {
            // skip unknown flags; flags with values we don't know stay as one token
            i += 1;
            continue;
        } else if url.is_none() && (t.starts_with("http://") || t.starts_with("https://")) {
            url = Some(t.to_string());
        }
        i += 1;
    }
    Some(ParsedCurl {
        url: url?,
        referer,
        origin,
        user_agent,
        cookie,
        cookie_host: None,
        subtitles: None,
    })
}

fn http_https(s: &str) -> Option<String> {
    let s = s.trim();
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    Url::parse(s).ok()?;
    Some(s.to_string())
}

fn capture_from_pairs<'a>(
    pairs: impl Iterator<Item = (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)>,
) -> Option<ParsedCurl> {
    let mut url = None;
    let mut referer = None;
    let mut origin = None;
    for (k, v) in pairs {
        match k.as_ref() {
            "url" => url = http_https(&v),
            "referer" | "initiator" => referer = http_https(&v),
            "origin" => origin = http_https(&v),
            _ => {}
        }
    }
    Some(ParsedCurl {
        url: url?,
        referer,
        origin,
        user_agent: None,
        cookie: None,
        cookie_host: None,
        subtitles: None,
    })
}

/// `play://open?url=...&referer=...&origin=...` from the extension fallback.
pub fn parse_capture_url(raw: &str) -> Option<ParsedCurl> {
    let parsed = Url::parse(raw.trim()).ok()?;
    if parsed.scheme() != "play" {
        return None;
    }
    capture_from_pairs(parsed.query_pairs())
}

/// Query string from `GET /open?url=...` on the loopback capture port.
pub fn parse_capture_query(query: &str) -> Option<ParsedCurl> {
    let q = query.trim().trim_start_matches('?');
    if q.is_empty() {
        return None;
    }
    let dummy = format!("http://127.0.0.1/open?{q}");
    let parsed = Url::parse(&dummy).ok()?;
    capture_from_pairs(parsed.query_pairs())
}

/// JSON `{url, referer?, frame?, origin?, initiator?, userAgent?, cookie?, cookieHost?, subtitles?}` from the extension POST.
pub fn parse_capture_json(body: &str) -> Option<ParsedCurl> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        url: String,
        referer: Option<String>,
        frame: Option<String>,
        origin: Option<String>,
        initiator: Option<String>,
        user_agent: Option<String>,
        cookie: Option<String>,
        cookie_host: Option<String>,
        subtitles: Option<Vec<String>>,
    }
    let b: Body = serde_json::from_str(body.trim()).ok()?;
    let url = http_https(&b.url)?;
    // Prefer the actual player frame over the site root: some CDNs
    // allowlist the exact embed path.
    let referer = b
        .frame
        .as_deref()
        .and_then(http_https)
        .or_else(|| b.referer.as_deref().and_then(http_https))
        .or_else(|| b.initiator.as_deref().and_then(http_https));
    let origin = b.origin.as_deref().and_then(http_https);
    let user_agent = b
        .user_agent
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let cookie = b
        .cookie
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Host that owned the request the cookie was read from. Exact-request
    // cookies must be replayed even cross-site.
    let cookie_host = b
        .cookie_host
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let subtitles = b.subtitles.map(|v| {
        v.into_iter()
            .filter_map(|s| http_https(&s))
            .collect::<Vec<_>>()
    });
    let subtitles = match subtitles {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    };
    Some(ParsedCurl {
        url,
        referer,
        origin,
        user_agent,
        cookie,
        cookie_host,
        subtitles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_url() {
        assert!(is_url("https://cdn.example/a.m3u8"));
        assert!(is_url("hls://https://cdn.example/master.txt"));
        assert!(!is_url("master.txt"));
    }

    #[test]
    fn test_default_ua_looks_like_a_browser() {
        assert!(UA_DEFAULT.contains("Mozilla/5.0 ("));
        assert!(UA_DEFAULT.contains("Chrome/"));
        assert!(UA_DEFAULT.len() > 40);
    }

    #[test]
    fn test_curl_connect_and_target_encodes_query_underscore() {
        let (base, target) =
            curl_connect_and_target("https://cdn.example/clip=.mp4?md5=token_id&expires=1");
        assert_eq!(base, "https://cdn.example/");
        assert!(target.starts_with("/clip=.mp4?"));
        assert!(target.contains("md5=token%5Fid"));
        assert!(target.contains("expires=1"));
        assert!(!target.contains("token_id&"));
        assert!(!target.contains("token_id"));
    }

    #[test]
    fn test_curl_url_args_plain_hls_is_the_full_url() {
        let url = "https://cdn.example/hls/vid/index_720p.m3u8?token=abc_def";
        assert_eq!(curl_url_args(url), vec![url]);
        let (base, target) = curl_connect_and_target(url);
        assert_eq!(base, "https://cdn.example/");
        assert!(
            target.starts_with("/hls/vid/index_720p.m3u8?"),
            "keep underscore in playlist filename, got {target}"
        );
        assert!(target.contains("abc_def"), "do not WAF-encode HLS tokens");
        assert!(!target.contains("%5F"));
    }

    #[test]
    fn test_curl_url_args_keeps_urlset_commas() {
        // nginx-vod urlset paths use commas. Percent-encoding them in
        // --request-target makes the signed hash fail on .ts (HTTP 471).
        let url = "https://cdn.example/_hls/v/id-,426-240-312,640-360,-h264.mp4.urlset/seg-1-f4-v1-a1.ts?validfrom=1&hash=A%2F00j5C5e4SWvoWyhESwGXVEB6k%3D";
        let args = curl_url_args(url);
        assert_eq!(
            args,
            vec![url],
            "comma paths must not force --request-target, got {args:?}"
        );
        assert!(!args.iter().any(|a| a.contains("%2C")));
        let joined = join_url(
            "https://cdn.example/_hls/v/id-,426-240-312,640-360,-h264.mp4.urlset/index-f4-v1-a1.m3u8?validfrom=1&hash=A%2F00j5C5e4SWvoWyhESwGXVEB6k%3D",
            "seg-1-f4-v1-a1.ts?validfrom=1&hash=A%2F00j5C5e4SWvoWyhESwGXVEB6k%3D",
        );
        assert!(
            joined.contains("hash=A%2F00j5C5e4SWvoWyhESwGXVEB6k%3D"),
            "signed query encoding must stay: {joined}"
        );
        assert!(
            joined.contains("426-240-312"),
            "urlset commas must stay: {joined}"
        );
    }

    #[test]
    fn test_curl_url_args_waf_keeps_full_url() {
        let url = "https://cdn.example/clip=.mp4?md5=token_id&expires=1";
        let args = curl_url_args(url);
        assert_eq!(args[0], "--request-target");
        assert!(args[1].contains("token%5Fid"));
        assert_eq!(args[2], url);
        assert!(!args.iter().any(|a| a == "https://cdn.example/"));
    }

    #[test]
    fn test_restore_effective_url_if_curl_reported_origin() {
        let orig = "https://cdn.example/hls/vid/index.m3u8?token=abc";
        assert_eq!(restore_effective_url(orig, "https://cdn.example/"), orig);
        assert_eq!(
            restore_effective_url(orig, "https://cdn.example/hls/vid/index.m3u8?token=abc"),
            "https://cdn.example/hls/vid/index.m3u8?token=abc"
        );
    }

    #[test]
    fn test_strip_proto() {
        let raw = "https://cdn.example/x";
        assert_eq!(strip_proto(&format!("hls://{raw}")), raw);
        assert_eq!(strip_proto(raw), raw);
    }

    #[test]
    fn test_detect_kind() {
        assert_eq!(detect_kind("https://x/master.txt"), Kind::Hls);
        assert_eq!(detect_kind("https://x/index.m3u8"), Kind::Hls);
        assert_eq!(detect_kind("https://x/file.mp4"), Kind::Http);
        assert_eq!(detect_kind("https://x/manifest.mpd"), Kind::Dash);
    }

    #[test]
    fn test_parse_curl_chrome_copy() {
        let cmd = r#"curl 'https://cdn.example/hls/show/id/master.m3u8' \
  -H 'accept: */*' \
  -H 'origin: https://player.example' \
  -H 'referer: https://player.example/watch/abc' \
  -H 'user-agent: Mozilla/5.0 Test' \
  -H 'cookie: a=1; b=2'"#;
        let p = parse_curl(cmd).unwrap();
        assert_eq!(p.url, "https://cdn.example/hls/show/id/master.m3u8");
        assert_eq!(
            p.referer.as_deref(),
            Some("https://player.example/watch/abc")
        );
        assert_eq!(p.origin.as_deref(), Some("https://player.example"));
        assert_eq!(p.user_agent.as_deref(), Some("Mozilla/5.0 Test"));
        assert_eq!(p.cookie.as_deref(), Some("a=1; b=2"));
    }

    #[test]
    fn test_parse_curl_referer_short_flag() {
        let p = parse_curl("curl -e https://embed.example/ https://cdn.example/a.m3u8").unwrap();
        assert_eq!(p.url, "https://cdn.example/a.m3u8");
        assert_eq!(p.referer.as_deref(), Some("https://embed.example/"));
    }

    #[test]
    fn test_parse_curl_ignores_plain_url() {
        assert!(parse_curl("https://cdn.example/master.m3u8").is_none());
    }

    #[test]
    fn test_parse_capture_play_scheme() {
        let p = parse_capture_url(
            "play://open?url=https%3A%2F%2Fcdn.example%2Fhls%2Findex_720p.m3u8%3Ftoken%3Dab_cd&referer=https%3A%2F%2Fembed.example%2Fwatch%2F1",
        )
        .unwrap();
        assert_eq!(p.url, "https://cdn.example/hls/index_720p.m3u8?token=ab_cd");
        assert_eq!(p.referer.as_deref(), Some("https://embed.example/watch/1"));
    }

    #[test]
    fn test_parse_capture_rejects_javascript() {
        assert!(parse_capture_url("play://open?url=javascript:alert(1)").is_none());
        assert!(parse_capture_json(r#"{"url":"file:///tmp/x"}"#).is_none());
        assert!(parse_capture_url("https://cdn.example/a.m3u8").is_none());
    }

    #[test]
    fn test_parse_capture_json_frame_beats_root() {
        let p = parse_capture_json(
            r#"{"url":"https://cdn.example/a.m3u8","referer":"https://embed.example/","frame":"https://embed.example/player/abc"}"#,
        )
        .unwrap();
        assert_eq!(
            p.referer.as_deref(),
            Some("https://embed.example/player/abc")
        );
        let p = parse_capture_json(
            r#"{"url":"https://cdn.example/a.m3u8","referer":"https://embed.example/"}"#,
        )
        .unwrap();
        assert_eq!(p.referer.as_deref(), Some("https://embed.example/"));
    }

    #[test]
    fn test_parse_capture_json_initiator() {
        let p = parse_capture_json(
            r#"{"url":"https://cdn.example/a.m3u8","initiator":"https://embed.example/"}"#,
        )
        .unwrap();
        assert_eq!(p.url, "https://cdn.example/a.m3u8");
        assert_eq!(p.referer.as_deref(), Some("https://embed.example/"));
    }

    #[test]
    fn test_parse_capture_json_cookie_and_ua() {
        let p = parse_capture_json(
            r#"{"url":"https://embed.example/wrap/token.m3u8","referer":"https://embed.example/watch/1","origin":"https://embed.example","userAgent":"Mozilla/5.0 Test","cookie":"sid=abc; other=1","cookieHost":"embed.example"}"#,
        )
        .unwrap();
        assert_eq!(p.cookie.as_deref(), Some("sid=abc; other=1"));
        assert_eq!(p.cookie_host.as_deref(), Some("embed.example"));
        assert_eq!(p.user_agent.as_deref(), Some("Mozilla/5.0 Test"));
        assert_eq!(p.origin.as_deref(), Some("https://embed.example"));
    }

    #[test]
    fn test_join_url_unwraps_proxy_query() {
        assert_eq!(
            unwrap_resource_url(
                "https://embed.example/proxy?url=https%3A%2F%2Fcdn.example%2Fmaster.m3u8"
            ),
            "https://cdn.example/master.m3u8"
        );
        assert_eq!(
            join_url(
                "https://embed.example/wrap/token.m3u8",
                "/proxy?url=https%3A%2F%2Fcdn.example%2Findex.m3u8"
            ),
            "https://cdn.example/index.m3u8"
        );
    }

    #[test]
    fn test_parse_capture_json_subtitles() {
        let p = parse_capture_json(
            r#"{"url":"https://cdn.example/a.mp4","referer":"https://embed.example/","subtitles":["https://cdn.example/en.vtt","https://cdn.example/es.srt","javascript:alert(1)"]}"#,
        )
        .unwrap();
        assert_eq!(
            p.subtitles.as_deref(),
            Some(
                [
                    "https://cdn.example/en.vtt".to_string(),
                    "https://cdn.example/es.srt".to_string()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn test_parse_capture_query() {
        let p = parse_capture_query(
            "url=https%3A%2F%2Fcdn.example%2Fa.m3u8&referer=https%3A%2F%2Fembed.example%2F",
        )
        .unwrap();
        assert_eq!(p.url, "https://cdn.example/a.m3u8");
        assert_eq!(p.referer.as_deref(), Some("https://embed.example/"));
    }

    #[test]
    fn test_origin_of_uses_url_host() {
        assert_eq!(
            origin_of("https://cdn.example/hls/master.m3u8").unwrap(),
            "https://cdn.example"
        );
    }

    #[test]
    fn test_strip_range_query_removes_bytes_slice() {
        let url = "https://cdn.example/?expires=1&id=620097243758&bytes=0-47101";
        let full = strip_range_query(url);
        assert!(!full.contains("bytes="), "range slice must go: {full}");
        assert!(full.contains("expires=1"));
        assert!(full.contains("id=620097243758"));
        assert_eq!(
            strip_range_query("https://cdn.example/hls/master.m3u8?token=abc"),
            "https://cdn.example/hls/master.m3u8?token=abc"
        );
    }

    #[test]
    fn test_strip_range_query_removes_instagram_bytestart() {
        let url = "https://scontent.example/o1/v/clip.mp4?_nc_cat=106&efg=eyJ2%3D%3D&oh=00_AQ&oe=6AAE&bytestart=818&byteend=909";
        let full = strip_range_query(url);
        assert!(!full.contains("bytestart"), "range start must go: {full}");
        assert!(!full.contains("byteend"), "range end must go: {full}");
        assert!(full.contains("_nc_cat=106"));
        assert!(
            full.contains("efg=eyJ2%3D%3D"),
            "signed param encoding must stay: {full}"
        );
        assert!(full.contains("oh=00_AQ"));
        assert!(full.contains("oe=6AAE"));
    }

    #[test]
    fn test_auto_filename_skips_generic() {
        assert_eq!(
            auto_filename("https://cdn.example/show/episode-id/master.txt"),
            "episode-id.mp4"
        );
        assert_eq!(
            auto_filename("https://cdn.example/12345/index.m3u8"),
            "12345.mp4"
        );
        assert_eq!(auto_filename("https://cdn.example/ep-3.m3u8"), "ep-3.mp4");
        assert_eq!(auto_filename("https://cdn.example/clip.mp4"), "clip.mp4");
        assert_eq!(
            auto_filename("https://cdn.example/show/manifest.mpd"),
            "show.mp4"
        );
    }
}
