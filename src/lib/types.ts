export type ToastKind = "info" | "success" | "warning" | "error";

export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
  actionLabel?: string;
  onAction?: () => void;
}

export interface Variant {
  label: string;
  quality: string;
  height: number;
  bandwidth: number;
}

export interface HostRow {
  host: string;
  referer: string;
  origin: string;
}

export type ResolveOk = {
  status: "ok";
  kind: string;
  host: string;
  variants: Variant[];
  referer: string;
  origin: string;
};

export type NeedReferer = {
  status: "needReferer";
  url: string;
  hint: string;
  host: string;
};

export type ResolveResponse = ResolveOk | NeedReferer;

export type LogLevel = "debug" | "info" | "warn" | "error";

export type LogSource = "play" | "vlc" | "job";

export interface LogLine {
  ms: number;
  level: LogLevel;
  msg: string;
  source: LogSource;
  id?: number | null;
}

export type LogFilter = {
  kind: "all" | LogSource;
  id?: number | null;
};

export type JobStatus =
  | "queued"
  | "running"
  | "remuxing"
  | "cancelling"
  | "done"
  | "error"
  | "cancelled"
  | "needReferer";

export interface DownloadConfig {
  workers: number;
  minWorkers: number;
  maxWorkers: number;
}

export interface Job {
  id: number;
  url: string;
  host: string;
  dest: string;
  quality: string;
  status: JobStatus;
  done: number;
  total: number;
  bytes: number;
  error?: string | null;
  hint?: string | null;
}
