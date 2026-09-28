const PLAY_OPEN = "http://127.0.0.1:17331/open";

const statusEl = document.getElementById("status");
const listEl = document.getElementById("list");
const clearEl = document.getElementById("clear");
const items = new Map();
let tabId = null;

clearEl.addEventListener("click", async () => {
  items.clear();
  render();
  setStatus("Watching…");
  if (tabId == null) return;
  try {
    await chrome.runtime.sendMessage({ cmd: "clear", tabId });
  } catch {
    /* background unreachable; local list already cleared */
  }
});

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

/** File stem tokens, e.g. "English.vtt" -> ["english"]. */
function stemTokens(url) {
  const base = resourcePath(url).split("/").pop() || "";
  const stem = base.split(".").slice(0, -1).join(".") || base;
  return stem.toLowerCase().split(/[^a-z]+/).filter(Boolean);
}

function isEnglishSub(url) {
  const toks = stemTokens(url);
  return toks.includes("english") || toks.includes("en") || toks.includes("eng");
}

/** English first, then newest. */
function sortSubs(subs) {
  return subs.sort((a, b) => {
    const ea = isEnglishSub(a.url) ? 0 : 1;
    const eb = isEnglishSub(b.url) ? 0 : 1;
    if (ea !== eb) return ea - eb;
    return (b.seenAt || 0) - (a.seenAt || 0);
  });
}

function streamRows() {
  return [...items.values()]
    .filter((v) => !isSub(v.url))
    .sort((a, b) => (b.seenAt || 0) - (a.seenAt || 0));
}

function subtitleRows() {
  return sortSubs([...items.values()].filter((v) => isSub(v.url)));
}

/** Subtitles captured in the same fetch group as the stream. */
function subsFor(stream, subs) {
  if (!stream.batch) return subs;
  const attached = subs.filter((s) => s.batch === stream.batch);
  return attached.length ? attached : [];
}

function downloadSub(url, btn) {
  btn.disabled = true;
  const done = (ok, msg) => {
    setStatus(msg, !ok);
    btn.disabled = false;
  };
  try {
    const p = chrome.downloads.download({ url, saveAs: false });
    if (p && typeof p.then === "function") {
      p.then(() => done(true, "Subtitle saved")).catch(() => done(false, "Download failed"));
    } else {
      done(true, "Subtitle saved");
    }
  } catch {
    done(false, "Download failed");
  }
}

function subRow(s) {
  const row = document.createElement("div");
  row.className = "subrow";
  const lab = document.createElement("div");
  lab.className = "sublabel";
  lab.textContent = `CC ${label(s.url)}`;
  const dl = document.createElement("button");
  dl.className = "dl";
  dl.type = "button";
  dl.textContent = "Download";
  dl.addEventListener("click", (e) => {
    e.stopPropagation();
    downloadSub(s.url, dl);
  });
  row.append(lab, dl);
  return row;
}

function render() {
  const streams = streamRows();
  const subs = subtitleRows();
  if (!streams.length && !subs.length) {
    listEl.innerHTML =
      '<div class="empty">No streams on this tab yet. Reload the page, play the video, and watch the badge. Then click here.</div>';
    return;
  }
  // Highlight only the latest fetch group.
  const rows = [...streams, ...subs];
  const latest = rows.reduce((m, v) => Math.max(m, v.batch || 0), 0);
  listEl.replaceChildren();
  const orphans = subs.filter((s) => !streams.some((st) => subsFor(st, subs).includes(s)));
  streams.forEach((v, i) => {
    const row = document.createElement("div");
    row.className = "item" + (latest && v.batch === latest ? " fresh" : "");
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
    for (const s of subsFor(v, subs)) meta.append(subRow(s));
    const btn = document.createElement("button");
    btn.className = "send";
    btn.type = "button";
    btn.textContent = "Play";
    btn.addEventListener("click", () => sendToPlay(v, btn));
    row.append(idx, meta, btn);
    listEl.append(row);
  });
  // Subtitles with no stream in their fetch group stay visible as orphans.
  orphans.forEach((s, k) => {
    const row = document.createElement("div");
    row.className = "item" + (latest && s.batch === latest ? " fresh" : "");
    const idx = document.createElement("div");
    idx.className = "idx";
    idx.textContent = String(streams.length + k + 1);
    const meta = document.createElement("div");
    meta.className = "meta";
    meta.append(subRow(s));
    const time = document.createElement("div");
    time.className = "time";
    time.textContent = formatTime(s.seenAt);
    meta.append(time);
    const tag = document.createElement("div");
    tag.className = "sub";
    tag.textContent = "subtitle";
    row.append(idx, meta, tag);
    listEl.append(row);
  });
}

function add(value) {
  if (!value?.url) return;
  items.set(value.url, value);
  const n = [...items.values()].filter((v) => !isSub(v.url)).length;
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
  const subtitles = subsFor(v, subtitleRows()).map((x) => x.url);
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
      setStatus(subtitles.length ? `Sent to Play (+${subtitles.length} subtitles)` : "Sent to Play");
      btn.disabled = false;
      return;
    }
  } catch {
    /* Play is not listening; fall through to play:// */
  }
  const u = new URL("play://open");
  u.searchParams.set("url", v.url);
  if (referer) u.searchParams.set("referer", referer);
  for (const s of subtitles) u.searchParams.append("sub", s);
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
