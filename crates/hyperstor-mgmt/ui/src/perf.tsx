// The load and drives (27-performance.md §30, D218-D222): the leader's 15
// minutes of each device at 5-s windows, its drive, the busiest volumes and
// the Advisor's findings, polled only while shown. Charts are plain SVG in
// the page's colours; longer history is Grafana's, from each node's
// metrics port.
import { useState } from "react";
import { Link } from "react-router-dom";
import { cluster } from "./api";
import { Card, Cli, ErrorText, Pill, Table, td, usePoll } from "./ui";
import type { DevicePerf, Finding, PerfSample, ServiceTime } from "./gen/hyperstor/v1/cluster_pb";

/** Upper bounds of the service-time buckets, µs; the last is slower. */
export const BUCKETS_US = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

export function fmtUs(us: number): string {
  if (!us) return "-";
  return us >= 1000 ? `${(us / 1000).toFixed(1)} ms` : `${us} µs`;
}

export function fmtRate(n: number): string {
  return n >= 10_000 ? `${(n / 1000).toFixed(0)}k` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : n.toFixed(0);
}

const MIB = 1048576;

/** The time under which a share `q` of I/Os completed. */
export function quantile(buckets: number[], max: number, q: number): number {
  const n = buckets.reduce((a, b) => a + b, 0);
  if (!n) return 0;
  const want = Math.max(1, Math.ceil(n * q));
  let seen = 0;
  for (let i = 0; i < buckets.length; i++) {
    seen += buckets[i];
    if (seen >= want) return Math.min(BUCKETS_US[i] ?? max, Math.max(max, 1));
  }
  return max;
}

/** The last `n` samples merged: requests/s, MiB/s, p99s, most in flight. */
export function recent(d: DevicePerf, n = 12) {
  const s = d.samples.slice(-n);
  const avg = (f: (x: PerfSample) => number) => (s.length ? s.reduce((a, x) => a + f(x), 0) / s.length : 0);
  const merge = (f: (x: PerfSample) => ServiceTime | undefined) => {
    const b = new Array(14).fill(0);
    let max = 0;
    for (const x of s) {
      const t = f(x);
      if (!t) continue;
      t.buckets.forEach((c, i) => (b[i] += c));
      max = Math.max(max, t.maxUs);
    }
    return quantile(b, max, 0.99);
  };
  return {
    ops: avg((x) => x.readOps + x.writeOps),
    mib: avg((x) => x.readBytes + x.writeBytes) / MIB,
    readP99: merge((x) => x.reads),
    commitP99: merge((x) => x.commits),
    inflight: Math.max(0, ...s.map((x) => x.inflightMax)),
  };
}

/** Polls every device, or one, every 5 s. */
export function useDevicePerf(device?: number) {
  return usePoll(async () => (await cluster.listDevicePerf({ device })).devices, 5000);
}

/** A small line over time; `values` oldest first. */
export function Sparkline({ values, color = "#0ea5e9", height = 28, width = 120, label }: { values: number[]; color?: string; height?: number; width?: number; label?: string }) {
  const max = Math.max(1e-9, ...values);
  const pts = values.map((v, i) => `${(i / Math.max(1, values.length - 1)) * width},${height - 2 - (v / max) * (height - 4)}`).join(" ");
  return (
    <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} role="img" aria-label={label}>
      {values.length > 1 && <polyline points={pts} fill="none" stroke={color} strokeWidth={1.5} />}
    </svg>
  );
}

/** Lines over the samples' time, with the y axis's top value. */
function Chart({ title, series, unit, height = 120 }: { title: string; series: { name: string; color: string; values: number[] }[]; unit: (v: number) => string; height?: number }) {
  const w = 600;
  const n = Math.max(0, ...series.map((s) => s.values.length));
  const max = Math.max(1e-9, ...series.flatMap((s) => s.values));
  const x = (i: number) => 40 + (i / Math.max(1, n - 1)) * (w - 50);
  const y = (v: number) => height - 18 - (v / max) * (height - 30);
  return (
    <figure className="grid gap-1">
      <figcaption className="text-xs text-slate-500 flex gap-3">
        <span className="font-semibold">{title}</span>
        {series.map((s) => (
          <span key={s.name} className="flex items-center gap-1">
            <span className="inline-block w-3 h-0.5" style={{ background: s.color }} /> {s.name}
          </span>
        ))}
      </figcaption>
      <svg viewBox={`0 0 ${w} ${height}`} className="w-full text-slate-400" role="img" aria-label={title}>
        <line x1={40} x2={w - 10} y1={height - 18} y2={height - 18} stroke="currentColor" strokeWidth={0.5} />
        <text x={36} y={14} fontSize={10} textAnchor="end" fill="currentColor">
          {unit(max)}
        </text>
        <text x={36} y={height - 18} fontSize={10} textAnchor="end" fill="currentColor">
          0
        </text>
        <text x={40} y={height - 4} fontSize={10} fill="currentColor">
          {n > 1 ? `${Math.round(((n - 1) * 5) / 60)} min ago` : ""}
        </text>
        <text x={w - 10} y={height - 4} fontSize={10} textAnchor="end" fill="currentColor">
          now
        </text>
        {series.map((s) => (
          <polyline key={s.name} fill="none" stroke={s.color} strokeWidth={1.5} points={s.values.map((v, i) => `${x(i)},${y(v)}`).join(" ")} />
        ))}
      </svg>
    </figure>
  );
}

