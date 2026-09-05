# Play

Desktop app and CLI to play HLS/HTTP URLs in VLC with seeking. Adds Referer/User-Agent from the URL host, unwraps disguised segments (`.txt` playlists, PNG/JPEG-wrapped MPEG-TS), and serves a local VOD playlist VLC can scrub.

Built with **Tauri 2**, **SvelteKit**, **TypeScript**, **Tailwind CSS**, and a **Rust** engine (same stack as Music Tags).

## Prerequisites

| Tool | Why |
|---|---|
| [Node.js](https://nodejs.org/) 18+ | UI |
| [Rust](https://www.rust-lang.org/tools/install) (stable) | Engine + CLI |
| [VLC](https://www.videolan.org/vlc/) | Player. Default: `/Applications/VLC.app/Contents/MacOS/VLC`. Override with `VLC=/path/to/vlc`. |
| [ffmpeg](https://ffmpeg.org/) | Optional. Remux downloads to MP4 (`-c copy`) |
| macOS | Window-close detection uses `osascript` / System Events |

Allow the app (or the terminal, for the CLI) in **System Settings → Privacy & Security → Automation** (System Events) so Play can see VLC windows close.

## Desktop app

```bash
npm install
npm run tauri dev
```

Paste a playlist URL, pick quality, Play opens VLC (seekable). Download writes MPEG-TS then remuxes to MP4 when ffmpeg is on `PATH`.

To capture a stream from a page in Brave, load the **Send to Play** unpacked extension (`extension/`) in that profile (`brave://extensions` → Developer mode → Load unpacked). The badge is the count of playlists (`.m3u8`) and files (`.mp4`, …) on the current tab. Reload the page after installing, play the video, then click the icon. It POSTs to `http://127.0.0.1:17331/open` while Play is running (`play://` if Play is installed but not running). See `extension/README.md`.

A 403 that needs a Referer shows a dialog. The embed page URL is stored in `.play.json` in the folder shown in the toolbar.

```bash
npm run tauri build
```

Produces a native app under `src-tauri/target/release/bundle/`.

## CLI

```bash
cargo install --path src-tauri --bin play
```

If you previously had a Python `play` on `PATH` (for example `~/bin/play`), replace that symlink with the binary from `cargo install`.

```bash
play 'https://cdn.example/hls/master.txt'
play 'https://cdn.example/hls/master.txt' 720p
play --download 'https://cdn.example/hls/show-id/master.txt'
play --download episode.mp4 'https://cdn.example/hls/master.txt'
play -o episode.ts 'https://cdn.example/hls/master.txt' 720p
play --dry-run -v 'https://cdn.example/hls/master.txt'
# Quote URLs that contain & so the shell does not split the query string.
```

Quality is optional (`best` default): `play URL 720p`.

Settings for the current directory live in `.play.json` (host → Referer/Origin). `play` loads that file first. If a playlist 403s because a Referer is required and stdin is a TTY, it asks for the embed page URL and stores it. `--referer` / `--origin` are saved the same way.

```json
{
  "cdn.example": {
    "referer": "https://embed.example/",
    "origin": "https://embed.example"
  }
}
```

`--download` without a filename uses the URL path. Generic names like `master.txt` / `index.m3u8` take the parent folder (`…/show-id/master.txt` → `show-id.mp4`). HLS is remuxed to MP4 with `ffmpeg -c copy` when ffmpeg is on `PATH`; otherwise the raw `.ts` is kept. Pass `-o episode.ts` to skip remux.

## Layout

```
src/                 SvelteKit UI
src-tauri/src/
  urls.rs            URL kind, default download names, capture/cURL parse
  fetch.rs           HTTP GET + 403 hints
  hls.rs             variant pick, playlist rewrite, image-unwrap
  proxy.rs           local HLS server for VLC
  capture.rs         loopback `play-open` from the Brave bridge
  player.rs          VLC launch and window-close wait
  download.rs        segment concat + ffmpeg remux
  prefs.rs           cwd `.play.json`
  commands.rs        Tauri commands
  cli.rs             `play` CLI
extension/           Send to Play (unpacked MV3; badge + send playlist to Play)
```

## Tests

```bash
cd src-tauri && cargo test
```

## License

MIT
