# Machine Inventory Enrichment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The machine info strip shows disk/memory/swap (from metrics already fetched) plus processor, uptime, and virtualization (newly agent-reported), with old agents never erasing stored inventory.

**Architecture:** Additive proto fields → agent `gather()` collects via sysinfo + `systemd-detect-virt` → migration adds four nullable columns → `agent_info_row` maps empty/zero to NULL and both write paths coalesce → detail payload + strip items with strict omission rules.

**Tech Stack:** protox/tonic-prost (additive fields), sysinfo (already an agent dep), sqlx migration + compile-checked queries, existing SpecStrip pattern.

**Design of record:** `docs/superpowers/specs/2026-07-29-inventory-design.md`.

## Global Constraints

- **Tri-state discipline:** `""`/`0` from the wire = "not reported" = SQL NULL; both machine-write paths `coalesce` so an old agent's Hello NEVER erases stored inventory. This is the slice's load-bearing invariant and gets a deliberate test.
- **Strip omission rule:** an absent datum renders NO item — never a blank, dash-only, or zero-pretending-to-be-a-fact entry (swap on a swapless host: omitted entirely).
- **Proto changes are additive only** (new field numbers 10–13); no field renumbering, no breaking change; agent stays musl-static.
- **`systemd-detect-virt` semantics:** exit status is NOT failure signal — it exits non-zero for `none` (bare metal), which is a real answer. Only spawn failure/missing binary means "unreported".
- **sqlx workflow:** `cargo sqlx prepare --workspace -- --all-targets` after query changes (DB container `argus-pg`); CI checks with `--check`.
- **Gates per task:** `cargo fmt --all --check`, `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, targeted `cargo test -p <crate>`; frontend tasks add `npm --prefix frontend run typecheck && npm --prefix frontend run build`. NEVER `cargo test --workspace -- --ignored`.
- Agent task also runs `cargo check -p argus-agent --target x86_64-unknown-linux-musl`.

---

### Task 1: Proto fields + agent collection

**Files:**
- Modify: `crates/proto/proto/argus.proto` (AgentInfo), `crates/agent/src/info.rs`
- Check-only: wherever the agent builds `AgentInfo` for `Hello` (read `crates/agent/src/session.rs` — if it calls `info::gather` the new fields flow automatically; if it constructs `AgentInfo` by hand, set the new fields there identically).

**Interfaces:**
- Produces: `AgentInfo { cpu_model = 10 (string), cpu_cores = 11 (uint32), boot_time = 12 (int64, epoch SECONDS), virt = 13 (string) }`; `info::gather` populates all four best-effort.

- [ ] **Step 1: Proto** — append inside `message AgentInfo` after field 9:

```proto
  // Hardware/runtime inventory (additive, best-effort). Empty string / zero
  // means "not reported" -- the server maps those to NULL and coalesces, so
  // an agent predating these fields cannot erase stored values.
  string cpu_model = 10;   // e.g. "AMD Ryzen 7 5800X 8-Core Processor"
  uint32 cpu_cores = 11;   // logical cores; 0 = not reported
  int64 boot_time = 12;    // epoch seconds; 0 = not reported
  string virt = 13;        // systemd-detect-virt stdout ("kvm", "lxc", "none")
```

`cargo check -p argus-proto` regenerates protoc-free.

- [ ] **Step 2: Agent collection** in `info.rs` — extend `gather()`:

```rust
    let (cpu_model, cpu_cores) = cpu_info();

    Ok(AgentInfo {
        // ... existing fields unchanged ...
        cpu_model,
        cpu_cores,
        boot_time: sysinfo::System::boot_time() as i64,
        virt: detect_virt(),
        capabilities: Vec::new(),
        capabilities_reported: false,
    })
```

with:

```rust
/// CPU brand + logical core count via sysinfo (already a dependency for the
/// metrics sampler). Best-effort: an empty brand string is simply reported
/// empty and becomes NULL server-side.
fn cpu_info() -> (String, u32) {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    let cpus = sys.cpus();
    let model = cpus.first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    (model, cpus.len() as u32)
}

/// `systemd-detect-virt` stdout, trimmed. CAREFUL: the command exits
/// NON-ZERO for its most interesting answer -- "none" (bare metal) -- so
/// failure is judged on spawn error / empty output, never on exit status.
/// A host without the binary (Alpine, containers without systemd) reports
/// empty -> NULL server-side.
fn detect_virt() -> String {
    match std::process::Command::new("systemd-detect-virt").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}
