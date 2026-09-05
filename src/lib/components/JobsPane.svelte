<script lang="ts">
  import type { Job } from "../types";

  interface Props {
    jobs: Job[];
    selectedId?: number | null;
    workers: number;
    minWorkers: number;
    maxWorkers: number;
    onSelect: (id: number) => void;
    onCancel: (id: number) => void;
    onRetry: (job: Job) => void;
    onClear: () => void;
    onReveal: (path: string) => void;
    onWorkers: (n: number) => void;
  }
  let { jobs, selectedId = null, workers, minWorkers, maxWorkers, onSelect, onCancel, onRetry, onClear, onReveal, onWorkers }: Props = $props();
  let open = $state(false);

  function setWorkers(raw: number) {
    const n = Math.min(maxWorkers, Math.max(minWorkers, Math.round(Number(raw)) || minWorkers));
    if (n !== workers) onWorkers(n);
  }

  const active = $derived(
    jobs.filter((j) =>
      ["queued", "running", "remuxing", "cancelling"].includes(j.status),
    ).length,
  );
  const finished = $derived(
    jobs.filter((j) =>
      ["done", "error", "cancelled", "needReferer"].includes(j.status),
    ).length,
  );

  function isActive(job: Job) {
    return ["queued", "running", "remuxing", "cancelling"].includes(job.status);
  }

  function base(p: string) {
    const i = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
    return i >= 0 ? p.slice(i + 1) : p;
  }

  function fmtBytes(n: number) {
    if (!n) return "";
    if (n < 1024) return `${n} B`;
    if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
    return `${(n / 1024 ** 3).toFixed(2)} GB`;
  }

  function pct(job: Job) {
    if (!job.total) return 0;
    return Math.min(100, Math.round((job.done / job.total) * 100));
  }

  function statusLabel(job: Job) {
    switch (job.status) {
      case "queued":
        return "Queued";
      case "running":
        return job.done >= job.total && job.total > 0
          ? "Finishing…"
          : `${job.done}/${job.total}`;
      case "remuxing":
        return "Remuxing…";
      case "cancelling":
        return "Cancelling…";
      case "done":
        return "Done";
      case "error":
        return "Failed";
      case "cancelled":
        return "Cancelled";
      case "needReferer":
        return "Needs Referer";
      default:
        return job.status;
    }
  }

  function barClass(job: Job) {
    if (job.status === "error" || job.status === "needReferer")
      return "bg-danger-400";
    if (job.status === "cancelled") return "bg-surface-600";
    if (job.status === "done") return "bg-accent-500";
    return "bg-accent-400";
  }
</script>

<aside
  class="flex shrink-0 flex-col border-l border-surface-800 bg-surface-900 {open
    ? 'w-80'
    : 'w-10'}"
