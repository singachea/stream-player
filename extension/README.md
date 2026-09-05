# Send to Play

Unpacked Brave/Chrome extension. Watches the tab for HLS/DASH playlists (`.m3u8`, `.mpd`), progressive files (`.mp4`, `.m4v`, `.webm`, `.mkv`, `.mov`), and subtitles (`.vtt`, `.srt`, `.ass`). The badge is the count. Play sends the stream plus any subtitles on the tab (URL + Referer). Wrapper `/proxy?url=` and `/fetch?url=` requests are unwrapped to the inner file. HLS fragments (`.ts`, `.m4s`, JPEG/PNG-wrapped segments) are ignored.

Load it in the Brave profile you use for streams:

1. `brave://extensions`
2. Developer mode on
3. Load unpacked → this folder
4. After updating files here, click **Reload** on the extension card

The badge is per-tab. Reload the page after installing so the observer sees the playlist request. Play the video, wait for the badge, then click the icon and **Play**.

Play should be running. The extension POSTs to `http://127.0.0.1:17331/open`. If Play is not running, it falls back to `play://` (needs `/Applications/Play.app` installed).