/** Service time over time: a column per window, a row per bucket, darker
 * for more I/Os. Slow episodes (garbage collection, throttling) show as
 * bands high up. */
function Heatmap({ title, samples, pick }: { title: string; samples: PerfSample[]; pick: (s: PerfSample) => ServiceTime | undefined }) {
  const w = 600;
  const rows = 14;
  const h = 150;
  const cw = (w - 50) / Math.max(1, samples.length);
  const rh = (h - 20) / rows;
  const max = Math.max(1, ...samples.flatMap((s) => pick(s)?.buckets ?? []));
  return (
    <figure className="grid gap-1">
      <figcaption className="text-xs text-slate-500 font-semibold">{title}</figcaption>
      <svg viewBox={`0 0 ${w} ${h}`} className="w-full text-slate-400" role="img" aria-label={title}>
        {[0, 4, 6, 8, 10, 12].map((b) => (
          <text key={b} x={44} y={h - 20 - b * rh - 2} fontSize={9} textAnchor="end" fill="currentColor">
            {fmtUs(BUCKETS_US[b])}
          </text>
        ))}
        {samples.map((s, i) =>
          (pick(s)?.buckets ?? []).map((c, b) =>
            c ? (
              <rect key={`${i}-${b}`} x={48 + i * cw} y={h - 20 - (b + 1) * rh} width={Math.max(1, cw - 0.5)} height={rh - 0.5} fill="#7c3aed" opacity={0.15 + (0.85 * Math.log1p(c)) / Math.log1p(max)}>
                <title>{`${c} I/Os at most ${fmtUs(BUCKETS_US[b] ?? 0) || "slower"}`}</title>
              </rect>
            ) : null,
          ),
        )}
      </svg>
    </figure>
  );
}

/** Each window's most in flight against its p99: where the drive's knee
 * is, past which more depth only adds latency. */
function Knee({ samples }: { samples: PerfSample[] }) {
  const w = 300;
  const h = 150;
  const pts = samples.filter((s) => (s.reads?.count ?? 0) + (s.commits?.count ?? 0) > 0).map((s) => ({ q: s.inflightMax, p: Math.max(s.reads?.p99Us ?? 0, s.commits?.p99Us ?? 0) }));
  const mq = Math.max(1, ...pts.map((p) => p.q));
  const mp = Math.max(1, ...pts.map((p) => p.p));
  return (
    <figure className="grid gap-1">
      <figcaption className="text-xs text-slate-500 font-semibold">In flight against p99</figcaption>
      <svg viewBox={`0 0 ${w} ${h}`} className="w-full max-w-sm text-slate-400" role="img" aria-label="In flight against p99">
        <line x1={40} x2={w - 5} y1={h - 18} y2={h - 18} stroke="currentColor" strokeWidth={0.5} />
        <line x1={40} x2={40} y1={5} y2={h - 18} stroke="currentColor" strokeWidth={0.5} />
        <text x={36} y={12} fontSize={9} textAnchor="end" fill="currentColor">
          {fmtUs(mp)}
        </text>
        <text x={w - 5} y={h - 4} fontSize={9} textAnchor="end" fill="currentColor">
          {mq} in flight
        </text>
        {pts.map((p, i) => (
          <circle key={i} cx={40 + (p.q / mq) * (w - 50)} cy={h - 18 - (p.p / mp) * (h - 30)} r={2.5} fill="#0ea5e9" opacity={0.7} />
        ))}
      </svg>
    </figure>
  );
}

function tempOf(d: DevicePerf): number | undefined {
  return d.drive?.temperatureC ?? (d.drive?.smart ? d.drive.smart.temperatureC : undefined);
}

/** Every device in a row, the last minute's numbers in shaded cells, so
 * the outlier stands out; sortable by any column. */
