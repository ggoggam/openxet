import { useState, useCallback } from "react";
import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import {
  Upload,
  FileUp,
  CheckCircle2,
  AlertCircle,
  KeyRound,
  Loader2,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { uploadFile, type UploadResult, type UploadStatus } from "@/lib/api";
import { useToken } from "@/lib/auth";
import { addToCatalog } from "@/lib/catalog";
import { formatBytes } from "@/lib/format";

export function UploadPage() {
  const token = useToken();
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [status, setStatus] = useState<UploadStatus | null>(null);
  const mutation = useMutation({
    mutationFn: (file: File) => uploadFile(file, setStatus),
    onSuccess: (result, file) => {
      setStatus(null);
      addToCatalog({
        hash: result.file_hash,
        name: file.name,
        size: file.size,
        uploadedAt: new Date().toISOString(),
      });
    },
    onError: () => {
      setStatus(null);
    },
  });

  const handleFile = useCallback((file: File) => {
    setSelectedFile(file);
  }, []);

  const handleDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      setDragOver(false);
      const file = e.dataTransfer.files[0];
      if (file) handleFile(file);
    },
    [handleFile],
  );

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    setDragOver(true);
  }, []);

  const handleDragLeave = useCallback(() => {
    setDragOver(false);
  }, []);

  const handleSubmit = () => {
    if (selectedFile) {
      mutation.mutate(selectedFile);
    }
  };

  const handleReset = () => {
    setSelectedFile(null);
    setStatus(null);
    mutation.reset();
  };

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Upload</h1>
        <p className="text-muted-foreground">
          Files are chunked, hashed, and packed into xorbs in your browser
          (WebAssembly), then uploaded over the Xet wire protocol — the same
          flow HuggingFace's xet client uses.
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>File Upload</CardTitle>
          <CardDescription>
            Drag and drop a file or click to select one.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {!token && (
            <div className="flex items-center gap-2 rounded-md border border-amber-500/50 bg-amber-500/10 p-3 text-sm text-amber-700 dark:text-amber-400">
              <KeyRound className="size-4 shrink-0" />
              No bearer token set — uploads work against a dev server (auth
              disabled). For an auth-enabled server, paste an access token from
              your OIDC provider into the key field in the header.
            </div>
          )}

          {/* Drop zone */}
          <div
            onDrop={handleDrop}
            onDragOver={handleDragOver}
            onDragLeave={handleDragLeave}
            onClick={() => document.getElementById("file-input")?.click()}
            className={`flex cursor-pointer flex-col items-center gap-3 rounded-lg border-2 border-dashed p-12 transition-colors ${
              dragOver
                ? "border-primary bg-primary/5"
                : "border-muted-foreground/25 hover:border-muted-foreground/50"
            }`}
          >
            <FileUp className="size-10 text-muted-foreground" />
            <div className="text-center">
              <p className="text-sm font-medium">
                Drop a file here or click to browse
              </p>
              <p className="text-xs text-muted-foreground">
                Any file type supported
              </p>
            </div>
          </div>

          <input
            id="file-input"
            type="file"
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) handleFile(file);
            }}
          />

          {/* Selected file info */}
          {selectedFile && (
            <div className="flex items-center justify-between rounded-md border p-3">
              <div>
                <p className="text-sm font-medium">{selectedFile.name}</p>
                <p className="text-xs text-muted-foreground">
                  {formatBytes(selectedFile.size)}
                </p>
              </div>
              <div className="flex gap-2">
                <Button variant="outline" size="sm" onClick={handleReset}>
                  Clear
                </Button>
                <Button
                  size="sm"
                  onClick={handleSubmit}
                  disabled={mutation.isPending}
                >
                  {mutation.isPending ? (
                    "Uploading..."
                  ) : (
                    <>
                      <Upload className="size-4" />
                      Upload
                    </>
                  )}
                </Button>
              </div>
            </div>
          )}

          {/* Upload progress */}
          {mutation.isPending && <UploadProgress status={status} />}

          {/* Upload result */}
          {mutation.isSuccess && <UploadSuccess result={mutation.data} />}

          {/* Upload error */}
          {mutation.isError && (
            <div className="flex items-center gap-2 rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
              <AlertCircle className="size-4 shrink-0" />
              Upload failed: {mutation.error.message}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

// The pipeline steps, in order, with the label shown while each is active.
const PHASE_STEPS = [
  { key: "hashing", label: "Chunking & hashing in your browser" },
  { key: "dedup", label: "Checking which chunks the server already has" },
  { key: "packing", label: "Packing new chunks into xorbs" },
  { key: "uploading", label: "Uploading xorbs" },
  { key: "registering", label: "Registering file" },
] as const;

function UploadProgress({ status }: { status: UploadStatus | null }) {
  // Before the first status arrives we're still reading the file / loading wasm.
  const activeIndex = status
    ? PHASE_STEPS.findIndex((s) => s.key === status.phase)
    : -1;

  const detail =
    status?.phase === "dedup"
      ? `${status.queried} probed`
      : status?.phase === "uploading"
        ? `${formatBytes(status.uploaded)} / ${formatBytes(status.total)}`
        : null;

  const pct =
    status?.phase === "uploading" && status.total > 0
      ? Math.round((status.uploaded / status.total) * 100)
      : null;

  return (
    <div className="space-y-3">
      <div className="flex justify-between text-sm">
        <span className="text-muted-foreground">
          {status ? PHASE_STEPS[activeIndex].label : "Preparing"}
          {detail ? <> — {detail}</> : "…"}
        </span>
        {pct !== null && <span className="font-medium">{pct}%</span>}
      </div>

      <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
        {pct !== null ? (
          <div
            className="h-full rounded-full bg-primary transition-all duration-200"
            style={{ width: `${pct}%` }}
          />
        ) : (
          // Indeterminate: transform-based, so it keeps sliding on the compositor
          // even while a blocking wasm call has frozen the main thread.
          <div
            className="h-full w-1/3 rounded-full bg-primary"
            style={{ animation: "indeterminate-slide 1.1s ease-in-out infinite" }}
          />
        )}
      </div>

      {/* Step checklist: done steps checked, current one highlighted. */}
      <ol className="space-y-1 text-xs">
        {PHASE_STEPS.map((step, i) => {
          const done = activeIndex > i;
          const current = activeIndex === i;
          return (
            <li
              key={step.key}
              className={`flex items-center gap-2 ${
                current
                  ? "font-medium text-foreground"
                  : done
                    ? "text-muted-foreground"
                    : "text-muted-foreground/50"
              }`}
            >
              {done ? (
                <CheckCircle2 className="size-3.5 text-green-600 dark:text-green-500" />
              ) : current ? (
                // animate-spin is transform-based, so it keeps spinning on the
                // compositor even while a blocking wasm step freezes the main thread.
                <Loader2 className="size-3.5 animate-spin text-primary" />
              ) : (
                <span className="size-3.5 rounded-full border border-muted-foreground/30" />
              )}
              {step.label}
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function UploadSuccess({ result }: { result: UploadResult }) {
  return (
    <div className="space-y-3 rounded-md border border-green-500/30 bg-green-500/5 p-4">
      <div className="flex items-center gap-2 text-sm font-medium text-green-700 dark:text-green-400">
        <CheckCircle2 className="size-4" />
        Upload successful
      </div>
      <div className="grid gap-2 text-sm">
        <div className="flex justify-between gap-2">
          <span className="text-muted-foreground">File Hash</span>
          <Link
            to="/files/$hash"
            params={{ hash: result.file_hash }}
            className="font-mono text-xs text-primary hover:underline"
          >
            {result.file_hash}
          </Link>
        </div>
        <div className="flex justify-between">
          <span className="text-muted-foreground">File Size</span>
          <span>{formatBytes(result.file_size)}</span>
        </div>
        <div className="flex justify-between">
          <span className="text-muted-foreground">Chunks</span>
          <span>{result.chunk_count}</span>
        </div>
        <div className="flex justify-between">
          <span className="text-muted-foreground">Uploaded (new)</span>
          <span>{result.chunk_count - result.deduped_chunk_count}</span>
        </div>
        {result.deduped_chunk_count > 0 && (
          <div className="flex justify-between">
            <span className="text-muted-foreground">
              Deduplicated (already stored)
            </span>
            <span>
              {result.deduped_chunk_count} (
              {Math.round(
                (result.deduped_chunk_count / result.chunk_count) * 100,
              )}
              %)
            </span>
          </div>
        )}
        <div className="flex justify-between">
          <span className="text-muted-foreground">Xorbs</span>
          <span>{result.xorb_hashes.length}</span>
        </div>
        {result.xorb_hashes.map((hash, i) => (
          <div key={hash} className="flex justify-between gap-2">
            <span className="text-muted-foreground">
              {result.xorb_hashes.length > 1 ? `Xorb ${i + 1}` : "Xorb Hash"}
            </span>
            <span className="font-mono text-xs">{hash}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
