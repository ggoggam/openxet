import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { fetchFiles } from "@/lib/api";
import { truncateHash } from "@/lib/format";

export function FilesPage() {
  const [filter, setFilter] = useState("");
  const { data: files, isLoading, error } = useQuery({
    queryKey: ["files"],
    queryFn: fetchFiles,
  });

  if (error) {
    return (
      <div className="text-destructive">
        Failed to load files: {error.message}
      </div>
    );
  }

  const filtered = files?.filter(
    (f) =>
      f.hash.toLowerCase().includes(filter.toLowerCase()) ||
      f.shard_hash.toLowerCase().includes(filter.toLowerCase())
  );

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Files</h1>
        <p className="text-muted-foreground">
          All files stored in the CAS server.
        </p>
      </div>

      <Input
        placeholder="Filter by hash..."
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        className="max-w-sm"
      />

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>File Hash</TableHead>
              <TableHead>Shard Hash</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {isLoading ? (
              Array.from({ length: 5 }).map((_, i) => (
                <TableRow key={i}>
                  <TableCell>
                    <Skeleton className="h-5 w-48" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-48" />
                  </TableCell>
                </TableRow>
              ))
            ) : filtered && filtered.length > 0 ? (
              filtered.map((file) => (
                <TableRow key={file.hash}>
                  <TableCell>
                    <Link
                      to="/files/$hash"
                      params={{ hash: file.hash }}
                      className="font-mono text-sm text-primary hover:underline"
                    >
                      {truncateHash(file.hash, 12)}
                    </Link>
                  </TableCell>
                  <TableCell className="font-mono text-sm text-muted-foreground">
                    {truncateHash(file.shard_hash, 12)}
                  </TableCell>
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell colSpan={2} className="text-center text-muted-foreground">
                  {filter ? "No files match your filter." : "No files stored yet."}
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
