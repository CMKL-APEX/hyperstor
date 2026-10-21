---
name: collect-decision-data
description: Collect benchmarks and measurements that the project owner needs for open hyperstor design decisions, and record them in spec/benchmarks.md and spec/decision-log.md. Use when asked to measure something, to gather data for a decision, before proposing a design change that rests on performance or size, or at the end of an implementation session.
---

# Collecting data for decisions

Hyperstor's owner decides design points from measurements. This skill
finds what is undecided, measures it, and records the result where the
decision will be made, honestly and reproducibly.

## 1. Find what needs data

- `spec/decision-log.md`: the **Open points**, and each point's
  recommendation; what number would change it?
- `spec/implementation-notes.md`: the "To discuss" column and the
  **Review** section.
- `spec/milestones.md` §22: host checks (`G…`) that can run on the host at
  hand. Skip those that need hardware it lacks (a spare NVMe namespace,
  an RDMA NIC, drives with power-loss protection) and say so.

Write down, before measuring, the question and the result that would
answer it either way.

## 2. Measure

- Benchmarks live in `crates/hyperstor-bench` (subcommands `crc`,
  `placement`, `mem`, `raw`, `engine`, `index`, `decisions`, `redb`,
  `failover`, `replay`, `logsize`, `cluster`, `remote`). Extend it rather than writing one-off programs, so
  the next run can repeat the measurement. Print Markdown table rows.
- `scripts/collect-decision-data.sh [suite …]` builds the tool, records
  the host (CPU, memory, kernel, drives, write cache, filesystem of the
  scratch directory), runs the suites, and saves raw output under
  `target/decision-data/<timestamp>/`.
- Every comparison is an A/B in the same run, on the same host, back to
  back (the dev host's consumer SSD slows down after a few GB of writes;
  numbers from different runs aren't comparable).
- Run anything noisy twice. Report percentiles (p50, p99, p99.9, max),
  not averages, for latency.
- Check that a measurement measures what it claims: a suite that passes
  with the feature disabled, or reads that hit the page cache or an
  unwritten extent when the device was meant, prove nothing. Prefill files
  (`posix_fallocate` alone leaves unwritten extents on ext4).

## 3. Safety on shared hosts

- Never touch raw block devices; use files in a scratch directory on the
  disk being measured (`HS_SCRATCH`), never a tmpfs, and delete them
  afterwards.
- Build with `-j2` and your own `CARGO_TARGET_DIR`; other sessions share
  the host.
- Don't use `pkill -f`/`pgrep -f` with a pattern from your own command
  line: it matches the shell running it. Kill by PID.
- Keep long runs in the background with a timeout.

## 4. Record

- `spec/benchmarks.md`: a dated section per run: host (or a reference to
  the host table), the exact commands, the raw tables, caveats, and
  numbered **Findings** stating what the numbers mean, including the ones
  that go against the current plan.
- `spec/decision-log.md`: under **Data for pending decisions**, one entry
  per open point: the question, the key numbers, what they imply, and a
  link to the benchmarks section. Don't decide; recommend, and say how
  confident the data makes you.
- `spec/implementation-notes.md`: decisions made while measuring
  (defaults chosen from the data, thresholds calibrated) as `I…` rows.
- In the final message to the owner, lead with the numbers that bear on
  each open decision.
