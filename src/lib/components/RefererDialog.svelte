<script lang="ts">
  interface Props {
    host: string;
    hint: string;
    busy?: boolean;
    onConfirm: (referer: string) => void;
    onCancel: () => void;
  }

  let { host, hint, busy = false, onConfirm, onCancel }: Props = $props();
  let value = $state("");

  function submit() {
    const v = value.trim();
    if (!v) return;
    onConfirm(v);
  }
</script>

<div
  class="fixed inset-0 z-40 flex items-center justify-center bg-black/60 p-6 backdrop-blur-sm"
  role="dialog"
  aria-modal="true"
  aria-labelledby="referer-title"
>
  <div
    class="w-full max-w-md overflow-hidden rounded-xl border border-surface-700 bg-surface-900 shadow-2xl"
  >
    <div class="px-5 py-4">
      <h2 id="referer-title" class="text-base font-semibold text-surface-100">
        Referer needed
      </h2>
      <p class="mt-2 text-sm leading-relaxed text-surface-300">
        HTTP 403 fetching <span class="font-mono text-surface-100">{host}</span>.
        Paste the player iframe URL from the m3u8 request in DevTools (not the
        outer page). Copy as cURL also works on Paste.
      </p>
      {#if hint}
        <p
          class="mt-2 rounded-lg bg-surface-850 px-3 py-2 text-xs text-surface-400"
        >
          {hint.replace(/^\s*\(|\)\s*$/g, "")}
        </p>
      {/if}
      <input
        class="mt-3 w-full rounded-lg border border-surface-600 bg-surface-850 px-3 py-2 text-sm text-surface-100 outline-none ring-accent-500 focus:ring-2"
        type="url"
        placeholder="https://embed.example/"
        bind:value
        onkeydown={(e) => {
          if (e.key === "Enter") submit();
          if (e.key === "Escape") onCancel();
        }}
      />
    </div>
    <footer
      class="flex items-center justify-end gap-2 border-t border-surface-800 px-5 py-3"
    >
      <button
        type="button"
        class="rounded-lg border border-surface-600 px-3 py-1.5 text-sm text-surface-200 hover:bg-surface-800 disabled:opacity-45"
        onclick={onCancel}
        disabled={busy}
      >
        Cancel
      </button>
      <button
        type="button"
        class="rounded-lg bg-accent-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-accent-500 disabled:opacity-45"
        onclick={submit}
        disabled={busy || !value.trim()}
      >
        {busy ? "Saving…" : "Save and retry"}
      </button>
    </footer>
  </div>
</div>
