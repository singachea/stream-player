import type { LogFilter, LogLine } from "./types";

export function matchLog(line: LogLine, f: LogFilter): boolean {
  if (f.kind === "all") return true;
  if (line.source !== f.kind) return false;
  if (f.id != null) return line.id === f.id;
  return true;
}

export function sourceLabel(line: LogLine): string {
  if (line.source === "vlc" && line.id != null) return `vlc ${line.id}`;
  if (line.source === "job" && line.id != null) return `job ${line.id}`;
  return line.source;
}

export function formatLogLine(line: LogLine): string {
  const t = new Date(line.ms).toLocaleTimeString(undefined, {
    hour12: false,
  });
  return `${t}\t${line.level.toUpperCase()}\t${sourceLabel(line)}\t${line.msg}`;
}
