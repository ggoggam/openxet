// ─── Types ───────────────────────────────────────────────────────────────────

export interface StatsResponse {
  files_count: number;
  xorbs_count: number;
  shards_count: number;
  total_size_bytes: number;
}

export interface FileEntry {
  hash: string;
  shard_hash: string;
}

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

export interface FileDetailResponse {
  hash: string;
  total_size: number;
  reconstruction: ReconstructionResponse;
}

export interface XorbEntry {
  hash: string;
  size: number;
  chunk_count: number;
}

export interface UploadResponse {
  file_hash: string;
  xorb_hashes: string[];
  shard_hash: string;
  file_size: number;
  chunk_count: number;
  xorb_count: number;
}

// ─── API Functions ───────────────────────────────────────────────────────────

const BASE = "/api";

async function fetchJson<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`);
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.json();
}

export function fetchStats(): Promise<StatsResponse> {
  return fetchJson("/stats");
}

export function fetchFiles(): Promise<FileEntry[]> {
  return fetchJson("/files");
}

export function fetchFileDetail(hash: string): Promise<FileDetailResponse> {
  return fetchJson(`/files/${hash}`);
}

export function fetchXorbs(): Promise<XorbEntry[]> {
  return fetchJson("/xorbs");
}

export async function fetchFileContent(hash: string): Promise<ArrayBuffer> {
  const res = await fetch(`${BASE}/files/${hash}/content`);
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.arrayBuffer();
}

/** Fetch a byte range from a file's content. */
export async function fetchFileContentRange(
  hash: string,
  start: number,
  end: number,
): Promise<ArrayBuffer> {
  const res = await fetch(`${BASE}/files/${hash}/content`, {
    headers: { Range: `bytes=${start}-${end}` },
  });
  if (!res.ok && res.status !== 206) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.arrayBuffer();
}

/** Returns the URL for streaming file content (supports HTTP Range requests). */
export function fileContentUrl(hash: string): string {
  return `${BASE}/files/${hash}/content`;
}

// ─── Multipart Upload ────────────────────────────────────────────────────────

const PART_SIZE = 32 * 1024 * 1024; // 32 MiB

interface InitUploadResponse {
  session_id: string;
}

interface PartUploadResponse {
  received: number;
}

async function initUpload(fileSize: number): Promise<InitUploadResponse> {
  const res = await fetch(`${BASE}/upload/init`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ file_size: fileSize }),
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.json();
}

async function uploadPart(
  sessionId: string,
  index: number,
  data: Blob,
): Promise<PartUploadResponse> {
  const res = await fetch(`${BASE}/upload/${sessionId}/${index}`, {
    method: "PUT",
    headers: { "Content-Type": "application/octet-stream" },
    body: data,
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.json();
}

async function completeUpload(sessionId: string): Promise<UploadResponse> {
  const res = await fetch(`${BASE}/upload/${sessionId}/complete`, {
    method: "POST",
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.json();
}

async function abortUpload(sessionId: string): Promise<void> {
  await fetch(`${BASE}/upload/${sessionId}`, { method: "DELETE" }).catch(
    () => {},
  );
}

export async function uploadFile(
  file: File,
  onProgress?: (uploaded: number, total: number) => void,
): Promise<UploadResponse> {
  const { session_id } = await initUpload(file.size);

  try {
    let uploaded = 0;
    let partIndex = 0;

    while (uploaded < file.size) {
      const end = Math.min(uploaded + PART_SIZE, file.size);
      const part = file.slice(uploaded, end);
      await uploadPart(session_id, partIndex, part);
      uploaded = end;
      partIndex++;
      onProgress?.(uploaded, file.size);
    }

    return await completeUpload(session_id);
  } catch (err) {
    await abortUpload(session_id);
    throw err;
  }
}
