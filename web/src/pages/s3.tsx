import { useMemo, useState } from "react";
import { Link } from "@tanstack/react-router";
import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Cloud,
  Copy,
  File as FileIcon,
  Folder,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-react";
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
import { Input } from "@/components/ui/input";
import { TreeView } from "@/components/tree-view";
import type { TreeNodeNested } from "@/lib/tree-types";
import {
  createS3Credential,
  deleteS3Credential,
  deleteS3Object,
  fetchS3Info,
  listS3Buckets,
  listS3Credentials,
  listS3Objects,
  registerS3Object,
  type CreateCredentialResult,
  type S3ObjectItem,
} from "@/lib/api";
import { formatBytes, formatUnixTime, truncateHash } from "@/lib/format";

const HASH_RE = /^[0-9a-f]{64}$/i;
const PAGE_SIZE = 100;

export function S3Page() {
  return (
    <div className="space-y-8">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">S3 Gateway</h1>
        <p className="text-muted-foreground">
          Browse and manage the S3-compatible name layer over the CAS: object
          names mapped onto uploaded files, and the SigV4 credentials that sign
          gateway requests.
        </p>
      </div>

      <ConnectionCard />
      <ObjectsSection />
      <CredentialsSection />
    </div>
  );
}

// ─── Connection info ─────────────────────────────────────────────────────────

function ConnectionCard() {
  const { data, error } = useQuery({ queryKey: ["s3-info"], queryFn: fetchS3Info });
  const endpoint =
    data && data.endpoint.startsWith("/")
      ? `${window.location.origin}${data.endpoint}`
      : data?.endpoint;

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-2">
          <Cloud className="size-5" />
          <CardTitle className="text-lg">Connection</CardTitle>
          {data &&
            (data.enabled ? (
              <Badge variant="secondary">Enabled</Badge>
            ) : (
              <Badge variant="outline">Disabled</Badge>
            ))}
        </div>
        <CardDescription>
          Point S3 tooling at this endpoint with path-style addressing, e.g.{" "}
          <code className="text-xs">
            aws s3 --endpoint-url {endpoint ?? "…"} ls
          </code>
          .
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {error && (
          <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
            Failed to load gateway info: {error.message}
          </div>
        )}
        {endpoint && (
          <div className="flex items-center gap-2">
            <code className="flex-1 truncate rounded-md border bg-muted/40 px-3 py-2 font-mono text-sm">
              {endpoint}
            </code>
            <CopyButton value={endpoint} />
          </div>
        )}
        {data && !data.enabled && (
          <p className="text-sm text-muted-foreground">
            The S3 data plane is off, so registered names aren't reachable over
            the S3 protocol yet. Management here still works. Set{" "}
            <code className="text-xs">OPENXET_S3_GATEWAY_ENABLED=true</code> to
            serve them.
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function CopyButton({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="outline"
      size="icon-sm"
      title="Copy"
      onClick={() => {
        navigator.clipboard.writeText(value).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        });
      }}
    >
      {copied ? (
        <Check className="size-4 text-green-600" />
      ) : (
        <Copy className="size-4" />
      )}
    </Button>
  );
}

// ─── Buckets + objects ───────────────────────────────────────────────────────

/** Data carried by each tree node: a folder segment, or a leaf object. */
type NodeData = { name: string; object?: S3ObjectItem };

/** Fold `/`-delimited object keys into a nested folder tree. */
function buildTree(objects: S3ObjectItem[]): TreeNodeNested<NodeData>[] {
  const roots: TreeNodeNested<NodeData>[] = [];
  const folders = new Map<string, TreeNodeNested<NodeData>>();

  for (const obj of objects) {
    const parts = obj.key.split("/");
    let children = roots;
    let prefix = "";
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      prefix = prefix ? `${prefix}/${part}` : part;
      if (i === parts.length - 1) {
        children.push({ id: obj.key, data: { name: part || obj.key, object: obj } });
      } else {
        let folder = folders.get(prefix);
        if (!folder) {
          folder = { id: `dir:${prefix}`, data: { name: part }, isGroup: true, children: [] };
          folders.set(prefix, folder);
          children.push(folder);
        }
        children = folder.children!;
      }
    }
  }
  return roots;
}

