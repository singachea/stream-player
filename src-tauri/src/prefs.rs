use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use url::Url;

use crate::error::{Error, Result};
use crate::urls::{origin_of, strip_proto};

pub const PREFS_NAME: &str = ".play.json";

pub type Prefs = BTreeMap<String, BTreeMap<String, String>>;

pub fn host_of(url: &str) -> String {
    let parsed = Url::parse(strip_proto(url)).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or("")
        .to_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

fn suffixes(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').filter(|p| !p.is_empty()).collect();
    if labels.len() < 2 {
        return if host.is_empty() {
            vec![]
        } else {
            vec![host.to_string()]
        };
    }
    (0..labels.len() - 1)
        .map(|i| labels[i..].join("."))
        .collect()
}

pub fn load_prefs(path: &Path) -> Result<Prefs> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(path).map_err(|e| {
        Error::msg(format!(
            "play: bad {}: {e}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
    })?;
    let data: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        Error::msg(format!(
            "play: bad {}: {e}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
    })?;
    let obj = data.as_object().ok_or_else(|| {
        Error::msg(format!(
            "play: bad {}: expected a JSON object",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
    })?;
    let mut prefs = BTreeMap::new();
    for (k, v) in obj {
        if let Some(entry) = v.as_object() {
            let mut m = BTreeMap::new();
            for (ek, ev) in entry {
                if let Some(s) = ev.as_str() {
                    m.insert(ek.clone(), s.to_string());
                }
            }
            prefs.insert(k.clone(), m);
        }
    }
    Ok(prefs)
}

pub fn save_prefs(path: &Path, prefs: &Prefs) -> Result<()> {
    let text = serde_json::to_string_pretty(prefs)?;
    fs::write(path, format!("{text}\n"))?;
    Ok(())
}

pub fn lookup_prefs<'a>(prefs: &'a Prefs, url: &str) -> Option<&'a BTreeMap<String, String>> {
    let host = host_of(url);
    let mut found = None;
    let mut keys = suffixes(&host);
    keys.reverse();
    for key in keys {
        if let Some(entry) = prefs.get(&key) {
            found = Some(entry);
        }
    }
    found
}

pub fn remember_prefs(prefs: &mut Prefs, url: &str, settings: &BTreeMap<String, String>) {
    let host = host_of(url);
    let mut key = host.clone();
    for suffix in suffixes(&host) {
        if prefs.contains_key(&suffix) {
            key = suffix;
            break;
        }
    }
    let entry = prefs.entry(key).or_default();
    for k in ["referer", "origin", "user_agent"] {
        if let Some(v) = settings.get(k) {
            if !v.is_empty() {
                entry.insert(k.to_string(), v.clone());
            }
        }
    }
}

pub fn settings_for(
    url: &str,
    prefs: &Prefs,
    referer: Option<&str>,
    origin: Option<&str>,
    user_agent: &str,
) -> Result<BTreeMap<String, String>> {
    let stored = lookup_prefs(prefs, url);
    let get = |k: &str| stored.and_then(|s| s.get(k)).cloned();
    let ref_ = if let Some(r) = referer.filter(|s| !s.is_empty()) {
        r.to_string()
    } else if let Some(r) = get("referer") {
        r
    } else {
        format!("{}/", origin_of(url)?)
    };
    let orig = if let Some(o) = origin.filter(|s| !s.is_empty()) {
        o.to_string()
    } else if referer.filter(|s| !s.is_empty()).is_some() {
        origin_of(&ref_)?
    } else if let Some(o) = get("origin") {
        o
    } else {
        origin_of(&ref_)?
    };
    let ua = if !user_agent.is_empty() {
        user_agent.to_string()
    } else {
        get("user_agent").unwrap_or_default()
    };
    Ok(BTreeMap::from([
        ("referer".into(), ref_),
        ("origin".into(), orig),
        ("user_agent".into(), ua),
    ]))
}

pub fn normalize_referer(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        Some(s.to_string())
    } else {
        Some(format!("https://{s}"))
    }
}

pub fn ask_referer() -> Result<Option<String>> {
    eprint!("play: Referer (embed page URL): ");
    let _ = io::stderr().flush();
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut raw = String::new();
    match io::stdin().read_line(&mut raw) {
        Ok(_) => Ok(normalize_referer(&raw)),
        Err(_) => Ok(None),
    }
}

pub fn prefs_path(dir: &Path) -> PathBuf {
    dir.join(PREFS_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_strips_www_and_port() {
        assert_eq!(host_of("https://www.CDN.Example:443/a"), "cdn.example");
        assert_eq!(host_of("https://a.b.cdn.example/x"), "a.b.cdn.example");
    }

    #[test]
    fn test_exact_and_suffix_match() {
        let mut prefs = Prefs::new();
        prefs.insert(
            "cdn.example".into(),
            BTreeMap::from([("referer".into(), "https://embed.example/".into())]),
        );
        assert_eq!(
            lookup_prefs(&prefs, "https://cdn.example/hls/master.m3u8")
                .unwrap()
                .get("referer")
                .unwrap(),
            "https://embed.example/"
        );
        assert_eq!(
            lookup_prefs(&prefs, "https://edge.cdn.example/a.m3u8")
                .unwrap()
                .get("referer")
                .unwrap(),
            "https://embed.example/"
        );
        assert!(lookup_prefs(&prefs, "https://other.example/a.m3u8").is_none());
    }

    #[test]
    fn test_longer_key_wins() {
        let mut prefs = Prefs::new();
        prefs.insert(
            "cdn.example".into(),
            BTreeMap::from([("referer".into(), "https://generic/".into())]),
        );
        prefs.insert(
            "edge.cdn.example".into(),
            BTreeMap::from([("referer".into(), "https://specific/".into())]),
        );
        assert_eq!(
            lookup_prefs(&prefs, "https://edge.cdn.example/x")
                .unwrap()
                .get("referer")
                .unwrap(),
            "https://specific/"
        );
    }

    #[test]
    fn test_remember_writes_host_and_updates_suffix() {
        let mut prefs = Prefs::new();
        remember_prefs(
            &mut prefs,
            "https://edge.cdn.example/a.m3u8",
            &BTreeMap::from([
                ("referer".into(), "https://embed.example/".into()),
                ("origin".into(), "https://embed.example".into()),
            ]),
        );
        assert_eq!(
            prefs["edge.cdn.example"]["referer"],
            "https://embed.example/"
        );
        remember_prefs(
            &mut prefs,
            "https://edge.cdn.example/b.m3u8",
            &BTreeMap::from([("referer".into(), "https://embed.example/v2/".into())]),
        );
        assert_eq!(
            prefs["edge.cdn.example"]["referer"],
            "https://embed.example/v2/"
        );
        assert!(!prefs.contains_key("cdn.example"));
    }

    #[test]
    fn test_load_save_roundtrip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join(PREFS_NAME);
        assert!(load_prefs(&path).unwrap().is_empty());
        let mut prefs = Prefs::new();
        prefs.insert(
            "cdn.example".into(),
            BTreeMap::from([("referer".into(), "https://embed.example/".into())]),
        );
        save_prefs(&path, &prefs).unwrap();
        let data: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            data["cdn.example"]["referer"].as_str().unwrap(),
            "https://embed.example/"
        );
        assert_eq!(
            load_prefs(&path).unwrap()["cdn.example"]["referer"],
            "https://embed.example/"
        );
    }

    #[test]
    fn test_cli_overrides_prefs_then_host_default() {
        let mut prefs = Prefs::new();
        prefs.insert(
            "cdn.example".into(),
            BTreeMap::from([
                ("referer".into(), "https://saved.example/".into()),
                ("origin".into(), "https://saved.example".into()),
            ]),
        );
        let s = settings_for("https://cdn.example/a.m3u8", &prefs, None, None, "UA").unwrap();
        assert_eq!(s["referer"], "https://saved.example/");
        assert_eq!(s["origin"], "https://saved.example");
        let s = settings_for(
            "https://cdn.example/a.m3u8",
            &prefs,
            Some("https://cli.example/"),
            None,
            "UA",
        )
        .unwrap();
        assert_eq!(s["referer"], "https://cli.example/");
        assert_eq!(s["origin"], "https://cli.example");
        let s = settings_for(
            "https://other.example/a.m3u8",
            &Prefs::new(),
            None,
            None,
            "UA",
        )
        .unwrap();
        assert_eq!(s["referer"], "https://other.example/");
        assert_eq!(s["origin"], "https://other.example");
    }

    #[test]
    fn test_blank_is_none_and_scheme_is_added() {
        assert!(normalize_referer("").is_none());
        assert!(normalize_referer("  ").is_none());
        assert_eq!(
            normalize_referer("embed.example/").as_deref(),
            Some("https://embed.example/")
        );
        assert_eq!(
            normalize_referer("https://embed.example/watch/x").as_deref(),
            Some("https://embed.example/watch/x")
        );
    }
}
