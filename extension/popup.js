const PLAY_OPEN = "http://127.0.0.1:17331/open";

const statusEl = document.getElementById("status");
const listEl = document.getElementById("list");
const items = new Map();
let tabId = null;

function setStatus(text, isErr) {
  statusEl.textContent = text;
  statusEl.className = isErr ? "err" : "";
}

function isSub(url) {
  const u = url.toLowerCase();
  return (
    u.includes(".vtt") ||
    u.includes(".srt") ||
    u.includes(".ass") ||
    u.includes(".ssa") ||
    u.includes(".webvtt")
  );
}

function resourceUrl(url) {
  try {
    const u = new URL(url);
    const inner = u.searchParams.get("url");
    if (inner && (inner.startsWith("http://") || inner.startsWith("https://"))) {
      return inner;
    }
  } catch {
    /* ignore */
  }
  return url;
}

function resourcePath(url) {
  try {
    return new URL(resourceUrl(url)).pathname.toLowerCase();
  } catch {
    return "";
  }
}

function rank(url) {
  const path = resourcePath(url);
  const base = path.split("/").pop() || "";
  if (isSub(url)) return 5;
  if (
    path.endsWith(".m3u8") ||
    path.includes(".m3u8") ||
    path.endsWith(".mpd") ||
    path.endsWith(".m3u")
  ) {
    if (/^(master|index|playlist|manifest)/i.test(base)) return 0;
    if (base.length > 64) return 2;
    return 1;
  }
  if (path.endsWith(".mp4") || path.endsWith(".m4v")) return 3;
  return 4;
}

function label(url) {
  try {
    const u = new URL(resourceUrl(url));
    const last = u.pathname.split("/").filter(Boolean).pop() || u.hostname;
    return decodeURIComponent(last);
  } catch {
    return url;
  }
}

function formatTime(seenAt) {
  if (!seenAt) return "";
  const d = new Date(seenAt);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function render() {
  const rows = [...items.values()].sort((a, b) => rank(a.url) - rank(b.url));
  if (!rows.length) {
    listEl.innerHTML =
      '<div class="empty">No streams on this tab yet. Reload the page, play the video, and watch the badge. Then click here.</div>';
    return;
  }
  listEl.replaceChildren();
  for (const [i, v] of rows.entries()) {
    const row = document.createElement("div");
    row.className = "item";
    const idx = document.createElement("div");
    idx.className = "idx";
    idx.textContent = String(i + 1);
    const meta = document.createElement("div");
    meta.className = "meta";
    const name = document.createElement("div");
    name.className = "name";
    name.textContent = label(v.url);
    const time = document.createElement("div");
    time.className = "time";
    time.textContent = formatTime(v.seenAt);
    const sub = document.createElement("div");
    sub.className = "sub";
    sub.textContent = v.referer || v.initiator || v.url;
    meta.append(name, time, sub);
    if (isSub(v.url)) {
      const tag = document.createElement("div");
      tag.className = "sub";
      tag.textContent = "subtitle";
      row.append(idx, meta, tag);
    } else {
      const btn = document.createElement("button");
      btn.className = "send";
      btn.type = "button";
      btn.textContent = "Play";
      btn.addEventListener("click", () => sendToPlay(v, btn));
      row.append(idx, meta, btn);
    }
    listEl.append(row);
  }
}

function add(value) {
  if (!value?.url) return;
  items.set(value.url, value);
  const n = items.size;
  setStatus(n === 1 ? "1 stream" : `${n} streams`);
  render();
}

async function sendToPlay(v, btn) {
  // Prefer the actual player frame over the site root; fall back to the
  // stored referer/initiator only when no frame URL was captured.
  const referer = [v.frame, v.referer, v.initiator].find((u) => {
    try {
      return u && (u.startsWith("http://") || u.startsWith("https://"));
    } catch {
      return false;
    }
  });
  const origin = v.origin || (() => {
    try {
      return new URL(referer || v.url).origin;
    } catch {
      return undefined;
    }
  })();
  const subtitles = [...items.values()]
    .filter((x) => isSub(x.url))
    .map((x) => x.url);
  btn.disabled = true;
  try {
    const res = await fetch(PLAY_OPEN, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        url: v.url,
        referer,
        origin: origin || undefined,
        userAgent: v.userAgent || undefined,
        cookie: v.cookie || undefined,
        cookieHost: v.cookieHost || undefined,
        subtitles: subtitles.length ? subtitles : undefined,
      }),
    });
    if (res.ok) {
      setStatus("Sent to Play");
      btn.disabled = false;
      return;
    }
  } catch {
    /* Play is not listening; fall through to play:// */
  }
  const u = new URL("play://open");
  u.searchParams.set("url", v.url);
  if (referer) u.searchParams.set("referer", referer);
  try {
    await chrome.tabs.create({ url: u.toString() });
    setStatus("Opening Play…");
  } catch (e) {
    setStatus("Start Play and try again", true);
    btn.disabled = false;
    console.warn(e);
  }
}

chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.cmd === "playlist-detected" && msg.tabId === tabId) {
    add(msg.value);
  }
});

async function main() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab?.id) {
    setStatus("No active tab", true);
    render();
    return;
  }
  tabId = tab.id;
  const res = await chrome.runtime.sendMessage({ cmd: "list", tabId });
  for (const v of res?.playlists || []) add(v);
  if (!items.size) {
    setStatus("Watching…");
    render();
  }
}

main().catch((e) => {
  setStatus(String(e), true);
  render();
});
