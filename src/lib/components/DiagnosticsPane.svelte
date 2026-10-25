<script lang="ts">
  import type { StreamSession } from "../api";
  import type { Job, LogFilter, LogLine } from "../types";
  import DebugLog from "./DebugLog.svelte";

  interface Props {
    streams: StreamSession[];
    jobs: Job[];
    logTotal: number;
    capturePort?: number;
    logLines: LogLine[];
    logFilter: LogFilter;
    onLogFilter: (f: LogFilter) => void;
    onLogClear: () => void;
    onLogCopy: () => void;
  }
  let {
    streams,
    jobs,
    logTotal,
    capturePort = 17331,
    logLines,
    logFilter,
    onLogFilter,
    onLogClear,
    onLogCopy,
  }: Props = $props();

  const activeJobs = $derived(
    jobs.filter((j) =>
      ["queued", "running", "remuxing", "cancelling"].includes(j.status),
    ),
  );

  const vlcPids = $derived(
    [...new Set(streams.map((s) => s.pid).filter((p) => p != null))].sort(
      (a, b) => (a as number) - (b as number),
    ),
  );

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

  function jobPct(j: Job) {
    if (!j.total) return 0;
    return Math.min(100, Math.round((j.done / j.total) * 100));
  }
</script>

<div class="flex min-h-0 flex-1 flex-col gap-3 overflow-auto p-4">
  <section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
    <h2 class="text-sm font-semibold">VLC processes</h2>
    {#if vlcPids.length === 0}
      <p class="mt-1 text-xs text-surface-500">
        No VLC instances running. Press Play to start one.
      </p>
    {:else}
      <ul class="mt-2 flex flex-col gap-1">
        {#each vlcPids as pid (pid)}
          <li class="flex items-center gap-2 font-mono text-xs text-surface-200">
            <span
              class="inline-block h-2 w-2 rounded-full bg-success-400"
            ></span>
            <span>pid {pid}</span>
            <span class="truncate text-surface-500">
              {streams
                .filter((s) => s.pid === pid)
                .map((s) => `#${s.id} ${streamName(s)}`)
                .join(", ")}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
    <h2 class="text-sm font-semibold">
      Downloads
      {#if activeJobs.length > 0}
        <span class="ml-1 text-xs font-normal text-accent-400"
          >{activeJobs.length} active</span
        >
      {/if}
    </h2>
    {#if activeJobs.length === 0}
      <p class="mt-1 text-xs text-surface-500">
        No downloads running. Queued downloads spawn worker tasks here.
      </p>
    {:else}
      <ul class="mt-2 flex flex-col gap-2">
        {#each activeJobs as j (j.id)}
          <li class="text-xs">
            <div class="flex items-center justify-between gap-2">
              <span class="truncate font-mono text-surface-200" title={j.dest}>
                job {j.id} · {j.dest.split(/[/\\]/).pop() || j.dest}
              </span>
              <span class="shrink-0 text-surface-500">
                {j.status} {jobPct(j)}%
              </span>
            </div>
            <div
              class="mt-1 h-1.5 overflow-hidden rounded-full bg-surface-800"
            >
              <div
                class="h-full rounded-full bg-accent-400"
                style="width: {jobPct(j)}%"
              ></div>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
    <h2 class="text-sm font-semibold">Capture listener</h2>
    <p class="mt-1 font-mono text-xs text-surface-300">
      127.0.0.1:{capturePort}/open · listening
    </p>
    <p class="mt-1 text-[11px] leading-4 text-surface-500">
      The Send to Play extension POSTs here while the app runs. {logTotal} log
      lines buffered this session.
    </p>
  </section>

  <div class="h-96 shrink-0">
    <DebugLog
      lines={logLines}
      total={logTotal}
      filter={logFilter}
      onFilter={onLogFilter}
      onClear={onLogClear}
      onCopy={onLogCopy}
    />
  </div>
</div>