>
  {#if !open}
    <button
      type="button"
      class="flex h-full w-full flex-col items-center gap-2 py-3 text-surface-400 hover:bg-surface-850 hover:text-surface-200"
      onclick={() => (open = true)}
      aria-expanded="false"
      title="Show jobs"
    >
      <span class="text-xs">›</span>
      <span class="rotate-180 text-xs tracking-wide [writing-mode:vertical-rl]"
        >Jobs</span
      >
      {#if active > 0}
        <span
          class="flex h-5 min-w-5 items-center justify-center rounded-full bg-accent-600/20 px-1 text-[10px] font-medium text-accent-400"
          >{active}</span
        >
      {/if}
    </button>
  {:else}
  <div
    class="flex items-center justify-between border-b border-surface-800 px-3 py-2"
  >
    <button
      type="button"
      class="flex items-center gap-2 text-sm font-semibold hover:text-accent-400"
      onclick={() => (open = false)}
      aria-expanded="true"
      title="Hide jobs"
    >
      <span class="text-xs font-normal text-surface-500">‹</span>
      Jobs
      {#if active > 0}
        <span class="text-xs font-normal text-accent-400">{active}</span>
      {/if}
    </button>
    <button
      type="button"
      class="rounded-md px-2 py-0.5 text-xs text-surface-400 hover:bg-surface-800 hover:text-surface-100 disabled:opacity-40"
      disabled={finished === 0}
      onclick={onClear}
    >
      Clear
    </button>
  </div>
  <div class="min-h-0 flex-1 overflow-auto">
    {#if jobs.length === 0}
      <p class="px-3 py-6 text-center text-xs text-surface-500">
        Downloads run here in the background. You can queue several at once.
      </p>
    {:else}
      <ul class="flex flex-col gap-px">
        {#each jobs as job (job.id)}
          <li
            class="border-b border-surface-800 px-3 py-2.5 {selectedId === job.id
              ? 'bg-surface-850'
              : 'hover:bg-surface-850/60'}"
          >
            <div class="flex items-start gap-2">
              <button
                type="button"
                class="min-w-0 flex-1 text-left"
                title="Show this job in Debug"
                onclick={() => onSelect(job.id)}
              >
                <p
                  class="truncate font-mono text-xs text-surface-100"
                  title={job.dest}
                >
                  {base(job.dest)}
                </p>
                <p class="mt-0.5 truncate text-[11px] text-surface-500">
                  {job.quality}
                  {#if job.host}
                    · {job.host}
                  {/if}
                  {#if job.bytes}
                    · {fmtBytes(job.bytes)}
                  {/if}
                </p>
              </button>
              {#if isActive(job)}
                <button
                  type="button"
                  class="shrink-0 rounded-md border border-surface-600 px-2 py-0.5 text-[11px] text-surface-300 hover:border-danger-400 hover:text-danger-400 disabled:cursor-default disabled:opacity-50"
                  disabled={job.status === "cancelling"}
                  onclick={(e) => {
                    e.stopPropagation();
                    onCancel(job.id);
                  }}
                >
                  {job.status === "cancelling" ? "Cancelling" : "Cancel"}
                </button>
              {:else if job.status === "done"}
                <button
                  type="button"
                  class="shrink-0 text-[11px] text-accent-400 hover:underline"
                  onclick={(e) => {
                    e.stopPropagation();
                    onReveal(job.dest);
                  }}
                >
                  Show
                </button>
              {:else if job.status === "error" || job.status === "needReferer" || job.status === "cancelled"}
                <button
                  type="button"
                  class="shrink-0 text-[11px] text-accent-400 hover:underline"
                  onclick={(e) => {
                    e.stopPropagation();
                    onRetry(job);
                  }}
                >
                  Retry
                </button>
              {/if}
            </div>
            <div
              class="mt-1.5 h-1.5 overflow-hidden rounded-full bg-surface-800"
            >
              <div
                class="h-full rounded-full transition-[width] duration-150 {barClass(
                  job,
                )} {job.status === 'remuxing' ||
                (job.status === 'running' && job.done >= job.total)
                  ? 'animate-pulse'
                  : ''}"
                style="width: {job.status === 'queued' ? 4 : pct(job)}%"
              ></div>
            </div>
            <p
              class="mt-1 text-[11px] {job.status === 'error' ||
              job.status === 'needReferer'
                ? 'text-danger-400'
                : job.status === 'done'
                  ? 'text-accent-400'
                  : 'text-surface-500'}"
            >
              {#if job.status === "error"}
                {job.error || "Failed"}
              {:else if job.status === "needReferer"}
                {job.hint || "This host needs a Referer"}
              {:else}
                {statusLabel(job)}
              {/if}
            </p>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
  <section class="border-t border-surface-800 px-3 py-3">
    <h3 class="text-xs font-semibold uppercase tracking-wide text-surface-400">
      Download
    </h3>
    <label
      for="dl-workers"
      class="mt-2 flex items-center justify-between text-xs text-surface-300"
    >
      <span>Parallel connections</span>
      <input
        id="dl-workers-num"
        class="w-12 rounded-md border border-surface-700 bg-surface-850 px-1.5 py-0.5 text-center font-mono text-xs outline-none ring-accent-500 focus:ring-2"
        type="number"
        min={minWorkers}
        max={maxWorkers}
        value={workers}
        onchange={(e) =>
          setWorkers(Number((e.currentTarget as HTMLInputElement).value))
        }
      />
    </label>
    <input
      id="dl-workers"
      class="mt-2 w-full accent-accent-500"
      type="range"
      min={minWorkers}
      max={maxWorkers}
      step="1"
      value={workers}
      oninput={(e) =>
        setWorkers(Number((e.currentTarget as HTMLInputElement).value))
      }
    />
    <p class="mt-1.5 text-[11px] leading-4 text-surface-500">
      Segments fetched at once. Default 6. Saved in app config; new downloads
      pick it up.
    </p>
  </section>
  {/if}
</aside>
