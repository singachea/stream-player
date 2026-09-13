<script lang="ts">
  import type { StreamSession } from "../api";

  interface Props {
    streams: StreamSession[];
    onStop: (id: number) => void;
  }
  let { streams, onStop }: Props = $props();
  let open = $state(true);

  const active = $derived(streams.filter((s) => s.status !== "stopped").length);

  function base(u: string) {
    const cleaned = u
      .replace(/[\r\n]+/g, "")
      .trim()
      .replace(/^(hlsvariant|hls|httpstream|dash):\/\//i, "");
    try {
      const url = new URL(cleaned);
      const parts = url.pathname.split("/").filter(Boolean);
      let last = parts.pop() || url.hostname;
      const stem = last.replace(/\.[^.]*$/, "");
      if (
        /^(master|index|playlist|chunklist|manifest|stream|video)([._-].*)?$/i.test(
          stem,
        )
      ) {
        const parent = parts.pop();
        if (parent) last = parent.replace(/\.[^.]*$/, "") || parent;
        else last = url.hostname;
      }
      return last.length > 32 ? `${last.slice(0, 32)}…` : last;
    } catch {
      return cleaned.length > 32 ? `${cleaned.slice(0, 32)}…` : cleaned;
    }
  }

  function fmtBytes(n: number) {
    if (!n) return "";
    if (n < 1024) return `${n} B`;
    if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
    return `${(n / 1024 ** 3).toFixed(2)} GB`;
  }

  function playheadLabel(s: StreamSession) {
    if (s.playhead == null) return "starting";
    if (!s.total) return `seg ${s.playhead}`;
    return `seg ${s.playhead}/${s.total}`;
  }

  function bufferPct(s: StreamSession) {
    if (!s.total) return 0;
    const ahead = Math.min(s.buffered, s.total);
    return Math.min(100, Math.round((ahead / s.total) * 100));
  }
</script>

<aside
  class="flex shrink-0 flex-col border-r border-surface-800 bg-surface-900 {open
    ? 'w-72'
    : 'w-10'}"
>
  {#if !open}
    <button
      type="button"
      class="flex h-full w-full flex-col items-center gap-2 py-3 text-surface-400 hover:bg-surface-850 hover:text-surface-200"
      onclick={() => (open = true)}
      aria-expanded="false"
      title="Show streams"
    >
      <span class="text-xs">‹</span>
      <span class="rotate-180 text-xs tracking-wide [writing-mode:vertical-rl]"
        >Streams</span
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
        title="Hide streams"
      >
        <span class="text-xs font-normal text-surface-500">›</span>
        Streams
        {#if active > 0}
          <span class="text-xs font-normal text-accent-400">{active}</span>
        {/if}
      </button>
    </div>
    <div class="min-h-0 flex-1 overflow-auto">
      {#if streams.length === 0}
        <p class="px-3 py-6 text-center text-xs text-surface-500">
          Playing streams show here with playhead and buffer ahead.
        </p>
      {:else}
        <ul class="flex flex-col gap-px">
          {#each streams as s (s.id)}
            <li class="border-b border-surface-800 px-3 py-2.5 hover:bg-surface-850/60">
              <div class="flex items-start gap-2">
                <div class="min-w-0 flex-1">
                  <p class="truncate font-mono text-xs text-surface-100" title={s.url}>
                    {base(s.url)}
                  </p>
                  <p class="mt-0.5 truncate text-[11px] text-surface-500">
                    {s.quality}
                    {#if s.host}
                      · {s.host}
                    {/if}
                    {#if s.cachedBytes}
                      · {fmtBytes(s.cachedBytes)}
                    {/if}
                  </p>
                </div>
                <button
                  type="button"
                  class="shrink-0 rounded-md border border-surface-600 px-2 py-0.5 text-[11px] text-surface-300 hover:border-danger-400 hover:text-danger-400"
                  onclick={() => onStop(s.id)}
                >
                  Stop
                </button>
              </div>
              <div class="mt-1.5 h-1.5 overflow-hidden rounded-full bg-surface-800">
                <div
                  class="h-full animate-pulse rounded-full bg-accent-500 transition-[width] duration-150"
                  style="width: {bufferPct(s)}%"
                ></div>
              </div>
              <p class="mt-1 text-[11px] text-accent-400">
                {playheadLabel(s)} · +{s.buffered} buffered
              </p>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</aside>
