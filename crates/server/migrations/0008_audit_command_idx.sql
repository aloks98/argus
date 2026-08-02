-- `update_command_result` runs `UPDATE audit_log ... WHERE command_id = $1`
-- on every CommandResult (and every offline-dispatch denial); without an
-- index that's a full scan of a table with no retention that only grows.
-- Partial: most audit rows (logins, enrolls, identity edits) carry no
-- command_id, so excluding the NULLs keeps the index at verb-row size.
create index audit_log_command_idx on audit_log (command_id)
    where command_id is not null;
