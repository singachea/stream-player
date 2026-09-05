import type { Toast } from "../types";

let toasts = $state<Toast[]>([]);
let seq = 0;

export const toastStore = {
  get items() {
    return toasts;
  },
  push(
    kind: Toast["kind"],
    message: string,
    opts?: { ms?: number; actionLabel?: string; onAction?: () => void },
  ) {
    const id = ++seq;
    const ms = opts?.ms ?? (opts?.onAction ? 8000 : 3200);
    toasts = [
      ...toasts,
      {
        id,
        kind,
        message,
        actionLabel: opts?.actionLabel,
        onAction: opts?.onAction,
      },
    ];
    window.setTimeout(() => {
      toasts = toasts.filter((t) => t.id !== id);
    }, ms);
  },
  dismiss(id: number) {
    toasts = toasts.filter((t) => t.id !== id);
  },
};
