const PLAY_HOST = "http://127.0.0.1:17331/";

function header(headers, name) {
  const n = name.toLowerCase();
  return (headers || []).find((h) => h.name.toLowerCase() === n)?.value || "";
}

function isHttpUrl(url) {
  return url.startsWith("http://") || url.startsWith("https://");
}

function isOurCapture(url) {
  return url.startsWith(PLAY_HOST);
}

function pathname(url) {
  try {
    return new URL(url).pathname.toLowerCase();
  } catch {
    return "";
  }
}

/** Outer request, or the inner `url=` on /proxy and /fetch wrappers. */
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
  return pathname(resourceUrl(url));
}

function isPlaylistUrl(url) {
  if (!isHttpUrl(url) || isOurCapture(url)) return false;
  const path = resourcePath(url);
  return (
    path.endsWith(".m3u8") ||
    path.endsWith(".mpd") ||
    path.endsWith(".m3u") ||
    path.includes(".m3u8") ||
    path.includes(".mpd")
  );
}

function isFragmentPath(path) {
  return (
    path.endsWith(".m4s") ||
    path.endsWith(".ts") ||
    path.endsWith(".cmfv") ||
    path.endsWith(".jpg") ||
    path.endsWith(".jpeg") ||
    path.endsWith(".png") ||
    path.endsWith(".gif") ||
    path.endsWith(".webp")
  );
}

function isMediaUrl(url) {
  if (!isHttpUrl(url) || isOurCapture(url)) return false;
  const path = resourcePath(url);
  if (isFragmentPath(path)) {
    return false;
  }
  return (
    path.endsWith(".mp4") ||
    path.endsWith(".m4v") ||
    path.endsWith(".webm") ||
    path.endsWith(".mkv") ||
    path.endsWith(".mov")
  );
}

function isSubSegmentPath(path) {
  const base = path.split("/").pop() || "";
  return /^\d+\.(vtt|srt)$/.test(base) || /seg[-_]?\d+\.(vtt|srt)$/.test(base);
}

function isSubUrl(url) {
  if (!isHttpUrl(url) || isOurCapture(url)) return false;
  const path = resourcePath(url);
  if (isSubSegmentPath(path)) return false;
  return (
    path.endsWith(".vtt") ||
    path.endsWith(".webvtt") ||
    path.endsWith(".srt") ||
    path.endsWith(".ass") ||
    path.endsWith(".ssa")
  );
}

function isCaptureUrl(url) {
  return isPlaylistUrl(url) || isMediaUrl(url) || isSubUrl(url);
}

function isPlaylistType(headers) {
  const ct = header(headers, "content-type").toLowerCase();
  if (!ct || ct.includes("text/html") || ct.includes("javascript")) return false;
  return (
    ct.includes("mpegurl") ||
    ct.includes("dash+xml") ||
    ct.includes("x-mpegurl") ||
    ct.includes("vnd.apple.mpegurl")
  );
}

function isMediaType(headers) {
  const ct = header(headers, "content-type").toLowerCase();
  if (!ct || ct.includes("text/html") || ct.includes("javascript")) return false;
  if (ct.includes("mp2t") || ct.includes("mpegurl")) return false;
  return (
    ct.startsWith("video/mp4") ||
    ct.startsWith("video/webm") ||
    ct.startsWith("video/quicktime") ||
    ct === "video/x-matroska" ||
    ct === "video/x-m4v" ||
    ct.includes("text/vtt") ||
    ct.includes("x-subrip") ||
    ct.includes("text/x-ssa")
  );
}

function tabKey(tabId) {
  return `tab:${tabId}`;
}

async function loadTab(tabId) {
  const key = tabKey(tabId);
  const data = await chrome.storage.session.get(key);
  return data[key] || {};
}

async function saveTab(tabId, map) {
  await chrome.storage.session.set({ [tabKey(tabId)]: map });
}

async function clearTab(tabId) {
  await chrome.storage.session.remove(tabKey(tabId));
  await setBadge(tabId, 0);
}

async function setBadge(tabId, n) {
  const text = n > 0 ? String(n) : "";
  try {
    await chrome.action.setBadgeBackgroundColor({ tabId, color: "#16a34a" });
    if (chrome.action.setBadgeTextColor) {
      await chrome.action.setBadgeTextColor({ tabId, color: "#ffffff" });
    }
    await chrome.action.setBadgeText({ tabId, text });
  } catch {
    /* tab gone */
  }
}

async function remember(tabId, url, extra) {
  if (!tabId || tabId < 0 || !url || !isHttpUrl(url) || isOurCapture(url)) return;
  const playUrl = resourceUrl(url);
  const map = await loadTab(tabId);
  const prev = map[playUrl] || { url: playUrl };
  const next = {
    url: playUrl,
    referer: extra.referer || prev.referer || extra.initiator || prev.initiator || "",
    initiator: extra.initiator || prev.initiator || extra.referer || prev.referer || "",
    origin: extra.origin || prev.origin || "",
    userAgent: extra.userAgent || prev.userAgent || "",
  };
  if (
    map[playUrl] &&
    map[playUrl].url === next.url &&
    map[playUrl].referer === next.referer &&
    map[playUrl].origin === next.origin &&
    map[playUrl].userAgent === next.userAgent
  ) {
    return;
  }
  map[playUrl] = next;
  const keys = Object.keys(map);
  if (keys.length > 50) {
    for (const k of keys.slice(0, keys.length - 50)) delete map[k];
  }
  await saveTab(tabId, map);
  await setBadge(tabId, Object.keys(map).length);
  chrome.runtime
    .sendMessage({ cmd: "playlist-detected", tabId, value: next })
    .catch(() => {});
}

chrome.action.setBadgeBackgroundColor({ color: "#16a34a" });

chrome.webRequest.onBeforeSendHeaders.addListener(
  (d) => {
    if (d.tabId < 0) return;
    if (!isCaptureUrl(d.url)) return;
    remember(d.tabId, d.url, {
      referer: header(d.requestHeaders, "referer"),
      origin: header(d.requestHeaders, "origin"),
      userAgent: header(d.requestHeaders, "user-agent"),
      initiator: d.initiator,
    });
  },
  { urls: ["<all_urls>"] },
  ["requestHeaders", "extraHeaders"],
);

chrome.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (d.tabId < 0) return;
    if (isOurCapture(d.url)) return;
    const playlist = isPlaylistType(d.responseHeaders);
    // JPEG/PNG-wrapped MPEG-TS segments often arrive as video/mp4. Keep
    // only if the response is actually an HLS/DASH playlist.
    if (isFragmentPath(resourcePath(d.url)) && !playlist) {
      return;
    }
    if (!isCaptureUrl(d.url) && !playlist && !isMediaType(d.responseHeaders)) {
      return;
    }
    remember(d.tabId, d.url, { initiator: d.initiator });
  },
  { urls: ["<all_urls>"] },
  ["responseHeaders"],
);

chrome.tabs.onRemoved.addListener((tabId) => {
  void clearTab(tabId);
});

chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status === "loading" && info.url) {
    void clearTab(tabId);
  }
});

chrome.runtime.onMessage.addListener((msg, _sender, respond) => {
  if (msg?.cmd === "list") {
    loadTab(msg.tabId)
      .then((map) => respond({ playlists: Object.values(map) }))
      .catch(() => respond({ playlists: [] }));
    return true;
  }
});