export function DeviceMatrix({ devices, onPick, picked }: { devices: DevicePerf[]; onPick: (d: number) => void; picked?: number | null }) {
  type Col = "device" | "ops" | "mib" | "readP99" | "commitP99" | "inflight" | "temp" | "wear";
  const [sort, setSort] = useState<Col>("device");
  const rows = devices.map((d) => ({ d, r: recent(d), temp: tempOf(d), wear: d.drive?.smart?.percentageUsed }));
  const val = (x: (typeof rows)[number], c: Col) => (c === "device" ? -x.d.device : c === "temp" ? (x.temp ?? -1) : c === "wear" ? (x.wear ?? -1) : x.r[c]);
  rows.sort((a, b) => val(b, sort) - val(a, sort));
  const max = (c: Col) => Math.max(1e-9, ...rows.map((x) => val(x, c)));
  const cell = (x: (typeof rows)[number], c: Col, text: string, color: string) => (
    <td className={td} style={{ background: `${color}${Math.round((Math.max(0, val(x, c)) / max(c)) * 0.45 * 255).toString(16).padStart(2, "0")}` }}>
      {text}
    </td>
  );
  const head = (c: Col, label: string) => (
    <button className={`font-semibold ${sort === c ? "text-sky-700 dark:text-sky-300" : ""}`} onClick={() => setSort(c)}>
      {label}
    </button>
  );
  return (
    <Table head={[head("device", "Device"), "Node", "Kind", head("ops", "Requests/s"), head("mib", "MiB/s"), head("readP99", "Read p99"), head("commitP99", "Commit p99"), head("inflight", "In flight"), head("temp", "Temp"), head("wear", "Wear")]} empty="No device has reported to the leader yet.">
      {rows.map((x) => (
        <tr key={x.d.device} data-testid={`perf-${x.d.device}`} className={`cursor-pointer ${picked === x.d.device ? "bg-sky-50 dark:bg-sky-950" : "hover:bg-slate-50 dark:hover:bg-slate-800"}`} onClick={() => onPick(x.d.device)}>
          <td className={td}>{x.d.device}</td>
          <td className={td}>{x.d.node}</td>
          <td className={td}>{x.d.kind}</td>
          {cell(x, "ops", fmtRate(x.r.ops), "#0ea5e9")}
          {cell(x, "mib", x.r.mib.toFixed(1), "#0ea5e9")}
          {cell(x, "readP99", fmtUs(x.r.readP99), "#7c3aed")}
          {cell(x, "commitP99", fmtUs(x.r.commitP99), "#7c3aed")}
          {cell(x, "inflight", String(x.r.inflight), "#f59e0b")}
          {cell(x, "temp", x.temp == null ? "-" : `${x.temp.toFixed(0)} °C`, "#ef4444")}
          {cell(x, "wear", x.wear == null ? "-" : `${x.wear}%`, "#ef4444")}
        </tr>
      ))}
    </Table>
  );
}

/** One device: its drive, then its 15 minutes. */
export function DevicePanel({ device, onClose }: { device: number; onClose: () => void }) {
  const { data, error } = useDevicePerf(device);
  const d = data?.[0];
  const s = d?.samples ?? [];
  const dr = d?.drive;
  const sm = dr?.smart;
  return (
    <Card title={`Device ${device}`} actions={<button className="text-sm text-sky-700 dark:text-sky-300 hover:underline" onClick={onClose}>Close</button>}>
      <ErrorText error={error} />
      {d && (
        <div className="grid gap-4" data-testid={`device-panel-${device}`}>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            <dt className="text-slate-500">Drive</dt>
            <dd>{dr?.model ? `${dr.model} (firmware ${dr.firmware})` : d.kind === "file" ? "a file: no drive to read" : "-"}</dd>
            {dr?.linkWidth != null && (
              <>
                <dt className="text-slate-500">PCIe link</dt>
                <dd>
                  x{dr.linkWidth} at {dr.linkSpeed?.toFixed(1)} GT/s {(dr.linkWidth < (dr.linkWidthMax ?? 0) || (dr.linkSpeed ?? 0) < (dr.linkSpeedMax ?? 0)) && <Pill tone="warn">can do x{dr.linkWidthMax} at {dr.linkSpeedMax?.toFixed(1)} GT/s</Pill>}
                </dd>
              </>
            )}
            {dr && dr.numaNode >= 0 && (
              <>
                <dt className="text-slate-500">NUMA</dt>
                <dd>
                  drive on node {dr.numaNode}, reactor on node {dr.reactorNumaNode} {dr.reactorNumaNode >= 0 && dr.numaNode !== dr.reactorNumaNode && <Pill tone="warn">crosses nodes</Pill>}
                </dd>
              </>
            )}
            {dr && (
              <>
                <dt className="text-slate-500">Write cache</dt>
                <dd>{dr.writeCache ? "volatile (flushes cost)" : "none or protected"}</dd>
              </>
            )}
            {sm ? (
              <>
                <dt className="text-slate-500">Health</dt>
                <dd>
                  {sm.temperatureC.toFixed(0)} °C (warns at {sm.warningThresholdC.toFixed(0)}), {sm.percentageUsed}% worn, spare {sm.availableSpare}% (threshold {sm.spareThreshold}%), {String(sm.mediaErrors)} media errors {sm.criticalWarning ? <Pill tone="error">critical warning {sm.criticalWarning}</Pill> : null}
                </dd>
                <dt className="text-slate-500">Lifetime</dt>
                <dd>
                  {(Number(sm.bytesWritten) / 1e12).toFixed(1)} TB written, {String(sm.powerOnHours)} h on, throttled {sm.throttleSeconds[0] ?? 0} s + {sm.throttleSeconds[1] ?? 0} s, {String(sm.unsafeShutdowns)} unsafe shutdowns
                </dd>
              </>
            ) : (
              dr?.smartNote && (
                <>
                  <dt className="text-slate-500">Health</dt>
                  <dd className="text-slate-500">{dr.smartNote}</dd>
                </>
              )
            )}
          </dl>
          <Chart title="Requests per second (as primary)" unit={fmtRate} series={[{ name: "reads", color: "#0ea5e9", values: s.map((x) => x.readOps) }, { name: "writes", color: "#f59e0b", values: s.map((x) => x.writeOps) }]} />
          <Chart title="MiB per second" unit={(v) => (v / MIB).toFixed(1)} series={[{ name: "read", color: "#0ea5e9", values: s.map((x) => x.readBytes) }, { name: "written", color: "#f59e0b", values: s.map((x) => x.writeBytes) }]} />
          <Chart title="Device p99" unit={fmtUs} series={[{ name: "reads", color: "#7c3aed", values: s.map((x) => x.reads?.p99Us ?? 0) }, { name: "commits", color: "#db2777", values: s.map((x) => x.commits?.p99Us ?? 0) }]} />
          <div className="grid md:grid-cols-2 gap-4">
            <Heatmap title="Device reads, by time taken" samples={s} pick={(x) => x.reads} />
            <Heatmap title="Device commits, by time taken" samples={s} pick={(x) => x.commits} />
          </div>
          <Knee samples={s} />
          <Cli cmd={`perf --device ${device}`} />
        </div>
      )}
    </Card>
  );
}

