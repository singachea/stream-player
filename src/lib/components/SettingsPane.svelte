<script lang="ts">
  interface Props {
    workdir: string;
    onPick: () => void;
  }
  let { workdir, onPick }: Props = $props();
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
</div>
