import { invoke } from "@tauri-apps/api/core";
import type { DownloadConfig, HostRow, Job, ResolveResponse } from "./types";

export type HeaderOpts = {
  referer?: string | null;
  origin?: string | null;
  userAgent?: string | null;
  cookie?: string | null;
  cookieHost?: string | null;
};

export type ParsedCurl = {
  url: string;
  referer?: string | null;
  origin?: string | null;
  userAgent?: string | null;
  cookie?: string | null;
  cookieHost?: string | null;
  subtitles?: string[] | null;
};

export function getWorkdir(): Promise<string> {
  return invoke("get_workdir_cmd");
}

export function setWorkdir(path: string): Promise<string> {
  return invoke("set_workdir_cmd", { path });
}

export function getDownloadConfig(): Promise<DownloadConfig> {
  return invoke("get_download_config_cmd");
}

export function setDownloadConfig(workers: number): Promise<DownloadConfig> {
  return invoke("set_download_config_cmd", { workers });
}

export function listHosts(): Promise<HostRow[]> {
  return invoke("list_hosts_cmd");
}

export function saveHost(
  url: string,
  referer: string,
  origin?: string | null,
): Promise<HostRow[]> {
  return invoke("save_host_cmd", { url, referer, origin: origin ?? null });
}

export function deleteHost(host: string): Promise<HostRow[]> {
  return invoke("delete_host_cmd", { host });
}

export function resolve(
  url: string,
  opts: HeaderOpts = {},
): Promise<ResolveResponse> {
  return invoke("resolve_cmd", {
    url,
    referer: opts.referer ?? null,
    origin: opts.origin ?? null,
    userAgent: opts.userAgent ?? null,
    cookie: opts.cookie ?? null,
    cookieHost: opts.cookieHost ?? null,
  });
}

export function play(
  url: string,
  quality?: string | null,
  opts: HeaderOpts & { extra?: boolean; subtitles?: string[] } = {},
): Promise<ResolveResponse> {
  return invoke("play_cmd", {
    url,
    quality: quality ?? null,
    referer: opts.referer ?? null,
    origin: opts.origin ?? null,
    userAgent: opts.userAgent ?? null,
    cookie: opts.cookie ?? null,
    cookieHost: opts.cookieHost ?? null,
    extra: opts.extra ?? false,
    subtitles: opts.subtitles ?? null,
  });
}

export function stop(): Promise<void> {
  return invoke("stop_cmd");
}

export interface StreamSession {
  id: number;
  url: string;
  host: string;
  quality: string;
  status: string;
  pid?: number | null;
  playhead?: number | null;
  buffered: number;
  total: number;
  cachedBytes: number;
  served: number;
}

export function listStreams(): Promise<StreamSession[]> {
  return invoke("list_streams_cmd");
}

export function stopStream(id: number): Promise<void> {
  return invoke("stop_stream_cmd", { id });
}

export function streamStats(): Promise<StreamSession[]> {
  return invoke("stream_stats_cmd");
}

export function download(
  url: string,
  dest: string,
  quality?: string | null,
  opts: HeaderOpts = {},
): Promise<Job> {
  return invoke("download_cmd", {
    url,
    dest,
    quality: quality ?? null,
    referer: opts.referer ?? null,
    origin: opts.origin ?? null,
    userAgent: opts.userAgent ?? null,
    cookie: opts.cookie ?? null,
    cookieHost: opts.cookieHost ?? null,
  });
}

export function listJobs(): Promise<Job[]> {
  return invoke("list_jobs_cmd");
}

export function cancelJob(id: number): Promise<Job> {
  return invoke("cancel_job_cmd", { id });
}

export function retryJob(
  id: number,
  opts: HeaderOpts = {},
): Promise<Job> {
  return invoke("retry_job_cmd", {
    id,
    referer: opts.referer ?? null,
    origin: opts.origin ?? null,
    userAgent: opts.userAgent ?? null,
    cookie: opts.cookie ?? null,
    cookieHost: opts.cookieHost ?? null,
  });
}

export function clearJobs(): Promise<Job[]> {
  return invoke("clear_jobs_cmd");
}

export function autoFilename(url: string): Promise<string> {
  return invoke("auto_filename_cmd", { url });
}

export function readClipboard(): Promise<string> {
  return invoke("read_clipboard_cmd");
}

export function writeClipboard(text: string): Promise<void> {
  return invoke("write_clipboard_cmd", { text });
}

export function parseCurl(text: string): Promise<ParsedCurl | null> {
  return invoke("parse_curl_cmd", { text });
}

export function takeCapture(): Promise<ParsedCurl | null> {
  return invoke("take_capture_cmd");
}
