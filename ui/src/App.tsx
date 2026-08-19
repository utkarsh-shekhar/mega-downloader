import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import TreeView from "./TreeView";
import { api, f, WS_URL } from "./api";
import {
  formatBytes,
  formatDuration,
  parseMegaLinks,
  type PathMapping,
  transfersToProgress,
  type JobSummary,
  type ProgressMap,
  type Tree,
} from "./types";

const LAST_JOB_KEY = "mega-dl:lastJob";

type WsStatus = "connecting" | "open" | "closed";

const statusBadge: Record<string, string> = {
  downloading: "bg-indigo-600",
  done: "bg-emerald-600",
  error: "bg-rose-700",
  paused: "bg-amber-600",
  pending: "bg-neutral-600",
};

export default function App() {
  const [wsStatus, setWsStatus] = useState<WsStatus>("connecting");
  const [engineVersion, setEngineVersion] = useState<string | null>(null);

  const [tokenSet, setTokenSet] = useState(false);
  const [tokenInput, setTokenInput] = useState("");
  const [dirInput, setDirInput] = useState("");
  const [concInput, setConcInput] = useState(4);
  const [aria2Url, setAria2Url] = useState("");
  const [aria2Secret, setAria2Secret] = useState("");
  const [aria2SecretSet, setAria2SecretSet] = useState(false);
  const [maxSpeed, setMaxSpeed] = useState("");
  const [aria2Status, setAria2Status] = useState<string | null>(null);
  const [pathMappings, setPathMappings] = useState<PathMapping[]>([]);
  const [savedMsg, setSavedMsg] = useState(false);

  const [speed, setSpeed] = useState(0);
  const speedSample = useRef<{ id: string; bytes: number; t: number } | null>(null);

  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  const showToast = (msg: string) => {
    setToast(msg);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 4000);
  };

  const [link, setLink] = useState("");
  const [loading, setLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const links = parseMegaLinks(link);

  const [jobs, setJobs] = useState<JobSummary[]>([]);
  // The currently-open tree: either a preview (no job yet) or a selected job.
  const [tree, setTree] = useState<Tree | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [progress, setProgress] = useState<ProgressMap>({});
  // Selected file handles for a preview tree (which files/folders to download).
  const [selectedFiles, setSelectedFiles] = useState<Set<string>>(new Set());
  const selectedIdRef = useRef<string | null>(null);
  selectedIdRef.current = selectedId;

  // The previewed tree is selectable only before it becomes a real job.
  const isPreview = tree !== null && selectedId === null;
  const previewFileHandles = useMemo(
    () => (tree ? tree.nodes.filter((n) => n.kind === "file").map((n) => n.handle) : []),
    [tree],
  );

  const toggleHandles = useCallback((handles: string[], checked: boolean) => {
    setSelectedFiles((prev) => {
      const next = new Set(prev);
      for (const h of handles) checked ? next.add(h) : next.delete(h);
      return next;
    });
  }, []);

  const fetchJobs = useCallback(async () => {
    try {
      const r = await f("/api/jobs").then((x) => x.json());
      setJobs(r.jobs ?? []);
    } catch {
      /* ignore */
    }
  }, []);

  const refreshSettings = useCallback(() => {
    f("/api/settings")
      .then((r) => r.json())
      .then((s) => {
        setTokenSet(!!s.rd_token_set);
        setDirInput(s.download_dir ?? "");
        setConcInput(s.concurrency ?? 4);
        setAria2Url(s.aria2_rpc_url ?? "");
        setAria2SecretSet(!!s.aria2_rpc_secret_set);
        setMaxSpeed(s.max_download_speed || "5M");
      })
      .catch(() => {});
  }, []);

  const fetchPathMappings = useCallback(() => {
    f("/api/path-mappings")
      .then((r) => r.json())
      .then((d) => setPathMappings(d.mappings ?? []))
      .catch(() => {});
  }, []);

  const refreshAria2Status = useCallback(() => {
    f("/api/aria2/status")
      .then((r) => r.json())
      .then((s) => {
        if (s.connected) setAria2Status(`Aria2 connected · ${s.version} · limit ${s.max_download_speed || "unlimited"}`);
        else if (s.configured) setAria2Status(`Aria2 unreachable${s.error ? ` (${s.error})` : ""}`);
        else setAria2Status("Aria2 not configured");
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    refreshSettings();
    fetchJobs();
    fetchPathMappings();
    refreshAria2Status();
    // Restore the last-viewed job across reloads.
    const last = localStorage.getItem(LAST_JOB_KEY);
    if (last) openJob(last);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshSettings, fetchJobs, fetchPathMappings, refreshAria2Status]);

  // Download-speed sampling for the selected job (updates as the list polls).
  useEffect(() => {
    const job = jobs.find((j) => j.id === selectedId);
    if (!job || job.status !== "downloading") {
      setSpeed(0);
      speedSample.current = null;
      return;
    }
    const now = Date.now();
    const prev = speedSample.current;
    if (prev && prev.id === job.id) {
      const dt = (now - prev.t) / 1000;
      if (dt >= 0.5) setSpeed((job.bytes_done - prev.bytes) / dt);
    }
    speedSample.current = { id: job.id, bytes: job.bytes_done, t: now };
  }, [jobs, selectedId]);

  // Poll the job list while anything is active (keeps aggregate bars live).
  useEffect(() => {
    const anyActive = jobs.some((j) => j.status === "downloading");
    if (!anyActive) return;
    const t = setInterval(fetchJobs, 2000);
    return () => clearInterval(t);
  }, [jobs, fetchJobs]);

  // WebSocket: engine status + live per-file events for the open job.
  // Reconnects with backoff so an engine restart doesn't freeze live progress
  // until the page is reloaded.
  useEffect(() => {
    let ws: WebSocket | null = null;
    let retries = 0;
    let timer: number | undefined;
    let disposed = false;

    const connect = () => {
      if (disposed) return;
      setWsStatus("connecting");
      ws = new WebSocket(WS_URL);
      ws.onopen = () => {
        retries = 0;
        setWsStatus("open");
        fetchJobs(); // catch up on anything missed while disconnected
      };
      ws.onclose = () => {
        setWsStatus("closed");
        if (disposed) return;
        const delay = Math.min(15000, 1000 * 2 ** retries++);
        timer = window.setTimeout(connect, delay);
      };
      ws.onerror = () => ws?.close();
      ws.onmessage = (ev) => {
      let msg: any;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (msg.type === "hello") {
        setEngineVersion(msg.version);
        return;
      }
      if (msg.type === "job_done") {
        fetchJobs();
        return;
      }
      // Per-file events only matter for the job currently on screen.
      if (!msg.handle || msg.job_id !== selectedIdRef.current) return;
      setProgress((p) => {
        const cur = p[msg.handle];
        switch (msg.type) {
          case "progress":
            return { ...p, [msg.handle]: { bytesDone: msg.bytes_done, bytesTotal: msg.bytes_total, status: "active" } };
          case "file_retry":
            return { ...p, [msg.handle]: { bytesDone: cur?.bytesDone ?? 0, bytesTotal: cur?.bytesTotal ?? 0, status: "active", note: `retry ${msg.attempt}/${msg.max}` } };
          case "file_fallback":
            return { ...p, [msg.handle]: { bytesDone: cur?.bytesDone ?? 0, bytesTotal: cur?.bytesTotal ?? 0, status: "active", note: "via MEGA" } };
          case "file_done":
            return { ...p, [msg.handle]: { bytesDone: cur?.bytesTotal ?? cur?.bytesDone ?? 0, bytesTotal: cur?.bytesTotal ?? 0, status: "done" } };
          case "file_error":
            return { ...p, [msg.handle]: { bytesDone: cur?.bytesDone ?? 0, bytesTotal: cur?.bytesTotal ?? 0, status: "error", error: msg.error } };
          default:
            return p;
        }
      });
      };
    };

    connect();
    return () => {
      disposed = true;
      window.clearTimeout(timer);
      ws?.close();
    };
  }, [fetchJobs]);

  const saveSettings = async () => {
    const body: Record<string, unknown> = {
      download_dir: dirInput.trim(),
      concurrency: Math.min(16, Math.max(1, concInput || 4)),
      aria2_rpc_url: aria2Url.trim(),
      max_download_speed: maxSpeed.trim() || "5M",
    };
    if (tokenInput.trim()) body.rd_token = tokenInput.trim();
    if (aria2Secret.trim()) body.aria2_rpc_secret = aria2Secret.trim();
    await f("/api/settings", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    setTokenInput("");
    setAria2Secret("");
    refreshSettings();
    refreshAria2Status();
    setSavedMsg(true);
    setTimeout(() => setSavedMsg(false), 1500);
  };

  const addMapping = async () => {
    const remote = prompt("Aria2 path prefix (as Aria2 sees it)", "/rdtdownloads");
    if (!remote) return;
    const local = prompt("Local path prefix (same folder on this host)", "/mnt/media/media/rdtdownloads");
    if (!local) return;
    await f("/api/path-mappings", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ remote_path: remote.trim(), local_path: local.trim() }),
    });
    fetchPathMappings();
  };

  const removeMapping = async (id?: number) => {
    if (id === undefined) return;
    await f(`/api/path-mappings/${id}`, { method: "DELETE" });
    fetchPathMappings();
  };

  // Preview the structure of the first pasted link (listing is free/unmetered).
  const inspect = async () => {
    const first = links[0];
    if (!first) return;
    setLoading(true);
    setError(null);
    setSelectedId(null);
    setTree(null);
    setProgress({});
    try {
      const res = await f("/api/inspect", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ link: first }),
      });
      if (!res.ok) {
        setError(await res.text());
      } else {
        const data: Tree = await res.json();
        setTree(data);
        // Start with everything selected; the user trims down from there.
        setSelectedFiles(new Set(data.nodes.filter((n) => n.kind === "file").map((n) => n.handle)));
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  // Queue one job per pasted link. Each link is independent, so a bad link only
  // fails its own job; the rest still queue. Opens the first newly-created job.
  const startDownloads = async () => {
    if (links.length === 0) return;
    setError(null);
    setSubmitting(true);
    // File selection applies only when downloading a single inspected link.
    // Omit include_handles when everything is selected (download the whole folder).
    const applySelection =
      isPreview && links.length === 1 && selectedFiles.size < previewFileHandles.length;
    let firstJobId: string | null = null;
    const failures: string[] = [];
    for (const l of links) {
      try {
        const reqBody: Record<string, unknown> = { link: l };
        if (applySelection) reqBody.include_handles = Array.from(selectedFiles);
        const res = await f("/api/jobs", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(reqBody),
        });
        if (!res.ok) {
          failures.push(`${shortenLink(l)}: ${await res.text()}`);
          continue;
        }
        const result = await res.json();
        if (!firstJobId) firstJobId = result.job_id;
      } catch (e) {
        failures.push(`${shortenLink(l)}: ${String(e)}`);
      }
    }
    setSubmitting(false);
    await fetchJobs();
    const queued = links.length - failures.length;
    if (firstJobId) await openJob(firstJobId);
    if (failures.length > 0) {
      setError(`${failures.length} of ${links.length} link(s) failed:\n${failures.join("\n")}`);
    } else {
      setLink("");
      showToast(`Queued ${queued} download${queued === 1 ? "" : "s"}.`);
    }
  };

  const openJob = async (id: string) => {
    setError(null);
    try {
      const res = await f(`/api/jobs/${id}`);
      if (!res.ok) {
        // Job gone (e.g. deleted): drop a stale last-viewed pointer quietly.
        if (localStorage.getItem(LAST_JOB_KEY) === id) localStorage.removeItem(LAST_JOB_KEY);
        return;
      }
      const r = await res.json();
      if (!r.tree) return;
      setSelectedId(id);
      setTree(r.tree);
      setProgress(transfersToProgress(r.transfers ?? {}));
      localStorage.setItem(LAST_JOB_KEY, id);
    } catch (e) {
      setError(String(e));
    }
  };

  const jobAction = async (id: string, action: string) => {
    await f(`/api/jobs/${id}/${action}`, { method: "POST" });
    await fetchJobs();
    if (selectedId === id) await openJob(id);
  };

  const deleteJob = async (id: string) => {
    if (!window.confirm("Delete this job? Downloaded files stay on disk.")) return;
    await f(`/api/jobs/${id}`, { method: "DELETE" });
    if (selectedId === id) {
      setSelectedId(null);
      setTree(null);
      setProgress({});
      localStorage.removeItem(LAST_JOB_KEY);
    }
    fetchJobs();
  };

  const statusColor =
    wsStatus === "open" ? "bg-emerald-500" : wsStatus === "connecting" ? "bg-amber-500" : "bg-rose-500";

  const selectedJob = jobs.find((j) => j.id === selectedId) ?? null;

  return (
    <div className="min-h-screen bg-neutral-950 text-neutral-100">
      <div className="mx-auto max-w-4xl p-6 space-y-5">
        <header className="flex items-end justify-between">
          <div>
            <h1 className="text-2xl font-semibold tracking-tight">MEGA Structured Downloader</h1>
            <p className="text-sm text-neutral-400">Structure-preserving MEGA downloads via Real-Debrid</p>
          </div>
          <div className="flex items-center gap-2 text-xs text-neutral-400">
            <span className={`inline-block h-2.5 w-2.5 rounded-full ${statusColor}`} />
            engine {engineVersion ? `v${engineVersion}` : wsStatus}
          </div>
        </header>

        {/* settings */}
        <section className="rounded-xl border border-neutral-800 bg-neutral-900 p-4 space-y-3">
          <div className="flex items-center justify-between">
            <span className="text-xs uppercase tracking-wide text-neutral-500">Settings</span>
            {savedMsg && <span className="text-xs text-emerald-400">saved ✓</span>}
          </div>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <label className="text-sm text-neutral-400">
              Real-Debrid token{" "}
              {tokenSet ? <span className="text-emerald-400">● set</span> : <span className="text-rose-400">● not set</span>}
              <input
                type="password"
                value={tokenInput}
                onChange={(e) => setTokenInput(e.target.value)}
                placeholder={tokenSet ? "replace token…" : "paste from real-debrid.com/apitoken"}
                className="mt-1 w-full rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
              />
            </label>
            <label className="text-sm text-neutral-400">
              Parallel downloads (1–16)
              <input
                type="number"
                min={1}
                max={16}
                value={concInput}
                onChange={(e) => setConcInput(Number(e.target.value))}
                className="mt-1 w-full rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
              />
            </label>
            <label className="text-sm text-neutral-400 sm:col-span-2">
              Download folder
              <input
                value={dirInput}
                onChange={(e) => setDirInput(e.target.value)}
                placeholder="C:\\Users\\you\\Downloads\\MegaDownloader"
                className="mt-1 w-full rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
              />
            </label>
          </div>

          {/* Aria2 backend */}
          <div className="space-y-3 border-t border-neutral-800 pt-3">
            <div className="flex items-center justify-between">
              <span className="text-xs uppercase tracking-wide text-neutral-500">Aria2 download backend</span>
              {aria2Status && (
                <span className={`text-xs ${aria2Status.startsWith("Aria2 connected") ? "text-emerald-400" : "text-amber-400"}`}>
                  {aria2Status}
                </span>
              )}
            </div>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <label className="text-sm text-neutral-400">
                RPC URL
                <input
                  value={aria2Url}
                  onChange={(e) => setAria2Url(e.target.value)}
                  placeholder="http://aria2:6800/jsonrpc"
                  className="mt-1 w-full rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
                />
              </label>
              <label className="text-sm text-neutral-400">
                RPC secret{" "}
                {aria2SecretSet ? <span className="text-emerald-400">● set</span> : <span className="text-rose-400">● not set</span>}
                <input
                  type="password"
                  value={aria2Secret}
                  onChange={(e) => setAria2Secret(e.target.value)}
                  placeholder={aria2SecretSet ? "replace secret…" : "rpc-secret"}
                  className="mt-1 w-full rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
                />
              </label>
              <label className="text-sm text-neutral-400">
                Max download speed (per file)
                <input
                  value={maxSpeed}
                  onChange={(e) => setMaxSpeed(e.target.value)}
                  placeholder="5M · 0 = unlimited"
                  className="mt-1 w-full rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
                />
              </label>
            </div>
            <p className="text-xs text-neutral-500">
              Leave RPC URL empty to use the built-in downloader. Files are rate-limited and visible in
              AriaNg; on completion the engine moves them into the correct MEGA folders via the path mappings below.
            </p>
          </div>

          {/* Remote path mappings */}
          <div className="space-y-2 border-t border-neutral-800 pt-3">
            <div className="flex items-center justify-between">
              <span className="text-xs uppercase tracking-wide text-neutral-500">Remote path mappings</span>
              <button
                onClick={addMapping}
                className="rounded-md bg-neutral-700 px-2 py-1 text-xs font-medium hover:bg-neutral-600"
              >
                + Add
              </button>
            </div>
            {pathMappings.length === 0 ? (
              <p className="text-xs text-neutral-500">No mappings. Aria2 paths are used as-is.</p>
            ) : (
              <table className="w-full text-left text-xs">
                <thead>
                  <tr className="text-neutral-500">
                    <th className="pb-1 font-medium">Aria2 path</th>
                    <th className="pb-1 font-medium">Local path</th>
                    <th className="w-8" />
                  </tr>
                </thead>
                <tbody>
                  {pathMappings.map((m) => (
                    <tr key={m.id ?? m.remote_path} className="border-t border-neutral-800">
                      <td className="py-1 pr-2 font-mono text-neutral-300">{m.remote_path}</td>
                      <td className="py-1 pr-2 font-mono text-neutral-300">{m.local_path}</td>
                      <td className="py-1 text-right">
                        <button
                          onClick={() => removeMapping(m.id)}
                          className="text-neutral-500 hover:text-rose-400"
                          aria-label="remove mapping"
                        >
                          ✕
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>

          <button onClick={saveSettings} className="rounded-md bg-neutral-700 px-3 py-2 text-sm font-medium hover:bg-neutral-600">
            Save settings
          </button>
        </section>

        {/* new download */}
        <section className="rounded-xl border border-neutral-800 bg-neutral-900 p-4 space-y-3">
          <div className="flex items-center justify-between">
            <label className="block text-sm text-neutral-400">MEGA folder links</label>
            {links.length > 0 && (
              <span className="text-xs text-neutral-500">
                {links.length} link{links.length === 1 ? "" : "s"} detected
              </span>
            )}
          </div>
          <textarea
            value={link}
            onChange={(e) => setLink(e.target.value)}
            onKeyDown={(e) => {
              // Ctrl/Cmd+Enter queues all; plain Enter stays a newline for multi-paste.
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                e.preventDefault();
                startDownloads();
              }
            }}
            rows={links.length > 1 ? 4 : 2}
            placeholder={"https://mega.nz/folder/<id>#<key>\nPaste one link per line to queue several at once."}
            className="w-full resize-y rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm font-mono outline-none focus:border-indigo-500"
          />
          <div className="flex gap-2">
            <button onClick={inspect} disabled={loading || links.length === 0} title={links.length > 1 ? "Previews the first link" : ""} className="rounded-md bg-neutral-700 px-4 py-2 text-sm font-medium hover:bg-neutral-600 disabled:opacity-40">
              {loading ? "Inspecting…" : links.length > 1 ? "Inspect first" : "Inspect"}
            </button>
            <button
              onClick={startDownloads}
              disabled={submitting || links.length === 0 || !tokenSet || (isPreview && links.length === 1 && selectedFiles.size === 0)}
              title={!tokenSet ? "Set your Real-Debrid token first" : isPreview && links.length === 1 && selectedFiles.size === 0 ? "Select at least one file" : ""}
              className="rounded-md bg-indigo-600 px-4 py-2 text-sm font-medium hover:bg-indigo-500 disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {submitting
                ? "Queuing…"
                : isPreview && links.length === 1 && selectedFiles.size < previewFileHandles.length
                  ? `Download ${selectedFiles.size} file${selectedFiles.size === 1 ? "" : "s"}`
                  : links.length > 1
                    ? `Download ${links.length}`
                    : "Download"}
            </button>
          </div>
          {error && <div className="whitespace-pre-wrap rounded-md border border-rose-900 bg-rose-950/50 px-3 py-2 text-sm text-rose-300">{error}</div>}
        </section>

        {/* jobs list */}
        {jobs.length > 0 && (
          <section className="rounded-xl border border-neutral-800 bg-neutral-900">
            <div className="border-b border-neutral-800 px-4 py-2 text-xs uppercase tracking-wide text-neutral-500">
              Jobs
            </div>
            <div className="divide-y divide-neutral-800">
              {jobs.map((j) => {
                const pct = j.bytes_total > 0 ? (j.bytes_done / j.bytes_total) * 100 : 0;
                return (
                  <div
                    key={j.id}
                    className={`flex items-center gap-3 px-4 py-2 cursor-pointer hover:bg-neutral-800/40 ${selectedId === j.id ? "bg-neutral-800/60" : ""}`}
                    onClick={() => openJob(j.id)}
                  >
                    <span className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] uppercase ${statusBadge[j.status] ?? "bg-neutral-600"}`}>
                      {j.status}
                    </span>
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm">{j.root_name ?? "(job)"}</div>
                      <div className="mt-1 h-1 w-full rounded bg-neutral-800 overflow-hidden">
                        <div className={`h-full ${j.status === "done" ? "bg-emerald-500" : j.error > 0 ? "bg-rose-500" : "bg-indigo-500"}`} style={{ width: `${Math.min(100, pct)}%` }} />
                      </div>
                    </div>
                    <span className="shrink-0 font-mono text-xs text-neutral-500">
                      {j.done}/{j.total}
                      {j.error > 0 ? ` · ${j.error}✗` : ""}
                    </span>
                    <div className="shrink-0 flex gap-1 text-xs" onClick={(e) => e.stopPropagation()}>
                      {j.status === "downloading" && <ActionBtn label="Pause" onClick={() => jobAction(j.id, "pause")} />}
                      {(j.status === "paused" || j.status === "error") && <ActionBtn label="Resume" onClick={() => jobAction(j.id, j.status === "error" ? "retry" : "resume")} />}
                      <a href={api(`/api/jobs/${j.id}/zip`)} onClick={(e) => { e.stopPropagation(); showToast("Preparing ZIP… your download will start shortly."); }} className="rounded bg-neutral-700 px-2 py-1 hover:bg-neutral-600">zip</a>
                      <ActionBtn label="✕" title="Delete job" onClick={() => deleteJob(j.id)} danger />
                    </div>
                  </div>
                );
              })}
            </div>
          </section>
        )}

        {/* selected job summary */}
        {selectedJob && tree && (
          <section className="rounded-xl border border-neutral-800 bg-neutral-900 p-4 space-y-2">
            <div className="flex items-center justify-between text-sm">
              <span className="text-neutral-300">
                {selectedJob.root_name} — {selectedJob.status} · {selectedJob.done}/{selectedJob.total}
                {selectedJob.error > 0 && <span className="text-rose-400"> · {selectedJob.error} failed</span>}
              </span>
              <span className="font-mono text-neutral-400">
                {formatBytes(selectedJob.bytes_done)} / {formatBytes(selectedJob.bytes_total)}
                {selectedJob.status === "downloading" && speed > 0 && (
                  <span className="text-neutral-500">
                    {" · "}
                    {formatBytes(speed)}/s · ETA{" "}
                    {formatDuration((selectedJob.bytes_total - selectedJob.bytes_done) / speed)}
                  </span>
                )}
              </span>
            </div>
            <div className="h-2 w-full rounded bg-neutral-800 overflow-hidden">
              <div className={`h-full ${selectedJob.status === "done" ? "bg-emerald-500" : selectedJob.error > 0 ? "bg-rose-500" : "bg-indigo-500"}`} style={{ width: `${selectedJob.bytes_total > 0 ? Math.min(100, (selectedJob.bytes_done / selectedJob.bytes_total) * 100) : 0}%` }} />
            </div>
            <div className="flex gap-2 pt-1">
              {selectedJob.status === "downloading" && <ActionBtn label="Pause" onClick={() => jobAction(selectedJob.id, "pause")} />}
              {selectedJob.status === "paused" && <ActionBtn label="Resume" onClick={() => jobAction(selectedJob.id, "resume")} />}
              {/* Retrying a running job is refused by the engine (two runs would
                  race over the same files), so only offer it once it stops. */}
              {selectedJob.error > 0 && selectedJob.status !== "downloading" && (
                <ActionBtn label={`Retry ${selectedJob.error} failed`} onClick={() => jobAction(selectedJob.id, "retry")} />
              )}
              {selectedJob.done > 0 && <a href={api(`/api/jobs/${selectedJob.id}/zip`)} onClick={() => showToast("Preparing ZIP of the whole job… your download will start shortly.")} className="rounded-md bg-neutral-700 px-3 py-1.5 text-sm font-medium hover:bg-neutral-600">Download all as ZIP</a>}
            </div>
          </section>
        )}

        {tree && (
          <TreeView
            tree={tree}
            progress={progress}
            jobId={selectedId}
            onZip={() => showToast("Preparing folder ZIP… your download will start shortly.")}
            selected={isPreview && links.length === 1 ? selectedFiles : undefined}
            onToggle={toggleHandles}
          />
        )}

        {!tree && jobs.length === 0 && !error && (
          <p className="text-center text-sm text-neutral-600">
            Paste a folder link to see its structure, then download it — folders preserved, with a native-MEGA fallback and zip export.
          </p>
        )}
      </div>

      {toast && (
        <div className="pointer-events-none fixed inset-x-0 bottom-6 flex justify-center">
          <div className="pointer-events-auto flex items-center gap-2 rounded-lg border border-neutral-700 bg-neutral-800 px-4 py-2 text-sm text-neutral-100 shadow-lg">
            <span className="inline-block h-3 w-3 animate-spin rounded-full border-2 border-neutral-500 border-t-indigo-400" />
            {toast}
          </div>
        </div>
      )}
    </div>
  );
}

/** Compact a link for error messages: drop the long decryption key fragment. */
function shortenLink(link: string): string {
  const noKey = link.split("#")[0];
  return noKey.length > 0 ? noKey : link;
}

function ActionBtn({ label, onClick, title, danger }: { label: string; onClick: () => void; title?: string; danger?: boolean }) {
  return (
    <button
      onClick={onClick}
      title={title}
      className={`rounded px-2 py-1 font-medium ${danger ? "bg-neutral-800 text-rose-400 hover:bg-rose-900/40" : "bg-neutral-700 hover:bg-neutral-600"}`}
    >
      {label}
    </button>
  );
}
