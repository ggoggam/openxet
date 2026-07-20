// Xet wire-protocol client plus the server's management/lifecycle endpoints.
// Uploads are chunked/hashed/packed in the browser (openxet-wasm) and POSTed
// as xorbs + a shard (/v1/*); downloads fetch the /v2 reconstruction plan and
// reassemble the file client-side from xorb byte ranges (chunk decoding via
// openxet-wasm). The management endpoints (file/xorb listings, accounting, GC)
// are plain JSON over the same bearer-token auth.

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

export interface XorbRangeDescriptor {
  /** Chunk index range within the xorb (end-exclusive). */
  chunks: ChunkRange;
  /** Physical byte range for the HTTP Range header (both inclusive). */
  bytes: ByteRange;
}

export interface XorbMultiRangeFetch {
  url: string;
  ranges: XorbRangeDescriptor[];
}

export interface ReconstructionResponse {
  offset_into_first_range: number;
  terms: ReconstructionTerm[];
  xorbs: Record<string, XorbMultiRangeFetch[]>;
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

/** GET a JSON endpoint with bearer auth. */
async function authJson<T>(path: string): Promise<T> {
  const res = await authFetch(path);
  return res.json() as Promise<T>;
}

// ─── Read paths ──────────────────────────────────────────────────────────────

export async function fetchFileDetail(hash: string): Promise<FileDetail> {
  const res = await authFetch(`/v2/reconstructions/${hash}`);
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
    `/v2/reconstructions/${hash}`,
    range ? { headers: { Range: `bytes=${range.start}-${range.end}` } } : {},
  );
  const recon: ReconstructionResponse = await res.json();
  const wasm = await loadWasm();

