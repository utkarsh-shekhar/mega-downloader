// The UI reaches the engine in one of three ways:
//
//  1. Vite dev server (port 5173) — /api and /ws are proxied, so we use the
//     same-origin (relative) URLs.
//  2. Self-contained single-process server — the engine itself serves this
//     page on its API port (8787 by default), so everything is same-origin and
//     relative URLs work with no CORS.
//  3. Packaged Tauri app — the UI runs on a custom protocol with no proxy, so
//     it must talk to the sidecar engine at its absolute localhost URL.
const sameOrigin = location.port === "5173" || location.port === "8787";

export const ENGINE = sameOrigin ? "" : "http://127.0.0.1:8787";
export const WS_URL = (sameOrigin ? `ws://${location.host}` : "ws://127.0.0.1:8787") + "/ws";

/** Absolute URL for an engine path (e.g. for `<a href>` downloads). */
export const api = (path: string) => `${ENGINE}${path}`;

/** `fetch` against the engine, resolving the base URL automatically. */
export const f = (path: string, init?: RequestInit) => fetch(api(path), init);
