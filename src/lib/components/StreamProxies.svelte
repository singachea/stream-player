<script lang="ts">
  import type { StreamSession } from "../api";

  interface Props {
    streams: StreamSession[];
    onStopStream: (id: number) => void;
    onSeekStream: (id: number, seg: number) => void;
  }
  let { streams, onStopStream, onSeekStream }: Props = $props();

  let expanded = $state<number[]>([]);

  function toggle(id: number) {
    expanded = expanded.includes(id)
      ? expanded.filter((x) => x !== id)
      : [...expanded, id];
  }

  function fmtBytes(n: number) {
    if (!n) return "—";
    if (n < 1024) return `${n} B`;
    if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
    return `${(n / 1024 ** 3).toFixed(2)} GB`;
  }

  function fmtSizeMB(n: number) {
    if (!n) return "—";
    if (n < 1024 ** 2) return `${(n / 1024).toFixed(0)} KB`;
    return `${(n / 1024 ** 2).toFixed(1)} MB`;
  }

  function fmtSpeed(bps?: number | null) {
    if (bps == null) return "—";
    if (bps < 1024) return `${bps} B/s`;
    if (bps < 1024 ** 2) return `${(bps / 1024).toFixed(1)} KB/s`;
    return `${(bps / 1024 ** 2).toFixed(1)} MB/s`;
  }

  const statusDot: Record<string, string> = {
    downloading: "bg-success-400",
    buffered: "bg-warning-400",
    error: "bg-danger-400",
    idle: "bg-surface-600",
  };

  const statusHint: Record<string, string> = {
    downloading: "Upstream traffic in flight or within the last 5s",
    buffered: "Idle, readahead window ahead is full",
    error: "Latest upstream result failed",
    idle: "No upstream activity",
  };

  function statusOf(s: StreamSession) {
    return s.fetchStatus || "idle";
  }

  function streamName(s: StreamSession) {
    if (s.title?.trim()) return s.title.trim();
    try {
      const u = new URL(
        s.url.replace(/^(hlsvariant|hls|httpstream|dash):\/\//i, ""),
      );
      const parts = u.pathname.split("/").filter(Boolean);
      return parts.pop() || u.hostname;
    } catch {
      return s.url;
    }
  }

  function playheadLabel(s: StreamSession) {
    if (s.playhead == null) return "starting";
    if (!s.total) return `seg ${s.playhead}`;
    return `seg ${s.playhead}/${s.total}`;
  }

  function cachedSet(s: StreamSession): Set<number> {
    return new Set(s.cached ?? []);
  }
</script>

<section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
  <div class="flex flex-wrap items-center gap-x-4 gap-y-1">
    <h2 class="text-sm font-semibold">
      Stream proxies
      {#if streams.length > 0}
        <span class="ml-1 text-xs font-normal text-accent-400"
          >{streams.length}</span
        >
      {/if}
    </h2>
  </div>
  {#if streams.length === 0}
    <p class="mt-1 text-xs text-surface-500">
      No local proxy sessions. Each Play spawns one proxy plus VLC.
    </p>
  {:else}
    <div class="mt-2 overflow-auto">
      <table class="w-full text-left text-xs">
        <thead>
          <tr class="text-surface-500">
            <th class="px-2 py-1 font-medium">Stream</th>
            <th class="px-2 py-1 font-medium">Quality</th>
            <th class="px-2 py-1 font-medium" title="Segments held in cache">Size</th>
            <th class="px-2 py-1 font-medium" title="Blocks ready for VLC">Ready</th>
            <th class="px-2 py-1 font-medium" title="Upstream bytes/sec, trailing 10s">Speed</th>
            <th class="px-2 py-1 font-medium">Status</th>
            <th class="px-2 py-1"><span class="sr-only">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each streams as s (s.id)}
            {@const have = cachedSet(s)}
            <tr class="border-t border-surface-800">
              <td
                class="max-w-48 truncate px-2 py-1.5 font-mono text-surface-100"
                title={s.url}
              >
                {streamName(s)}
              </td>
              <td class="px-2 py-1.5 font-mono text-surface-300">
                {s.quality}
              </td>
              <td class="px-2 py-1.5 font-mono text-surface-300">
                {fmtSizeMB(s.cachedBytes)}
              </td>
              <td class="px-2 py-1.5 font-mono text-surface-300">
                {#if s.total}{have.size}/{s.total}{:else}—{/if}
              </td>
              <td class="px-2 py-1.5 font-mono text-surface-300">
                {fmtSpeed(s.speedBps)}
              </td>
              <td class="px-2 py-1.5">
                <span
                  class="flex items-center gap-1.5 text-surface-200"
                  title={s.fetchStatus === "error" && s.lastError
                    ? s.lastError
                    : statusHint[statusOf(s)]}
                >
                  <span
                    class="inline-block h-2 w-2 shrink-0 rounded-full {statusDot[statusOf(s)] ??
                      statusDot.idle}"
                  ></span>
                  {statusOf(s)}
                </span>
              </td>
              <td class="whitespace-nowrap px-2 py-1.5 text-right">
                <button
                  type="button"
                  class="rounded-md px-2 py-0.5 text-[11px] text-accent-400 hover:underline"
                  aria-expanded={expanded.includes(s.id)}
                  onclick={() => toggle(s.id)}
                >
                  {expanded.includes(s.id) ? "Hide" : "Detail"}
                </button>
                <button
                  type="button"
                  class="ml-1 shrink-0 rounded-md border border-surface-600 px-2 py-0.5 text-[11px] text-surface-300 hover:border-danger-400 hover:text-danger-400"
                  onclick={() => onStopStream(s.id)}
                >
                  Stop
                </button>
              </td>
            </tr>
            {#if s.total}
              <tr class="border-t border-surface-800">
                <td colspan={7} class="px-3 py-2">
                  <div
                    class="flex flex-wrap items-center gap-px"
                    role="img"
                    aria-label={`${have.size} of ${s.total} blocks ready`}
                  >
                    {#each Array(s.total) as _, n (n)}
                      {@const playing = s.playhead === n}
                      <button
                        type="button"
                        class="h-2.5 w-2.5 cursor-pointer rounded-[2px] {playing
                          ? 'seg-playhead'
                          : have.has(n)
                            ? 'bg-accent-500'
                            : 'bg-surface-800'}"
                        title={playing
                          ? `seg ${n} playing — click to replay from here`
                          : `seg ${n}${have.has(n) ? " cached" : ""} — click to jump VLC here`}
                        aria-label={`Jump VLC to segment ${n}`}
                        onclick={() => onSeekStream(s.id, n)}
                      ></button>
                    {/each}
                  </div>
                </td>
              </tr>
            {/if}
            {#if expanded.includes(s.id)}
              <tr class="border-t border-surface-800 bg-surface-850/50">
                <td colspan={7} class="px-3 py-2.5">
                  <dl class="grid gap-x-6 gap-y-1.5 text-[11px] sm:grid-cols-2">
                    <div class="min-w-0">
                      <dt class="text-surface-500">Remote URL</dt>
                      <dd class="break-all font-mono text-surface-200">{s.url}</dd>
                    </div>
                    <div class="min-w-0">
                      <dt class="text-surface-500">Local URL (VLC opens this)</dt>
                      <dd class="break-all font-mono text-surface-200">
                        {s.localUrl || "—"}
                      </dd>
                    </div>
                    <div>
                      <dt class="text-surface-500">Host · Quality</dt>
                      <dd class="font-mono text-surface-200">
                        {s.host || "—"} · {s.quality}
                      </dd>
                    </div>
                    <div>
                      <dt class="text-surface-500">VLC pid · Playhead</dt>
                      <dd class="font-mono text-surface-200">
                        {s.pid ?? "—"} · {playheadLabel(s)} · +{s.buffered} buffered
                      </dd>
                    </div>
                    <div>
                      <dt class="text-surface-500">Segments · Served</dt>
                      <dd class="font-mono text-surface-200">
                        {s.total || "—"} total · {s.served} served
                      </dd>
                    </div>
                    <div>
                      <dt class="text-surface-500">Fetched · Cached</dt>
                      <dd class="font-mono text-surface-200">
                        {fmtBytes(s.fetchedBytes ?? 0)} fetched · {fmtBytes(
                          s.cachedBytes,
                        )} cached
                      </dd>
                    </div>
                  </dl>
                  {#if s.lastError}
                    <p class="mt-1.5 break-all font-mono text-[11px] text-danger-400">
                      Last error: {s.lastError}
                    </p>
                  {/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</section>

<style>
  @keyframes seg-playhead-flash {
    0%,
    100% {
      background-color: #9a3412;
    }
    50% {
      background-color: #fb923c;
    }
  }
  .seg-playhead {
    animation: seg-playhead-flash 1.8s ease-in-out infinite;
  }
  @media (prefers-reduced-motion: reduce) {
    .seg-playhead {
      animation: none;
      background-color: #fb923c;
    }
  }
</style>
