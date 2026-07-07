// Xet wire-protocol client (/v1/*). The server exposes nothing else:
// uploads are chunked/hashed/packed in the browser (openxet-wasm) and POSTed
// as xorbs + a shard; downloads fetch the reconstruction plan and reassemble
// the file client-side from xorb byte ranges (chunk decoding via openxet-wasm).

import { authHeaders } from "./auth";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ChunkRange {
  start: number;
  end: number;
}

export interface ByteRange {
  start: number;
  end: number;
}

export interface ReconstructionTerm {
  hash: string;
  unpacked_length: number;
  range: ChunkRange;
}

export interface FetchInfo {
  range: ChunkRange;
  url: string;
  url_range: ByteRange;
}

export interface ReconstructionResponse {
  offset_into_first_range: number;
  terms: ReconstructionTerm[];
  fetch_info: Record<string, FetchInfo[]>;
}

export interface FileDetail {
  hash: string;
  total_size: number;
  reconstruction: ReconstructionResponse;
}

export interface UploadResult {
  file_hash: string;
  file_size: number;
  chunk_count: number;
  /** Chunks resolved to already-stored xorbs — skipped, not re-uploaded. */
  deduped_chunk_count: number;
  xorb_hashes: string[];
}

/**
 * What the upload pipeline is doing right now, for the UI. `hashing`,
 * `packing`, and `registering` are indeterminate (single blocking steps);
 * `dedup` reports how many chunk probes have settled; `uploading` reports
 * bytes.
 */
export type UploadStatus =
  | { phase: "hashing" }
  | { phase: "dedup"; queried: number }
  | { phase: "packing" }
  | { phase: "uploading"; uploaded: number; total: number }
  | { phase: "registering" };

// ─── HTTP helpers ────────────────────────────────────────────────────────────

async function authFetch(path: string, init?: RequestInit): Promise<Response> {
  const res = await fetch(path, {
    ...init,
    headers: { ...init?.headers, ...authHeaders() },
  });
  if (!res.ok && res.status !== 206) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res;
}

// ─── Read paths ──────────────────────────────────────────────────────────────

export async function fetchFileDetail(hash: string): Promise<FileDetail> {
  const res = await authFetch(`/v1/reconstructions/${hash}`);
  const reconstruction: ReconstructionResponse = await res.json();
  const total_size = reconstruction.terms.reduce(
    (sum, t) => sum + t.unpacked_length,
    0,
  );
  return { hash, total_size, reconstruction };
}

/**
 * Download file bytes the Xet way: fetch the reconstruction plan (Range-aware),
 * pull each term's xorb byte range from its presigned fetch URL, decode the
 * chunk frames in wasm, and reassemble.
 */
async function reconstructContent(
  hash: string,
  range?: { start: number; end: number },
): Promise<ArrayBuffer> {
  const res = await authFetch(
    `/v1/reconstructions/${hash}`,
    range ? { headers: { Range: `bytes=${range.start}-${range.end}` } } : {},
  );
  const recon: ReconstructionResponse = await res.json();
  const wasm = await loadWasm();

  const parts = await Promise.all(
    recon.terms.map(async (term) => {
      const info = recon.fetch_info[term.hash]?.find(
        (f) =>
          f.range.start <= term.range.start && f.range.end >= term.range.end,
      );
      if (!info) throw new Error(`no fetch info for xorb ${term.hash}`);
      // fetch_info URLs are presigned (token in query) — no auth header.
      const r = await fetch(info.url, {
        headers: {
          Range: `bytes=${info.url_range.start}-${info.url_range.end}`,
        },
      });
      if (!r.ok && r.status !== 206) {
        throw new Error(`xorb fetch failed: HTTP ${r.status}`);
      }
      const bytes = new Uint8Array(await r.arrayBuffer());
      return wasm.decode_chunks(
        bytes,
        term.range.start - info.range.start,
        term.range.end - info.range.start,
      );
    }),
  );

  const total = parts.reduce((sum, p) => sum + p.length, 0);
  const joined = new Uint8Array(total);
  let offset = 0;
  for (const p of parts) {
    joined.set(p, offset);
    offset += p.length;
  }

  // Terms are chunk-aligned; trim to the exact requested byte window.
  const from = recon.offset_into_first_range;
  const to = range ? Math.min(from + (range.end - range.start + 1), total) : total;
  return joined.buffer.slice(from, to);
}

