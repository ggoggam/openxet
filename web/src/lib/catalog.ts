// Local file catalog. The CAS is content-addressed and deliberately has no
// listing endpoint — the naming layer (name → hash) lives outside it. For this
// console that layer is the browser's localStorage.

export interface CatalogEntry {
  hash: string;
  name: string;
  size: number;
  uploadedAt: string; // ISO timestamp
}

const CATALOG_KEY = "openxet.catalog";

export function listCatalog(): CatalogEntry[] {
  try {
    return JSON.parse(localStorage.getItem(CATALOG_KEY) ?? "[]");
  } catch {
    return [];
  }
}

export function addToCatalog(entry: CatalogEntry) {
  const rest = listCatalog().filter((e) => e.hash !== entry.hash);
  localStorage.setItem(CATALOG_KEY, JSON.stringify([entry, ...rest]));
}

export function removeFromCatalog(hash: string) {
  localStorage.setItem(
    CATALOG_KEY,
    JSON.stringify(listCatalog().filter((e) => e.hash !== hash)),
  );
}
