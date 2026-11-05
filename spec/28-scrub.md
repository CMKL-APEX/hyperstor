# Scrub settings and reports

How often scrub checks each pool's copies, who may change that, what
scrub reports and where it is kept, when health warns, and the API, CLI
and UI for all of it. Part of the hyperstor design; sections keep their
numbers across documents, so `§N` in one document is the same section
everywhere (see [README.md](README.md)).

Status: **settled and built** (2026-11-05; 01-implementation-notes.md
I263, I265, I267, I271, I272). The owner accepted D224–D230 as written
(points 183–189 of [05-decision-log.md](05-decision-log.md)), and chose
where the metrics are served in D234 and D235 (points 198, 199). It
builds on scrub as built (09-replication.md §8.5; I58, I60, I104, I105,
D223, I249, I253), the cluster API with its roles and step-up
(19-administration.md §23.9.2), the audit log (D112), operations (D121)
and the PG grid (D154).

## 31. Scrub settings and reports (D224–D230)

### 31.1 Goal and gaps

Scrub is how hyperstor knows that the copies it holds are the data that
was written. An operator should be able to say, for each pool, when every
copy was last compared and read, and what was found. Nobody should be
able to weaken those checks silently.

Issue #158 and the pilot's repair (2026-11-02) showed these gaps:

| Gap | What it cost |
|---|---|
| Scrub's settings are `hyperstord` flags, per node | Turning on hole repair meant a systemd drop-in on every host; a node started with `--scrub-hours 0` stops scrubbing every PG it leads, and nothing says so |
| Results are only Prometheus counters, per device, reset at each restart | Telling how many objects the repair fixed meant reading three hosts' journals; a deploy mid-pass reset every count |
| No record of when a PG was last scrubbed | Nobody can say "every PG of pool 1 was read in the last week"; a restart starts a new pass from PG 0 and may never reach the last PGs if restarts come faster than the interval |
| Only unresolved objects reach the map service, in the leader's memory | A leader change forgets them until the next pass |
| No way to scrub now | After the fix, checking a pool meant waiting for the next pass |
| Recovery isn't checked soon after it runs | #158's damage came from pushes and went unseen for weeks |

### 31.2 Settings in the map, per pool (D224, D225)

Each pool carries its scrub settings in the map:

| Setting | Default | Range | Meaning |
|---|---|---|---|
| `scrub_hours` | 24 | 1–720, or 0 (off) | Every PG of the pool is light-scrubbed once in this many hours |
| `deep_every` | 7 | 1–28 | Every *n*-th pass is deep: every block is read and its CRC checked |
| `hole_repair` | `repair` | `repair`, `report` | Whether copies holding zeros where another has holes (D223) are replaced, or only reported |
| `deep_rate` | 64 MiB/s | 8–1024 MiB/s | The most a deep scrub reads per second, per device |

- **Per pool:** pools differ. A pool on hard disks may need a slower
  deep rate than one on NVMe, and a test pool may need less scrubbing
  than a pool with VM disks. Cache pools aren't scrubbed (§8.7) and keep
  the defaults unused.
- **In the map:** devices already get each new map. A change takes
  effect at the PG after it, with no restart and no flag to keep in
  sync across hosts. A map from before these fields reads as the
  defaults, as `cache_grace` did (D189).
- **Defaults are safe:** hole repair defaults to `repair`. The pilot
  showed why (point 184): 292 copies replaced, no copy changed back in
  four passes since, and the copy with the holes is right by
  construction (D223). The dry run stays possible with `report`.
- **The flags go:** `hyperstord --scrub-hours` and `--hole-repair` are
  removed, and a node started with either logs that the map now decides.
  The tests set the map instead. On the pilot, the `scrub.conf` drop-in
  is removed with the deploy.

### 31.3 Who may change them (D226)

- `SetScrubSettings(pool, …)` is a write for storage administrators,
  as `SetPool` is.
- Changes that **weaken** checks need **step-up**, a fresh login as for
  the trash settings (19-administration.md §23.9.2), and a **reason**, kept in the audit event.
  Weakening means:
  - scrub off;
  - `scrub_hours` above 168;
  - `deep_every` above the default;
  - `hole_repair` to `report`;
  - `deep_rate` below 32 MiB/s.
- Changes that **strengthen** them need neither.
- Every change is an audit event with the old and new values.
- While a pool's settings are weaker than the defaults, health warns
  (`scrub-reduced`, §31.5), naming the pool and the setting. The warning
  stays as long as the settings do.

Why: a scrub turned off loses nothing at once. That is what makes it
dangerous: damage then goes unseen until a read needs the copy. Step-up
and a reason make the choice deliberate, and the warning keeps it
visible.

### 31.4 What scrub reports, and where it is kept (D227)

After each PG it scrubs, the primary sends the map service a
`ScrubReport`. Today the report carries only the unresolved count. It
grows to carry:

- when the scrub started and how long it took;
- whether it was deep;
- the counts of objects:
  - compared;
  - inconsistent;
  - repaired;
  - unresolved;
  - damaged;
  - holes found and holes repaired;
- whether the hole check was skipped, and why (I253).

The leader keeps, for each PG, its last light and its last deep report.
These summaries are **replicated through Raft**, batched at most once a
minute. They are not part of the published map.

- **Size:** about 100 bytes per PG. A pool of 4,096 PGs scrubbed daily
  adds one batched entry a minute, under 300 KiB a day of Raft log.
