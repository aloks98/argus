-- The deliberately-boring metrics table (PRD §6.2, §12 -- do NOT reopen):
-- plain rows, BRIN on ts, nightly prune. No Timescale/partitioning until a
-- MEASURED problem forces it.
create table metrics (
    machine_id   uuid not null references machines(id) on delete cascade,
    ts           timestamptz not null,
    cpu_pct      real,
    mem_used     bigint,
    mem_total    bigint,
    swap_used    bigint,
    swap_total   bigint,
    load1        real,
    load5        real,
    load15       real,
    disk_used    bigint,
    disk_total   bigint,
    net_rx_bytes bigint,   -- cumulative counters; deltas computed at read time
    net_tx_bytes bigint,
    uptime_secs  bigint,
    extra        jsonb not null default '{}'   -- per-disk, per-net, temps, ZFS ARC
);

-- BRIN is the whole point: a tiny index over an append-only time column.
create index metrics_ts_brin    on metrics using brin (ts) with (pages_per_range = 32);
create index metrics_machine_ts on metrics (machine_id, ts desc);