```

(Verify the exact sysinfo API against the version in Cargo.lock — `refresh_cpu_all` vs `refresh_cpu` renamed across releases; `System::boot_time()` is an associated function in current sysinfo. The installed crate is the truth.)

- [ ] **Step 3: Session path** — read how `Hello`'s `AgentInfo` is built in `crates/agent/src/session.rs`. If it reuses `gather()`, done. If not, populate the four fields the same way (extract shared helpers rather than duplicating).

- [ ] **Step 4: Tests** — extend the existing `gather_returns_non_empty_*` test: on this dev host, `cpu_cores > 0`, `boot_time > 0`, and `cpu_model` non-empty; add a `detect_virt`-shape test only if the function is factored to allow it without a real binary (otherwise the live host IS the test — `systemd-detect-virt` exists on the dev box and returns `none` or a virt name; assert non-empty there).

- [ ] **Step 5: Gates** — `cargo test -p argus-agent`, fmt, clippy (SQLX_OFFLINE), musl check. Compile the workspace: the server WON'T compile if generated proto types changed shape — it doesn't reference the new fields yet, which is fine (prost generates them additively).

- [ ] **Step 6: Commit** — `git commit -am "feat(agent): report cpu model/cores, boot time, and virtualization"`

---

### Task 2: Migration + server write path (the coalesce test is the point)

**Files:**
- Create: `crates/server/migrations/0007_inventory.sql`
- Modify: `crates/server/src/repo.rs` (AgentInfoRow + both write queries), `crates/server/src/grpc.rs` (`agent_info_row` mapping)

**Interfaces:**
- Produces: nullable `machines.cpu_model/cpu_cores/boot_time/virt`; `AgentInfoRow` gains `cpu_model: Option<String>, cpu_cores: Option<i32>, boot_time: Option<OffsetDateTime>, virt: Option<String>`.

- [ ] **Step 1: Migration**

```sql
-- 0007_inventory.sql
-- Agent-reported hardware inventory (inventory slice). All nullable: NULL =
-- the agent has not reported (predates the fields, or the probe failed) --
-- the write paths coalesce so a non-reporting agent never erases these.
alter table machines add column cpu_model text;
alter table machines add column cpu_cores integer;
alter table machines add column boot_time timestamptz;
alter table machines add column virt text;
```

- [ ] **Step 2: Wire mapping** in `grpc.rs::agent_info_row` — follow the file's existing empty→None idiom exactly:

```rust
        cpu_model: none_if_empty(&info.cpu_model),
        cpu_cores: (info.cpu_cores > 0).then_some(info.cpu_cores as i32),
        boot_time: (info.boot_time > 0)
            .then(|| OffsetDateTime::from_unix_timestamp(info.boot_time).ok())
            .flatten(),
        virt: none_if_empty(&info.virt),
```

(whatever the existing helper is called — read the function; if fields are mapped inline, match that style. A `boot_time` that fails `from_unix_timestamp` — absurd value from a bad clock — degrades to None rather than erroring the session.)

- [ ] **Step 3: Both write queries** — `upsert_machine` and `update_machine_inventory` gain the four columns with the capabilities-style guard:

```sql
    cpu_model  = coalesce($N,  machines.cpu_model),
    cpu_cores  = coalesce($N+1, machines.cpu_cores),
    boot_time  = coalesce($N+2, machines.boot_time),
    virt       = coalesce($N+3, machines.virt),
```

(and in `upsert_machine`'s INSERT column list + `ON CONFLICT` SET; `EXCLUDED.x` form there: `coalesce(EXCLUDED.cpu_model, machines.cpu_model)`.) `cargo sqlx prepare --workspace -- --all-targets`.

- [ ] **Step 4: The deliberate test** (repo test module, `#[sqlx::test]`):

```rust
#[sqlx::test]
async fn old_agent_hello_does_not_erase_inventory(pool: PgPool) -> anyhow::Result<()> {
    // Full inventory arrives once (new agent)...
    let mut info = test_agent_info("m-inv");
    info.cpu_model = Some("AMD Ryzen 7 5800X".into());
    info.cpu_cores = Some(8);
    info.boot_time = Some(OffsetDateTime::from_unix_timestamp(1_700_000_000)?);
    info.virt = Some("kvm".into());
    let id = repo::upsert_machine(&pool, &info).await?;

    // ...then an OLD agent's Hello: the same machine, all four fields None
    // (that's exactly what ""/0 on the wire map to). Nothing may be erased.
    let old = test_agent_info("m-inv");
    repo::update_machine_inventory(&pool, id, &old).await?;
    repo::upsert_machine(&pool, &old).await?; // both paths, same invariant

    let row = sqlx::query!(
        "SELECT cpu_model, cpu_cores, virt, boot_time FROM machines WHERE id = $1", id
    ).fetch_one(&pool).await?;
    assert_eq!(row.cpu_model.as_deref(), Some("AMD Ryzen 7 5800X"));
    assert_eq!(row.cpu_cores, Some(8));
    assert_eq!(row.virt.as_deref(), Some("kvm"));
    assert!(row.boot_time.is_some());
    Ok(())
}
```

Prove it bites: temporarily change one `coalesce($N, machines.x)` to plain `$N`, watch the assertion fail, restore. Break evidence in the report.

