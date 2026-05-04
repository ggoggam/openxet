import * as duckdb from "@duckdb/duckdb-wasm";
import duckdbWasm from "@duckdb/duckdb-wasm/dist/duckdb-eh.wasm?url";
import duckdbWorker from "@duckdb/duckdb-wasm/dist/duckdb-browser-eh.worker.js?url";

let dbPromise: Promise<duckdb.AsyncDuckDB> | null = null;

export function getDuckDB(): Promise<duckdb.AsyncDuckDB> {
  if (!dbPromise) {
    dbPromise = (async () => {
      const worker = new Worker(duckdbWorker);
      const logger = new duckdb.ConsoleLogger();
      const db = new duckdb.AsyncDuckDB(logger, worker);
      await db.instantiate(duckdbWasm);
      return db;
    })();
  }
  return dbPromise;
}
