import React, { useMemo, useEffect, Suspense } from "react";
import { useQuery } from "@tanstack/react-query";
import { useParams, Link } from "@tanstack/react-router";
import { ArrowLeft, Download } from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
  fetchFileDetail,
  fetchFileContent,
  fetchFileContentRange,
  fileContentUrl,
} from "@/lib/api";
import { formatBytes, truncateHash } from "@/lib/format";

/** Max bytes we try to render as text in the preview. */
const TEXT_PREVIEW_LIMIT = 512 * 1024; // 512 KiB

/** Bytes to show in hex dump preview. */
const HEX_DUMP_BYTES = 256;

/** Check whether a buffer looks like valid UTF-8 text (sample first 8 KiB). */
function looksLikeText(buf: Uint8Array): boolean {
  const sample = buf.slice(0, 8192);
  for (let i = 0; i < sample.length; i++) {
    const b = sample[i];
    if (b === 0 || (b < 0x20 && b !== 9 && b !== 10 && b !== 13)) {
      return false;
    }
  }
  return true;
}

const LazyTablePreview = React.lazy(() => import("@/components/table-preview"));

type DetectedContent =
  | { kind: "image"; mime: string; label: string; bytes: Uint8Array }
  | { kind: "pdf"; bytes: Uint8Array }
  | { kind: "table"; format: "csv" | "tsv" | "parquet"; bytes: Uint8Array }
  | { kind: "text"; text: string; truncated: boolean; size: number }
  | { kind: "binary"; size: number; bytes: Uint8Array };

/** Check if text looks like CSV or TSV by analyzing column consistency. */
function detectTabular(text: string): "csv" | "tsv" | null {
  const lines = text.split("\n").filter((l) => l.trim().length > 0);
  const sample = lines.slice(0, 20);
  if (sample.length < 2) return null;

  for (const delim of ["\t", ","] as const) {
    const counts = sample.map((line) => line.split(delim).length);
    const headerCols = counts[0];
    if (headerCols < 2) continue;

    const consistent = counts.filter((c) => c === headerCols).length;
    if (consistent / counts.length >= 0.8) {
      return delim === "\t" ? "tsv" : "csv";
    }
  }
  return null;
}

function detectContentType(buf: Uint8Array): DetectedContent {
  // PNG: 89 50 4E 47 0D 0A 1A 0A
  if (
    buf.length >= 8 &&
    buf[0] === 0x89 &&
    buf[1] === 0x50 &&
    buf[2] === 0x4e &&
    buf[3] === 0x47 &&
    buf[4] === 0x0d &&
    buf[5] === 0x0a &&
    buf[6] === 0x1a &&
    buf[7] === 0x0a
  ) {
    return { kind: "image", mime: "image/png", label: "PNG Image", bytes: buf };
  }

  // JPEG: FF D8 FF
  if (
    buf.length >= 3 &&
    buf[0] === 0xff &&
    buf[1] === 0xd8 &&
    buf[2] === 0xff
  ) {
    return {
      kind: "image",
      mime: "image/jpeg",
      label: "JPEG Image",
      bytes: buf,
    };
  }

  // GIF: 47 49 46 38
  if (
    buf.length >= 4 &&
    buf[0] === 0x47 &&
    buf[1] === 0x49 &&
    buf[2] === 0x46 &&
    buf[3] === 0x38
  ) {
    return { kind: "image", mime: "image/gif", label: "GIF Image", bytes: buf };
  }

  // WebP: RIFF at 0 + WEBP at 8
  if (
    buf.length >= 12 &&
    buf[0] === 0x52 &&
    buf[1] === 0x49 &&
    buf[2] === 0x46 &&
    buf[3] === 0x46 &&
    buf[8] === 0x57 &&
    buf[9] === 0x45 &&
    buf[10] === 0x42 &&
    buf[11] === 0x50
  ) {
    return {
      kind: "image",
      mime: "image/webp",
      label: "WebP Image",
      bytes: buf,
    };
  }

  // PDF: %PDF-
  if (
    buf.length >= 5 &&
    buf[0] === 0x25 &&
    buf[1] === 0x50 &&
    buf[2] === 0x44 &&
    buf[3] === 0x46 &&
    buf[4] === 0x2d
  ) {
    return { kind: "pdf", bytes: buf };
  }

  // Parquet: magic bytes "PAR1" at start AND end
  if (
    buf.length >= 8 &&
    buf[0] === 0x50 &&
    buf[1] === 0x41 &&
    buf[2] === 0x52 &&
    buf[3] === 0x31 &&
    buf[buf.length - 4] === 0x50 &&
    buf[buf.length - 3] === 0x41 &&
    buf[buf.length - 2] === 0x52 &&
    buf[buf.length - 1] === 0x31
  ) {
    return { kind: "table", format: "parquet", bytes: buf };
  }

  // Text-based detection (SVG, CSV/TSV, plain text)
  if (looksLikeText(buf)) {
    const decoder = new TextDecoder("utf-8", { fatal: false });
    const truncated = buf.length > TEXT_PREVIEW_LIMIT;
    const text = decoder.decode(
      truncated ? buf.slice(0, TEXT_PREVIEW_LIMIT) : buf,
    );

    // SVG: check for <svg tag
    const trimmed = text.trimStart().toLowerCase();
    if (
      trimmed.startsWith("<svg") ||
      (trimmed.startsWith("<?xml") && trimmed.includes("<svg"))
    ) {
      return {
        kind: "image",
        mime: "image/svg+xml",
        label: "SVG Image",
        bytes: buf,
      };
    }

    // CSV/TSV detection
    const tabular = detectTabular(text);
    if (tabular) {
      return { kind: "table", format: tabular, bytes: buf };
    }

    return { kind: "text", text, truncated, size: buf.length };
  }

  return { kind: "binary", size: buf.length, bytes: buf };
}

