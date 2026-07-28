# ntfy alerts — design (DEFERRED, decisions locked 2026-07-29)

**Status: deliberately not built yet.** The design below was brainstormed to
completion and then parked, by explicit decision, until its delivery
mechanism matures — see "Why deferred". Nothing here is tentative: when the
unblock condition is met, this doc goes straight to an implementation plan.

## Why deferred (the apalis gate, run 2026-07-29)

Alert delivery is retried background work, which CLAUDE.md routes to apalis
on Postgres behind a validation gate. The gate was run and **failed on every
current option**:

- `apalis-sql` 0.7.4 (stable): has the Postgres backend but pins **sqlx
  ^0.8.1** against our workspace 0.9 — two sqlx majors in one binary, and
  apalis's pool cannot be our pool. It is also the legacy line.
- `apalis-sql` 1.0.0-rc.9: the Postgres backend was **removed** from the
  crate entirely (only `chrono`/`time` features remain).
- **`apalis-postgres` 1.0.0-rc.8** (apalis-dev org — the official successor,
  workflow support included): resolves and compiles cleanly next to sqlx 0.9
  (probe verified: no aws-lc / native-tls / openssl anywhere in the tree,
  ring stays the only crypto) — **but it still pins sqlx 0.8.6** and is a
  release candidate with breaking schema changes between RCs.
- `pgmq` crate 0.33.7: also pins sqlx 0.8; the extension-via-raw-SQL route
  avoids that but adds a Postgres extension dependency to every environment.

A hand-rolled outbox table was considered and declined: rather than build an
interim mechanism and migrate later, the decision (user, 2026-07-29) is to
wait and build alerts directly on `apalis-postgres`.

**Unblock condition — re-probe when EITHER holds:**
1. `apalis-postgres` publishes a **stable 1.0** targeting **sqlx ≥ 0.9**, or
2. the slice becomes urgent enough that its current state is re-accepted
   deliberately.

The re-probe is five minutes: scratch crate with our sqlx line +
`apalis-postgres`, then `cargo tree` — no aws-lc, one sqlx major, done.
Watch: https://github.com/apalis-dev/apalis-postgres

## Decisions locked (all user-confirmed)

### Conditions (four kinds)

1. **Machine offline / back online** — fires on status *transitions*: the
   45s sweeper (`mark_stale_offline`, extended to return which machines it
   flipped rather than a count) and the heartbeat-restore path. The recovery
   notification fires only if an offline alert was active for that machine.
2. **Disk threshold** — root filesystem ≥ `ARGUS_ALERT_DISK_PCT` (default
   90) from the metrics already ingested every 15s, with hysteresis:
   resolves below threshold − 5 so 89.9↔90.1 cannot flap.
3. **Sustained CPU** — `cpu_pct` ≥ `ARGUS_ALERT_CPU_PCT` (default 95)
   continuously for `ARGUS_ALERT_CPU_MINS` (default 10); recovery requires
   the same window below threshold.
4. **Argus-door security events** (stateless, per-kind cooldown so a brute
   force sends one alert, not thirty): denied enrollment attempts
   (`agent.enroll` denied), the local-admin rate limiter engaging
   (burst-exhausted — today only a `tracing::warn!` in
   `auth/ratelimit.rs`), and OIDC sign-ins rejected for a missing role.
   Explicitly chosen over guest-SSH watching (fail2ban territory — its own
   slice if ever). Note: failed systemd units were offered and NOT selected.

### Delivery

- **Self-hosted ntfy, one topic.** `ARGUS_NTFY_URL` (server + topic),
  optional `ARGUS_NTFY_TOKEN`. Priorities carry severity: offline/security
  high, disk/CPU default, recoveries low. Title/body use the machine's
  display name; click-through URL to the machine page via
  `ARGUS_PUBLIC_URL`. Both env vars absent ⇒ alerting disabled with one
  boot-time INFO (not an error — alerting is optional).
- **Fire on transition, never per sample.** State per (machine, kind) in an
  `alert_state` table (next free migration number; PG `unique nulls not
  distinct` for the machine-less security kinds) so a restart neither
  re-fires active alerts nor loses them.
- **Delivery is an apalis job** (`SendNtfy { title, body, priority, tags,
  click_url }`) with built-in retry/backoff — ntfy briefly down is exactly
  when alerts matter; a pod restart must not lose a queued alert.
  apalis-postgres runs its own schema/migrations and (until the sqlx lines
  converge) its own small pool from `ARGUS_DATABASE_URL`.
- **Every fired/resolved alert writes an audit row** (`alert.fired` /
  `alert.resolved`, actor System, detail = kind + machine + message) — free
  in-app history, inherited by the future audit-viewer slice.

### Config surface

Env-only thresholds (global, no per-machine overrides): the PRD explicitly
defers the alert-config table to V1.1/V2, and this slice must not create it.
`ARGUS_ALERT_COOLDOWN_MINS` (default 30) suppresses transition flapping per
(machine, kind) and rate-limits the stateless security kinds.

### Testing sketch

Pure state-machine tests (hysteresis boundaries, sustain window, cooldown);
DB tests proving transition → exactly one enqueued job; a delivery test
against an in-test HTTP listener asserting payload/priority/auth header;
live E2E: kill the dev agent → offline alert → restart → recovery; hammer
the local login past the burst → exactly one security alert.

## When unblocked

Write the implementation plan from this doc (superpowers:writing-plans), and
amend CLAUDE.md's background-work section in the same slice: the 2026-07-29
gate results above replace the "apalis is not yet a compiled dependency"
paragraph, whatever the final mechanism turns out to be.
