import {
  useState,
  useEffect,
  useRef,
  useCallback,
  type Dispatch,
  type SetStateAction,
} from "react";
import { EditorView, basicSetup } from "codemirror";
import { sql } from "@codemirror/lang-sql";
import { oneDark } from "@codemirror/theme-one-dark";
import { keymap } from "@codemirror/view";
import { getDuckDB } from "@/lib/duckdb";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Play, ChevronLeft, ChevronRight, Loader2 } from "lucide-react";

type TableFormat = "csv" | "tsv" | "parquet";

const ROWS_PER_PAGE = 50;

interface QueryResult {
  columns: string[];
  rows: (string | null)[][];
  rowCount: number;
  durationMs: number;
  /** Column indices that contain HuggingFace image structs (rendered as <img>). */
  imageColumns: Set<number>;
  /** Truncated raw JSON for image cells, keyed by "row-col". */
  imageRawJson: Map<string, string>;
}

interface DuckDBQueryOptions {
  format: TableFormat;
  /** In-memory file bytes. Required for CSV/TSV; optional for parquet. */
  bytes?: Uint8Array;
  /** URL for HTTP Range-based streaming. Used for parquet when bytes is not provided. */
  contentUrl?: string;
}

function useDuckDBQuery({ format, bytes, contentUrl }: DuckDBQueryOptions) {
  const [result, setResult] = useState<QueryResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const connRef = useRef<Awaited<
    ReturnType<Awaited<ReturnType<typeof getDuckDB>>["connect"]>
  > | null>(null);
  const readyRef = useRef(false);

  useEffect(() => {
    let cancelled = false;

    (async () => {
      try {
        const db = await getDuckDB();
        const conn = await db.connect();
        connRef.current = conn;

        let viewSql: string;

        if (format === "parquet" && contentUrl && !bytes) {
          // Stream parquet via HTTP Range requests — no full download needed.
          const filename = "data.parquet";
          await db.registerFileURL(
            filename,
            contentUrl,
            4, // DuckDBDataProtocol.HTTP
            false,
          );
          viewSql = `CREATE OR REPLACE VIEW data AS SELECT * FROM read_parquet('${filename}')`;
        } else if (bytes) {
          const filename =
            format === "parquet" ? "data.parquet" : `data.${format}`;
          await db.registerFileBuffer(filename, bytes);

          if (format === "parquet") {
            viewSql = `CREATE OR REPLACE VIEW data AS SELECT * FROM read_parquet('${filename}')`;
          } else {
            const delim = format === "tsv" ? "\\t" : ",";
            viewSql = `CREATE OR REPLACE VIEW data AS SELECT * FROM read_csv('${filename}', delim='${delim}', header=true, auto_detect=true)`;
          }
        } else {
          throw new Error("Either bytes or contentUrl must be provided");
        }

        await conn.query(viewSql);
        readyRef.current = true;

        if (cancelled) return;

        await executeQuery("SELECT * FROM data LIMIT 100", conn, setResult, setError, setLoading);
      } catch (e) {
        if (!cancelled) {
          setError(e instanceof Error ? e.message : String(e));
          setLoading(false);
        }
      }
    })();

    return () => {
      cancelled = true;
      connRef.current?.close();
      connRef.current = null;
      readyRef.current = false;
    };
  }, [bytes, format, contentUrl]);

  const runQuery = useCallback(async (querySql: string) => {
    const conn = connRef.current;
    if (!conn || !readyRef.current) return;
    await executeQuery(querySql, conn, setResult, setError, setLoading);
  }, []);

  return { result, error, loading, runQuery };
}