function contentLabel(detected: DetectedContent): string {
  switch (detected.kind) {
    case "image":
      return detected.label;
    case "pdf":
      return "PDF Document";
    case "table":
      return detected.format === "parquet"
        ? "Parquet"
        : detected.format.toUpperCase();
    case "text":
      return "UTF-8 Text";
    case "binary":
      return "Binary";
  }
}

function ImagePreview({ bytes, mime }: { bytes: Uint8Array; mime: string }) {
  const url = useMemo(() => {
    const blob = new Blob([bytes.buffer as ArrayBuffer], { type: mime });
    return URL.createObjectURL(blob);
  }, [bytes, mime]);

  useEffect(() => {
    return () => URL.revokeObjectURL(url);
  }, [url]);

  return (
    <div className="flex justify-center rounded-md bg-muted p-4">
      <img
        src={url}
        alt="File preview"
        className="max-h-[32rem] object-contain"
      />
    </div>
  );
}

function PdfPreview({ bytes }: { bytes: Uint8Array }) {
  const url = useMemo(() => {
    const blob = new Blob([bytes.buffer as ArrayBuffer], {
      type: "application/pdf",
    });
    return URL.createObjectURL(blob);
  }, [bytes]);

  useEffect(() => {
    return () => URL.revokeObjectURL(url);
  }, [url]);

  return (
    <iframe
      src={url}
      title="PDF preview"
      className="h-[40rem] w-full rounded-md border"
    />
  );
}

