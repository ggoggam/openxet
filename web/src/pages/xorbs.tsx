import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import { fetchXorbs } from "@/lib/api";
import { formatBytes, truncateHash } from "@/lib/format";

export function XorbsPage() {
  const [filter, setFilter] = useState("");
  const { data: xorbs, isLoading, error } = useQuery({
    queryKey: ["xorbs"],
    queryFn: fetchXorbs,
  });

  if (error) {
    return (
      <div className="text-destructive">
        Failed to load xorbs: {error.message}
      </div>
    );
  }

  const filtered = xorbs?.filter((x) =>
    x.hash.toLowerCase().includes(filter.toLowerCase())
  );

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Xorbs</h1>
        <p className="text-muted-foreground">
          Browse stored xorbs with chunk counts and sizes.
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
              <TableHead>Xorb Hash</TableHead>
              <TableHead>Chunks</TableHead>
              <TableHead className="text-right">Size on Disk</TableHead>
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
                    <Skeleton className="h-5 w-12" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="ml-auto h-5 w-20" />
                  </TableCell>
                </TableRow>
              ))
            ) : filtered && filtered.length > 0 ? (
              filtered.map((xorb) => (
                <TableRow key={xorb.hash}>
                  <TableCell className="font-mono text-sm">
                    {truncateHash(xorb.hash, 12)}
                  </TableCell>
                  <TableCell>
                    <Badge variant="secondary">{xorb.chunk_count}</Badge>
                  </TableCell>
                  <TableCell className="text-right">
                    {formatBytes(xorb.size)}
                  </TableCell>
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell
                  colSpan={3}
                  className="text-center text-muted-foreground"
                >
                  {filter
                    ? "No xorbs match your filter."
                    : "No xorbs stored yet."}
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
