// A pool's PGs as a grid of coloured cells (D154). The states come a byte
// per PG from GetPgGrid, which sends them again only when the map's epoch
// has moved: polling an unchanged cluster costs a few bytes.
import { useEffect, useRef, useState, type MouseEvent } from "react";
import { cluster } from "./api";
import { ErrorText } from "./ui";

/** A PgState's name and colour (Tailwind's 500s, as the Pools page bars). */
export const PG_STATES: Record<number, [string, string]> = {
  1: ["clean", "#10b981"],
  2: ["degraded", "#f59e0b"],
  3: ["backfilling", "#0ea5e9"],
  4: ["inactive", "#ef4444"],
};

/** Polls `pool`'s states every `every` ms, keeping the last ones while
 * the map is unchanged. */
export function usePgGrid(pool: number, every = 2000) {
  const [states, setStates] = useState<Uint8Array>();
  const [error, setError] = useState<unknown>();
  useEffect(() => {
    let epoch = 0n;
    let live = true;
    const load = async () => {
      try {
        const r = await cluster.getPgGrid({ pool, sinceEpoch: epoch });
        if (!live) return;
        if (!r.unchanged) {
          epoch = r.epoch;
          setStates(r.states);
        }
        setError(undefined);
      } catch (e) {
        if (live) setError(e);
      }
    };
    load();
    const t = setInterval(load, every);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [pool, every]);
  return { states, error };
}

/** Cells in a rectangle about three times as wide as tall, as large as
 * fits `width` pixels (2 to `max`). */
function layout(n: number, width: number, max: number) {
  const cols = Math.max(1, Math.ceil(Math.sqrt(n * 3)));
  const cell = Math.max(2, Math.min(max, Math.floor(width / cols)));
  return { cols, rows: Math.ceil(n / cols), cell };
}

export function PgGrid({
  pool,
  width = 560,
  max = 12,
  picked,
  onPick,
}: {
  pool: number;
  width?: number;
  max?: number;
  /** A PG to outline. */
  picked?: number | null;
  onPick?: (pg: number) => void;
}) {
  const { states, error } = usePgGrid(pool);
  const canvas = useRef<HTMLCanvasElement>(null);
  const n = states?.length ?? 0;
  const { cols, rows, cell } = layout(n, width, max);
  const gap = cell >= 6 ? 1 : 0;
  useEffect(() => {
    const c = canvas.current;
    const g = c?.getContext("2d");
    if (!c || !g || !states) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = cols * cell * dpr;
    c.height = rows * cell * dpr;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, cols * cell, rows * cell);
    states.forEach((s, i) => {
      g.fillStyle = PG_STATES[s]?.[1] ?? "#64748b";
      g.fillRect((i % cols) * cell, Math.floor(i / cols) * cell, cell - gap, cell - gap);
    });
    if (picked != null && picked < states.length && cell >= 6) {
      g.strokeStyle = "#0f172a";
      g.lineWidth = 2;
      g.strokeRect((picked % cols) * cell + 1, Math.floor(picked / cols) * cell + 1, cell - gap - 2, cell - gap - 2);
    }
  }, [states, cols, rows, cell, gap, picked]);
  const at = (e: MouseEvent<HTMLCanvasElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const x = Math.floor((e.clientX - r.left) / cell);
    const y = Math.floor((e.clientY - r.top) / cell);
    const i = y * cols + x;
    return x < cols && i < n ? i : null;
  };
  const counts = new Map<number, number>();
  states?.forEach((s) => counts.set(s, (counts.get(s) ?? 0) + 1));
  return (
    <div data-testid={`pg-grid-${pool}`}>
      <ErrorText error={error} />
      <div className="overflow-x-auto">
        <canvas
          ref={canvas}
          role="img"
          aria-label={`Pool ${pool}'s ${n} PGs`}
          style={{ width: cols * cell, height: rows * cell }}
          className={onPick ? "cursor-pointer" : undefined}
          onMouseMove={(e) => {
            const i = at(e);
            e.currentTarget.title = i == null ? "" : `PG ${pool}.${i}: ${PG_STATES[states![i]]?.[0] ?? "unknown"}`;
          }}
          onClick={(e) => {
            const i = at(e);
            if (i != null) onPick?.(i);
          }}
        />
      </div>
      <ul className="flex flex-wrap gap-x-4 mt-1 text-xs text-slate-600 dark:text-slate-400">
        {Object.entries(PG_STATES).map(([s, [name, color]]) => (
          <li key={s} className="flex items-center gap-1" data-testid={`pg-grid-${pool}-${name}`}>
            <span className="inline-block w-2.5 h-2.5 rounded-sm" style={{ background: color }} />
            {counts.get(Number(s)) ?? 0} {name}
          </li>
        ))}
      </ul>
    </div>
  );
}