function TextPreview({
  text,
  truncated,
  size,
}: {
  text: string;
  truncated: boolean;
  size: number;
}) {
  const lines = text.split("\n");

  return (
    <div>
      <div className="max-h-[32rem] overflow-auto rounded-md bg-muted text-sm leading-relaxed">
        <table className="w-full border-collapse">
          <tbody>
            {lines.map((line, i) => (
              <tr key={i} className="hover:bg-muted-foreground/5">
                <td className="select-none border-r border-border px-3 py-0 text-right align-top text-muted-foreground/50 tabular-nums">
                  {i + 1}
                </td>
                <td className="px-3 py-0">
                  <pre className="whitespace-pre-wrap break-all">
                    <code>{line}</code>
                  </pre>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {truncated && (
        <p className="mt-2 text-xs text-muted-foreground">
          Showing first {formatBytes(TEXT_PREVIEW_LIMIT)} of {formatBytes(size)}
          . Download to see the full file.
        </p>
      )}
    </div>
  );
}

function HexDumpPreview({ bytes, size }: { bytes: Uint8Array; size: number }) {
  const rows: string[] = [];
  const limit = Math.min(bytes.length, HEX_DUMP_BYTES);

  for (let offset = 0; offset < limit; offset += 16) {
    const slice = bytes.slice(offset, Math.min(offset + 16, limit));

    // Offset column
    const offsetStr = offset.toString(16).padStart(8, "0");

    // Hex column
    const hexParts: string[] = [];
    for (let j = 0; j < 16; j++) {
      if (j < slice.length) {
        hexParts.push(slice[j].toString(16).padStart(2, "0"));
      } else {
        hexParts.push("  ");
      }
    }
    const hexStr = hexParts.join(" ");

    // ASCII column
    const asciiParts: string[] = [];
    for (let j = 0; j < slice.length; j++) {
      const b = slice[j];
      asciiParts.push(b >= 0x20 && b <= 0x7e ? String.fromCharCode(b) : ".");
    }
    const asciiStr = asciiParts.join("");

    rows.push(`${offsetStr}  ${hexStr}  |${asciiStr}|`);
  }

  return (
    <div>
      <div className="max-h-[32rem] overflow-auto rounded-md bg-muted p-4">
        <pre className="text-sm leading-relaxed">
          <code>{rows.join("\n")}</code>
        </pre>
      </div>
      {size > HEX_DUMP_BYTES && (
        <p className="mt-2 text-xs text-muted-foreground">
          Showing first {HEX_DUMP_BYTES} of {formatBytes(size)} bytes. Download
          to view the full file.
        </p>
      )}
    </div>
  );
}

/** Probe first few bytes to check if a file is parquet (starts with "PAR1"). */
function isParquetProbe(buf: Uint8Array): boolean {
  return (
    buf.length >= 4 &&
    buf[0] === 0x50 &&
    buf[1] === 0x41 &&
    buf[2] === 0x52 &&
    buf[3] === 0x31
  );
}

function FileContentPreview({
  hash,
  totalSize,
}: {
  hash: string;
  totalSize: number;
}) {
  // Probe the first 16 bytes to detect parquet before downloading the full file.
  const probe = useQuery({
    queryKey: ["file-content-probe", hash],
    queryFn: () => fetchFileContentRange(hash, 0, 15),
  });

  const isParquet = useMemo(() => {
    if (!probe.data) return false;
    return isParquetProbe(new Uint8Array(probe.data));
  }, [probe.data]);

  // Only fetch full content for non-parquet files.
  const { data, isLoading, error } = useQuery({
    queryKey: ["file-content", hash],
    queryFn: () => fetchFileContent(hash),
    enabled: probe.isSuccess && !isParquet,
  });

  const detected = useMemo(() => {
    if (!data) return null;
    // Copy the buffer so downstream consumers (e.g. DuckDB registerFileBuffer)
    // that transfer/detach the ArrayBuffer don't corrupt React Query's cache.
    const bytes = new Uint8Array(data.slice(0));
    return detectContentType(bytes);
  }, [data]);

  const handleDownload = () => {
    if (!data) return;
    const blob = new Blob([data]);
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = hash;
    a.click();
    URL.revokeObjectURL(url);
  };

  if (probe.error || error) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-lg">Content</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-destructive">
            Failed to load content:{" "}
            {(probe.error || error)?.message}
          </p>
        </CardContent>
      </Card>
    );
  }

  const showLoading = probe.isLoading || (!isParquet && isLoading);

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <div>
          <CardTitle className="text-lg flex items-center gap-2">
            Content
            {isParquet && <Badge variant="secondary">Parquet</Badge>}
            {detected && !isParquet && (
              <Badge variant="secondary">{contentLabel(detected)}</Badge>
            )}
          </CardTitle>
          <CardDescription>
            {showLoading
              ? "Loading..."
              : formatBytes(
                  isParquet
                    ? totalSize
                    : detected
                      ? detected.kind === "text" || detected.kind === "binary"
                        ? detected.size
                        : detected.bytes.length
                      : 0,
                )}
          </CardDescription>
        </div>
        {data && !isParquet && (
          <Button variant="outline" size="sm" onClick={handleDownload}>
            <Download className="mr-1.5 size-4" />
            Download
          </Button>
        )}
      </CardHeader>
      <CardContent>
        {showLoading ? (
          <div className="space-y-2">
            <Skeleton className="h-4 w-full" />
            <Skeleton className="h-4 w-3/4" />
            <Skeleton className="h-4 w-5/6" />
          </div>
        ) : isParquet ? (
          <Suspense
            fallback={
              <div className="flex items-center justify-center py-8 text-sm text-muted-foreground">
                Loading table viewer...
              </div>
            }
          >
            <LazyTablePreview
              format="parquet"
              contentUrl={fileContentUrl(hash)}
            />
          </Suspense>
        ) : detected?.kind === "image" ? (
          <ImagePreview bytes={detected.bytes} mime={detected.mime} />
        ) : detected?.kind === "pdf" ? (
          <PdfPreview bytes={detected.bytes} />
        ) : detected?.kind === "table" ? (
          <Suspense
            fallback={
              <div className="flex items-center justify-center py-8 text-sm text-muted-foreground">
                Loading table viewer...
              </div>
            }
          >
            <LazyTablePreview bytes={detected.bytes} format={detected.format} />
          </Suspense>
        ) : detected?.kind === "text" ? (
          <TextPreview
            text={detected.text}
            truncated={detected.truncated}
            size={detected.size}
          />
        ) : detected?.kind === "binary" ? (
          <HexDumpPreview bytes={detected.bytes} size={detected.size} />
        ) : null}
      </CardContent>
    </Card>
  );
}

