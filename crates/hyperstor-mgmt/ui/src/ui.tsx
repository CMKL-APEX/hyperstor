// The pieces every page uses: cards, tables, buttons, errors, polling, the
// CLI hint (every action shows its hyperstorctl command, §23.7), and who
// may do what (the server decides; the UI only hides what would be
// refused).
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Modal, useSession } from "@glidex/ui-kit";
import type { Me, Methods } from "./api";

export function Card({ title, actions, children }: { title?: string; actions?: ReactNode; children: ReactNode }) {
  return (
    <section className="bg-white dark:bg-slate-900 border border-slate-200 dark:border-slate-700 rounded-xl p-4">
      {(title || actions) && (
        <div className="flex items-center gap-3 mb-3">
          {title && <h2 className="text-xs font-semibold uppercase tracking-wide text-slate-500">{title}</h2>}
          <div className="ml-auto flex gap-2">{actions}</div>
        </div>
      )}
      {children}
    </section>
  );
}

export function Table({ head, children, empty }: { head: ReactNode[]; children: ReactNode; empty?: string }) {
  const rows = Array.isArray(children) ? children.flat().filter(Boolean) : children ? [children] : [];
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm tabular-nums">
        <thead>
          <tr className="text-left text-xs text-slate-500 border-b border-slate-200 dark:border-slate-700">
            {head.map((h, i) => (
              <th key={i} className="py-2 pr-4 font-semibold whitespace-nowrap">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y divide-slate-100 dark:divide-slate-800">
          {rows.length ? (
            children
          ) : (
            <tr>
              <td colSpan={head.length} className="py-3 text-slate-500">
                {empty ?? "None."}
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

export const td = "py-2 pr-4 whitespace-nowrap align-top";

type Kind = "primary" | "danger" | "plain";

export function Button({
  kind = "plain",
  onClick,
  disabled,
  children,
  type = "button",
  title,
}: {
  kind?: Kind;
  onClick?: () => void;
  disabled?: boolean;
  children: ReactNode;
  type?: "button" | "submit";
  title?: string;
}) {
  const style = {
    primary: "bg-sky-600 hover:bg-sky-700 text-white border-sky-600",
    danger: "bg-red-600 hover:bg-red-700 text-white border-red-600",
    plain: "bg-white dark:bg-slate-800 hover:bg-slate-50 dark:hover:bg-slate-700 border-slate-300 dark:border-slate-600",
  }[kind];
  return (
    <button
      type={type}
      title={title}
      disabled={disabled}
      onClick={onClick}
      className={`px-3 py-1.5 text-sm rounded-lg border ${style} disabled:opacity-50 disabled:cursor-not-allowed`}
    >
      {children}
    </button>
  );
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="grid gap-1 text-xs text-slate-500">
      {label}
      {children}
    </label>
  );
}

export const input =
  "px-2 py-1.5 text-sm rounded-lg border border-slate-300 dark:border-slate-600 bg-white dark:bg-slate-800 text-slate-900 dark:text-slate-100";

export function ErrorText({ error }: { error: unknown }) {
  if (!error) return null;
  const e = error as { message?: string; code?: string };
  return (
    <p role="alert" className="text-sm text-red-600">
      {e.message ?? String(error)}
      {e.code ? ` (${e.code})` : ""}
    </p>
  );
}

/** The hyperstorctl command that does the same, for scripts. */
export function Cli({ cmd }: { cmd: string }) {
  return (
    <p className="mt-2 text-xs text-slate-500">
      CLI: <code className="px-1 py-0.5 rounded bg-slate-100 dark:bg-slate-800">hyperstorctl {cmd}</code>
    </p>
  );
}

/** `f`'s value, again every `every` ms (0: once) and on `reload()`. */
export function usePoll<T>(f: () => Promise<T>, every = 0) {
  const [data, setData] = useState<T | undefined>();
  const [error, setError] = useState<unknown>();
  const fn = useRef(f);
  fn.current = f;
  const reload = useCallback(async () => {
    try {
      setData(await fn.current());
      setError(undefined);
    } catch (e) {
      setError(e);
    }
  }, []);
  useEffect(() => {
    reload();
    if (!every) return;
    const t = setInterval(reload, every);
    return () => clearInterval(t);
  }, [reload, every]);
  return { data, error, reload };
}

/** A dialog with a form: `run` on submit; closes when it succeeds. */
export function ActionDialog({
  title,
  submit,
  kind = "primary",
  run,
  onClose,
  cli,
  children,
}: {
  title: string;
  submit: string;
  kind?: Kind;
  run: () => Promise<unknown>;
  onClose: (done: boolean) => void;
  cli?: string;
  children?: ReactNode;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  return (
    <Modal title={title} onClose={() => onClose(false)}>
      <form
        className="grid gap-3"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(undefined);
          try {
            await run();
            onClose(true);
          } catch (err) {
            setError(err);
          } finally {
            setBusy(false);
          }
        }}
      >
        {children}
        <ErrorText error={error} />
        {cli && <Cli cmd={cli} />}
        <div className="flex gap-2 justify-end">
          <Button onClick={() => onClose(false)}>Cancel</Button>
          <Button type="submit" kind={kind} disabled={busy}>
            {busy ? "Working..." : submit}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

const GROUPS: Record<string, string[]> = {
  read: ["viewer", "operator", "storage-admin"],
  operate: ["operator", "storage-admin"],
  store: ["storage-admin"],
  secure: ["security-admin"],
};

/** Whether the person's roles allow a group of actions (policy.rs's). */
export function useCan() {
  const { me } = useSession<Me, Methods>();
  return (group: keyof typeof GROUPS) => me.roles.some((r) => GROUPS[group].includes(r));
}

export function fmtBytes(n: number | bigint): string {
  let v = Number(n);
  const u = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${i ? v.toFixed(1) : v.toFixed(0)} ${u[i]}`;
}

/** `1G`, `512M`, `4096`: bytes, or null. */
export function parseSize(s: string): bigint | null {
  const m = /^\s*(\d+)\s*([KMGT]?)i?B?\s*$/i.exec(s);
  if (!m) return null;
  const mult = { "": 1n, K: 1n << 10n, M: 1n << 20n, G: 1n << 30n, T: 1n << 40n }[m[2].toUpperCase() as "" | "K" | "M" | "G" | "T"];
  return BigInt(m[1]) * mult;
}

export function fmtTime(unix: number | bigint): string {
  const n = Number(unix);
  return n ? new Date(n * 1000).toLocaleString() : "–";
}

export function Pill({ tone, children }: { tone: "ok" | "warn" | "error" | "muted"; children: ReactNode }) {
  const c = {
    ok: "bg-emerald-100 text-emerald-800 dark:bg-emerald-900 dark:text-emerald-200",
    warn: "bg-amber-100 text-amber-800 dark:bg-amber-900 dark:text-amber-200",
    error: "bg-red-100 text-red-800 dark:bg-red-900 dark:text-red-200",
    muted: "bg-slate-100 text-slate-700 dark:bg-slate-800 dark:text-slate-300",
  }[tone];
  return <span className={`inline-block px-2 py-0.5 rounded-full text-xs font-medium ${c}`}>{children}</span>;
}
