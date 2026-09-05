<script lang="ts">
  import { toastStore } from "../stores/toasts.svelte";

  const styles: Record<string, string> = {
    info: "border-surface-600 bg-surface-800 text-surface-100",
    success: "border-success-400/40 bg-surface-800 text-success-400",
    warning: "border-warning-400/40 bg-surface-800 text-warning-400",
    error: "border-danger-400/40 bg-surface-800 text-danger-400",
  };
</script>

<div class="pointer-events-none fixed right-4 top-4 z-50 flex w-96 flex-col gap-2">
  {#each toastStore.items as toast (toast.id)}
    <div
      class="pointer-events-auto flex items-start gap-2 rounded-lg border px-3 py-2 text-sm shadow-lg backdrop-blur {styles[
        toast.kind
      ]}"
    >
      <button
        type="button"
        class="min-w-0 flex-1 text-left"
        onclick={() => toastStore.dismiss(toast.id)}
      >
        {toast.message}
      </button>
      {#if toast.actionLabel}
        <button
          type="button"
          class="shrink-0 text-xs font-semibold text-accent-400 hover:text-accent-500"
          onclick={() => {
            toast.onAction?.();
            toastStore.dismiss(toast.id);
          }}
        >
          {toast.actionLabel}
        </button>
      {/if}
    </div>
  {/each}
</div>
