// Shared config for talking to the dep2 read-only query API.
//   GET /relations/<name> -> { name, count, rows: string[][] }

export const DEFAULT_API: string =
  (import.meta.env.VITE_DEP2_API as string | undefined) ?? "http://127.0.0.1:7878";

export function trimBase(base: string): string {
  return base.replace(/\/+$/, "");
}
