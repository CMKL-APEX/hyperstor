// Which storage nodes and map service members share a host (D281), and a
// member's Raft state in the leader's view (D280), as `hyperstorctl
// member list` gives it.
import type { Member } from "./gen/hyperstor/v1/admin_pb";
import type { Node } from "./gen/hyperstor/v1/cluster_pb";

/** More entries behind than the leader keeps in its log: such a member
 * catches up from a snapshot. */
export const BEHIND_ENTRIES = 1000n;
/** Twenty of the leader's heartbeats (50 ms) without an answer. */
export const NO_REPLY_MS = 1000n;

/** The IP address of `IP:PORT` or `[IP]:PORT`; none for an unspecified one. */
export function ipOf(addr: string): string | undefined {
  const m = /^\[(.*)\]:\d+$/.exec(addr) ?? /^([^:]*):\d+$/.exec(addr);
  const ip = m?.[1];
  return ip && ip !== "0.0.0.0" && ip !== "::" ? ip : undefined;
}

function memberIps(m: Member): Set<string> {
  return new Set([ipOf(m.raftAddress), ipOf(m.mapAddress)].filter((x): x is string => !!x));
}

function nodeIps(n: Node): Set<string> {
  const all = n.devices.flatMap((d) => [d.addr, ...d.endpoints.map((e) => e.address)]);
  return new Set(all.map(ipOf).filter((x): x is string => !!x));
}

/** On one host: an address in common. */
export function sameHost(m: Member, n: Node): boolean {
  const ips = nodeIps(n);
  return [...memberIps(m)].some((ip) => ips.has(ip));
}

export type MemberState = "in step" | "behind" | "no reply" | "–";

/** In the leader's list alone; another member's has no progress. */
export function memberState(m: Member, all: Member[]): MemberState {
  if (!all.some((x) => x.leader && x.matched != null)) return "–";
  if (m.leader) return "in step";
  if (m.heardMs == null || m.heardMs > NO_REPLY_MS) return "no reply";
  if (m.behind == null || m.behind > BEHIND_ENTRIES) return "behind";
  return "in step";
}

export const stateTone = { "in step": "ok", behind: "warn", "no reply": "error", "–": "muted" } as const;