export async function fetchFileContent(hash: string): Promise<ArrayBuffer> {
  return reconstructContent(hash);
}

/** Fetch a byte range (inclusive end) from a file's content. */
export async function fetchFileContentRange(
  hash: string,
  start: number,
  end: number,
): Promise<ArrayBuffer> {
  return reconstructContent(hash, { start, end });
}

// ─── Upload (client-side chunking via openxet-wasm) ─────────────────────────

async function loadWasm() {
  const mod = await import("./openxet-wasm/openxet_wasm");
  await mod.default();
  return mod;
}

// Let the browser paint the latest phase label before we hand the main thread
// to a blocking wasm call (chunk/hash, pack). Double rAF = one fully painted
// frame; a compositor-driven bar keeps animating through the freeze that follows.
const yieldToPaint = () =>
  new Promise<void>((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
  );

export async function uploadFile(
  file: File,
  onStatus?: (status: UploadStatus) => void,
): Promise<UploadResult> {
  const data = new Uint8Array(await file.arrayBuffer());
  const wasm = await loadWasm();

  // Chunk + hash the whole file in wasm. One synchronous main-thread call with
  // no sub-progress hook, so we can only announce the phase — yield first so
  // the label is on screen before the tab freezes for the hash.
  onStatus?.({ phase: "hashing" });
  await yieldToPaint();
  const session = new wasm.UploadSession(data);
  let plan;
  try {
    // Global dedup pass: probe chunk hashes against the server and resolve
    // already-stored chunks to their existing xorbs, so only new data uploads.
    // Probes within a batch are independent, so fire them concurrently;
    // each settled batch may create new candidates (continuations of hits).
    let queried = 0;
    const PROBE_BATCH = 16;
    for (;;) {
      const batch: string[] = session.next_query_batch(PROBE_BATCH);
      if (batch.length === 0) break;
      const headers = authHeaders();
      await Promise.all(
        batch.map(async (probe) => {
          const res = await fetch(`/v1/chunks/default-merkledb/${probe}`, {
            headers,
          });
          if (res.ok) {
            // JS is single-threaded: applies run one at a time as responses
            // arrive, so mutating the session here is safe.
            session.apply_dedup_shard(new Uint8Array(await res.arrayBuffer()));
          } else if (res.status !== 404) {
            throw new Error(`dedup query failed: HTTP ${res.status}`);
          }
          // 404 = chunk unknown to the server; it will be packed and uploaded.
        }),
      );
      queried += batch.length;
      // The loop awaits above, so this repaints between batches on its own.
      onStatus?.({ phase: "dedup", queried });
    }
    onStatus?.({ phase: "packing" });
    await yieldToPaint();
    plan = session.finish();
  } finally {
    session.free();
  }

  try {
    const xorbCount = plan.xorb_count;
    const xorbHashes: string[] = [];
    let total = 0;
    for (let i = 0; i < xorbCount; i++) {
      xorbHashes.push(plan.xorb_hash(i));
      total += plan.xorb_size(i); // cheap length read — does not clone the xorb
    }

    // Upload xorbs concurrently with a bounded pool (mirrors xet-core's
    // parallel_xorb_uploader). Serial POSTs stall on a full round trip per
    // xorb; a small pool overlaps them without flooding the server.
    let uploaded = 0;
    onStatus?.({ phase: "uploading", uploaded: 0, total });
    const CONCURRENCY = 4;
    let next = 0;
    const worker = async () => {
      while (next < xorbCount) {
        const i = next++;
        const size = plan.xorb_size(i);
        await authFetch(`/v1/xorbs/default/${xorbHashes[i]}`, {
          method: "POST",
          headers: { "Content-Type": "application/octet-stream" },
          body: new Blob([new Uint8Array(plan.xorb_data(i))]), // clone once, here
        });
        uploaded += size;
        onStatus?.({ phase: "uploading", uploaded, total });
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, xorbCount) }, worker),
    );

    onStatus?.({ phase: "registering" });
    await authFetch("/v1/shards", {
      method: "POST",
      headers: { "Content-Type": "application/octet-stream" },
      body: new Blob([new Uint8Array(plan.shard_bytes)]),
    });

    return {
      file_hash: plan.file_hash,
      file_size: file.size,
      chunk_count: plan.chunk_count,
      deduped_chunk_count: plan.deduped_chunk_count,
      xorb_hashes: xorbHashes,
    };
  } finally {
    plan.free();
  }
}
