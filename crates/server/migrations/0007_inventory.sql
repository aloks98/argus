-- Agent-reported hardware inventory (inventory slice). All nullable: NULL =
-- the agent has not reported (predates the fields, or the probe failed) --
-- the write paths coalesce so a non-reporting agent never erases these.
alter table machines add column cpu_model text;
alter table machines add column cpu_cores integer;
alter table machines add column boot_time timestamptz;
alter table machines add column virt text;
