<script lang="ts">
  import type { LogFilter, LogLine } from "../types";
  import { sourceLabel } from "../log";

  interface Props {
    lines: LogLine[];
    total: number;
    filter: LogFilter;
    onFilter: (f: LogFilter) => void;
    onClear: () => void;
    onCopy: () => void;
  }
  let { lines, total, filter, onFilter, onClear, onCopy }: Props = $props();

  let scroller: HTMLDivElement | undefined = $state();
  let pin = $state(true);

  const colors: Record<string, string> = {
    debug: "text-surface-500",
    info: "text-surface-300",
    warn: "text-warning-400",
    error: "text-danger-400",
  };

  const chips: { kind: LogFilter["kind"]; label: string }[] = [
    { kind: "all", label: "All" },
    { kind: "play", label: "Play" },
    { kind: "vlc", label: "VLC" },
    { kind: "job", label: "Downloads" },
  ];

  function chipLabel(kind: LogFilter["kind"]) {
    if (kind === "job" && filter.kind === "job" && filter.id != null) {
      return `job ${filter.id}`;
    }
    return chips.find((c) => c.kind === kind)?.label ?? kind;
  }

  function clickChip(kind: LogFilter["kind"]) {
    if (kind === "job" && filter.kind === "job" && filter.id != null) {
      onFilter({ kind: "job" });
      return;
    }
    onFilter({ kind });
  }

  function clock(ms: number) {
    const d = new Date(ms);
    return d.toLocaleTimeString(undefined, { hour12: false });
  }

  $effect(() => {
    lines.length;
    if (pin && scroller) {
      scroller.scrollTop = scroller.scrollHeight;
    }
  });

  function onScroll() {
    if (!scroller) return;
    const gap = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    pin = gap < 24;
  }
</script>

<section
  class="flex min-h-0 flex-1 flex-col rounded-xl border border-surface-800 bg-surface-900"
>
  <div
    class="flex items-center justify-between gap-2 border-b border-surface-800 px-4 py-2"
  >
    <h2 class="text-sm font-semibold">Debug</h2>
    <div class="flex items-center gap-2">
      <span class="text-xs text-surface-500">
        {#if filter.kind === "all" || lines.length === total}
          {total}
        {:else}
          {lines.length} / {total}
        {/if}
      </span>
      <button
        type="button"
        class="rounded-md px-2 py-0.5 text-xs text-surface-400 hover:bg-surface-800 hover:text-surface-100"
        onclick={onCopy}
        disabled={lines.length === 0}
      >
        Copy
      </button>
      <button
        type="button"
        class="rounded-md px-2 py-0.5 text-xs text-surface-400 hover:bg-surface-800 hover:text-surface-100"
        onclick={onClear}
      >
        Clear
      </button>
    </div>
  </div>
  <div class="flex flex-wrap gap-1 border-b border-surface-800 px-4 py-1.5">
    {#each chips as chip (chip.kind)}
      <button
        type="button"
        class="rounded-full border px-2.5 py-0.5 text-[11px] font-medium {filter.kind ===
        chip.kind
          ? 'border-accent-500 bg-accent-600/20 text-accent-400'
          : 'border-surface-700 bg-surface-850 text-surface-400 hover:bg-surface-800 hover:text-surface-200'}"
        onclick={() => clickChip(chip.kind)}
      >
        {chipLabel(chip.kind)}
      </button>
    {/each}
  </div>
  <div
    bind:this={scroller}
    onscroll={onScroll}
    class="min-h-[8rem] flex-1 overflow-auto px-3 py-2 font-mono text-[11px] leading-5"
  >
    {#if total === 0}
      <p class="text-surface-600">
        Logs from fetch, VLC, and downloads show up here.
      </p>
    {:else if lines.length === 0}
      <p class="text-surface-600">No lines for this filter.</p>
    {:else}
      {#each lines as line, i (line.ms + ":" + i)}
        <div class="flex gap-2 {colors[line.level] ?? "text-surface-300"}">
          <span class="shrink-0 text-surface-600">{clock(line.ms)}</span>
          <span class="w-10 shrink-0 uppercase">{line.level}</span>
          <span class="w-[4.75rem] shrink-0 truncate text-surface-500" title={sourceLabel(line)}
            >{sourceLabel(line)}</span
          >
          <span class="min-w-0 break-all">{line.msg}</span>
        </div>
      {/each}
    {/if}
  </div>
</section>
