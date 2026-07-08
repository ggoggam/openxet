import { useMemo, useState } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  useInfiniteQuery,
  useMutation,
  useQueryClient,
} from "@tanstack/react-query";
import { Search, Trash2, Loader2, RefreshCw } from "lucide-react";
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
import { listFiles, deleteFile, type FileListItem } from "@/lib/api";
import { listCatalog } from "@/lib/catalog";
import { formatBytes, truncateHash } from "@/lib/format";

const HASH_RE = /^[0-9a-f]{64}$/i;
const PAGE_SIZE = 50;

export function FilesPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [filter, setFilter] = useState("");
  const [ownerInput, setOwnerInput] = useState("");
  const [owner, setOwner] = useState("");
  const [openHash, setOpenHash] = useState("");

  // Local catalog maps hashes uploaded from this browser to friendly names;
  // the CAS itself is content-addressed and stores no names.
  const names = useMemo(() => {
    const map = new Map<string, string>();
    for (const entry of listCatalog()) map.set(entry.hash, entry.name);
    return map;
  }, []);

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
    queryKey: ["files", owner],
    queryFn: ({ pageParam }) =>
      listFiles({ cursor: pageParam, limit: PAGE_SIZE, owner: owner || undefined }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
  });

  const remove = useMutation({
    mutationFn: (hash: string) => deleteFile(hash),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["files"] }),
  });

  const rows: FileListItem[] = useMemo(
    () => data?.pages.flatMap((p) => p.items) ?? [],
    [data],
  );

  // Client-side narrowing of already-loaded rows by hash or catalog name.
  const filtered = useMemo(() => {
    const q = filter.toLowerCase();
    if (!q) return rows;
    return rows.filter(
      (r) =>
        r.file_hash.toLowerCase().includes(q) ||
        (names.get(r.file_hash) ?? "").toLowerCase().includes(q),
    );
  }, [rows, filter, names]);

  const handleOpen = () => {
    const hash = openHash.trim().toLowerCase();
    if (HASH_RE.test(hash)) {
      navigate({ to: "/files/$hash", params: { hash } });
    }
  };

  const handleRemove = (hash: string) => {
    const name = names.get(hash) ?? truncateHash(hash, 8);
    if (
      window.confirm(
        `Release your ownership claim on "${name}"?\n\n` +
          "The file stays available while other owners still claim it, and is " +
          "removed once the last claim is released. Storage is reclaimed by the " +
          "next garbage-collection pass.",
      )
    ) {
      remove.mutate(hash);
    }
  };

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Files</h1>
        <p className="text-muted-foreground">
          Files registered on the server, newest indexed first. Names come from
          this browser's local catalog when available — the CAS itself is
          content-addressed and keeps no name index.
        </p>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <Input
          placeholder="Filter loaded rows by name or hash..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="max-w-xs"
        />
        <Input
          placeholder="Filter by owner..."
          value={ownerInput}
          onChange={(e) => setOwnerInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && setOwner(ownerInput.trim())}
          onBlur={() => setOwner(ownerInput.trim())}
          className="max-w-[12rem]"
          title="Server-side filter: list only this owner's files. Press Enter to apply."
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
        <div className="ml-auto flex items-center gap-2">
          <Input
            placeholder="Open any file by 64-hex hash..."
            value={openHash}
            onChange={(e) => setOpenHash(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && handleOpen()}
            className="w-96 font-mono text-xs"
          />
          <Button
            variant="outline"
            size="sm"
            onClick={handleOpen}
            disabled={!HASH_RE.test(openHash.trim())}
          >
            <Search className="size-4" />
            Inspect
          </Button>
        </div>
      </div>

      {error && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          Failed to load files: {error.message}
        </div>
      )}
      {remove.isError && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          Delete failed: {remove.error.message}
        </div>
      )}

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>File Hash</TableHead>
              <TableHead className="text-right">Logical Size</TableHead>
              <TableHead>Shard</TableHead>
              <TableHead className="w-12" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {isLoading ? (
              <TableRow>
                <TableCell colSpan={5} className="text-center text-muted-foreground">
                  <Loader2 className="mr-2 inline size-4 animate-spin" />
                  Loading…
                </TableCell>
              </TableRow>
            ) : filtered.length > 0 ? (
              filtered.map((entry) => (
                <TableRow key={entry.file_hash}>
                  <TableCell className="font-medium">
                    {names.get(entry.file_hash) ?? (
                      <span className="text-muted-foreground">—</span>
                    )}
                  </TableCell>
                  <TableCell>
                    <Link
                      to="/files/$hash"
                      params={{ hash: entry.file_hash }}
                      className="font-mono text-sm text-primary hover:underline"
                    >
                      {truncateHash(entry.file_hash, 12)}
                    </Link>
                  </TableCell>
                  <TableCell className="text-right">
                    {formatBytes(entry.logical_bytes)}
                  </TableCell>
                  <TableCell className="font-mono text-xs text-muted-foreground">
                    {truncateHash(entry.shard_hash, 8)}
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => handleRemove(entry.file_hash)}
                      disabled={
                        remove.isPending && remove.variables === entry.file_hash
                      }
                      title="Release your ownership claim on this file"
                    >
                      {remove.isPending &&
                      remove.variables === entry.file_hash ? (
                        <Loader2 className="size-4 animate-spin" />
                      ) : (
                        <Trash2 className="size-4 text-muted-foreground" />
                      )}
                    </Button>
                  </TableCell>
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell colSpan={5} className="text-center text-muted-foreground">
                  {filter || owner
                    ? "No files match your filter."
                    : "No files registered on the server yet."}
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
