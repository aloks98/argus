-- V1 core schema (PRD §6.1). Run on startup via sqlx::migrate!.

-- machines: identity + inventory + agent connectivity + org + (nullable) PVE map
create table machines (
    id            uuid primary key default gen_random_uuid(),
    machine_id    text not null unique,            -- /etc/machine-id
    hostname      text not null,
    os            text,
    kernel        text,
    arch          text,
    primary_ip    inet,
    agent_version text,
    status        text not null default 'pending', -- pending|online|offline
    last_seen_at  timestamptz,
    enrolled_at   timestamptz not null default now(),
    pve_node      text,                            -- Proxmox correlation (V1.1)
    pve_vmid      integer,
    tags          text[] not null default '{}',
    notes         text,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index machines_status_idx on machines (status);
create index machines_tags_idx   on machines using gin (tags);

-- enrollment_tokens: join tokens. The raw token is NEVER stored (only sha256).
create table enrollment_tokens (
    id         uuid primary key default gen_random_uuid(),
    name       text not null,
    token_hash bytea not null unique,
    max_uses   integer,                            -- null = unlimited
    uses       integer not null default 0,
    expires_at timestamptz,                        -- null = never
    revoked    boolean not null default false,
    created_by text,
    created_at timestamptz not null default now()
);

-- agent_certs: issued client certs -> mTLS identity + revocation
create table agent_certs (
    id          uuid primary key default gen_random_uuid(),
    machine_id  uuid not null references machines(id) on delete cascade,
    serial      numeric not null unique,           -- x509 serial
    fingerprint text not null unique,              -- sha256(DER), hex
    not_before  timestamptz not null,
    not_after   timestamptz not null,
    revoked     boolean not null default false,
    revoked_at  timestamptz,
    created_at  timestamptz not null default now()
);
create index agent_certs_machine_idx on agent_certs (machine_id);

-- ca_material: singleton internal-CA root. The private key is AES-256-GCM
-- encrypted with the field key from env. Persisting it here is what lets the
-- stateless pod reschedule without losing the fleet's identity root (PRD §5).
create table ca_material (
    id             integer primary key default 1 check (id = 1),
    cert_pem       text  not null,
    key_ciphertext bytea not null,
    key_nonce      bytea not null,
    created_at     timestamptz not null default now()
);

-- audit_log: every verb, from day one. Never bolted on later.
create table audit_log (
    id         bigint generated always as identity primary key,
    ts         timestamptz not null default now(),
    actor      text not null,                       -- OIDC subject/email, or 'system'
    action     text not null,                       -- container.restart, unit.stop, terminal.open, agent.enroll, ...
    machine_id uuid references machines(id) on delete set null,
    target_ref text,                                -- container id / unit name
    command_id uuid,                                -- correlate to the gRPC Command
    result     text,                                -- ok|error|denied
    detail     jsonb not null default '{}'
);
create index audit_log_ts_idx      on audit_log (ts);
create index audit_log_machine_idx on audit_log (machine_id, ts);
