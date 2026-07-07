// Xet wire-protocol client (/v1/*). The server exposes nothing else:
// uploads are chunked/hashed/packed in the browser (openxet-wasm) and POSTed
// as xorbs + a shard; downloads fetch the reconstruction plan and reassemble
// the file client-side from xorb byte ranges (chunk decoding via openxet-wasm).

import { mintToken, type Scope } from "./auth";

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
  xorb_hashes: string[];
}

// ─── HTTP helpers ────────────────────────────────────────────────────────────

async function authFetch(
  path: string,
  scope: Scope,
  init?: RequestInit,
): Promise<Response> {
  const token = await mintToken(scope);
  const res = await fetch(path, {
    ...init,
    headers: { ...init?.headers, Authorization: `Bearer ${token}` },
  });
  if (!res.ok && res.status !== 206) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res;
}

// ─── Read paths ──────────────────────────────────────────────────────────────

export async function fetchFileDetail(hash: string): Promise<FileDetail> {
  const res = await authFetch(`/v1/reconstructions/${hash}`, "read");
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
    "read",
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

export async function uploadFile(
  file: File,
  onProgress?: (uploaded: number, total: number) => void,
): Promise<UploadResult> {
  const data = new Uint8Array(await file.arrayBuffer());
  const wasm = await loadWasm();

  // Global dedup pass: probe chunk hashes against the server and resolve
  // already-stored chunks to their existing xorbs, so only new data uploads.
  const session = new wasm.UploadSession(data);
  let plan;
  try {
    // Probes within a batch are independent, so fire them concurrently;
    // each settled batch may create new candidates (continuations of hits).
    const PROBE_BATCH = 16;
    for (;;) {
      const batch: string[] = session.next_query_batch(PROBE_BATCH);
      if (batch.length === 0) break;
      const token = await mintToken("read");
      await Promise.all(
        batch.map(async (probe) => {
          const res = await fetch(`/v1/chunks/default-merkledb/${probe}`, {
            headers: { Authorization: `Bearer ${token}` },
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
    }
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
    onProgress?.(0, total);
    const CONCURRENCY = 4;
    let next = 0;
    const worker = async () => {
      while (next < xorbCount) {
        const i = next++;
        const size = plan.xorb_size(i);
        await authFetch(`/v1/xorbs/default/${xorbHashes[i]}`, "write", {
          method: "POST",
          headers: { "Content-Type": "application/octet-stream" },
          body: new Blob([new Uint8Array(plan.xorb_data(i))]), // clone once, here
        });
        uploaded += size;
        onProgress?.(uploaded, total);
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, xorbCount) }, worker),
    );

    await authFetch("/v1/shards", "write", {
      method: "POST",
      headers: { "Content-Type": "application/octet-stream" },
      body: new Blob([new Uint8Array(plan.shard_bytes)]),
    });

    return {
      file_hash: plan.file_hash,
      file_size: file.size,
      chunk_count: plan.chunk_count,
      xorb_hashes: xorbHashes,
    };
  } finally {
    plan.free();
  }
}
