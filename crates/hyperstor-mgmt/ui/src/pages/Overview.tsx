// Health with its reasons, capacity, throughput and latency, nodes and
// PG states at a glance. Status every 2 s; rates between two metric
// scrapes.
import { useRef } from "react";
import { cluster, metrics, type NodeMetrics } from "../api";
import { Health } from "../gen/hyperstor/v1/cluster_pb";
import { PgGrid } from "../PgGrid";
import { Card, ErrorText, Pill, Space, fmtBytes, usePoll } from "../ui";

export const healthTone = (h: Health) =>
  h === Health.OK ? "ok" : h === Health.WARN ? "warn" : h === Health.ERROR ? "error" : "muted";
export const healthText = (h: Health) =>
  ({ [Health.OK]: "ok", [Health.WARN]: "warn", [Health.ERROR]: "error" })[h as 1 | 2 | 3] ?? "unknown";

function sum(n: NodeMetrics, name: string, want: Record<string, string> = {}) {
  let t = 0;
  for (const [m, labels, v] of n.samples) {
    if (m === name && Object.entries(want).every(([k, x]) => labels[k] === x)) t += v;
  }
  return t;
}

/** The q quantile of a histogram's change between two scrapes, in seconds. */
function quantile(now: NodeMetrics[], before: NodeMetrics[] | undefined, kind: string, q: number): number | null {
  const buckets = (ns: NodeMetrics[] | undefined) => {
    const b = new Map<number, number>();
    for (const n of ns ?? []) {
      for (const [m, l, v] of n.samples) {
        if (m !== "hyperstor_request_duration_seconds_bucket" || l.kind !== kind) continue;
        const le = l.le === "+Inf" ? Infinity : parseFloat(l.le);
        b.set(le, (b.get(le) ?? 0) + v);
      }
    }
    return b;
  };
  const a = buckets(now);
  const b = buckets(before);
  const d = [...a.entries()].sort((x, y) => x[0] - y[0]).map(([le, v]) => [le, v - (b.get(le) ?? 0)] as const);
  const total = d.length ? d[d.length - 1][1] : 0;
  if (total <= 0) return null;
  for (const [le, v] of d) if (v >= q * total) return le === Infinity ? null : le;
  return null;
}

const fmtLat = (s: number | null) => (s == null ? "–" : s < 1e-3 ? `${(s * 1e6).toFixed(0)} µs` : `${(s * 1e3).toFixed(1)} ms`);

function Tile({ value, label, tone }: { value: string; label: string; tone?: "warn" | "error" }) {
  const c = tone === "error" ? "text-red-600" : tone === "warn" ? "text-amber-600" : "";
  return (
    <div>
      <div className={`text-2xl font-semibold tabular-nums ${c}`}>{value}</div>
      <div className="text-xs text-slate-500">{label}</div>
    </div>
  );
}