/** Sniff MIME type from the first bytes of an image buffer. */
function detectImageMime(bytes: Uint8Array): string | null {
  if (
    bytes.length >= 8 &&
    bytes[0] === 0x89 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x4e &&
    bytes[3] === 0x47
  )
    return "image/png";
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8)
    return "image/jpeg";
  if (
    bytes.length >= 4 &&
    bytes[0] === 0x47 &&
    bytes[1] === 0x49 &&
    bytes[2] === 0x46
  )
    return "image/gif";
  if (
    bytes.length >= 12 &&
    bytes[0] === 0x52 &&
    bytes[1] === 0x49 &&
    bytes[2] === 0x46 &&
    bytes[3] === 0x46 &&
    bytes[8] === 0x57
  )
    return "image/webp";
  return null;
}

/**
 * Build a truncated JSON representation of an image struct.
 * The `bytes` array is abbreviated to keep the string short.
 */
function truncatedImageJson(val: Record<string, unknown>, maxLen = 120): string {
  // Build a shallow copy replacing bytes with a placeholder
  const clone: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(val)) {
    if (v instanceof Uint8Array) {
      const preview = Array.from(v.slice(0, 8)).join(", ");
      clone[k] = `__BYTES_PLACEHOLDER_[${preview}, ...](${v.length} bytes)`;
    } else {
      clone[k] = v;
    }
  }
  let json = JSON.stringify(clone);
  // Unwrap the placeholder quotes so it reads naturally
  json = json.replace(/"__BYTES_PLACEHOLDER_(.+?)"/g, "$1");
  if (json.length > maxLen) {
    json = json.slice(0, maxLen - 1) + "\u2026";
  }
  return json;
}

/** Convert a Uint8Array to a base64 data-URL with the given MIME type. */
function uint8ToDataUrl(bytes: Uint8Array, mime: string): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return `data:${mime};base64,${btoa(binary)}`;
}

async function executeQuery(
  querySql: string,
  conn: Awaited<ReturnType<Awaited<ReturnType<typeof getDuckDB>>["connect"]>>,
  setResult: Dispatch<SetStateAction<QueryResult | null>>,
  setError: Dispatch<SetStateAction<string | null>>,
  setLoading: Dispatch<SetStateAction<boolean>>,
) {
  setLoading(true);
  setError(null);
  const start = performance.now();
  try {
    const table = await conn.query(querySql);
    const durationMs = performance.now() - start;

    const columns = table.schema.fields.map((f) => f.name);
    const imageColumns = new Set<number>();
    const imageRawJson = new Map<string, string>();
    const rows: (string | null)[][] = [];
    for (let i = 0; i < table.numRows; i++) {
      const row = table.get(i);
      rows.push(
        columns.map((col, colIdx) => {
          const val = row?.[col];
          if (val === null || val === undefined) return null;

          // Detect HuggingFace image struct: {bytes: Uint8Array, path: string}
          if (val !== null && typeof val === "object") {
            const rec = val as Record<string, unknown>;
            const maybeBytes = rec.bytes;
            if (maybeBytes instanceof Uint8Array && maybeBytes.length > 0) {
              imageColumns.add(colIdx);
              imageRawJson.set(`${i}-${colIdx}`, truncatedImageJson(rec));
              const mime = detectImageMime(maybeBytes) ?? "image/png";
              return uint8ToDataUrl(maybeBytes, mime);
            }
          }

          return String(val);
        }),
      );
    }

    setResult({ columns, rows, rowCount: table.numRows, durationMs, imageColumns, imageRawJson });
  } catch (e) {
    setError(e instanceof Error ? e.message : String(e));
  } finally {
    setLoading(false);
  }
}

