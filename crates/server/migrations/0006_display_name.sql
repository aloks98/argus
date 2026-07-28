-- 0006_display_name.sql
-- Identity metadata (fleet-identity slice). machines.tags/notes and the
-- token's max_uses/expires_at/revoked already exist from 0001 — this adds
-- only what's missing. display_name is nullable ON PURPOSE: null means
-- "show the hostname", so a machine never renamed keeps tracking hostname
-- changes instead of freezing a stale copy taken at enroll time.
alter table machines add column display_name text;
alter table enrollment_tokens add column display_name text;
alter table enrollment_tokens add column tags text[] not null default '{}';
