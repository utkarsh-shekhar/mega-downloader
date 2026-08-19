export type NodeKind = "file" | "folder";

export interface TreeNode {
  handle: string;
  parent: string | null;
  kind: NodeKind;
  name: string;
  size: number;
  rel_path: string;
}

export interface Tree {
  root_handle: string;
  root_name: string;
  total_files: number;
  total_folders: number;
  total_bytes: number;
  nodes: TreeNode[];
}

export type TransferStatus = "active" | "done" | "error";

export interface FileProgress {
  bytesDone: number;
  bytesTotal: number;
  status: TransferStatus;
  error?: string;
  /** transient note, e.g. "retry 2/8" */
  note?: string;
}

/** handle -> progress, accumulated from WebSocket events. */
export type ProgressMap = Record<string, FileProgress>;

export interface JobSummary {
  id: string;
  status: string; // pending | downloading | done | error | paused
  created_at: string;
  root_name: string | null;
  total: number;
  done: number;
  error: number;
  bytes_total: number;
  bytes_done: number;
}

export interface TransferState {
  bytes_done: number;
  bytes_total: number;
  status: string; // queued | active | done | error
}

/** Remote path mapping (Aria2-reported → local), the Sonarr/Radarr pattern. */
export interface PathMapping {
  id?: number;
  remote_path: string;
  local_path: string;
  position?: number;
}

/** Build the UI progress map from a job's persisted transfer states. */
export function transfersToProgress(
  transfers: Record<string, TransferState>,
): ProgressMap {
  const out: ProgressMap = {};
  for (const [handle, t] of Object.entries(transfers)) {
    if (t.status === "queued") continue;
    const status =
      t.status === "done" ? "done" : t.status === "error" ? "error" : "active";
    out[handle] = {
      bytesDone: status === "done" && t.bytes_total > 0 ? t.bytes_total : t.bytes_done,
      bytesTotal: t.bytes_total,
      status,
    };
  }
  return out;
}

/**
 * Extract MEGA share links from arbitrary pasted text. Splits on whitespace so
 * users can paste many links (one per line, or space-separated) at once, keeps
 * only tokens that look like mega.nz folder/file links, and de-duplicates while
 * preserving order. The engine re-validates each link, so this is a loose filter.
 */
export function parseMegaLinks(text: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of text.split(/\s+/)) {
    const tok = raw.trim();
    if (!tok || !tok.includes("#")) continue;
    if (!/mega(\.co)?\.nz\/(folder|file)\//i.test(tok)) continue;
    if (seen.has(tok)) continue;
    seen.add(tok);
    out.push(tok);
  }
  return out;
}

export function formatDuration(seconds: number): string {
  if (!isFinite(seconds) || seconds <= 0) return "—";
  const s = Math.round(seconds);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

export function formatBytes(n: number): string {
  if (n <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), units.length - 1);
  const v = n / Math.pow(1024, i);
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}