function ObjectsSection() {
  const queryClient = useQueryClient();
  const [bucket, setBucket] = useState<string>("");
  const [prefix, setPrefix] = useState("");
  const [appliedPrefix, setAppliedPrefix] = useState("");

  const buckets = useQuery({ queryKey: ["s3-buckets"], queryFn: listS3Buckets });

  // Default to the first bucket once loaded.
  const selected =
    bucket || (buckets.data && buckets.data.length > 0 ? buckets.data[0].bucket : "");

  const objects = useInfiniteQuery({
    queryKey: ["s3-objects", selected, appliedPrefix],
    enabled: !!selected,
    queryFn: ({ pageParam }) =>
      listS3Objects({
        bucket: selected,
        prefix: appliedPrefix || undefined,
        cursor: pageParam,
        limit: PAGE_SIZE,
      }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
  });

  const rows: S3ObjectItem[] = useMemo(
    () => objects.data?.pages.flatMap((p) => p.items) ?? [],
    [objects.data],
  );
  const tree = useMemo(() => buildTree(rows), [rows]);

  const remove = useMutation({
    mutationFn: (obj: S3ObjectItem) => deleteS3Object(obj.bucket, obj.key),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["s3-objects"] });
      queryClient.invalidateQueries({ queryKey: ["s3-buckets"] });
    },
  });

  const handleRemove = (obj: S3ObjectItem) => {
    if (
      window.confirm(
        `Delete the name "${obj.bucket}/${obj.key}"?\n\n` +
          "This removes the S3 name only. The underlying file and your " +
          "ownership claim on it are left intact (the same content may be " +
          "named under other keys).",
      )
    ) {
      remove.mutate(obj);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Objects</h2>
        <Button
          variant="ghost"
          size="icon-sm"
          title="Refresh"
          disabled={buckets.isFetching || objects.isFetching}
          onClick={() => {
            buckets.refetch();
            objects.refetch();
          }}
        >
          <RefreshCw
            className={
              buckets.isFetching || objects.isFetching
                ? "size-4 animate-spin"
                : "size-4"
            }
          />
        </Button>
      </div>

      <RegisterForm bucketHint={selected} />

      {buckets.error && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          Failed to load buckets: {buckets.error.message}
        </div>
      )}

      {buckets.data && buckets.data.length === 0 ? (
        <div className="rounded-md border p-6 text-center text-sm text-muted-foreground">
          No object names registered yet. Use the form above, or "Expose as S3
          object" on a file's detail page.
        </div>
      ) : (
        <div className="grid gap-4 md:grid-cols-[16rem_1fr]">
          {/* Bucket list */}
          <div className="rounded-md border">
            <div className="border-b px-3 py-2 text-xs font-medium text-muted-foreground">
              Buckets
            </div>
            <ul className="max-h-96 overflow-auto p-1">
              {buckets.data?.map((b) => (
                <li key={b.bucket}>
                  <button
                    onClick={() => setBucket(b.bucket)}
                    className={
                      "flex w-full items-center justify-between gap-2 rounded-md px-2 py-1.5 text-left text-sm " +
                      (b.bucket === selected
                        ? "bg-accent text-accent-foreground"
                        : "hover:bg-accent/50")
                    }
                  >
                    <span className="flex min-w-0 items-center gap-1.5">
                      <Folder className="size-4 shrink-0 text-muted-foreground" />
                      <span className="truncate font-medium">{b.bucket}</span>
                    </span>
                    <span className="shrink-0 text-xs text-muted-foreground">
                      {b.object_count} · {formatBytes(b.total_size)}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>

          {/* Object tree */}
          <div className="rounded-md border">
            <div className="flex items-center gap-2 border-b px-3 py-2">
              <Input
                placeholder="Filter by key prefix…"
                value={prefix}
                onChange={(e) => setPrefix(e.target.value)}
                onKeyDown={(e) =>
                  e.key === "Enter" && setAppliedPrefix(prefix.trim())
                }
                onBlur={() => setAppliedPrefix(prefix.trim())}
                className="h-8 max-w-xs font-mono text-xs"
                title="Server-side prefix filter. Press Enter to apply."
              />
              {remove.isError && (
                <span className="text-xs text-destructive">
                  Delete failed: {remove.error.message}
                </span>
              )}
            </div>

            <div className="p-2">
              {objects.isLoading ? (
                <div className="flex items-center gap-2 p-4 text-sm text-muted-foreground">
                  <Loader2 className="size-4 animate-spin" />
                  Loading…
                </div>
              ) : rows.length === 0 ? (
                <div className="p-4 text-center text-sm text-muted-foreground">
                  No objects{appliedPrefix ? " match this prefix" : ""} in{" "}
                  <span className="font-mono">{selected}</span>.
                </div>
              ) : (
                <TreeView<NodeData>
                  key={selected}
                  items={tree}
                  selectionMode="none"
                  defaultExpandAll
                  indentationWidth={16}
                  renderNode={({ node, isExpanded, hasChildren, depth, toggle }) => {
                    const obj = node.data.object;
                    return (
                      <div
                        className="flex items-center gap-2 rounded-md py-1 pr-2 text-sm hover:bg-accent/40"
                        style={{ paddingLeft: depth * 16 + 4 }}
                      >
                        {hasChildren || node.isGroup ? (
                          <button
                            onClick={toggle}
                            className="flex items-center gap-1 text-muted-foreground"
                          >
                            {isExpanded ? (
                              <ChevronDown className="size-4" />
                            ) : (
                              <ChevronRight className="size-4" />
                            )}
                            <Folder className="size-4" />
                          </button>
                        ) : (
                          <FileIcon className="ml-5 size-4 shrink-0 text-muted-foreground" />
                        )}

                        <span className="min-w-0 flex-1 truncate font-medium">
                          {node.data.name}
                        </span>

                        {obj && (
                          <>
                            <Link
                              to="/files/$hash"
                              params={{ hash: obj.file_hash }}
                              className="font-mono text-xs text-primary hover:underline"
                              title={obj.file_hash}
                            >
                              {truncateHash(obj.file_hash, 8)}
                            </Link>
                            <span className="w-20 text-right tabular-nums text-muted-foreground">
                              {formatBytes(obj.size)}
                            </span>
                            <span className="hidden w-40 truncate text-right text-xs text-muted-foreground lg:inline">
                              {formatUnixTime(obj.last_modified)}
                            </span>
                            <Button
                              variant="ghost"
                              size="icon-xs"
                              title="Delete this object name"
                              disabled={
                                remove.isPending && remove.variables?.key === obj.key
                              }
                              onClick={() => handleRemove(obj)}
                            >
                              {remove.isPending &&
                              remove.variables?.key === obj.key ? (
                                <Loader2 className="size-3 animate-spin" />
                              ) : (
                                <Trash2 className="size-3 text-muted-foreground" />
                              )}
                            </Button>
                          </>
                        )}
                      </div>
                    );
                  }}
                />
              )}

              {objects.hasNextPage && (
                <div className="flex justify-center pt-2">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => objects.fetchNextPage()}
                    disabled={objects.isFetchingNextPage}
                  >
                    {objects.isFetchingNextPage ? (
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
          </div>
        </div>
      )}
    </div>
  );
}

function RegisterForm({ bucketHint }: { bucketHint: string }) {
  const queryClient = useQueryClient();
  const [bucket, setBucket] = useState("");
  const [key, setKey] = useState("");
  const [hash, setHash] = useState("");

  const register = useMutation({
    mutationFn: () =>
      registerS3Object({ bucket: bucket.trim(), key: key.trim(), file_hash: hash.trim() }),
    onSuccess: () => {
      setKey("");
      setHash("");
      queryClient.invalidateQueries({ queryKey: ["s3-buckets"] });
      queryClient.invalidateQueries({ queryKey: ["s3-objects"] });
    },
  });

  const effectiveBucket = bucket.trim() || bucketHint;
  const valid =
    effectiveBucket.length > 0 && key.trim().length > 0 && HASH_RE.test(hash.trim());

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="text-base">Register a name</CardTitle>
        <CardDescription>
          Map a <code className="text-xs">(bucket, key)</code> name onto an
          already-uploaded file by its 64-hex file hash.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex flex-wrap items-center gap-2">
          <Input
            placeholder={bucketHint ? `Bucket (default: ${bucketHint})` : "Bucket"}
            value={bucket}
            onChange={(e) => setBucket(e.target.value)}
            className="w-40"
          />
          <Input
            placeholder="Key (e.g. dir/file.bin)"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            className="w-64 font-mono text-sm"
          />
          <Input
            placeholder="File hash (64 hex)"
            value={hash}
            onChange={(e) => setHash(e.target.value)}
            className="w-96 font-mono text-xs"
          />
          <Button
            onClick={() => {
              // Fall back to the selected bucket when the field is blank.
              if (!bucket.trim()) setBucket(effectiveBucket);
              register.mutate();
            }}
            disabled={!valid || register.isPending}
          >
            {register.isPending ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <Plus className="size-4" />
            )}
            Register
          </Button>
        </div>
        {register.isError && (
          <p className="text-sm text-destructive">
            Register failed: {register.error.message}
          </p>
        )}
        {register.isSuccess && (
          <p className="text-sm text-green-700 dark:text-green-400">
            Registered {register.data.bucket}/{register.data.key}.
          </p>
        )}
      </CardContent>
    </Card>
  );
}

// ─── Credentials ─────────────────────────────────────────────────────────────

function CredentialsSection() {
  const queryClient = useQueryClient();
  const [minted, setMinted] = useState<CreateCredentialResult | null>(null);

  const creds = useQuery({
    queryKey: ["s3-credentials"],
    queryFn: listS3Credentials,
  });

  const mint = useMutation({
    mutationFn: createS3Credential,
    onSuccess: (result) => {
      setMinted(result);
      queryClient.invalidateQueries({ queryKey: ["s3-credentials"] });
    },
  });

  const revoke = useMutation({
    mutationFn: (accessKeyId: string) => deleteS3Credential(accessKeyId),
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["s3-credentials"] }),
  });

  const handleRevoke = (accessKeyId: string) => {
    if (
      window.confirm(
        `Revoke credential ${accessKeyId}?\n\n` +
          "Requests signed with this key will stop being accepted immediately.",
      )
    ) {
      if (minted?.access_key_id === accessKeyId) setMinted(null);
      revoke.mutate(accessKeyId);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <KeyRound className="size-5" />
          <h2 className="text-lg font-semibold">Credentials</h2>
        </div>
        <Button onClick={() => mint.mutate()} disabled={mint.isPending} size="sm">
          {mint.isPending ? (
            <Loader2 className="size-4 animate-spin" />
          ) : (
            <Plus className="size-4" />
          )}
          Mint credential
        </Button>
      </div>

      {mint.isError && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          Mint failed: {mint.error.message}
        </div>
      )}

      {minted && (
        <Card className="border-amber-500/40 bg-amber-500/5">
          <CardHeader className="pb-2">
            <CardTitle className="text-base text-amber-700 dark:text-amber-400">
              New credential — copy the secret now
            </CardTitle>
            <CardDescription>
              The secret is shown only once and cannot be recovered later.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            <SecretRow label="Access key ID" value={minted.access_key_id} />
            <SecretRow label="Secret access key" value={minted.secret_access_key} />
          </CardContent>
        </Card>
      )}

      {creds.error && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          Failed to load credentials: {creds.error.message}
        </div>
      )}

      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Access Key ID</TableHead>
              <TableHead>Owner</TableHead>
              <TableHead>Created</TableHead>
              <TableHead className="w-12" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {creds.isLoading ? (
              <TableRow>
                <TableCell colSpan={4} className="text-center text-muted-foreground">
                  <Loader2 className="mr-2 inline size-4 animate-spin" />
                  Loading…
                </TableCell>
              </TableRow>
            ) : creds.data && creds.data.length > 0 ? (
              creds.data.map((c) => (
                <TableRow key={c.access_key_id}>
                  <TableCell className="font-mono text-sm">
                    {c.access_key_id}
                  </TableCell>
                  <TableCell>{c.owner_id}</TableCell>
                  <TableCell className="text-muted-foreground">
                    {c.created_at > 0 ? formatUnixTime(c.created_at) : "—"}
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      title="Revoke this credential"
                      disabled={
                        revoke.isPending && revoke.variables === c.access_key_id
                      }
                      onClick={() => handleRevoke(c.access_key_id)}
                    >
                      {revoke.isPending && revoke.variables === c.access_key_id ? (
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
                <TableCell colSpan={4} className="text-center text-muted-foreground">
                  No credentials minted yet.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}

function SecretRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="w-36 shrink-0 text-sm text-muted-foreground">{label}</span>
      <code className="flex-1 truncate rounded-md border bg-background px-3 py-1.5 font-mono text-sm">
        {value}
      </code>
      <CopyButton value={value} />
    </div>
  );
}
