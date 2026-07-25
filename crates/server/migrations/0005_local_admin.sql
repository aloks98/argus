-- The break-glass credential (design §6). At most ONE row, enforced by the
-- schema rather than by application discipline: `id` is a boolean primary key
-- that may only be true, so a second insert collides on the primary key.
--
-- Only the argon2id PHC string is stored. `last_login_at` exists to make use of
-- the break-glass credential visible -- it is meant to be the exception, and
-- noticing it being used routinely is the cheapest signal available.
create table local_admin (
    id            boolean     primary key default true,
    username      text        not null,
    password_hash text        not null,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    last_login_at timestamptz,
    constraint local_admin_single_row check (id)
);