- [ ] **Step 5: Gates + commit** — `git commit -am "feat(server): store agent-reported inventory, old agents never erase it"`

---

### Task 3: Read path

**Files:**
- Modify: `crates/server/src/repo.rs` (`MachineDetail` + `machine_detail`), `crates/server/src/http.rs` (`MachineDetailDto` + From impl + payload-shape test)

- [ ] **Step 1:** `MachineDetail` gains `pub cpu_model: Option<String>, pub cpu_cores: Option<i32>, pub boot_time: Option<OffsetDateTime>, pub virt: Option<String>`; add the columns to `machine_detail`'s SELECT. `MachineDetailDto` mirrors (boot_time serialized `time::serde::rfc3339::option` like `last_seen_at`). Fleet payload unchanged.

- [ ] **Step 2:** Extend the existing detail payload test (`machine_detail_json_carries_the_capability_tri_state` or a sibling): seed a machine with inventory via raw SQL, GET, assert the four keys present with values AND that a machine without inventory carries explicit nulls (presence-checked with `contains_key`, per the fleet-payload lesson — index-access on serde_json returns Null for absent keys too).

- [ ] **Step 3:** sqlx prepare, gates, commit — `git commit -am "feat(server): inventory in the machine detail payload"`

---

### Task 4: Frontend — strip items + formatUptime

**Files:**
- Modify: `frontend/src/api.ts` (MachineDetail type), `frontend/src/lib/format.ts` (formatUptime), `frontend/src/lib/metrics.ts` (latestResources helper), `frontend/src/pages/MachineDetailPage.tsx` (specItems)

- [ ] **Step 1: Types** — `MachineDetail` gains `cpu_model: string | null; cpu_cores: number | null; boot_time: string | null; virt: string | null;`.

- [ ] **Step 2: formatUptime** in lib/format.ts:

```ts
/** "up 3d 4h" / "up 2h 14m" / "up 12m" from an RFC3339 boot time. Derived
 *  client-side on every render, so it stays current without agent traffic. */
export function formatUptime(bootTimeIso: string): string {
  const secs = Math.max(0, (Date.now() - Date.parse(bootTimeIso)) / 1000);
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `up ${d}d ${h}h`;
  if (h > 0) return `up ${h}h ${m}m`;
  return `up ${m}m`;
}
```

- [ ] **Step 3: latestResources** in lib/metrics.ts (same shape as `latestMem`): walk backward for the last point carrying `disk_used`+`disk_total` (>0) and the last carrying `swap_used`+`swap_total`; return `{ disk: {used,total} | null, swap: {used,total} | null }` (independent walks — a point can have one without the other).

- [ ] **Step 4: specItems** — append conditionally (existing pattern):

```tsx
    ...(machine.cpu_model !== null
      ? [{
          label: "Processor",
          value: machine.cpu_cores !== null
            ? `${machine.cpu_model} · ${machine.cpu_cores} cores`
            : machine.cpu_model,
        }]
      : []),
    ...(machine.boot_time !== null
      ? [{ label: "Uptime", value: formatUptime(machine.boot_time) }]
      : []),
    ...(machine.virt !== null
      ? [{ label: "Virtualization", value: machine.virt === "none" ? "bare metal" : machine.virt }]
      : []),
    ...(resources.disk !== null
      ? [{
          label: "Disk",
          value: `${formatBytes(resources.disk.used)} / ${formatBytes(resources.disk.total)} (${((100 * resources.disk.used) / resources.disk.total).toFixed(0)}%)`,
        }]
      : []),
    ...(memNow !== null
      ? [{ label: "Memory", value: formatBytes(memNow.total) }]
      : []),
    ...(resources.swap !== null && resources.swap.total > 0
      ? [{
          label: "Swap",
          value: `${formatBytes(resources.swap.used)} / ${formatBytes(resources.swap.total)}`,
        }]
      : []),
```

(`memNow` already exists from the memory-chart work; `resources = latestResources(metrics)` beside it. Placement within specItems: after the existing Agent entry, before Last seen — read the current array and keep Last seen last.)

- [ ] **Step 5: Gates** (typecheck + build); visual pass is the controller's. Commit — `git commit -am "feat(fleet): inventory and resource facts in the info strip"`

---

## Final verification (controller)

- Whole-branch review (mid-tier — compact slice).
- Rebuild + restart the dev agent (root, --config or env per DEV.md) → strip shows Processor/Uptime/Virtualization (dev host: `none` → "bare metal") + Disk/Memory/Swap.
- Restart the dev server first (migration 0007 + new payload).
- The old-agent preservation invariant is covered by Task 2's deliberate-break test; spot-check live via psql after an agent reconnect (values still present).
- Strip at 390px (the mobile slice's discipline — more items must still wrap cleanly).
- DEV.md verification record; PR.
