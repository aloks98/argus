-- Subsystems the agent reports this host actually has ("systemd", "docker",
-- "journal"). NULLABLE ON PURPOSE and the distinction is load-bearing:
--   NULL -> the agent never reported (predates capability reporting); the UI
--           must gate NOTHING, because absence of evidence is not evidence of
--           absence and blanking a working machine is the worst outcome here.
--   {}   -> the agent reported and this host has none; the UI gates everything.
alter table machines add column capabilities text[];