export default function Overview() {
  const status = usePoll(() => cluster.getStatus({}), 2000);
  const prev = useRef<{ t: number; nodes: NodeMetrics[] } | undefined>(undefined);
  const rates = usePoll(async () => {
    const nodes = await metrics();
    const t = performance.now() / 1000;
    const before = prev.current;
    prev.current = { t, nodes };
    const dt = before ? t - before.t : 0;
    const total = (ns: NodeMetrics[], name: string, want?: Record<string, string>) =>
      ns.filter((n) => n.up).reduce((a, n) => a + sum(n, name, want), 0);
    const rate = (name: string, want?: Record<string, string>) =>
      before && dt > 0 ? Math.max(0, (total(nodes, name, want) - total(before.nodes, name, want)) / dt) : 0;
    const data = total(nodes, "hyperstor_device_data_bytes");
    const free = total(nodes, "hyperstor_device_free_bytes");
    return {
      w: rate("hyperstor_requests_total", { kind: "write" }),
      r: rate("hyperstor_requests_total", { kind: "read" }),
      wb: rate("hyperstor_request_bytes_total", { kind: "write" }),
      rb: rate("hyperstor_request_bytes_total", { kind: "read" }),
      wp99: quantile(nodes, before?.nodes, "write", 0.99),
      rp99: quantile(nodes, before?.nodes, "read", 0.99),
      data,
      used: data - free,
      up: nodes.filter((n) => n.up).length,
      all: nodes.length,
    };
  }, 2000);
  const s = status.data;
  const m = rates.data;
  const used = m && m.data ? m.used / m.data : 0;
  const devices = s?.nodes.flatMap((n) => n.devices) ?? [];
  const pgs = s?.pools.reduce(
    (a, p) => ({
      clean: a.clean + (p.pgs?.clean ?? 0),
      degraded: a.degraded + (p.pgs?.degraded ?? 0),
      backfilling: a.backfilling + (p.pgs?.backfilling ?? 0),
      inactive: a.inactive + (p.pgs?.inactive ?? 0),
    }),
    { clean: 0, degraded: 0, backfilling: 0, inactive: 0 },
  );
  return (
    <>
      <Card title="Health">
        <ErrorText error={status.error} />
        {s && (
          <div className="flex items-start gap-4" data-testid="health">
            <Pill tone={healthTone(s.health)}>{healthText(s.health)}</Pill>
            <ul className="text-sm grid gap-1">
              {s.reasons.length ? (
                s.reasons.map((r, i) => (
                  <li key={i}>
                    <span className="font-semibold mr-2">{healthText(r.level)}</span>
                    {r.text}
                  </li>
                ))
              ) : (
                <li className="text-slate-500">Every PG has all its copies.</li>
              )}
              <li className="text-xs text-slate-500">
                cluster {s.clusterId} · map epoch {String(s.epoch)}
              </li>
            </ul>
          </div>
        )}
      </Card>
      <Card title="Cluster">
        <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
          <Tile value={s ? `${s.nodes.filter((n) => n.up).length} of ${s.nodes.length}` : "–"} label="nodes up" />
          <Tile value={s ? `${devices.filter((d) => d.upIn).length} of ${devices.length}` : "–"} label="devices in" />
          <Tile value={s ? String(s.pools.length) : "–"} label="pools" />
          <Tile value={pgs ? String(pgs.clean) : "–"} label="PGs clean" />
          <Tile
            value={pgs ? String(pgs.degraded + pgs.backfilling + pgs.inactive) : "–"}
            label="PGs degraded, backfilling or inactive"
            tone={pgs && pgs.inactive ? "error" : pgs && pgs.degraded + pgs.backfilling ? "warn" : undefined}
          />
        </div>
      </Card>
      <Card title="PGs">
        <div className="flex flex-wrap gap-6">
          {(s?.pools ?? []).map((p) => (
            <div key={p.id}>
              <div className="text-sm font-semibold mb-1">
                {p.id} {p.name}
              </div>
              <PgGrid pool={p.id} width={320} max={8} />
              <div className="mt-2 w-80">
                <Space total={p.totalBytes} free={p.freeBytes} />
              </div>
            </div>
          ))}
        </div>
      </Card>
      <Card title="I/O and space">
        <ErrorText error={rates.error} />
        <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
          <Tile value={m ? m.w.toFixed(m.w >= 100 ? 0 : 1) : "–"} label="writes / s" />
          <Tile value={m ? m.r.toFixed(m.r >= 100 ? 0 : 1) : "–"} label="reads / s" />
          <Tile value={m ? `${fmtBytes(m.wb)}/s` : "–"} label="written" />
          <Tile value={m ? `${fmtBytes(m.rb)}/s` : "–"} label="read" />
          <Tile value={m ? fmtLat(m.wp99) : "–"} label="write p99" />
          <Tile value={m ? fmtLat(m.rp99) : "–"} label="read p99" />
          <Tile
            value={m && m.data ? `${(100 * used).toFixed(1)} %` : "–"}
            label={m ? `used of ${fmtBytes(m.data)}` : "used"}
            tone={used >= 0.95 ? "error" : used >= 0.85 ? "warn" : undefined}
          />
          <Tile value={m ? `${m.up} of ${m.all}` : "–"} label="nodes with metrics" />
        </div>
        {used >= 0.85 && (
          <p className="mt-3 text-sm text-amber-700">
            Devices are {(100 * used).toFixed(0)} % used; writes will fail when they fill.
          </p>
        )}
      </Card>
    </>
  );
}
