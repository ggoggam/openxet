import { useMemo, useState } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import { Loader2, RefreshCw } from "lucide-react";
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
import { listXorbs, type XorbListItem } from "@/lib/api";
import { formatBytes, truncateHash } from "@/lib/format";

const PAGE_SIZE = 50;

export function XorbsPage() {
  const [filter, setFilter] = useState("");

  const {
    data,
    isLoading,
    error,
    fetchNextPage,
    hasNextPage,
    isFetchingNextPage,
    isFetching,
    refetch,
  } = useInfiniteQuery({
    queryKey: ["xorbs"],
    queryFn: ({ pageParam }) => listXorbs({ cursor: pageParam, limit: PAGE_SIZE }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
  });

  const rows: XorbListItem[] = useMemo(
    () => data?.pages.flatMap((p) => p.items) ?? [],
    [data],
  );

  const filtered = useMemo(() => {
    const q = filter.toLowerCase();
    if (!q) return rows;
    return rows.filter((r) => r.xorb_hash.toLowerCase().includes(q));
  }, [rows, filter]);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Xorbs</h1>
        <p className="text-muted-foreground">
          Content-addressed chunk archives known to the deduplication index,
          ordered by hash. Sizes are the stored (compressed) bytes on disk.
        </p>
      </div>

      <div className="flex items-center gap-2">
        <Input
          placeholder="Filter loaded rows by hash..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="max-w-xs"
        />
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
          Failed to load xorbs: {error.message}
        </div>
      )}

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Xorb Hash</TableHead>
              <TableHead className="text-right">Chunks</TableHead>
              <TableHead className="text-right">Stored Size</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {isLoading ? (
              <TableRow>
                <TableCell colSpan={3} className="text-center text-muted-foreground">
                  <Loader2 className="mr-2 inline size-4 animate-spin" />
                  Loading…
                </TableCell>
              </TableRow>
            ) : filtered.length > 0 ? (
              filtered.map((xorb) => (
                <TableRow key={xorb.xorb_hash}>
                  <TableCell className="font-mono text-sm">
                    {truncateHash(xorb.xorb_hash, 16)}
                  </TableCell>
                  <TableCell className="text-right tabular-nums">
                    {xorb.chunk_count.toLocaleString()}
                  </TableCell>
                  <TableCell className="text-right">
                    {formatBytes(xorb.num_bytes_on_disk)}
                  </TableCell>
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell colSpan={3} className="text-center text-muted-foreground">
                  {filter ? "No xorbs match your filter." : "No xorbs stored yet."}
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>

      {hasNextPage && (
        <div className="flex justify-center">
          <Button
            variant="outline"
            size="sm"
            onClick={() => fetchNextPage()}
            disabled={isFetchingNextPage}
          >
            {isFetchingNextPage ? (
              <>
                <Loader2 className="size-4 animate-spin" />
                Loading…
              </>
            ) : (
              "Load more"
            )}
          </Button>
        </div>
      )}
    </div>
  );
}