  const parts = await Promise.all(
    recon.terms.map(async (term) => {
      // Find the range descriptor covering this term's chunks. Our server
      // emits one descriptor per fetch entry, but scan every entry's ranges
      // so a spec-general multi-range response still resolves; each
      // descriptor's byte range is independently fetchable, so one ordinary
      // 206 request per term suffices either way.
      let url: string | undefined;
      let desc: XorbRangeDescriptor | undefined;
      for (const entry of recon.xorbs[term.hash] ?? []) {
        desc = entry.ranges.find(
          (r) =>
            r.chunks.start <= term.range.start &&
            r.chunks.end >= term.range.end,
        );
        if (desc) {
          url = entry.url;
          break;
        }
      }
      if (!url || !desc) throw new Error(`no fetch info for xorb ${term.hash}`);
      // Fetch URLs are presigned (token in query) — no auth header.
      const r = await fetch(url, {
        headers: {
          Range: `bytes=${desc.bytes.start}-${desc.bytes.end}`,
        },
      });
      if (!r.ok && r.status !== 206) {
        throw new Error(`xorb fetch failed: HTTP ${r.status}`);
      }
      const bytes = new Uint8Array(await r.arrayBuffer());
      return wasm.decode_chunks(
        bytes,
        term.range.start - desc.chunks.start,
        term.range.end - desc.chunks.start,
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

// ─── Management / lifecycle (JSON) ──────────────────────────────────────────

/** One page of a cursor-paginated listing. `next_cursor` is absent on the
 * last page; pass it back as the `cursor` param to fetch the next page. */
export interface Page<T> {
  items: T[];
  next_cursor?: string;
}

export interface FileListItem {
  file_hash: string;
  shard_hash: string;
  /** Logical (pre-dedup) size, or 0 for a file with no ownership claim. */
  logical_bytes: number;
}

export interface OwnerClaim {
  owner: string;
  logical_bytes: number;
  created_at_unix: number;
}

export interface FileManagementDetail {
  file_hash: string;
  shard_hash: string;
  logical_bytes: number;
  owners: OwnerClaim[];
  /** Distinct xorbs this file's reconstruction terms reference. */
  xorbs: string[];
}

export interface XorbListItem {
  xorb_hash: string;
  /** Stored (compressed) size on disk. */
  num_bytes_on_disk: number;
  chunk_count: number;
}

export interface DeleteFileResult {
  /** Whether the file's index entry was removed (its last claim released). */
  deleted: boolean;
  /** Ownership claims still outstanding after this call. */
  remaining_owners: number;
}

export interface OwnerUsage {
  owner: string;
  file_count: number;
  logical_bytes: number;
}

export interface Accounting {
  owners: OwnerUsage[];
  files: number;
  claimed_files: number;
  /** Logical bytes counting each distinct file once. */
  unique_file_bytes: number;
  xorb_count: number;
  /** Stored (compressed, post-dedup) xorb bytes. */
  physical_xorb_bytes: number;
  shard_count: number;
  physical_shard_bytes: number;
  /** unique_file_bytes / physical_xorb_bytes. */
  dedup_ratio: number;
}

export interface GcReport {
  live_files: number;
  live_shards: number;
  live_xorbs: number;
  deleted_xorbs: number;
  freed_xorb_bytes: number;
  deleted_shards: number;
  freed_shard_bytes: number;
  /** Unreferenced objects left alone because they are within the grace period. */
  skipped_in_grace: number;
}

function pageQuery(params: {
  cursor?: string;
  limit?: number;
  owner?: string;
}): string {
  const q = new URLSearchParams();
  if (params.cursor) q.set("cursor", params.cursor);
  if (params.limit != null) q.set("limit", String(params.limit));
  if (params.owner) q.set("owner", params.owner);
  const s = q.toString();
  return s ? `?${s}` : "";
}

/** List files (cursor-paginated), optionally filtered to one owner's files. */
export function listFiles(params: {
  cursor?: string;
  limit?: number;
  owner?: string;
}): Promise<Page<FileListItem>> {
  return authJson(`/v1/files${pageQuery(params)}`);
}

/** Full server-side detail for one file: shard, size, owners, referenced xorbs. */
export function fetchFileManagementDetail(
  hash: string,
): Promise<FileManagementDetail> {
  return authJson(`/v1/files/${hash}`);
}

/** List indexed xorbs (cursor-paginated) with stored size and chunk count. */
export function listXorbs(params: {
  cursor?: string;
  limit?: number;
}): Promise<Page<XorbListItem>> {
  return authJson(`/v1/xorbs${pageQuery(params)}`);
}

/** Release the current caller's ownership claim on a file. The file is removed
 * (and its exclusive storage becomes collectable) once its last claim goes. */
export async function deleteFile(hash: string): Promise<DeleteFileResult> {
  const res = await authFetch(`/v1/files/${hash}`, { method: "DELETE" });
  return res.json();
}

/** Per-owner logical usage plus global physical storage stats. */
export function fetchAccounting(): Promise<Accounting> {
  return authJson(`/v1/accounting`);
}

/** Run one mark-and-sweep GC pass. `graceSeconds` overrides the server default
 * for this pass (0 collects everything unreferenced regardless of age). */
export async function runGc(graceSeconds?: number): Promise<GcReport> {
  const q =
    graceSeconds != null ? `?grace_seconds=${graceSeconds}` : "";
  const res = await authFetch(`/v1/gc${q}`, { method: "POST" });
  return res.json();
}

// ─── S3 gateway management (JSON) ───────────────────────────────────────────

export interface S3Info {
  /** Whether the S3 data plane (GetObject/PutObject/…) is mounted. */
  enabled: boolean;
  /** Path prefix the gateway is mounted under, e.g. `/s3`. */
  prefix: string;
  /** Endpoint URL S3 clients target (`--endpoint-url`); absolute when the
   * server has a public URL, otherwise just the prefix. */
  endpoint: string;
}

export interface S3BucketSummary {
  bucket: string;
  object_count: number;
  /** Sum of the objects' logical sizes in bytes. */
  total_size: number;
}

export interface S3ObjectItem {
  bucket: string;
  key: string;
  /** The content-addressed file this name resolves to. */
  file_hash: string;
  size: number;
  etag: string;
  owner_id: string;
  /** Registration time in unix seconds. */
  last_modified: number;
}

export interface S3CredentialSummary {
  access_key_id: string;
  owner_id: string;
  /** Mint time in unix seconds (0 for credentials minted before timestamps). */
  created_at: number;
}

export interface CreateCredentialResult {
  access_key_id: string;
  /** Shown once, at creation; not recoverable afterward. */
  secret_access_key: string;
  owner_id: string;
}

/** Gateway connection details for the management UI. */
export function fetchS3Info(): Promise<S3Info> {
  return authJson(`/v1/s3/info`);
}

/** Distinct buckets with per-bucket object counts and total logical size. */
export async function listS3Buckets(): Promise<S3BucketSummary[]> {
  const res = await authJson<{ buckets: S3BucketSummary[] }>(`/v1/s3/buckets`);
  return res.buckets;
}

/** Object names within a bucket (cursor-paginated, ordered by key). */
export function listS3Objects(params: {
  bucket: string;
  prefix?: string;
  cursor?: string;
  limit?: number;
}): Promise<Page<S3ObjectItem>> {
  const q = new URLSearchParams({ bucket: params.bucket });
  if (params.prefix) q.set("prefix", params.prefix);
  if (params.cursor) q.set("cursor", params.cursor);
  if (params.limit != null) q.set("limit", String(params.limit));
  return authJson(`/v1/s3/objects?${q.toString()}`);
}

/** Register a friendly `(bucket, key)` name for an already-uploaded file. */
export async function registerS3Object(params: {
  bucket: string;
  key: string;
  file_hash: string;
}): Promise<{ bucket: string; key: string; file_hash: string; size: number; etag: string }> {
  const res = await authFetch(`/v1/s3/objects`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(params),
  });
  return res.json();
}

/** Remove an object name. Does not release the underlying file's ownership
 * claim (matches the gateway's DeleteObject; per-name refcounting is deferred). */
export async function deleteS3Object(
  bucket: string,
  key: string,
): Promise<{ deleted: boolean }> {
  const q = new URLSearchParams({ bucket, key });
  const res = await authFetch(`/v1/s3/objects?${q.toString()}`, {
    method: "DELETE",
  });
  return res.json();
}

/** All minted credentials without their secrets, newest first. */
export async function listS3Credentials(): Promise<S3CredentialSummary[]> {
  const res = await authJson<{ items: S3CredentialSummary[] }>(
    `/v1/s3/credentials`,
  );
  return res.items;
}

/** Mint a SigV4 access-key/secret pair. The secret is returned once here. */
export async function createS3Credential(): Promise<CreateCredentialResult> {
  const res = await authFetch(`/v1/s3/credentials`, { method: "POST" });
  return res.json();
}

/** Revoke a credential by access-key id. */
export async function deleteS3Credential(
  accessKeyId: string,
): Promise<{ deleted: boolean }> {
  const res = await authFetch(
    `/v1/s3/credentials/${encodeURIComponent(accessKeyId)}`,
    { method: "DELETE" },
  );
  return res.json();
}
