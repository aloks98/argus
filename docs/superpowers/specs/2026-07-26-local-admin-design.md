# Local admin account — design

**Status:** design of record for the local-admin slice.
**Date:** 2026-07-26.
**Depends on:** the OIDC slice (`2026-07-25-oidc-design.md`). This branches from
`oidc-slice` and must land after it — it reuses `sessions`, `Identity`,
`create_session`, the session cookie builder, and the auth middleware unchanged.

---

## 1. Why

The OIDC slice made Argus authenticate against an identity provider and, by
design, gave it **no bypass mode**. That is the right posture, and it has one
consequence: when the provider is unavailable, so is the control plane — at
precisely the moment an operator is most likely to need it. A self-hosted IdP
in the same homelab is a plausible simultaneous casualty.

This slice adds one local credential that works without the provider.

## 2. Goal

A single local admin account with a generated password, usable at any time,
that mints an ordinary session — the same session an OIDC login produces, with
the same expiry, revocation, logout and audit behaviour.

## 3. Non-goals

- **Multiple local users.** One account. This is a break-glass credential, not
  a user system.
- **Choosing a password.** Rotation always generates (§7).
- **Password reset by email**, TOTP, or any second factor.
- **Any distinct role.** The local admin gets exactly the access an admitted
  OIDC user gets. RBAC remains V2 per the PRD.
- **An unauthenticated first-run setup page** — see §5.

## 4. The boot rule becomes "at least one auth method"

Today the control plane refuses to start without all five OIDC variables. That
becomes: **boot succeeds if OIDC is configured *or* a local admin row exists**,
checked with one query after migrations run.

This is not a weakening of the OIDC slice's §5.1. That rule existed to guarantee
the browser surface is never served unauthenticated; what actually mattered was
*"authentication is configured"*, and stating it as *"OIDC specifically is
configured"* was an overspecification. With neither present the server still
refuses to boot — with an error naming the CLI command that fixes it.

Without this change the feature would be half a safety net: it would rescue an
IdP that is *unreachable*, but not one whose client secret was lost or whose
application was deleted, because the process would not start at all.

## 5. Provisioning: two paths, because there are two states

### 5.1 CLI — the bootstrap and true break-glass

`argus local-admin reset` connects to Postgres directly and works **with the
server stopped**. It generates a password, writes the argon2id hash, and prints
the password once.

This requires host and database access, which is the honest trust boundary:
anyone who can run the binary against the database already controls the
deployment.

**The CLI must not require OIDC configuration to run.** This is the detail that
makes or breaks the feature: `Config::from_env` currently demands the OIDC
variables, so a naive implementation would refuse to run the recovery command in
exactly the situation the recovery command exists for. Configuration loading
splits — the CLI path loads only `ARGUS_DATABASE_URL`; OIDC settings are loaded
only when serving.

Argument handling is a hand-rolled match on `std::env::args()`, not a new
dependency. There is one subcommand.

### 5.2 In-app rotation — authenticated, ergonomic

A control on the browser surface, available while signed in **by either method**,
regenerates the password and shows it once.

### 5.3 There is deliberately no unauthenticated setup page

A page that can create an admin whenever none exists is a takeover vector the
moment it re-arms — and it re-arms exactly when the table is empty, which is
also what a restore from backup, a fresh volume, or a botched migration looks
like. The CLI covers that state without ever exposing an unauthenticated write.

## 6. Data model (migration `0005_local_admin.sql`)

```sql
create table local_admin (
    id            boolean     primary key default true,
    username      text        not null,
    password_hash text        not null,      -- argon2id PHC string
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    last_login_at timestamptz,
    constraint local_admin_single_row check (id)
);
```

The boolean primary key with `check (id)` makes "at most one row" a schema
guarantee rather than application discipline: `id` can only be `true`, and the
primary key makes `true` unique.

Only the argon2id PHC string is stored — never the password, never a reversible
form. `updated_at` records rotations; `last_login_at` is stamped on success and
is the cheapest way to notice the break-glass credential being used when it
should not be.

## 7. Password generation

Always generated, never chosen: 24 characters from a cryptographically secure
RNG (the same `rand` already used for session tokens), shown exactly once at
creation and at every rotation.

A generated password of that length is arithmetically unguessable online, which
is what demotes rate limiting to a backstop rather than the primary control. It
also removes the worst failure mode — a memorable password on an endpoint that
grants a root shell on every machine in the fleet.

