// Thin client over the dep2 read-only query API.
//   GET /relations            -> { relations: [{ name, count }] }
//   GET /relations/<name>     -> { name, count, rows: string[][] }

export interface RelationDump {
  name: string;
  count: number;
  rows: string[][];
}

export const DEFAULT_API: string =
  (import.meta.env.VITE_DEP2_API as string | undefined) ?? "http://127.0.0.1:7878";

function trimBase(base: string): string {
  return base.replace(/\/+$/, "");
}

/** Fetch one relation's rows. Throws on HTTP error (e.g. 404 unserved). */
export async function fetchRelation(base: string, name: string): Promise<string[][]> {
  const res = await fetch(`${trimBase(base)}/relations/${name}`);
  if (!res.ok) {
    let detail = `${res.status}`;
    try {
      const body = (await res.json()) as { error?: string };
      if (body.error) detail = body.error;
    } catch {
      /* non-JSON body */
    }
    throw new Error(`${name}: ${detail}`);
  }
  const data = (await res.json()) as RelationDump;
  return data.rows ?? [];
}

/** Fetch several relations at once; rejects if any fail. */
export async function fetchRelations(
  base: string,
  names: string[],
): Promise<Record<string, string[][]>> {
  const dumps = await Promise.all(names.map((n) => fetchRelation(base, n)));
  const out: Record<string, string[][]> = {};
  names.forEach((n, i) => (out[n] = dumps[i]));
  return out;
}