- **Why replicated:** "when was this PG last read" must survive a
  leader change and a full restart. Kept only in memory, as load reports
  are (D218), a failover would make every PG look never scrubbed until
  the next pass reached it, and the overdue warning (§31.5) would be
  either wrong or turned off.
- **Restarts:** the scrubber starts its pass at the PG scrubbed longest
  ago (from the map service's record, sent with the map lease) rather
  than at PG 0. A restart then can't keep the last PGs from being
  scrubbed.

The device counters stay for Prometheus, unchanged.

### 31.5 Health (D228)

`GetStatus`, `hyperstorctl status` and the UI's health warn or fail for
these conditions:

| Check | Level | When |
|---|---|---|
| `scrub-overdue` | warn | A PG has had no light scrub in 2 × `scrub_hours`, or no deep scrub in 2 × its deep interval |
| `scrub-reduced` | warn | A pool's settings are weaker than the defaults (§31.3) |
| `scrub-unchecked` | warn | A PG's hole check was skipped in its last two scrubs |
| `inconsistent` | warn | As today (I105): objects whose copies differ with no majority |
| `damaged` | error | A deep scrub found a damaged copy and no good copy to repair it from |

Each warning names the pool and the PGs, up to ten, and links to them in
the UI. A clean scrub of the PG clears its warnings. New PGs from a
split, and PGs of a new pool, count as scrubbed when they were made, so
they don't warn before their first pass is due.

### 31.6 Scrubbing now (D229)

`StartScrub(pool, pgs?, deep)` is an operation (D121): it shows
progress, it can be cancelled, and it is audited. The PGs' primaries
scrub them ahead of their turn, at the pool's deep rate, and the normal
schedule goes on around them. It answers questions such as "after the
fix, are there still copies with zeros?" in one command, without waiting
a day.

### 31.7 Scrub after recovery (D230)

A PG that received objects by backfill, refill or peering (any push to a
member) is light-scrubbed, with the hole check, within an hour of
becoming clean. Its turn in the pass then moves to that time.

Why: pushes are where #158's damage came from, and every bug in a copy
path makes its damage there. A check soon after catches it while the
copy it came from is still at hand, before a failure makes the bad copy
the only one. The cost is one light pass of the PG (digests, which come
from the digest cache) per recovery.

### 31.8 API, CLI and UI

**Cluster API** (19-administration.md §23):

| RPC | Role | What |
|---|---|---|
| `GetScrubStatus(pool?)` | read | Per pool: its settings, PG count, oldest light and deep scrub, overdue PGs, and the last 7 days' totals |
| `ListPgScrubs(pool)` | read | Per PG: last light and deep report (§31.4) |
| `SetScrubSettings(pool, …, reason)` | write (step-up when weakening) | §31.3 |
| `StartScrub(pool, pgs, deep)` | write | §31.6; returns an operation |

**`hyperstorctl`:**

```
hyperstorctl scrub status [--pool P]
hyperstorctl scrub pgs --pool P [--overdue]
hyperstorctl scrub set --pool P [--hours N] [--deep-every N]
                       [--hole-repair repair|report] [--deep-rate MIB]
                       [--reason TEXT]
hyperstorctl scrub start --pool P [--pg N ...] [--deep]
```

**Web UI:**

- **Pools:** each pool's page gets a Scrub card with its settings
  (edited in place, asking for a fresh login and a reason when
  weakening), its coverage (oldest light and deep scrub, overdue PGs)
  and a "Scrub now" button.
- **PG grid:** a Scrub view shades each PG by the age of its last deep
  scrub, from fresh to overdue, and marks PGs with findings. The detail
  shows the last reports.
- **Overview:** the health checks of §31.5 link to the pool and its PGs.

**Metrics:** every map service member exports, per pool,
`hyperstor_pool_scrub_oldest_seconds{kind="light|deep"}` and
`hyperstor_pool_scrub_overdue_pgs`, with `hyperstor_meta_leader`, so an
alert can fire without the API's RPCs (D235). They are served at
`GET /metrics` on the cluster API's port, to a client certificate of
role `metrics` (D234).

### 31.9 Upgrades

- **Map:** an older storage node ignores the pool's new fields and goes
  on with its flags. Those flags are removed with the build that reads
  the fields. Upgrade storage nodes first: a mixed cluster scrubs on the
  old schedule until every node is new.
- **Reports:** an older map service decodes the grown `ScrubReport` by
  its leading fields (the new ones are trailing). A newer one treats a
  short report as one without the new counts.
- **Raft:** the scrub summaries are a new entry kind. Map service
  members are upgraded together, as for operations (D121).
- **As built** (01-implementation-notes.md I265): the map protocol's
  decoder refuses a body longer than it knows, so the new counts ride in
  the report's payload, which an older map service ignores. The summaries
  are rows of the store's existing `settings` table, which an older
  member applies without reading, where a new table would fail its
  apply. Hosts can be upgraded one at a time, in any order.

### 31.10 Not in this proposal

- **Time windows:** scrub only at night, say. The deep rate bounds the
  cost instead; windows can come later on top of `scrub_hours`.
- **Scrub per volume.** Scrub works by PG, and a volume spans many.
- **EC pools:** they scrub by their own rules when HS7 lands.
- **Alerts sent by hyperstor itself.** Prometheus does that from the
  metrics in §31.8.
