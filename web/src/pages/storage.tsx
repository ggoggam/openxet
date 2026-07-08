import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2, RefreshCw, Trash2 } from "lucide-react";
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
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  fetchAccounting,
  runGc,
  type GcReport,
} from "@/lib/api";
import { formatBytes } from "@/lib/format";

function Stat({
  label,
  value,
  sub,
}: {
  label: string;
  value: string;
  sub?: string;
}) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardDescription>{label}</CardDescription>
      </CardHeader>
      <CardContent>
        <CardTitle className="text-xl">{value}</CardTitle>
        {sub && <p className="mt-1 text-xs text-muted-foreground">{sub}</p>}
      </CardContent>
    </Card>
  );
}

export function StoragePage() {
  const queryClient = useQueryClient();
  const [grace, setGrace] = useState("");
  const [lastReport, setLastReport] = useState<GcReport | null>(null);

  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ["accounting"],
    queryFn: fetchAccounting,
  });

  const gc = useMutation({
    mutationFn: (graceSeconds?: number) => runGc(graceSeconds),
    onSuccess: (report) => {
      setLastReport(report);
      queryClient.invalidateQueries({ queryKey: ["accounting"] });
      queryClient.invalidateQueries({ queryKey: ["files"] });
      queryClient.invalidateQueries({ queryKey: ["xorbs"] });
    },
  });

  const handleGc = () => {
    const trimmed = grace.trim();
    const parsed = trimmed === "" ? undefined : Number(trimmed);
    if (parsed !== undefined && (!Number.isFinite(parsed) || parsed < 0)) return;
    if (
      window.confirm(
        parsed === 0
          ? "Run GC with grace period 0? This collects every unreferenced " +
              "object immediately, including uploads that may be in flight. " +
              "Only safe when nothing is uploading."
          : "Run a garbage-collection pass now?",
      )
    ) {
      gc.mutate(parsed);
    }
  };

  const dedup = data ? data.dedup_ratio : 0;

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Storage</h1>
          <p className="text-muted-foreground">
            Logical usage per owner versus physical bytes on disk, and
            garbage collection.
          </p>
        </div>
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={() => refetch()}
          title="Refresh"
          disabled={isFetching}
        >
          <RefreshCw className={isFetching ? "size-4 animate-spin" : "size-4"} />
        </Button>
      </div>

      {error && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          Failed to load accounting: {error.message}
        </div>
      )}

      {isLoading ? (
        <div className="flex items-center gap-2 text-muted-foreground">
          <Loader2 className="size-4 animate-spin" />
          Loading…
        </div>
      ) : data ? (
        <>
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
            <Stat
              label="Logical (unique files)"
              value={formatBytes(data.unique_file_bytes)}
              sub={`${data.claimed_files.toLocaleString()} claimed of ${data.files.toLocaleString()} files`}
            />
            <Stat
              label="Physical (xorbs)"
              value={formatBytes(data.physical_xorb_bytes)}
              sub={`${data.xorb_count.toLocaleString()} xorbs stored`}
            />
            <Stat
              label="Dedup + compression"
              value={dedup > 0 ? `${dedup.toFixed(2)}×` : "—"}
              sub="logical ÷ physical xorb bytes"
            />
            <Stat
              label="Shards"
              value={data.shard_count.toLocaleString()}
              sub={formatBytes(data.physical_shard_bytes)}
            />
          </div>

          <div>
            <h2 className="mb-3 text-lg font-semibold">Usage by owner</h2>
            <div className="rounded-md border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Owner</TableHead>
                    <TableHead className="text-right">Files</TableHead>
                    <TableHead className="text-right">Logical Size</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {data.owners.length > 0 ? (
                    data.owners.map((o) => (
                      <TableRow key={o.owner}>
                        <TableCell className="font-medium">{o.owner}</TableCell>
                        <TableCell className="text-right tabular-nums">
                          {o.file_count.toLocaleString()}
                        </TableCell>
                        <TableCell className="text-right">
                          {formatBytes(o.logical_bytes)}
                        </TableCell>
                      </TableRow>
                    ))
                  ) : (
                    <TableRow>
                      <TableCell
                        colSpan={3}
                        className="text-center text-muted-foreground"
                      >
                        No ownership claims recorded yet.
                      </TableCell>
                    </TableRow>
                  )}
                </TableBody>
              </Table>
            </div>
          </div>
        </>
      ) : null}

      <Card>
        <CardHeader>
          <CardTitle className="text-lg">Garbage collection</CardTitle>
          <CardDescription>
            Deletes xorbs and shards no live file references. Objects newer than
            the grace period are kept (they may be uploads in flight). Leave the
            grace field blank to use the server default.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex flex-wrap items-center gap-2">
            <Input
              type="number"
              min={0}
              placeholder="Grace seconds (default)"
              value={grace}
              onChange={(e) => setGrace(e.target.value)}
              className="w-56"
            />
            <Button onClick={handleGc} disabled={gc.isPending}>
              {gc.isPending ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Running…
                </>
              ) : (
                <>
                  <Trash2 className="size-4" />
                  Run GC
                </>
              )}
            </Button>
          </div>

          {gc.isError && (
            <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
              GC failed: {gc.error.message}
            </div>
          )}

          {lastReport && (
            <div className="rounded-md border border-green-500/30 bg-green-500/5 p-4 text-sm">
              <div className="mb-2 font-medium text-green-700 dark:text-green-400">
                Last pass
              </div>
              <div className="grid gap-x-6 gap-y-1 sm:grid-cols-2">
                <Row label="Live files" value={lastReport.live_files} />
                <Row label="Live xorbs" value={lastReport.live_xorbs} />
                <Row
                  label="Deleted xorbs"
                  value={`${lastReport.deleted_xorbs} (${formatBytes(lastReport.freed_xorb_bytes)})`}
                />
                <Row
                  label="Deleted shards"
                  value={`${lastReport.deleted_shards} (${formatBytes(lastReport.freed_shard_bytes)})`}
                />
                <Row label="Kept (in grace)" value={lastReport.skipped_in_grace} />
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function Row({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="flex justify-between gap-4">
      <span className="text-muted-foreground">{label}</span>
      <span className="tabular-nums">{value}</span>
    </div>
  );
}