const TONE: Record<string, "ok" | "warn" | "error" | "muted"> = { info: "muted", warn: "warn", error: "error" };

/** Snoozed findings, per viewer, a day each: (rule, subject) → until. */
const SNOOZE_KEY = "hyperstor.advisor.snoozed";
function snoozed(): Record<string, number> {
  try {
    return JSON.parse(localStorage.getItem(SNOOZE_KEY) ?? "{}");
  } catch {
    return {};
  }
}

/** The Advisor's findings, each with its numbers and what to do. */
export function Findings({ limit, compact }: { limit?: number; compact?: boolean }) {
  const { data, error } = usePoll(async () => (await cluster.listFindings({})).findings, 10000);
  const [snooze, setSnooze] = useState(snoozed);
  const key = (f: Finding) => `${f.rule} ${f.subject}`;
  const now = Date.now();
  const shown = (data ?? []).filter((f) => !(snooze[key(f)] > now));
  const hidden = (data?.length ?? 0) - shown.length;
  const later = (f: Finding) => {
    const next = { ...snooze, [key(f)]: now + 86_400_000 };
    try {
      localStorage.setItem(SNOOZE_KEY, JSON.stringify(next));
    } catch {
      // A private window: snoozed until the page reloads.
    }
    setSnooze(next);
  };
  const link = (subject: string) => {
    const [kind, id] = subject.split(" ");
    return kind === "device" ? `/nodes?device=${id}` : kind === "volume" ? `/volumes?id=${id}` : "/";
  };
  return (
    <div className="grid gap-2" data-testid="findings">
      <ErrorText error={error} />
      {data && !shown.length && <p className="text-sm text-slate-500">Nothing to suggest: no device, drive or volume stands out.</p>}
      {shown.slice(0, limit).map((f) => (
        <article key={key(f)} className="border border-slate-200 dark:border-slate-700 rounded-lg p-3 grid gap-1 text-sm" data-testid={`finding-${f.rule}`}>
          <div className="flex items-center gap-2">
            <Pill tone={TONE[f.severity] ?? "muted"}>{f.severity}</Pill>
            <Link to={link(f.subject)} className="font-semibold hover:underline">
              {f.title}
            </Link>
            {!compact && (
              <button className="ml-auto text-xs text-slate-500 hover:underline" onClick={() => later(f)}>
                Snooze a day
              </button>
            )}
          </div>
          <p className="text-slate-600 dark:text-slate-400">Seen: {f.evidence}</p>
          {!compact && <p>Do: {f.action}</p>}
          {!compact && f.command && <Cli cmd={f.command.replace(/^hyperstorctl /, "")} />}
        </article>
      ))}
      {!compact && hidden > 0 && <p className="text-xs text-slate-500">{hidden} snoozed in this browser.</p>}
    </div>
  );
}
