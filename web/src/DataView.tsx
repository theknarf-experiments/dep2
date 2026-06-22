// Raw-data view: browse the engine's relations as tables. Built on TanStack
// Table (sorting + global filter); rows poll live via useRawData.

import { useEffect, useMemo, useState } from "react";
import {
  ColumnDef,
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getSortedRowModel,
  SortingState,
  useReactTable,
} from "@tanstack/react-table";
import { RELATION_COLUMNS } from "./db";
import { useRelationList, useRelationRows } from "./useRawData";
import { ViewSwitch, View } from "./ViewSwitch";
import s from "./DataView.module.css";

type Row = string[];

interface Props {
  view: View;
  setView: (v: View) => void;
  paused: boolean;
  togglePause: () => void;
  status: "connecting" | "live" | "paused";
}

export function DataView({ view, setView, paused, togglePause, status }: Props) {
  const relations = useRelationList();
  const [selected, setSelected] = useState<string | null>(null);

  // Default to the first relation (or keep the selection if it still exists).
  useEffect(() => {
    if (relations.length === 0) return;
    if (!selected || !relations.some((r) => r.name === selected)) {
      setSelected(relations[0].name);
    }
  }, [relations, selected]);

  const rows = useRelationRows(selected);
  const [sorting, setSorting] = useState<SortingState>([]);
  const [filter, setFilter] = useState("");

  // Column count = widest row; header names come from the known-relation map,
  // else positional (c0, c1, …).
  const width = useMemo(() => rows.reduce((m, r) => Math.max(m, r.length), 0), [rows]);
  const columns = useMemo<ColumnDef<Row>[]>(() => {
    const names = selected ? RELATION_COLUMNS[selected] : undefined;
    return Array.from({ length: width }, (_, i) => ({
      id: String(i),
      header: names?.[i] ?? `c${i}`,
      accessorFn: (r: Row) => r[i] ?? "",
    }));
  }, [width, selected]);

  // Reset sort/filter when switching relations so stale column sorts don't apply.
  useEffect(() => {
    setSorting([]);
    setFilter("");
  }, [selected]);

  const table = useReactTable({
    data: rows,
    columns,
    state: { sorting, globalFilter: filter },
    onSortingChange: setSorting,
    onGlobalFilterChange: setFilter,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
  });

  const filtered = table.getRowModel().rows.length;
  const statusCls = [s.status, status === "live" ? s.live : status === "connecting" ? s.connecting : ""]
    .filter(Boolean)
    .join(" ");

  return (
    <div className={s.wrap}>
      <div className={s.bar}>
        <span className={s.brand}>dep2</span>
        <ViewSwitch view={view} setView={setView} />
        <button className={s.ghost} onClick={togglePause}>
          {paused ? "Resume" : "Pause"}
        </button>
        <input
          className={s.filter}
          placeholder="Filter rows…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data-testid="data-filter"
        />
        <span className={s.counts} data-testid="data-count">
          {filtered === rows.length ? `${rows.length} rows` : `${filtered} / ${rows.length} rows`}
        </span>
        <span className={statusCls}>
          <span className={s.dot} />
          {status}
        </span>
      </div>

      <div className={s.body}>
        <nav className={s.rail} data-testid="relation-list">
          {relations.map((r) => (
            <button
              key={r.name}
              className={r.name === selected ? `${s.rel} ${s.relOn}` : s.rel}
              aria-pressed={r.name === selected}
              onClick={() => setSelected(r.name)}
            >
              <span className={s.relName}>{r.name}</span>
              <span className={s.relCount}>{r.count}</span>
            </button>
          ))}
        </nav>

        <div className={s.tableWrap}>
          <table className={s.table} data-testid="data-table">
            <thead>
              {table.getHeaderGroups().map((hg) => (
                <tr key={hg.id}>
                  {hg.headers.map((h) => {
                    const dir = h.column.getIsSorted();
                    return (
                      <th key={h.id} onClick={h.column.getToggleSortingHandler()}>
                        {flexRender(h.column.columnDef.header, h.getContext())}
                        <span className={s.sortArrow}>{dir === "asc" ? " ▲" : dir === "desc" ? " ▼" : ""}</span>
                      </th>
                    );
                  })}
                </tr>
              ))}
            </thead>
            <tbody>
              {table.getRowModel().rows.map((row) => (
                <tr key={row.id}>
                  {row.getVisibleCells().map((cell) => (
                    <td key={cell.id} title={String(cell.getValue() ?? "")}>
                      {flexRender(cell.column.columnDef.cell, cell.getContext())}
                    </td>
                  ))}
                </tr>
              ))}
              {filtered === 0 && (
                <tr>
                  <td className={s.empty} colSpan={Math.max(1, columns.length)}>
                    {selected ? "no rows" : "no relation selected"}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
