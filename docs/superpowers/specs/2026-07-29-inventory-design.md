# Machine inventory enrichment — design

One slice, two halves (user decision: "everything in a single slice"):
the info strip surfaces resource facts the page ALREADY fetches, and the
agent starts reporting the hardware facts nothing collects today.

Builds on `mobile-pass-slice` (both touch the machine page); rebased onto
main when PR #16 merges.

## What the info strip gains

From the **latest metrics sample** (already on the page, zero new fetches):
- **Disk** — `used / total (pct)` from `disk_used`/`disk_total`.
- **Memory** — total (the used/pct story lives on the chart card).
- **Swap** — `used / total`; omitted entirely when `swap_total` is 0 or the
  counters are absent (a swapless host shouldn't advertise a zero).

From **new agent-reported inventory** (this slice's proto/agent/schema work):
- **Processor** — model string plus logical core count ("AMD Ryzen 7 5800X
  · 8 cores").
- **Uptime** — derived from the reported boot time, so it stays current on
  every poll without the agent resending anything.
- **Virtualization** — `systemd-detect-virt` output (`kvm`, `lxc`, `none` →
  shown as "bare metal"); directly useful on a Proxmox fleet where VM vs
  LXC matters.

Every new strip item is omitted when its datum is absent (old agent, no
metrics yet) — never a blank or a zero pretending to be a fact. This is the
capabilities slice's tri-state discipline applied to inventory.

## The additive-field loop (the well-trodden shape)

**Proto** (`argus.proto`, additive — no breaking change): `AgentInfo` gains
`cpu_model = 10` (string), `cpu_cores = 11` (uint32, logical),
`boot_time = 12` (int64, epoch seconds), `virt = 13` (string). proto3 can't
distinguish absent from zero/empty, so the SERVER treats `""` and `0` as
"not reported" (`nullif` at bind time) — an old agent's Hello carries
exactly those defaults and must not erase stored values (coalesce on both
write paths, same as `capabilities`).

**Agent** (`info.rs`): `sysinfo` provides cpu brand (`cpus()[0].brand()`),
logical count (`cpus().len()`), and `System::boot_time()`. `virt` comes
from running `systemd-detect-virt` once (trimmed stdout; the command
missing or failing → unreported — Alpine guests etc. degrade gracefully;
note `none` is a SUCCESS value meaning bare metal, and systemd-detect-virt
exits non-zero for `none`, so "failed" must be judged on
missing-binary/spawn-error, not exit status). Collected once at startup
alongside the existing info fields; sent on Enroll and Hello as today.

**Schema** (next free migration): `machines` gains nullable `cpu_model
text`, `cpu_cores int`, `boot_time timestamptz`, `virt text`.

**Server**: `AgentInfoRow` + `upsert_machine`/`update_machine_inventory`
(coalesce, nullif) + `MachineDetail`/`MachineDetailDto` carry the four
fields. Fleet payload does NOT (nothing on the fleet page needs them).

**Frontend**: `MachineDetail` type + the strip items above. Uptime formats
via a small `formatUptime(bootTimeIso)` ("up 3d 4h" / "up 2h 14m") in
`lib/format.ts`, pure and unit-testable by inspection.

## Out of scope

Per-filesystem disk breakdown (the metrics slice deliberately reports one
root filesystem); temperature; physical-vs-logical core split; CPU
frequency; any fleet-page changes.

## Testing

- Agent: unit tests for the virt-probe classification (missing binary →
  None; "none" → Some("none")) where the probe is factored to allow it;
  `cargo test -p argus-agent` + musl check.
- Server: `#[sqlx::test]` proving (a) a full AgentInfo round-trips into the
  new columns, (b) an old-agent Hello (empty strings, zero numbers) leaves
  previously stored inventory untouched — the coalesce/nullif pair is the
  whole feature, so it gets the deliberate test.
- Frontend: strip omission logic is conditional-array building (the
  existing `specItems` pattern); verified in the browser against the live
  dev agent, which after rebuild genuinely reports all four fields.
- Live E2E: rebuild + restart the dev agent → Processor/Uptime/Virt/Disk/
  Memory/Swap appear; then simulate an old agent (raw-SQL a machine row
  with inventory, replay a Hello without it via the existing test
  harness — or assert via the sqlx test) → values survive.
