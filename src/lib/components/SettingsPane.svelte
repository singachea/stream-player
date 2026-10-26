<script lang="ts">
  interface Props {
    workdir: string;
    onPick: () => void;
    lookahead: number;
    minLookahead: number;
    maxLookahead: number;
    onLookahead: (n: number) => void;
  }
  let { workdir, onPick, lookahead, minLookahead, maxLookahead, onLookahead }: Props =
    $props();

  function setLookahead(raw: number) {
    const n =
      Math.min(maxLookahead, Math.max(minLookahead, Math.round(Number(raw) / 5) * 5)) ||
      minLookahead;
    if (n !== lookahead) onLookahead(n);
  }
</script>

<div class="flex min-h-0 flex-1 flex-col gap-3 overflow-auto p-4">
  <section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
    <h2 class="text-sm font-semibold">Folder</h2>
    <p class="mt-1 text-[11px] leading-4 text-surface-500">
      Holds <span class="font-mono">.play.json</span> (saved hosts) and is the
      default location for downloads.
    </p>
    <div class="mt-2 flex items-center gap-2">
      <p
        class="min-w-0 flex-1 truncate rounded-lg border border-surface-700 bg-surface-850 px-3 py-1.5 font-mono text-xs text-surface-300"
        title={workdir || "No folder chosen"}
      >
        {workdir || "Choose folder"}
      </p>
      <button
        type="button"
        class="shrink-0 rounded-lg border border-surface-600 bg-surface-800 px-3 py-1.5 text-xs font-medium text-surface-100 hover:bg-surface-700"
        onclick={onPick}
        title="Folder for .play.json"
      >
        Choose…
      </button>
    </div>
  </section>
  <section class="rounded-xl border border-surface-800 bg-surface-900 p-4">
    <h2 class="text-sm font-semibold">Lookahead</h2>
    <p class="mt-1 text-[11px] leading-4 text-surface-500">
      Playlist percent prefetched ahead of playback. Saved in app config;
      every stream inherits it unless overridden in its detail.
    </p>
    <label
      for="lookahead"
      class="mt-2 flex items-center justify-between text-xs text-surface-300"
    >
      <span>Prefetch window</span>
      <span class="font-mono">{lookahead}%</span>
    </label>
    <input
      id="lookahead"
      class="mt-2 w-full accent-accent-500"
      type="range"
      min={minLookahead}
      max={maxLookahead}
      step="5"
      value={lookahead}
      oninput={(e) =>
        setLookahead(Number((e.currentTarget as HTMLInputElement).value))}
    />
  </section>
</div>
