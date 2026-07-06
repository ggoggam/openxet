import { useState } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { Search, Trash2 } from "lucide-react";
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
  listCatalog,
  removeFromCatalog,
  type CatalogEntry,
} from "@/lib/catalog";
import { formatBytes, truncateHash } from "@/lib/format";

const HASH_RE = /^[0-9a-f]{64}$/i;

export function FilesPage() {
  const navigate = useNavigate();
  const [entries, setEntries] = useState<CatalogEntry[]>(listCatalog);
  const [filter, setFilter] = useState("");
  const [openHash, setOpenHash] = useState("");

  const handleRemove = (hash: string) => {
    removeFromCatalog(hash);
    setEntries(listCatalog());
  };

  const handleOpen = () => {
    const hash = openHash.trim().toLowerCase();
    if (HASH_RE.test(hash)) {
      navigate({ to: "/files/$hash", params: { hash } });
    }
  };

  const filtered = entries.filter(
    (e) =>
      e.hash.toLowerCase().includes(filter.toLowerCase()) ||
      e.name.toLowerCase().includes(filter.toLowerCase()),
  );

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Files</h1>
        <p className="text-muted-foreground">
          Files uploaded from this browser. The CAS itself is content-addressed
          and keeps no name index — this catalog is stored locally.
        </p>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <Input
          placeholder="Filter by name or hash..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="max-w-sm"
        />
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

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>File Hash</TableHead>
              <TableHead className="text-right">Size</TableHead>
              <TableHead>Uploaded</TableHead>
              <TableHead className="w-12" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {filtered.length > 0 ? (
              filtered.map((entry) => (
                <TableRow key={entry.hash}>
                  <TableCell className="font-medium">{entry.name}</TableCell>
                  <TableCell>
                    <Link
                      to="/files/$hash"
                      params={{ hash: entry.hash }}
                      className="font-mono text-sm text-primary hover:underline"
                    >
                      {truncateHash(entry.hash, 12)}
                    </Link>
                  </TableCell>
                  <TableCell className="text-right">
                    {formatBytes(entry.size)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {new Date(entry.uploadedAt).toLocaleString()}
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => handleRemove(entry.hash)}
                      title="Remove from local catalog (does not delete from the CAS)"
                    >
                      <Trash2 className="size-4 text-muted-foreground" />
                    </Button>
                  </TableCell>
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell
                  colSpan={5}
                  className="text-center text-muted-foreground"
                >
                  {filter
                    ? "No files match your filter."
                    : "Nothing uploaded from this browser yet. Files uploaded elsewhere can be opened by hash above."}
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
