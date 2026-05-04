import { useState, useCallback } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Upload, FileUp, CheckCircle2, AlertCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { uploadFile, type UploadResponse } from "@/lib/api";
import { formatBytes } from "@/lib/format";

export function UploadPage() {
  const queryClient = useQueryClient();
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [progress, setProgress] = useState<{
    uploaded: number;
    total: number;
  } | null>(null);
  const mutation = useMutation({
    mutationFn: (file: File) =>
      uploadFile(file, (uploaded, total) => setProgress({ uploaded, total })),
    onSuccess: () => {
      setProgress(null);
      queryClient.invalidateQueries({ queryKey: ["stats"] });
      queryClient.invalidateQueries({ queryKey: ["files"] });
      queryClient.invalidateQueries({ queryKey: ["xorbs"] });
    },
    onError: () => {
      setProgress(null);
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
    setProgress(null);
    mutation.reset();
  };

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Upload</h1>
        <p className="text-muted-foreground">
          Upload a file to the CAS server. The server will chunk, compress, and
          store it.
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
          {mutation.isPending && progress && (
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span className="text-muted-foreground">
                  Uploading... {formatBytes(progress.uploaded)} /{" "}
                  {formatBytes(progress.total)}
                </span>
                <span className="font-medium">
                  {Math.round((progress.uploaded / progress.total) * 100)}%
                </span>
              </div>
              <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full rounded-full bg-primary transition-all duration-200"
                  style={{
                    width: `${(progress.uploaded / progress.total) * 100}%`,
                  }}
                />
              </div>
              {progress.uploaded < progress.total && (
                <p className="text-xs text-muted-foreground">
                  Part {Math.ceil(progress.uploaded / (32 * 1024 * 1024))} of{" "}
                  {Math.ceil(progress.total / (32 * 1024 * 1024))}
                </p>
              )}
              {progress.uploaded >= progress.total && (
                <p className="text-xs text-muted-foreground">
                  Processing file on server...
                </p>
              )}
            </div>
          )}

          {/* Upload result */}
          {mutation.isSuccess && <UploadResult result={mutation.data} />}

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

function UploadResult({ result }: { result: UploadResponse }) {
  return (
    <div className="space-y-3 rounded-md border border-green-500/30 bg-green-500/5 p-4">
      <div className="flex items-center gap-2 text-sm font-medium text-green-700 dark:text-green-400">
        <CheckCircle2 className="size-4" />
        Upload successful
      </div>
      <div className="grid gap-2 text-sm">
        <ResultRow label="File Hash" hash={result.file_hash} link="/files" />
        <ResultRow label="Shard Hash" hash={result.shard_hash} />
        <div className="flex justify-between">
          <span className="text-muted-foreground">File Size</span>
          <span>{formatBytes(result.file_size)}</span>
        </div>
        <div className="flex justify-between">
          <span className="text-muted-foreground">Chunks</span>
          <span>{result.chunk_count}</span>
        </div>
        <div className="flex justify-between">
          <span className="text-muted-foreground">Xorbs</span>
          <span>{result.xorb_count}</span>
        </div>
        {result.xorb_hashes.map((hash, i) => (
          <ResultRow
            key={hash}
            label={result.xorb_count > 1 ? `Xorb ${i + 1}` : "Xorb Hash"}
            hash={hash}
          />
        ))}
      </div>
    </div>
  );
}

function ResultRow({
  label,
  hash,
  link,
}: {
  label: string;
  hash: string;
  link?: string;
}) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-muted-foreground">{label}</span>
      {link ? (
        <Link
          to="/files/$hash"
          params={{ hash }}
          className="font-mono text-xs text-primary hover:underline"
        >
          {hash}
        </Link>
      ) : (
        <span className="font-mono text-xs">{hash}</span>
      )}
    </div>
  );
}
