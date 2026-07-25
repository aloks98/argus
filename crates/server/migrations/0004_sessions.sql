-- Browser sessions (PRD §9.1). Server-side and therefore revocable: logout
-- deletes the row and the credential is dead immediately, which matters for a
-- surface that hands out root shells.
--
-- Only the sha256 of the cookie value is stored, never the value itself, so a
-- backup or a leaked dump yields no usable session tokens. Same convention as
-- enrollment_tokens.token_hash.
create table sessions (
    token_hash   bytea       primary key,
    subject      text        not null,
    email        text,
    display_name text,
    created_at   timestamptz not null default now(),
    expires_at   timestamptz not null
);

create index sessions_expires_at_idx on sessions (expires_at);
