// A pool's PGs as a grid of coloured cells (D154). The states come a byte
// per PG from GetPgGrid, which sends them again only when the map's epoch
// has moved: polling an unchanged cluster costs a few bytes. So does each
// PG's usage (D155), a level a byte that shades its cell, sent again only
// when a primary's report changed it.
import { useEffect, useRef, useState, type MouseEvent } from "react";
import { cluster } from "./api";
import { ErrorText, fmtBytes } from "./ui";

/** A PgState's name and colour (Tailwind's 500s, as the Pools page bars). */
export const PG_STATES: Record<number, [string, string]> = {
  1: ["clean", "#10b981"],
  2: ["degraded", "#f59e0b"],
  3: ["backfilling", "#0ea5e9"],
  4: ["inactive", "#ef4444"],
};

/** A PG's usage: its level (1 to 255) and the fullest PG's bytes. */
export type Usage = { levels: Uint8Array; max: bigint };

/** Polls `pool`'s states and usage every `every` ms, keeping the last
 * ones while unchanged. Without a leader to answer for usage, the states
 * alone. */
export function usePgGrid(pool: number, every = 2000) {
  const [states, setStates] = useState<Uint8Array>();
  const [usage, setUsage] = useState<Usage>();
  const [error, setError] = useState<unknown>();
  useEffect(() => {
    let epoch = 0n;
    let version = 0n;
    let live = true;
    const load = async () => {
      try {
        const r = await cluster
          .getPgGrid({ pool, sinceEpoch: epoch, sinceUsage: version })
          .catch(() => {
            version = 0n;
            return cluster.getPgGrid({ pool, sinceEpoch: epoch });
          });
        if (!live) return;
        if (!r.unchanged) {
          epoch = r.epoch;
          setStates(r.states);
        }
        if (r.usageVersion === 0n) {
          setUsage(undefined);
        } else if (!r.usageUnchanged) {
          version = r.usageVersion;
          setUsage(r.usage.some((l) => l > 0) ? { levels: r.usage, max: r.maxBytes } : undefined);
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
  return { states, usage, error };
}

/** A cell's opacity: full without a report, else from 0.3 (empty) to 1
 * (the fullest), so a PG's state shows whatever it holds. */
function shade(level: number | undefined) {
  return !level ? 1 : 0.3 + (0.7 * (level - 1)) / 254;
}

/** About the bytes a level stands for: the inverse of the API's log
 * scale. */
function approx(level: number, max: bigint) {
  return Math.round(Math.expm1(((level - 1) / 254) * Math.log1p(Number(max))));
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
  const { states, usage, error } = usePgGrid(pool);
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
      g.globalAlpha = shade(usage?.levels[i]);
      g.fillRect((i % cols) * cell, Math.floor(i / cols) * cell, cell - gap, cell - gap);
    });
    g.globalAlpha = 1;
    if (picked != null && picked < states.length && cell >= 6) {
      g.strokeStyle = "#0f172a";
      g.lineWidth = 2;
      g.strokeRect((picked % cols) * cell + 1, Math.floor(picked / cols) * cell + 1, cell - gap - 2, cell - gap - 2);
    }
  }, [states, usage, cols, rows, cell, gap, picked]);
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
            const l = i == null ? 0 : (usage?.levels[i] ?? 0);
            const held = l ? `, ≈ ${fmtBytes(approx(l, usage!.max))} on its primary` : "";
            e.currentTarget.title =
              i == null ? "" : `PG ${pool}.${i}: ${PG_STATES[states![i]]?.[0] ?? "unknown"}${held}`;
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
        {usage && (
          <li className="flex items-center gap-1" data-testid={`pg-grid-${pool}-usage`}>
            <span
              className="inline-block w-8 h-2.5 rounded-sm"
              style={{ background: `linear-gradient(to right, ${PG_STATES[1][1]}4d, ${PG_STATES[1][1]})` }}
            />
            less to more data, the fullest PG {fmtBytes(usage.max)}
          </li>
        )}
      </ul>
    </div>
  );
}
