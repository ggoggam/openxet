// Xet wire-protocol client (/v1/*). The server exposes nothing else:
// uploads are chunked/hashed/packed in the browser (openxet-wasm) and POSTed
// as xorbs + a shard; downloads go through reconstruction/content endpoints.

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

export async function fetchFileContent(hash: string): Promise<ArrayBuffer> {
  const res = await authFetch(`/v1/content/${hash}`, "read");
  return res.arrayBuffer();
}

/** Fetch a byte range from a file's content. */
export async function fetchFileContentRange(
  hash: string,
  start: number,
  end: number,
): Promise<ArrayBuffer> {
  const res = await authFetch(`/v1/content/${hash}`, "read", {
    headers: { Range: `bytes=${start}-${end}` },
  });
  return res.arrayBuffer();
}

/**
 * URL for streaming file content (supports HTTP Range requests). Carries the
 * token as a query parameter so consumers that can't set headers (download
 * links, DuckDB-WASM range reads) can fetch it directly.
 */
export async function fileContentUrl(hash: string): Promise<string> {
  const token = await mintToken("read");
  return `/v1/content/${hash}?token=${token}`;
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
  const plan = wasm.plan_upload(data);

  try {
    const xorbCount = plan.xorb_count;
    const xorbHashes: string[] = [];
    const xorbSizes: number[] = [];
    let total = 0;
    for (let i = 0; i < xorbCount; i++) {
      xorbHashes.push(plan.xorb_hash(i));
      const size = plan.xorb_data(i).length;
      xorbSizes.push(size);
      total += size;
    }

    let uploaded = 0;
    onProgress?.(0, total);
    for (let i = 0; i < xorbCount; i++) {
      await authFetch(`/v1/xorbs/default/${xorbHashes[i]}`, "write", {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: new Blob([new Uint8Array(plan.xorb_data(i))]),
      });
      uploaded += xorbSizes[i];
      onProgress?.(uploaded, total);
    }

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