function SqlEditor({
  defaultValue,
  onRun,
  loading,
}: {
  defaultValue: string;
  onRun: (sql: string) => void;
  loading: boolean;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;

    const isDark = document.documentElement.classList.contains("dark");

    const runKeymap = keymap.of([
      {
        key: "Mod-Enter",
        run: (view) => {
          onRun(view.state.doc.toString());
          return true;
        },
      },
    ]);

    const extensions = [runKeymap, basicSetup, sql()];
    if (isDark) extensions.push(oneDark);

    const view = new EditorView({
      doc: defaultValue,
      extensions,
      parent: containerRef.current,
    });
    viewRef.current = view;

    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // Only create editor once on mount
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleRun = () => {
    if (!viewRef.current) return;
    onRun(viewRef.current.state.doc.toString());
  };

  return (
    <div className="space-y-2">
      <div
        ref={containerRef}
        className="overflow-hidden rounded-md border text-sm [&_.cm-editor]:max-h-40 [&_.cm-editor.cm-focused]:outline-none"
      />
      <div className="flex items-center justify-between">
        <p className="text-xs text-muted-foreground">
          Ctrl/Cmd + Enter to run
        </p>
        <Button size="sm" onClick={handleRun} disabled={loading}>
          {loading ? (
            <Loader2 className="mr-1.5 size-4 animate-spin" />
          ) : (
            <Play className="mr-1.5 size-4" />
          )}
          Run Query
        </Button>
      </div>
    </div>
  );
}

function QueryResults({ result }: { result: QueryResult }) {
  const [page, setPage] = useState(0);

  const totalPages = Math.ceil(result.rows.length / ROWS_PER_PAGE);
  const start = page * ROWS_PER_PAGE;
  const pageRows = result.rows.slice(start, start + ROWS_PER_PAGE);

  // Reset to first page when results change
  useEffect(() => {
    setPage(0);
  }, [result]);

  return (
    <div className="space-y-2">
      <div className="max-h-[32rem] overflow-auto rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              {result.columns.map((col) => (
                <TableHead key={col} className="font-mono text-xs">
                  {col}
                </TableHead>
              ))}
            </TableRow>
          </TableHeader>
          <TableBody>
            {pageRows.map((row, i) => (
              <TableRow key={start + i}>
                {row.map((cell, j) => (
                  <TableCell
                    key={j}
                    className={
                      result.imageColumns.has(j)
                        ? "p-1"
                        : "font-mono text-xs max-w-64 truncate"
                    }
                  >
                    {cell === null ? (
                      <span className="text-muted-foreground/50">NULL</span>
                    ) : result.imageColumns.has(j) ? (
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <img
                            src={cell}
                            alt=""
                            className="h-16 w-auto object-contain rounded"
                          />
                        </TooltipTrigger>
                        <TooltipContent
                          side="bottom"
                          className="max-w-sm font-mono text-xs break-all"
                        >
                          {result.imageRawJson.get(`${start + i}-${j}`)}
                        </TooltipContent>
                      </Tooltip>
                    ) : (
                      cell
                    )}
                  </TableCell>
                ))}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>

      <div className="flex items-center justify-between text-xs text-muted-foreground">
        <span>
          {result.rowCount} row{result.rowCount !== 1 && "s"} in{" "}
          {result.durationMs.toFixed(0)}ms
        </span>
        {totalPages > 1 && (
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={() => setPage((p) => p - 1)}
              disabled={page === 0}
            >
              <ChevronLeft className="size-4" />
              Previous
            </Button>
            <span>
              Page {page + 1} of {totalPages}
            </span>
            <Button
              variant="outline"
              size="sm"
              onClick={() => setPage((p) => p + 1)}
              disabled={page >= totalPages - 1}
            >
              Next
              <ChevronRight className="size-4" />
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}

export default function TablePreview({
  bytes,
  format,
  contentUrl,
}: {
  bytes?: Uint8Array;
  format: TableFormat;
  contentUrl?: string;
}) {
  const { result, error, loading, runQuery } = useDuckDBQuery({
    format,
    bytes,
    contentUrl,
  });

  return (
    <div className="space-y-4">
      <SqlEditor
        defaultValue="SELECT * FROM data LIMIT 100"
        onRun={runQuery}
        loading={loading}
      />

      {error && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          {error}
        </div>
      )}

      {loading && !result && (
        <div className="flex items-center gap-2 py-8 justify-center text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" />
          Loading table data...
        </div>
      )}

      {result && <QueryResults result={result} />}
    </div>
  );
}