Rotation is the only way to change it. There is no "set this specific password"
path, because that path is the one someone reaches for under pressure during an
outage, and it is the weak one.

## 8. Login

`POST /auth/local` with a username and password, mounted alongside the other
public `/auth/*` routes.

On success it calls **the same `create_session` used by the OIDC callback**,
with:

```rust
Identity {
    subject: "local:admin".into(),
    email: None,
    display_name: Some("Local admin".into()),
}
```

The `local:` prefix namespaces the subject so it can never collide with a
provider's `sub`, no matter what that provider issues.

Reusing the session path is the point: the resulting session is an ordinary one,
so expiry, revocation, logout, the `/api/*` middleware and the audit trail all
behave identically. There is no second session concept to keep in step.

`last_login_at` is stamped on success.

## 9. Audit

`auth.login` and `auth.denied` as today, written through `audit_with_detail`
with `detail = {"method":"local"}`.

The existing OIDC path gains `{"method":"oidc"}` in the same change, so the
method is explicit on both rather than inferred from the absence of a field on
one. A one-line change that makes "show me every local-credential login" a
direct query.

## 10. Rate limiting

**A global limit only**, held in memory: a token bucket on the endpoint plus an
escalating delay on consecutive failures, rising to a cap of roughly 30 seconds.

### 10.1 Why not per-IP

Behind Traefik the peer address is the proxy's, so per-IP limiting either does
nothing or requires trusting `X-Forwarded-For` — a client-controlled header. An
attacker rotates it and the limit evaporates, having added a security dependency
on untrusted input. A global limit cannot be evaded that way, and there is
exactly one legitimate user to inconvenience.

### 10.2 No permanent lockout, ever

A hard lock would be a denial of service on the one credential that exists to
rescue the operator: an attacker unable to guess the password could still deny
the recovery path. An escalating-but-capped delay costs an attacker effectively
everything while costing the legitimate user seconds.

### 10.3 Why in memory

One replica, and an attacker cannot force the restart that would reset the
counter. A table would buy durability against a crash that nobody can induce,
at the cost of a write on every failed attempt.

Three layers stack here, and the order matters: the generated password makes
guessing hopeless, argon2id's ~100ms cost makes each attempt expensive, and the
global limit is the backstop.

## 11. Error handling

Wrong username, wrong password, and no admin configured return **one
indistinguishable failure**. When no row exists the handler still verifies
against a dummy hash, so response timing does not reveal whether a local admin
is configured at all.

## 12. UI

The sign-in view keeps SSO as the primary action, with a collapsed "Use a local
account" disclosure beneath it — present but not competing.

Rotation displays the new password once in a dialog with a copy control and an
explicit warning that it will not be shown again.

## 13. Dependency gate

`argon2` (RustCrypto) is pure Rust and pulls no OpenSSL and no cmake — but this
is re-verified in the real workspace before the code is written, exactly as
`openidconnect` was, because feature unification differs between a scratch crate
and this one. The agent crate must gain nothing.

## 14. Testing

**Unit:** argon2id hash/verify roundtrip; verify rejects a wrong password; the
dummy-hash path when no admin row exists; password generation length and
alphabet; the rate limiter's bucket refill, its delay cap, and that no input
sequence produces a permanent lock.

**Router (`oneshot`):** `POST /auth/local` with wrong credentials returns the
generic failure and sets no cookie; with correct credentials sets a session
cookie and the session resolves through the existing middleware.

**Boot rule:** OIDC absent with a local admin present boots; both absent refuses
to boot and the error names the CLI command.

**Database (`sqlx::test`):** the single-row constraint rejects a second insert;
rotation updates the hash and `updated_at`; `last_login_at` is stamped.

**Live:** create the account via the CLI with the server stopped, start the
server with the OIDC variables *unset*, log in with the generated password, and
confirm the audit row records `method=local`. That sequence is the whole feature
in one test — if it passes, the recovery path works.

## 15. Risks

- **The local credential becomes the routine login.** It is meant to be the
  exception; `last_login_at` plus the audit method field make usage visible.
- **A generated password is only as safe as where it is stored.** It is shown
  once, so it lands in a password manager or it is lost — losing it is recovered
  by rotation, which needs an existing session or host access.
- **Boot-rule regression.** The startup query is the only thing standing between
  "no auth configured" and a running control plane; its tests are not optional.
