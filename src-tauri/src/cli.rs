use std::collections::BTreeMap;
use std::env;
use std::io::IsTerminal;
use std::process;

use clap::{Arg, Command};

use crate::download::download_stream;
use crate::error::Error;
use crate::fetch::{http_403_hint_for, http_headers, referer_needed_for};
use crate::player::{dry_run_hls, start_hls_playback, start_http_playback, vlc_path, wait_for_vlc};
use crate::prefs::{
    ask_referer, host_of, load_prefs, prefs_path, remember_prefs, save_prefs, settings_for,
};
use crate::urls::{
    auto_filename, detect_kind, is_url, origin_of, parse_curl, strip_proto, Kind, UA_DEFAULT,
};

pub fn peel_download(argv: &[String]) -> (Vec<String>, bool, Option<String>) {
    let mut download = false;
    let mut download_file: Option<String> = None;
    let mut out = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if a == "-o" || a == "--output" || a == "--download" {
            download = true;
            if i + 1 < argv.len() && !argv[i + 1].starts_with('-') && !is_url(&argv[i + 1]) {
                download_file = Some(argv[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    (out, download, download_file)
}

pub struct Args {
    pub url: String,
    pub quality: Option<String>,
    pub referer: Option<String>,
    pub origin: Option<String>,
    pub user_agent: String,
    pub dry_run: bool,
    pub verbose: bool,
    pub download: bool,
    pub download_file: Option<String>,
}

pub fn parse_args(argv: &[String]) -> Result<Args, i32> {
    let (rest, download, download_file) = peel_download(argv);
    let cmd = Command::new("play")
        .about("Play HLS/HTTP in VLC with seeking. Sets Referer from the URL host.")
        .after_help(
            "examples:\n  \
             play 'https://cdn.example/hls/master.txt'          # list qualities, Enter = best\n  \
             play 'https://cdn.example/hls/master.txt' 720p\n  \
             play --download 'https://cdn.example/hls/show-id/master.txt'\n  \
             play --download episode.mp4 'https://cdn.example/hls/master.txt'\n  \
             play -o episode.ts 'https://cdn.example/hls/master.txt' 720p\n  \
             play --referer 'https://embed.example/' 'https://cdn.example/index.m3u8'",
        )
        .arg(Arg::new("url").required(true).help("Playlist or file URL"))
        .arg(Arg::new("quality").help("best, worst, 720p, … (omit to pick; Enter = best)"))
        .arg(
            Arg::new("referer")
                .long("referer")
                .help("Override Referer (default https://<host>/, or .play.json)"),
        )
        .arg(
            Arg::new("origin")
                .long("origin")
                .help("Override Origin (default https://<host>)"),
        )
        .arg(
            Arg::new("user-agent")
                .long("user-agent")
                .default_value(UA_DEFAULT),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .action(clap::ArgAction::SetTrue)
                .help("Rewrite playlist and exit"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("download")
                .short('o')
                .long("output")
                .alias("download")
                .value_name("FILE")
                .num_args(0..=1)
                .help("Download instead of playing (optional filename)"),
        )
        .no_binary_name(true);
    let m = match cmd.try_get_matches_from(&rest) {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            return Err(if e.use_stderr() { 2 } else { 0 });
        }
    };
    Ok(Args {
        url: m.get_one::<String>("url").unwrap().clone(),
        quality: m.get_one::<String>("quality").cloned(),
        referer: m.get_one::<String>("referer").cloned(),
        origin: m.get_one::<String>("origin").cloned(),
        user_agent: m
            .get_one::<String>("user-agent")
            .cloned()
            .unwrap_or_else(|| UA_DEFAULT.into()),
        dry_run: m.get_flag("dry-run"),
        verbose: m.get_flag("verbose"),
        download,
        download_file,
    })
}

pub fn run() -> i32 {
    let argv: Vec<String> = env::args().skip(1).collect();
    match run_with(argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            e.exit_code()
        }
    }
}

pub fn run_with(argv: Vec<String>) -> crate::error::Result<i32> {
    let ns = match parse_args(&argv) {
        Ok(a) => a,
        Err(0) => return Ok(0),
        Err(c) => process::exit(c),
    };
    let mut ns = ns;
    let mut cookie = None;
    if let Some(p) = parse_curl(&ns.url) {
        ns.url = p.url;
        if ns.referer.is_none() {
            ns.referer = p.referer;
        }
        if ns.origin.is_none() {
            ns.origin = p.origin;
        }
        if let Some(ua) = p.user_agent {
            ns.user_agent = ua;
        }
        cookie = p.cookie;
    }
    if !is_url(&ns.url) {
        return Err(Error::msg(format!("play: not a URL: {}", ns.url)));
    }

    let prefs_path = prefs_path(&env::current_dir()?);
    let mut prefs = load_prefs(&prefs_path)?;
    let cfg = settings_for(
        &ns.url,
        &prefs,
        ns.referer.as_deref(),
        ns.origin.as_deref(),
        &ns.user_agent,
    )?;
    let referer = cfg["referer"].clone();
    let origin = cfg["origin"].clone();
    let ua = cfg["user_agent"].clone();
    if ns.referer.is_some() || ns.origin.is_some() {
        remember_prefs(
            &mut prefs,
            &ns.url,
            &BTreeMap::from([
                ("referer".into(), referer.clone()),
                ("origin".into(), origin.clone()),
                ("user_agent".into(), ua.clone()),
            ]),
        );
        save_prefs(&prefs_path, &prefs)?;
        if ns.verbose {
            eprintln!("play: saved settings in {}", crate::prefs::PREFS_NAME);
        }
    }

    let referer_set = ns.referer.is_some();
    let mut headers = http_headers(&ua, &referer, &origin);
    if let Some(c) = cookie.filter(|s| !s.is_empty()) {
        headers.insert("Cookie".into(), c);
    }
    let kind = detect_kind(&ns.url);
    let vlc = vlc_path();
    let tty = std::io::stdin().is_terminal();

    let mut on_403 =
        |url: &str, headers: &mut std::collections::HashMap<String, String>, body: &[u8]| {
            let sent = headers.get("Referer").map(|s| s.as_str());
            if referer_set || !tty || !referer_needed_for(url, body, sent) {
                return None;
            }
            eprintln!(
                "play: HTTP 403 fetching {url}{}",
                http_403_hint_for(url, body, sent)
            );
            let ref_ = match ask_referer() {
                Ok(Some(r)) => r,
                _ => return None,
            };
            let orig = match origin_of(&ref_) {
                Ok(o) => o,
                Err(_) => return None,
            };
            remember_prefs(
                &mut prefs,
                url,
                &BTreeMap::from([
                    ("referer".into(), ref_.clone()),
                    ("origin".into(), orig.clone()),
                ]),
            );
            let _ = save_prefs(&prefs_path, &prefs);
            eprintln!(
                "play: saved Referer for {} in {}",
                host_of(url),
                crate::prefs::PREFS_NAME
            );
            headers.insert("Referer".into(), ref_.clone());
            headers.insert("Origin".into(), orig.clone());
            Some(std::collections::HashMap::from([
                ("Referer".into(), ref_),
                ("Origin".into(), orig),
            ]))
        };

    if ns.download {
        let out_file = ns
            .download_file
            .clone()
            .unwrap_or_else(|| auto_filename(&ns.url));
        download_stream(
            &ns.url,
            &mut headers,
            ns.quality.as_deref(),
            &out_file,
            ns.verbose,
            tty,
            Some(&mut on_403),
            |_, _, _, _| {},
            None,
            crate::download::worker_count(),
        )?;
        return Ok(0);
    }

    if ns.dry_run {
        dry_run_hls(
            strip_proto(&ns.url),
            &mut headers,
            ns.quality.as_deref(),
            ns.verbose,
            Some(&mut on_403),
        )?;
        return Ok(0);
    }

    match kind {
        Kind::Http | Kind::Dash => {
            let mut pb = start_http_playback(&ns.url, &headers, &vlc, ns.verbose, &[])?;
            let code = wait_for_vlc(&mut pb.child, ns.verbose);
            let _ = pb.child.kill();
            pb.proxy.shutdown();
            return Ok(code);
        }
        Kind::Hls => {
            match start_hls_playback(
                strip_proto(&ns.url),
                &mut headers,
                ns.quality.as_deref(),
                &vlc,
                ns.verbose,
                tty,
                &[],
                Some(&mut on_403),
            ) {
                Ok(mut pb) => {
                    let code = wait_for_vlc(&mut pb.child, ns.verbose);
                    let _ = pb.child.kill();
                    pb.proxy.shutdown();
                    return Ok(code);
                }
                Err(Error::Progressive { .. }) => {
                    if ns.verbose {
                        eprintln!("play: not a playlist, playing as a file");
                    }
                    let mut pb = start_http_playback(&ns.url, &headers, &vlc, ns.verbose, &[])?;
                    let code = wait_for_vlc(&mut pb.child, ns.verbose);
                    let _ = pb.child.kill();
                    pb.proxy.shutdown();
                    return Ok(code);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quality_arg_optional() {
        let a = parse_args(&["https://x/a.m3u8".into()]).unwrap();
        assert!(a.quality.is_none());
        let a = parse_args(&["https://x/a.m3u8".into(), "720p".into()]).unwrap();
        assert_eq!(a.quality.as_deref(), Some("720p"));
    }

    #[test]
    fn test_download_flag_optional_file() {
        let a = parse_args(&["--download".into(), "https://x/a.m3u8".into()]).unwrap();
        assert!(a.download);
        assert!(a.download_file.is_none());
        let a = parse_args(&[
            "--download".into(),
            "episode.mp4".into(),
            "https://x/a.m3u8".into(),
        ])
        .unwrap();
        assert_eq!(a.download_file.as_deref(), Some("episode.mp4"));
    }
}
