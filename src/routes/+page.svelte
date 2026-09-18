<script lang="ts">
  import { onMount } from "svelte";
  import { open, save } from "@tauri-apps/plugin-dialog";
  import { listen } from "@tauri-apps/api/event";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import * as api from "$lib/api";
  import type {
    DownloadConfig,
    HostRow,
    Job,
    LogFilter,
    LogLine,
    NeedReferer,
    Subtitle,
    Variant,
  } from "$lib/types";
  import { formatLogLine, matchLog } from "$lib/log";
  import { toastStore } from "$lib/stores/toasts.svelte";
  import ToastStack from "$lib/components/ToastStack.svelte";
  import RefererDialog from "$lib/components/RefererDialog.svelte";
  import DebugLog from "$lib/components/DebugLog.svelte";
  import JobsPane from "$lib/components/JobsPane.svelte";
  import StreamsPane from "$lib/components/StreamsPane.svelte";

  let url = $state("");
  let referer = $state("");
  let origin = $state("");
  let userAgent = $state("");
  let cookie = $state("");
  let cookieHost = $state("");
  let advancedOpen = $state(false);
  let workdir = $state("");
  let variants = $state<Variant[]>([]);
  let quality = $state<string>("best");
  let subTracks = $state<Subtitle[]>([]);
  let subtitle = $state<string>("auto");
  let kind = $state("hls");
  let hosts = $state<HostRow[]>([]);
  let busy = $state(false);
  let playingOp = $state<"play" | null>(null);
  let playStage = $state("");
  let playDone = $state(0);
  let playTotal = $state(1);
  let resolving = $state(false);
  let playing = $state(false);
  let extraCount = $state(0);
  let playMenu = $state(false);
  let jobs = $state<Job[]>([]);
  let streams = $state<api.StreamSession[]>([]);
  let needReferer = $state<NeedReferer | null>(null);
  let pendingAfterReferer = $state<"resolve" | "play" | "download" | null>(
    null,
  );
  let pendingExtra = $state(false);
  let pendingJobId = $state<number | null>(null);
  let logs = $state<LogLine[]>([]);
  let logFilter = $state<LogFilter>({ kind: "all" });
  const visibleLogs = $derived(logs.filter((l) => matchLog(l, logFilter)));
  let urlBox: HTMLTextAreaElement | undefined = $state();
  let lastClipOffer = $state("");
  let subtitles = $state<string[]>([]);
  let dlConfig = $state<DownloadConfig>({
    workers: 6,
    minWorkers: 1,
    maxWorkers: 16,
  });
  const jobsActive = $derived(
    jobs.filter((j) =>
      ["queued", "running", "remuxing", "cancelling"].includes(j.status),
    ).length,
  );

  const canPlay = $derived(!!cleanUrl(url) && playingOp === null);
  const playPct = $derived(
    playingOp === null ? 0 : Math.min(100, Math.round((playDone / Math.max(1, playTotal)) * 100)),
  );
  const playStageLabel = $derived(playStage || "Preparing");
  const headers = $derived.by(() => {
    const ref = referer.trim() || null;
    let orig = origin.trim() || null;
    try {
      const streamOrigin = new URL(cleanUrl(url)).origin;
      if (orig && orig.replace(/\/$/, "") === streamOrigin) {
        orig = null;
      }
    } catch {
      /* not a URL yet */
    }
    return {
      referer: ref,
      origin: orig,
      userAgent: userAgent.trim() || null,
      cookie: cookie.trim() || null,
      cookieHost: cookieHost.trim().toLowerCase() || null,
    };
  });

  function cleanUrl(s: string) {
    return s.replace(/[\r\n]+/g, "").trim();
  }

  function looksLikeUrl(s: string) {
    const t = cleanUrl(s);
    if (/^(hls|hlsvariant|httpstream|dash):\/\//i.test(t)) return true;
    try {
      const u = new URL(t);
      return (
        (u.protocol === "http:" || u.protocol === "https:") &&
        u.hostname.includes(".")
      );
    } catch {
      return false;
    }
  }

  function streamHost(raw: string): string {
    try {
      let s = cleanUrl(raw);
      s = s.replace(/^(hlsvariant|hls|httpstream|dash):\/\//i, "");
      const host = new URL(s).hostname.toLowerCase();
      return host.replace(/^www\./, "");
    } catch {
      return "";
    }
  }

  function hostSuffixes(host: string): string[] {
    const labels = host.split(".").filter(Boolean);
    if (labels.length < 2) return host ? [host] : [];
    const keys: string[] = [];
    for (let i = 0; i < labels.length - 1; i++) {
      keys.push(labels.slice(i).join("."));
    }
    return keys;
  }

  function savedHostFor(streamUrl: string): HostRow | undefined {
    const host = streamHost(streamUrl);
    if (!host) return;
    let found: HostRow | undefined;
    for (const key of hostSuffixes(host).reverse()) {
      const row = hosts.find((h) => h.host === key);
      if (row) found = row;
    }
    return found;
  }

  function applyRefererForUrl(streamUrl: string) {
    const row = savedHostFor(streamUrl);
    if (row?.referer) {
      referer = row.referer;
      origin = row.origin || "";
      addLog("info", `using Referer for ${row.host}`);
    } else {
      referer = "";
      origin = "";
    }
  }

  function addLog(
    level: LogLine["level"],
    msg: string,
    source: LogLine["source"] = "play",
    id?: number,
  ) {
    logs = [
      ...logs.slice(-499),
      { ms: Date.now(), level, msg, source, id: id ?? null },
    ];
  }

  function statusRank(s: Job["status"]) {
    switch (s) {
      case "queued":
        return 0;
      case "running":
        return 1;
      case "remuxing":
        return 2;
      case "cancelling":
        return 3;
      case "cancelled":
        return 5;
      default:
        return 4;
    }
  }

  function upsertJob(j: Job) {
    const i = jobs.findIndex((x) => x.id === j.id);
    if (i < 0) {
      jobs = [j, ...jobs];
      return;
    }
    const cur = jobs[i];
    const incoming = statusRank(j.status);
    const current = statusRank(cur.status);
    if (
      (cur.status === "cancelling" || cur.status === "cancelled") &&
      (j.status === "done" || j.status === "error")
    ) {
      return;
    }
    // Retry resets a finished job back to queued.
    if (current >= 4 && incoming < 3) {
      jobs = jobs.map((x, k) => (k === i ? j : x));
      return;
    }
    if (incoming < current) return;
    if (incoming === current && j.done < cur.done) return;
    jobs = jobs.map((x, k) => (k === i ? j : x));
  }

  onMount(() => {
    void refresh();
    urlBox?.focus();
    const unsubs: Array<() => void> = [];
    void listen<number>("player-stopped", (e) => {
      extraCount = Math.max(0, extraCount - 1);
      if (extraCount === 0) playing = false;
      const pid = e.payload;
      addLog(
        "debug",
        `player-stopped pid=${pid ?? ""}`,
        "vlc",
        typeof pid === "number" ? pid : undefined,
      );
    }).then((u) => unsubs.push(u));
    void listen<{ stage: string; stageKey: string; done: number; total: number }>(
      "play-progress",
      (e) => {
        if (playingOp === null) return;
        playStage = e.payload.stage || e.payload.stageKey;
        playDone = e.payload.done;
        playTotal = Math.max(1, e.payload.total);
      },
    ).then((u) => unsubs.push(u));
    void listen<Job>("job-updated", (e) => {
      const j = e.payload;
      if (!j?.id) return;
      upsertJob(j);
      if (j.status === "done") {
        const name = j.dest.split(/[/\\]/).pop() || j.dest;
        toastStore.push("success", `Saved ${name}`);
      } else if (j.status === "error") {
        toastStore.push("error", j.error || "Download failed");
      } else if (j.status === "needReferer") {
        pendingAfterReferer = "download";
        pendingJobId = j.id;
        needReferer = {
          status: "needReferer",
          url: j.url,
          hint: j.hint || "",
          host: j.host,
        };
      }
    }).then((u) => unsubs.push(u));
    void api.listJobs().then((list) => {
      jobs = list;
    }).catch(() => {});
    void api.listStreams().then((list) => {
      streams = list;
    }).catch(() => {});
    void listen<api.StreamSession>("stream-updated", (e) => {
      const s = e.payload;
      if (s?.id == null) return;
      const i = streams.findIndex((x) => x.id === s.id);
      if (i < 0) streams = [...streams, s];
      else streams = streams.map((x, k) => (k === i ? s : x));
    }).then((u) => unsubs.push(u));
    void listen<number>("stream-removed", (e) => {
      streams = streams.filter((x) => x.id !== e.payload);
    }).then((u) => unsubs.push(u));
    void listen<api.ParsedCurl>("play-open", (e) => {
      const cap = e.payload;
      if (!cap?.url) return;
      applyCapture(cap, "capture");
    }).then((u) => unsubs.push(u));
    void api.takeCapture().then((cap) => {
      if (cap?.url) applyCapture(cap, "capture");
    }).catch(() => {});
    function onFocus() {
      void offerClipboard();
    }
    window.addEventListener("focus", onFocus);
    void offerClipboard();
    startStreamPolling();
    void listen<LogLine>("play-log", (e) => {
      const line = e.payload;
      if (!line?.msg) return;
      const source =
        line.source === "vlc" || line.source === "job" || line.source === "play"
          ? line.source
          : "play";
      logs = [
        ...logs.slice(-499),
        {
          ms: line.ms || Date.now(),
          level: line.level || "info",
          msg: line.msg,
          source,
          id: line.id ?? null,
        },
      ];
    }).then((u) => unsubs.push(u));
    function onDocClick(ev: MouseEvent) {
      const t = ev.target as HTMLElement | null;
      if (t && !t.closest("[data-play-split]")) playMenu = false;
    }
    document.addEventListener("click", onDocClick);
    return () => {
      unsubs.forEach((u) => u());
      document.removeEventListener("click", onDocClick);
      window.removeEventListener("focus", onFocus);
      window.clearInterval(streamPoller);
    };
  });

  async function refresh() {
    try {
      workdir = await api.getWorkdir();
      hosts = await api.listHosts();
      dlConfig = await api.getDownloadConfig();
      streams = await api.listStreams();
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  let workersTimer: number | undefined;
  function setWorkers(n: number) {
    dlConfig = { ...dlConfig, workers: n };
    window.clearTimeout(workersTimer);
    workersTimer = window.setTimeout(() => {
      void api.setDownloadConfig(n).then(
        (cfg) => {
          dlConfig = cfg;
        },
        (e) => toastStore.push("error", String(e)),
      );
    }, 200);
  }

  async function pickFolder() {
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked !== "string") return;
    try {
      workdir = await api.setWorkdir(picked);
      hosts = await api.listHosts();
      addLog("info", `workdir ${workdir}`);
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  const subtitleOptions = $derived.by(() => {
    const opts = subTracks.map((t, i) => ({ id: String(i), label: t.label }));
    if (!subTracks.length) return opts;
    return [{ id: "auto", label: "Auto (English preferred)" }, ...opts, { id: "off", label: "Off" }];
  });

  function applySubtitles(incoming: Subtitle[]) {
    subTracks = incoming;
    if (!subTracks.length) {
      subtitle = "auto";
      return;
    }
    if (subtitle !== "auto" && subtitle !== "off") {
      const n = Number(subtitle);
      if (!Number.isInteger(n) || n < 0 || n >= subTracks.length) subtitle = "auto";
    }
  }

  function applyResolve(res: Awaited<ReturnType<typeof api.resolve>>) {
    if (res.status === "needReferer") {
      needReferer = res;
      return false;
    }
    kind = res.kind;
    variants = res.variants;
    applySubtitles(res.subtitles || []);
    if (variants.length && !variants.some((v) => v.quality === quality)) {
      quality = variants[0].quality;
    }
    if (!variants.length) quality = "best";
    return true;
  }

  function applyPlayResult(res: Awaited<ReturnType<typeof api.play>>) {
    if (res.status === "needReferer") {
      needReferer = res;
      return false;
    }
    kind = res.kind;
    if (res.variants.length) {
      variants = res.variants;
      if (!variants.some((v) => v.quality === quality)) {
        quality = variants[0].quality;
      }
    } else if (res.kind !== "hls") {
      variants = [];
      quality = "best";
    }
    if ((res.subtitles || []).length) applySubtitles(res.subtitles || []);
    else if (res.kind !== "hls") applySubtitles([]);
    return true;
  }

  let resolveGen = 0;
  let resolveTimer: number | undefined;

  function scheduleResolve() {
    window.clearTimeout(resolveTimer);
    const u = cleanUrl(url);
    if (!looksLikeUrl(u)) {
      variants = [];
      subTracks = [];
      subtitle = "auto";
      return;
    }
    resolveTimer = window.setTimeout(() => void autoResolve(), 450);
  }

  async function autoResolve() {
    if (/^\s*curl\b/i.test(url)) {
      await applyPastedText(url);
    }
    const u = cleanUrl(url);
    if (!looksLikeUrl(u)) return;
    const gen = ++resolveGen;
    resolving = true;
    try {
      const res = await api.resolve(u, headers);
      if (gen !== resolveGen) return;
      pendingAfterReferer = "resolve";
      applyResolve(res);
    } catch (e) {
      if (gen !== resolveGen) return;
      addLog("error", String(e));
      variants = [];
      subTracks = [];
      subtitle = "auto";
    } finally {
      if (gen === resolveGen) resolving = false;
    }
  }

  function applyCapture(
    parsed: api.ParsedCurl,
    source: string,
  ) {
    url = parsed.url;
    subtitles = (parsed.subtitles || []).filter((s) => /^https?:\/\//i.test(s));
    cookie = parsed.cookie || "";
    cookieHost = (parsed.cookieHost || "").toLowerCase();
    if (parsed.referer) {
      referer = parsed.referer;
      origin = parsed.origin || "";
    } else {
      applyRefererForUrl(parsed.url);
      if (parsed.origin) origin = parsed.origin;
    }
    if (parsed.userAgent) {
      userAgent = parsed.userAgent;
    }
    if (source !== "capture") {
      addLog(
        "info",
        `${source} ${parsed.url}${parsed.referer ? ` Referer=${parsed.referer}` : ""}${subtitles.length ? ` subs=${subtitles.length}` : ""}`,
      );
    }
    lastClipOffer = parsed.url;
    window.clearTimeout(resolveTimer);
    void autoResolve();
  }

  async function applyPastedText(text: string) {
    const parsed = await api.parseCurl(text);
    if (parsed?.url) {
      applyCapture(parsed, "pasted cURL");
      return true;
    }
    const next = text.replace(/[\r\n]+/g, "").trim();
    if (!next) return false;
    url = next;
    subtitles = [];
    applyRefererForUrl(next);
    cookie = "";
    cookieHost = "";
    addLog("info", `pasted ${next.length} chars`);
    return true;
  }

  function looksLikeStreamClipboard(text: string) {
    const t = text.trim();
    if (/^\s*curl\b/i.test(t)) return true;
    const one = t.replace(/[\r\n]+/g, "");
    try {
      const u = new URL(one);
      if (u.protocol !== "http:" && u.protocol !== "https:") return false;
      return /\.(m3u8|mpd|m3u)(\?|$)/i.test(u.pathname);
    } catch {
      return false;
    }
  }

  async function offerClipboard() {
    try {
      const text = await api.readClipboard();
      const t = text.trim();
      if (!t || t === lastClipOffer) return;
      if (!looksLikeStreamClipboard(t)) return;
      const parsed = await api.parseCurl(t);
      const candidate =
        parsed?.url || t.replace(/[\r\n]+/g, "").trim();
      if (candidate === cleanUrl(url)) return;
      lastClipOffer = t;
      toastStore.push("info", "Clipboard has a stream URL", {
        actionLabel: "Use this",
        onAction: () => {
          void (async () => {
            if (await applyPastedText(t)) {
              window.clearTimeout(resolveTimer);
              void autoResolve();
            }
          })();
        },
      });
    } catch {
      /* clipboard unavailable */
    }
  }

  async function pasteUrl() {
    try {
      const text = await api.readClipboard();
      if (!(await applyPastedText(text))) {
        addLog("warn", "clipboard is empty");
        return;
      }
      window.clearTimeout(resolveTimer);
      void autoResolve();
    } catch (e) {
      toastStore.push("error", String(e));
      addLog("error", String(e));
    }
  }

  async function doPlay(extra = false) {
    playMenu = false;
    if (!canPlay) return;
    const u = cleanUrl(url);
    busy = true;
    playingOp = "play";
    playStage = "";
    playDone = 0;
    playTotal = 1;
    pendingExtra = extra;
    try {
      const res = await api.play(u, quality || null, {
        ...headers,
        extra,
        subtitles,
        subtitle,
      });
      pendingAfterReferer = "play";
      if (!applyPlayResult(res)) return;
      playing = true;
      if (extra) extraCount += 1;
      else extraCount = Math.max(1, extraCount);
      toastStore.push(
        "success",
        extra ? "Opened another VLC" : "Opened in VLC",
      );
    } catch (e) {
      toastStore.push("error", String(e));
    } finally {
      busy = false;
      playingOp = null;
      playStage = "";
      playDone = 0;
      playTotal = 1;
    }
  }

  async function doDownload() {
    playMenu = false;
    const u = cleanUrl(url);
    if (!u) return;
    let dest: string;
    try {
      const suggested = await api.autoFilename(u);
      const picked = await save({
        defaultPath: `${workdir}/${suggested}`,
        filters: [{ name: "Video", extensions: ["mp4", "ts", "mkv"] }],
      });
      if (!picked) return;
      dest = picked;
    } catch (e) {
      toastStore.push("error", String(e));
      return;
    }
    try {
      const job = await api.download(u, dest, quality || null, headers);
      upsertJob(job);
      addLog("info", `queued download ${dest}`, "job", job.id);
    } catch (e) {
      toastStore.push("error", String(e));
      addLog("error", String(e));
    }
  }

  async function cancelJob(id: number) {
    try {
      const job = await api.cancelJob(id);
      upsertJob(job);
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  let streamPoller: number | undefined;
  function startStreamPolling() {
    window.clearInterval(streamPoller);
    streamPoller = window.setInterval(() => {
      if (streams.length === 0) return;
      void api.streamStats().then((list) => {
        streams = list;
      }).catch(() => {});
    }, 1500);
  }

  async function stopStream(id: number) {
    try {
      await api.stopStream(id);
      streams = streams.filter((x) => x.id !== id);
      addLog("info", `stopped stream ${id}`);
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  async function retryJob(job: Job) {
    if (job.status === "needReferer") {
      pendingAfterReferer = "download";
      pendingJobId = job.id;
      needReferer = {
        status: "needReferer",
        url: job.url,
        hint: job.hint || "",
        host: job.host,
      };
      return;
    }
    try {
      const next = await api.retryJob(job.id, headers);
      upsertJob(next);
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  async function clearJobs() {
    try {
      jobs = await api.clearJobs();
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  async function revealJob(path: string) {
    try {
      await revealItemInDir(path);
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  async function onRefererSave(value: string) {
    busy = true;
    try {
      hosts = await api.saveHost(
        cleanUrl(url) || needReferer?.url || "",
        value,
        origin.trim() || null,
      );
      referer = value;
      needReferer = null;
      const next = pendingAfterReferer;
      const extra = pendingExtra;
      const jobId = pendingJobId;
      pendingAfterReferer = null;
      pendingExtra = false;
      pendingJobId = null;
      busy = false;
      window.clearTimeout(resolveTimer);
      if (next === "play") await doPlay(extra);
      else if (next === "download" && jobId != null) {
        try {
          const job = await api.retryJob(jobId, {
            ...headers,
            referer: value,
          });
          upsertJob(job);
        } catch (e) {
          toastStore.push("error", String(e));
        }
      } else await autoResolve();
    } catch (e) {
      toastStore.push("error", String(e));
      busy = false;
    }
  }

  async function removeHost(host: string) {
    try {
      hosts = await api.deleteHost(host);
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  async function copyLogs() {
    if (!visibleLogs.length) return;
    const text = visibleLogs.map(formatLogLine).join("\n");
    try {
      await api.writeClipboard(text);
      toastStore.push("success", "Log copied");
    } catch (e) {
      toastStore.push("error", String(e));
    }
  }

  function useHost(row: HostRow) {
    referer = row.referer;
    if (row.origin) origin = row.origin;
    addLog("info", `using Referer for ${row.host}`);
    window.clearTimeout(resolveTimer);
    void autoResolve();
  }
</script>

<div class="flex h-full flex-col bg-surface-950 text-surface-100">
  <header
    class="flex items-center gap-3 border-b border-surface-800 bg-surface-900 px-4 py-2.5"
  >
    <div
      class="flex h-8 w-8 items-center justify-center rounded-lg bg-accent-600 text-sm font-semibold text-white"
    >
      ▶
    </div>
    <div>
      <h1 class="text-sm font-semibold leading-none">Play</h1>
      <p class="mt-0.5 text-xs text-surface-400">HLS in VLC, with seeking</p>
    </div>
    <button
      type="button"
      class="ml-auto max-w-md truncate rounded-lg border border-surface-700 bg-surface-850 px-3 py-1.5 text-xs text-surface-300 hover:bg-surface-800"
      onclick={pickFolder}
      title="Folder for .play.json"
    >
      {workdir || "Choose folder"}
    </button>
  </header>

  <main class="flex min-h-0 flex-1">
    <StreamsPane
      {streams}
      onStop={(id) => void stopStream(id)}
    />
    <div class="flex min-h-0 min-w-0 flex-1 flex-col gap-3 overflow-hidden p-4">
    <section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
      <div class="flex items-center justify-between gap-2">
        <label
          for="stream-url"
          class="text-xs font-medium uppercase tracking-wide text-surface-400"
          >Stream URL</label
        >
        {#if resolving}
          <span class="text-xs text-accent-400">Listing qualities…</span>
        {/if}
      </div>
      <div class="mt-1.5 flex items-start gap-2">
        <textarea
          id="stream-url"
          bind:this={urlBox}
          class="min-h-[5.5rem] flex-1 resize-y rounded-lg border border-surface-600 bg-surface-850 px-3 py-2 font-mono text-sm leading-5 outline-none ring-accent-500 focus:border-accent-500 focus:ring-2"
          placeholder="https://cdn.example/hls/master.m3u8 or paste Copy as cURL"
          spellcheck="false"
          wrap="soft"
          bind:value={url}
          oninput={scheduleResolve}
          onpaste={(e) => {
            const text = e.clipboardData?.getData("text") ?? "";
            if (!text.trim()) return;
            e.preventDefault();
            void (async () => {
              if (await applyPastedText(text)) {
                window.clearTimeout(resolveTimer);
                void autoResolve();
              }
            })();
          }}
          onkeydown={(e) => {
            if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
              e.preventDefault();
              void doPlay(false);
            }
          }}
        ></textarea>
        <div class="flex w-28 shrink-0 flex-col gap-2" data-play-split>
          <button
            type="button"
            class="rounded-lg border border-surface-600 bg-surface-800 px-3 py-2 text-sm font-medium text-surface-100 hover:bg-surface-700"
            onclick={pasteUrl}
            title="Paste URL or Copy as cURL from DevTools"
          >
            Paste
          </button>
          <div class="relative flex w-full">
            <button
              type="button"
              class="min-w-0 flex-1 rounded-l-lg bg-accent-600 px-3 py-2 text-sm font-semibold text-white shadow hover:bg-accent-500 disabled:opacity-45"
              onclick={() => doPlay(false)}
              disabled={!canPlay}
            >
              {playingOp === "play" ? "Opening…" : "Play"}
            </button>
            <button
              type="button"
              class="rounded-r-lg border-l border-accent-500/70 bg-accent-600 px-2 text-white hover:bg-accent-500 disabled:opacity-45"
              onclick={(e) => {
                e.stopPropagation();
                playMenu = !playMenu;
              }}
              disabled={!cleanUrl(url) || playingOp !== null}
              aria-label="More play options"
            >
              ▾
            </button>
            {#if playMenu}
              <div
                class="absolute left-0 right-0 top-full z-20 mt-1 overflow-hidden rounded-lg border border-surface-700 bg-surface-850 py-1 shadow-xl"
              >
                <button
                  type="button"
                  class="block w-full px-3 py-2 text-left text-sm text-surface-100 hover:bg-surface-800"
                  onclick={() => doPlay(true)}
                >
                  Play +1
                </button>
                <button
                  type="button"
                  class="block w-full px-3 py-2 text-left text-sm text-surface-100 hover:bg-surface-800"
                  onclick={() => {
                    playMenu = false;
                    void doDownload();
                  }}
                >
                  Download
                </button>
              </div>
            {/if}
          </div>
        </div>
      </div>
      <p class="mt-1.5 text-[11px] leading-4 text-surface-500">
        Or send from the Send to Play extension in Brave.
      </p>
      {#if playingOp === "play"}
        <div class="mt-2" role="status" aria-live="polite">
          <div class="flex items-center justify-between text-[11px] text-surface-400">
            <span>{playStageLabel}…</span>
            <span>{playPct}%</span>
          </div>
          <div class="mt-1 h-1.5 overflow-hidden rounded-full bg-surface-800">
            <div
              class="h-full rounded-full bg-accent-500 transition-[width]"
              style={`width: ${playPct}%`}
            ></div>
          </div>
        </div>
      {/if}
      {#if subtitles.length > 0}
        <p class="mt-1 text-[11px] text-accent-400">
          {subtitles.length === 1
            ? "1 subtitle will be sent to VLC"
            : `${subtitles.length} subtitles will be sent to VLC`}
        </p>
      {/if}

      {#if variants.length > 0}
        <div class="mt-3 flex flex-wrap items-center gap-1.5">
          {#each variants as v (v.quality + String(v.bandwidth))}
            <button
              type="button"
              class="rounded-full border px-3 py-1 text-xs font-medium {quality ===
              v.quality
                ? 'border-accent-500 bg-accent-600/20 text-accent-400'
                : 'border-surface-600 bg-surface-850 text-surface-300 hover:bg-surface-800'}"
              onclick={() => {
                quality = v.quality;
                addLog("debug", `quality ${v.label}`);
              }}
            >
              {v.label}
            </button>
          {/each}
          {#if subTracks.length > 0}
            <label class="ml-1 flex items-center gap-1.5 text-xs text-surface-400">
              <span>Subtitles</span>
              <select
                class="rounded-lg border border-surface-600 bg-surface-850 px-2 py-1 text-xs text-surface-100 outline-none focus:border-accent-500"
                bind:value={subtitle}
                onchange={() => {
                  const found = subtitleOptions.find((o) => o.id === subtitle);
                  addLog("debug", `subtitle ${found?.label || subtitle}`);
                }}
              >
                {#each subtitleOptions as o (o.id)}
                  <option value={o.id}>{o.label}</option>
                {/each}
              </select>
            </label>
          {/if}
        </div>
      {:else if subTracks.length > 0}
        <div class="mt-3 flex flex-wrap items-center gap-1.5">
          <label class="flex items-center gap-1.5 text-xs text-surface-400">
            <span>Subtitles</span>
            <select
              class="rounded-lg border border-surface-600 bg-surface-850 px-2 py-1 text-xs text-surface-100 outline-none focus:border-accent-500"
              bind:value={subtitle}
              onchange={() => {
                const found = subtitleOptions.find((o) => o.id === subtitle);
                addLog("debug", `subtitle ${found?.label || subtitle}`);
              }}
            >
              {#each subtitleOptions as o (o.id)}
                <option value={o.id}>{o.label}</option>
              {/each}
            </select>
          </label>
        </div>
      {/if}

      <details class="mt-3" bind:open={advancedOpen}>
        <summary
          class="cursor-pointer select-none text-xs text-surface-500 hover:text-surface-300"
          >Referer, Origin and User-Agent</summary
        >
        <div class="mt-2">
          <label
            for="referer-url"
            class="text-xs uppercase tracking-wide text-surface-500">Referer</label
          >
          <input
            id="referer-url"
            class="mt-1 w-full rounded-lg border border-surface-700 bg-surface-850 px-3 py-1.5 font-mono text-xs outline-none ring-accent-500 focus:ring-2"
            type="text"
            spellcheck="false"
            placeholder="player iframe URL from the m3u8 request, not the outer page"
            bind:value={referer}
            oninput={() => {
              window.clearTimeout(resolveTimer);
              resolveTimer = window.setTimeout(() => void autoResolve(), 450);
            }}
          />
        </div>
        <div class="mt-2 grid gap-2 sm:grid-cols-2">
          <div>
            <label
              for="origin-url"
              class="text-xs uppercase tracking-wide text-surface-500">Origin</label
            >
            <input
              id="origin-url"
              class="mt-1 w-full rounded-lg border border-surface-700 bg-surface-850 px-3 py-1.5 font-mono text-xs outline-none ring-accent-500 focus:ring-2"
              type="url"
              placeholder="https://embed.example"
              bind:value={origin}
            />
          </div>
          <div>
            <label
              for="ua"
              class="text-xs uppercase tracking-wide text-surface-500"
              >User-Agent</label
            >
            <input
              id="ua"
              class="mt-1 w-full rounded-lg border border-surface-700 bg-surface-850 px-3 py-1.5 font-mono text-xs outline-none ring-accent-500 focus:ring-2"
              type="text"
              placeholder="browser default"
              bind:value={userAgent}
            />
          </div>
        </div>
        <div class="mt-2 rounded-lg border border-surface-700">
          <div class="flex items-center justify-between px-3 py-1.5">
            <span class="text-xs uppercase tracking-wide text-surface-500">Saved hosts</span>
            <span class="text-xs font-normal text-surface-500">
              {#if hosts.length > 0}{hosts.length}{/if}
              .play.json
            </span>
          </div>
          <div class="max-h-36 overflow-auto border-t border-surface-700">
            {#if hosts.length === 0}
              <p class="px-3 py-3 text-center text-xs text-surface-500">
                Referers you save after a 403 land here.
              </p>
            {:else}
              <table class="w-full text-left text-sm">
                <tbody>
                  {#each hosts as row (row.host)}
                    <tr class="border-t border-surface-700 hover:bg-surface-800">
                      <td class="px-3 py-1.5 font-mono text-xs">{row.host}</td>
                      <td
                        class="max-w-xs truncate px-3 py-1.5 font-mono text-xs text-surface-300"
                        >{row.referer}</td
                      >
                      <td class="px-3 py-1.5 text-right">
                        <button
                          type="button"
                          class="text-xs text-accent-400 hover:underline"
                          onclick={() => useHost(row)}>Use</button
                        >
                        <button
                          type="button"
                          class="ml-3 text-xs text-danger-400 hover:underline"
                          onclick={() => removeHost(row.host)}>Delete</button
                        >
                      </td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            {/if}
          </div>
        </div>
      </details>

    </section>

    <DebugLog
      lines={visibleLogs}
      total={logs.length}
      filter={logFilter}
      onFilter={(f) => (logFilter = f)}
      onClear={() => (logs = [])}
      onCopy={() => void copyLogs()}
    />

    </div>
    <JobsPane
      {jobs}
      selectedId={logFilter.kind === "job" ? (logFilter.id ?? null) : null}
      workers={dlConfig.workers}
      minWorkers={dlConfig.minWorkers}
      maxWorkers={dlConfig.maxWorkers}
      onSelect={(id) => (logFilter = { kind: "job", id })}
      onCancel={(id) => void cancelJob(id)}
      onRetry={(job) => void retryJob(job)}
      onClear={() => void clearJobs()}
      onReveal={(path) => void revealJob(path)}
      onWorkers={(n) => void setWorkers(n)}
    />
  </main>

  <footer
    class="flex items-center gap-3 border-t border-surface-800 bg-surface-900 px-4 py-1.5 text-xs text-surface-400"
  >
    <span>{kind}</span>
    {#if hosts.length > 0}
      <span class="text-surface-600">·</span>
      <span>{hosts.length} {hosts.length === 1 ? "host" : "hosts"}</span>
    {/if}
    <span class="ml-auto flex items-center gap-2">
      {#if jobsActive > 0}
        <span
          class="inline-block h-2 w-2 animate-pulse rounded-full bg-accent-400"
        ></span>
        <span
          >{jobsActive}
          {jobsActive === 1 ? "download" : "downloads"}</span
        >
      {:else if busy}
        <span
          class="inline-block h-2 w-2 animate-pulse rounded-full bg-accent-400"
        ></span>
        <span>Working…</span>
      {:else if playing}
        <span class="inline-block h-2 w-2 rounded-full bg-success-400"></span>
        <span
          >Playing in VLC{#if extraCount > 1}
            ({extraCount})
          {/if}</span
        >
      {:else}
        <span class="text-surface-600">Ready</span>
      {/if}
    </span>
  </footer>
</div>

<ToastStack />

{#if needReferer}
  <RefererDialog
    host={needReferer.host}
    hint={needReferer.hint}
    busy={busy}
    onConfirm={onRefererSave}
    onCancel={() => {
      needReferer = null;
      pendingAfterReferer = null;
      pendingJobId = null;
    }}
  />
{/if}