export function FileDetailPage() {
  const { hash } = useParams({ from: "/files/$hash" });
  const { data, isLoading, error } = useQuery({
    queryKey: ["file", hash],
    queryFn: () => fetchFileDetail(hash),
  });

  if (error) {
    return (
      <div className="text-destructive">
        Failed to load file: {error.message}
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-3">
        <Link
          to="/files"
          className="text-muted-foreground hover:text-foreground"
        >
          <ArrowLeft className="size-5" />
        </Link>
        <div>
          <h1 className="text-2xl font-bold tracking-tight">File Detail</h1>
          <p className="font-mono text-sm text-muted-foreground">
            {isLoading ? <Skeleton className="h-4 w-96 inline-block" /> : hash}
          </p>
        </div>
      </div>

      {isLoading ? (
        <div className="space-y-4">
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-48 w-full" />
        </div>
      ) : data ? (
        <>
          <div className="grid gap-4 sm:grid-cols-3">
            <Card>
              <CardHeader className="pb-2">
                <CardDescription>Total Size</CardDescription>
              </CardHeader>
              <CardContent>
                <CardTitle className="text-xl">
                  {formatBytes(data.total_size)}
                </CardTitle>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardDescription>Reconstruction Terms</CardDescription>
              </CardHeader>
              <CardContent>
                <CardTitle className="text-xl">
                  {data.reconstruction.terms.length}
                </CardTitle>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardDescription>Referenced Xorbs</CardDescription>
              </CardHeader>
              <CardContent>
                <CardTitle className="text-xl">
                  {Object.keys(data.reconstruction.fetch_info).length}
                </CardTitle>
              </CardContent>
            </Card>
          </div>

          <FileContentPreview hash={hash} totalSize={data.total_size} />

          <div>
            <h2 className="mb-3 text-lg font-semibold">Reconstruction Terms</h2>
            <div className="rounded-md border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Xorb Hash</TableHead>
                    <TableHead>Chunk Range</TableHead>
                    <TableHead className="text-right">Unpacked Size</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {data.reconstruction.terms.map((term, i) => (
                    <TableRow key={i}>
                      <TableCell className="font-mono text-sm">
                        {truncateHash(term.hash, 12)}
                      </TableCell>
                      <TableCell>
                        <Badge variant="secondary">
                          [{term.range.start}, {term.range.end})
                        </Badge>
                      </TableCell>
                      <TableCell className="text-right">
                        {formatBytes(term.unpacked_length)}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          </div>

          <div>
            <h2 className="mb-3 text-lg font-semibold">Fetch Info</h2>
            <div className="space-y-3">
              {Object.entries(data.reconstruction.fetch_info).map(
                ([xorbHash, infos]) => (
                  <Card key={xorbHash}>
                    <CardHeader className="pb-2">
                      <CardDescription className="font-mono text-xs">
                        {xorbHash}
                      </CardDescription>
                    </CardHeader>
                    <CardContent>
                      {infos.map((info, i) => (
                        <div
                          key={i}
                          className="flex items-center gap-4 text-sm"
                        >
                          <Badge variant="outline">
                            chunks [{info.range.start}, {info.range.end})
                          </Badge>
                          <span className="text-muted-foreground">
                            bytes {info.url_range.start}–{info.url_range.end}
                          </span>
                        </div>
                      ))}
                    </CardContent>
                  </Card>
                ),
              )}
            </div>
          </div>
        </>
      ) : null}
    </div>
  );
}
