# OIDC authentication — design

**Status:** design of record for the OIDC slice.
**Date:** 2026-07-25.
**Supersedes nothing.** Implements PRD §9.1's `Auth` block and §2.3's browser half.

---

## 1. Why now

Every browser surface in Argus is unauthenticated. That was a defensible call
while the app was read-mostly dashboards on a trusted LAN, and each slice
deferred auth on exactly that reasoning. The terminal slice (PR #10) changed the
facts: `/api/machines/:id/terminal` hands any client that can reach port 8080 a
**root PTY on every machine in the fleet**, with no credential of any kind. The
risk did not grow gradually — it changed in one commit.

This slice closes that. It is the last cross-cutting slice before the k8s
manifests, which CLAUDE.md schedules as the final deliverable.

## 2. Goal

A human proves who they are against an OIDC provider, holds a revocable
server-side session, and every verb they run is attributed to them in
`audit_log`. Agents are unaffected: they authenticate by mTLS and never touch
this path.

Revocation is immediate for *new* requests: the moment the session row is
deleted, the next `/api/*` request (or WebSocket/SSE upgrade) with that cookie
is rejected. It is **not** immediate for connections already established
before revocation -- see §7.1.

## 3. Non-goals

Each of these is deliberately excluded, not overlooked:

- **RBAC / per-verb permissions.** The PRD puts RBAC in V2. This slice is *one
  admission gate*: you are either allowed to operate the fleet or you are not.
- **Refresh tokens and silent renewal.** A session lives `SESSION_TTL_HOURS`
  (12), then you log in again.
- **Provider-side (RP-initiated) logout.** Logout is local: the session row is
  deleted and the cookie cleared. With SSO still active at the provider,
  clicking "Sign in" signs you straight back in. That is expected OIDC
  behaviour and will be documented rather than worked around — logging a user
  out of *every* application from Argus is a surprising side effect.
- **API tokens** for scripts or CI.
- **Any user-management UI.** Access is managed in the provider.
- **The agent surface.** Unchanged: mTLS on its own listener, never via the
  browser entrypoint (PRD §2.4, a decision that must not be reopened).

## 4. Provider-agnostic by construction

Argus supports **any spec-compliant OIDC provider**. The only provider-specific
input is the issuer URL; every endpoint (authorize, token, userinfo, JWKS) and
the supported client-authentication methods are read from
`<issuer>/.well-known/openid-configuration`. No provider name, endpoint or quirk
appears anywhere in the code.

Zitadel is the instance this slice is *verified against* (it is what the
maintainer runs), not a provider the code knows about.

### 4.1 The one genuinely non-standard part: roles

OIDC standardises identity, not authorisation. There is no roles claim in the
specification, and real providers disagree on both the **name** and the
**shape**:

| Provider | Claim | Shape |
|---|---|---|
| Keycloak | `realm_access.roles` | array of strings, nested one level |
| Authentik, Okta | `groups` | array of strings |
| Zitadel | `urn:zitadel:iam:org:project:roles` | **object** whose *keys* are the role names |
| Auth0 | namespaced custom claim | usually an array |

Two configuration values absorb that variance:

- `ARGUS_OIDC_ROLES_CLAIM` — a dot-path into the claims object (default
  `groups`). Dot-path, not a flat name, because Keycloak nests.
- `ARGUS_OIDC_REQUIRED_ROLE` — the role a user must hold.

The extractor is deliberately tolerant of the three shapes that actually occur:

1. JSON array of strings → the roles.
2. JSON object → its **keys** are the roles (Zitadel).
3. Space-delimited string → split on whitespace.

Anything else yields "no roles", which denies admission. This is a pure function
and is the single highest-value unit test in the slice (§11).

### 4.2 Claims are merged before the path is resolved

Providers disagree about whether roles ride in the ID token or only at the
userinfo endpoint. Argus fetches userinfo after the token exchange and **merges**
those claims over the ID token's, then resolves `ARGUS_OIDC_ROLES_CLAIM` against
the merged object. This removes a whole class of "works on Keycloak, silently
denies on Zitadel" failures.

`ARGUS_OIDC_SCOPES` (default `openid profile email`) is configurable because some
providers need an extra scope before they will emit roles at all.

## 5. Configuration

**Updated by the local-admin slice** (`2026-07-26-local-admin-design.md` §4):
OIDC as a whole is now optional — the control plane boots if OIDC is configured
*or* a local admin row exists. What follows describes the OIDC-specific
variables themselves, which remain all-or-nothing whenever OIDC is used at
all: either all four below are set, or none of them are, or the control plane
refuses to start naming the missing one.

| Variable | Meaning |
|---|---|
| `ARGUS_OIDC_ISSUER` | Issuer URL, e.g. `https://auth.example.com`. Must match the `iss` claim exactly — trailing-slash mismatches are the classic discovery failure. |
| `ARGUS_OIDC_CLIENT_ID` | Client ID of the Argus app registration. |
| `ARGUS_OIDC_CLIENT_SECRET` | Client secret (confidential client). |
| `ARGUS_OIDC_REQUIRED_ROLE` | Role required for admission, or the literal `any`. |

`ARGUS_PUBLIC_URL` is **required unconditionally, independent of OIDC** — it now
lives on `Config` rather than being one of the OIDC-specific variables above,
because it also decides the session cookie's `Secure` attribute for *every*
login method, local admin included (see §5.3 below and local-admin design §4).
It still feeds into the all-or-nothing check above when OIDC is being
configured at all: `Config::from_env` treats it as part of a complete OIDC
config once any of the four OIDC-specific variables is set, so a half-set OIDC
config (e.g. issuer set, secret forgotten) is still rejected rather than
silently treated as absent.

| Variable | Meaning |
|---|---|
| `ARGUS_PUBLIC_URL` | Externally reachable base URL, e.g. `https://argus.lab.example.com` or `http://localhost:8080`. Required in every deployment, OIDC or local-admin-only. |

Optional:

| Variable | Default | Meaning |
|---|---|---|
| `ARGUS_OIDC_ROLES_CLAIM` | `groups` | Dot-path to the roles claim. |
| `ARGUS_OIDC_SCOPES` | `openid profile email` | Space-delimited scopes. |
| `ARGUS_OIDC_CA_CERT` | *(unset)* | PEM path for an IdP behind an internal CA. Homelab providers frequently are. |

Session lifetime is a constant, not configuration: `SESSION_TTL_HOURS = 12` in
`argus-common`, beside `TERMINAL_IDLE_SECS`. It is a security property of the
product rather than a per-deployment knob, and the existing timeouts live there.

### 5.1 There is no "auth disabled" mode

No bypass flag, no dev-only skip branch, no unauthenticated fallback. Missing
*all* authentication configuration produces a server that **will not boot**,
never one that silently serves a root shell. Local development authenticates
against the real provider exactly as production does; the only accommodation
is registering `http://localhost:8080/auth/callback` and
`http://localhost:5173/auth/callback` as additional redirect URIs.

**Updated by the local-admin slice:** the rule above was written as "OIDC must
be configured", but what it actually guaranteed was "authentication must be
configured" — stating it the first way was an overspecification. The
local-admin design (§4) makes OIDC configuration optional, contingent on a
local admin row existing instead; with neither present the server still
refuses to boot, now naming the CLI command that creates one.

### 5.2 `ARGUS_OIDC_REQUIRED_ROLE=any` is explicit on purpose

To allow *any* authenticated user, the operator must type the literal `any`.
Leaving the variable unset does **not** mean "allow everyone" — it means the
server does not start.

Unset-means-open is precisely how a homelab application ends up serving root
shells to the internet: the operator who forgets a variable gets the least safe
behaviour. Making the open mode a value you have to write means the safe path is
the default and the unsafe one is a conscious act. Providers with no roles
concept are still fully supported; you simply have to say so.

### 5.3 `ARGUS_PUBLIC_URL` decides the cookie's `Secure` attribute

`https` → `Secure` is set; `http` → it is not. This is derived from how the
deployment is reachable, not a toggle: it can weaken a cookie flag on localhost,
and can never disable authentication. It also builds the `redirect_uri`, which
must be exact — deriving it from `Host`/`X-Forwarded-Proto` headers means
trusting values a client controls.

**Updated by the local-admin slice:** this decision does not belong to `OidcConfig`
any more, precisely because it is not OIDC-specific — a local admin login sets
a session cookie in states where no `OidcConfig` exists at all. `ARGUS_PUBLIC_URL`
now lives on `Config` (§5 above) and is read from there for the `Secure`
attribute; `OidcConfig` keeps its own copy of the same value only to build
`redirect_uri`.

## 6. Architecture

One new module, `crates/server/src/auth.rs`, owning the OIDC client, the session
store and the axum middleware. Three small edits elsewhere:

- `config.rs` — the settings above, validated at startup.
- `http.rs` — mount `/auth/*` and `/api/me`, apply the middleware layer.
- `jobs.rs` — delete expired sessions on the existing sweeper tick.

If `auth.rs` grows past roughly 400 lines it splits into `auth/oidc.rs` (client +
claims) and `auth/session.rs` (store + middleware); the boundary is already clean.

### 6.1 Crate choice, and the build-time validation gate

Intended: **`openidconnect`** (discovery, JWKS rotation, ID-token validation,
nonce and PKCE handling are all things not worth hand-rolling), with `reqwest`
configured `default-features = false` plus `rustls-tls-native-roots` and `json`.

**Gate — before any auth code is written**, confirm on the pinned versions that:

1. The dependency tree pulls **no OpenSSL and no cmake**, and `rustls` resolves
   to the **`ring`** provider. CLAUDE.md: `ring` everywhere, never `aws-lc-rs`.
2. `cargo build -p argus-agent --target x86_64-unknown-linux-musl` still
   produces a static binary. The agent must not gain this dependency at all —
   it is a server-only crate — and CI's `agent-musl` job proves it.

If `openidconnect` fails the gate, fall back to `oauth2` + `jsonwebtoken` with a
hand-rolled JWKS cache. This mirrors the apalis gate CLAUDE.md already defines:
a dependency is not adopted until its constraints are checked against reality.

## 7. Data model

### 7.1 `sessions` (migration `0004_sessions.sql`)

```sql
create table sessions (
    token_hash   bytea primary key,          -- sha256 of the cookie value
    subject      text        not null,       -- OIDC `sub`; stable identity
    email        text,
    display_name text,
    created_at   timestamptz not null default now(),
    expires_at   timestamptz not null
);

create index sessions_expires_at_idx on sessions (expires_at);
```

The cookie carries 32 cryptographically random bytes, base64url-encoded. **Only
its SHA-256 is stored**, so a database read — a backup, a replica, a leaked dump
— yields no usable session tokens. This is the same convention
`enrollment_tokens.token_hash` already uses; following it keeps one rule for
secret-shaped values in this schema.

Sessions are server-side and therefore **revocable**: logout deletes the row,
and the credential is dead immediately *for new requests* -- the next `/api/*`
call, or the next WebSocket/SSE *upgrade*, presenting that cookie is rejected.
They also survive the `Recreate` rollout CLAUDE.md mandates, because the
control plane keeps state in Postgres.

**Revocation does not reach a connection already established before it.**
`AuthUser` is resolved once, at WebSocket upgrade (`terminal.rs`) or SSE
stream open (`log_stream` in `http.rs`), and neither re-checks the session
afterwards. A revoked session therefore leaves any already-open terminal
WebSocket or log SSE stream live -- for the terminal, that is a root PTY --
until it closes on its own (shell exit, idle timeout, tab switch, or the
client disconnecting). Because the idle timer resets on activity and
`SESSION_TTL_HOURS` is not applied to established sockets, an attacker who
keeps a terminal busy can hold a revoked session's PTY indefinitely. This is a
known, deliberately deferred gap, not an oversight: periodic re-validation of
long-lived connections against the `sessions` table is recorded as the first
follow-up in §15.

### 7.2 The in-flight login is *not* a table

`state`, `nonce` and the PKCE verifier ride in a short-lived cookie sealed with
the existing `FieldCipher` (AES-256-GCM, `crypto.rs`). It is a ten-minute
pre-auth artifact that expires by itself; revocability is meaningless for it, so
it earns neither a table nor a sweeper.

## 8. Flow

```
any /api/* request ─▶ no or invalid cookie ─▶ 401 {"error":"unauthenticated"}
SPA sees 401       ─▶ renders the sign-in view ─▶ link to /auth/login

GET /auth/login     ─▶ generate state + nonce + PKCE verifier
                       seal them (and the return path) into the flow cookie (10 min)
                       302 → provider authorize endpoint

GET /auth/callback  ─▶ verify state against the flow cookie
                       exchange code + PKCE verifier for tokens
                       validate ID token: signature (JWKS), iss, aud, exp, nonce
                       fetch userinfo, merge claims
                       resolve ARGUS_OIDC_ROLES_CLAIM, check ARGUS_OIDC_REQUIRED_ROLE
                       ─▶ insert session row, set cookie, clear flow cookie,
                          302 → the return path

GET  /api/me        ─▶ 200 { subject, email, display_name }, or 401 when
                       unauthenticated — it is under /api/*, so the middleware
                       answers it. That 401 is exactly how the SPA detects a
                       signed-out state; it is not an error path.
POST /auth/logout   ─▶ delete session row, clear cookie, 204. The SPA then
                       refetches /api/me, gets 401 and renders the sign-in view.
```

### 8.0 Returning to where you were

`/auth/login` accepts `?next=<path>` and the sign-in view passes the current
location, so a session that expires deep in a machine's terminal tab returns
there rather than to the fleet page. The value is carried inside the **sealed
flow cookie**, not round-tripped through the provider.

It is accepted only if it starts with a single `/` and not `//` — anything else
falls back to `/`. Skipping that check is the textbook open-redirect: a link to
`/auth/login?next=https://evil.example` would otherwise bounce an authenticated
operator off-site, and it would look like a working login the whole way.

### 8.1 What the middleware protects

**Protected — everything under `/api/*`.** That deliberately includes the SSE log
streams and the terminal WebSocket: cookies ride the upgrade request, so a single
layer covers all three transports with no per-transport special casing.

**Public:** `/healthz`, `/readyz` (PRD §9.1 places infra outside OIDC),
`/auth/*`, and the static SPA bundle.

Serving the JavaScript bundle unauthenticated is deliberate and standard. It is
an empty shell; every byte of fleet data sits behind `/api`. Gating the bundle
would buy nothing and would break the sign-in view, which has to render *before*
there is a session.

### 8.2 CSRF

`SameSite=Lax` prevents a cross-site POST from carrying the session cookie, which
covers the verb endpoints. `HttpOnly` keeps the cookie away from JavaScript.

`SameSite` does not govern WebSocket upgrades — which is exactly what the
`Origin` check added in the terminal slice is for. That check was defensive when
written; **this slice is what makes it load-bearing**, because until now there
was no ambient credential for a cross-origin page to abuse.

## 9. Audit identity: make `"anonymous"` unrepresentable

Four call sites currently pass the literal `"anonymous"`: `terminal.rs`
(`terminal.open`), `http.rs` (`logs.open`, `logs.page`, and the shared verb
actor). Four further occurrences are test fixtures.

Rather than substitute a different string, the audit helpers take a closed enum:

```rust
pub enum Actor<'a> {
    User(&'a Identity),   // a signed-in human
    Agent(Uuid),          // agent.online, and enrollment keyed by machine
    System,               // no principal — e.g. a denied enrollment
}
```

All three variants are already live: `grpc.rs` writes a `"system"` actor when an
enrollment is denied (there is no machine yet to attribute it to), so no variant
is introduced speculatively.

`repo::audit` and `repo::audit_command` accept `Actor` instead of `&str`. A
browser-initiated verb can then only be recorded by producing an `Identity`,
which only the auth middleware can mint — so "forgot to wire the actor through"
becomes a **compile error** rather than a plausible-looking audit row. It also
gives the agent-side rows an honest name instead of informally sharing a field.

The actor string is the email when present, else the subject, per PRD §7. Email
is for reading; `sub` is the stable identity, and the session row carries both.

The flow writes its own audit rows from day one, as CLAUDE.md requires of every
verb: `auth.login`, `auth.denied` (carrying the rejected subject — a denial
nobody can see is a support problem), and `auth.logout`.

## 10. Availability: the IdP must not be able to take the fleet down

Configuration *presence* is validated at boot. **Discovery is lazy, cached and
retried** — never fetched during startup.

Agents authenticate by mTLS and have nothing to do with OIDC. If the provider is
down, slow or mid-restart when Argus starts, the agent gRPC listener, heartbeats,
metrics ingestion and the health endpoints must all come up regardless; only
browser *login* degrades. Fetching discovery at startup would couple the entire
fleet's connectivity to an unrelated service being reachable at one specific
moment, which is a far worse failure than "you cannot log in for thirty seconds".

The JWKS document is cached with the same posture, refreshed on an unknown key
ID so provider key rotation resolves itself without a restart.

## 11. Testing

The genuinely testable parts are pure, which is a design goal rather than a
coincidence.

**Unit (no IdP, no database):**
- The roles extractor, table-driven over **real claim blobs in all four provider
  shapes** from §4.1 — this is what makes "generic OIDC" verified rather than
  merely asserted. Includes the empty, wrong-type and missing-path cases, each of
  which must deny.
- Claim merging (userinfo overriding the ID token).
- Dot-path resolution, including a missing intermediate node.
- Cookie attribute construction for both `http` and `https` public URLs.
- Session token generation and hashing.

**Router (`axum` `oneshot`, no IdP):** no cookie → 401; unknown cookie → 401;
expired session row → 401; valid session → 200; every public route → 200 while
unauthenticated. This covers each route class in §8.1.

**Database (`sqlx::test`):** session insert, lookup-by-hash, expiry boundary,
delete-on-logout, and the sweeper deleting only expired rows.

**`/auth/login`:** returns 302 with `state`, `nonce` and a PKCE challenge
present, and sets a sealed flow cookie.

What cannot be unit-tested is the real token exchange, so the flow shell stays
thin and is verified live (§12).

## 12. Live verification (real provider, recorded in `docs/DEV.md`)

1. Log in through the real provider; land back on the fleet page signed in.
2. `GET /api/me` returns the expected subject and email.
3. Run a container or unit verb; the `audit_log` row carries the **email**, not
   `anonymous`.
4. Open a terminal — the WebSocket upgrade carries the cookie and succeeds.
5. Sign out; `/api/fleet` then 401s and the SPA shows the sign-in view.
6. Tamper with a cookie value → 401.
7. `UPDATE sessions SET expires_at = now() - interval '1 hour'` → next request
   401s, proving expiry is enforced server-side and not merely by cookie age.
8. Set `ARGUS_OIDC_REQUIRED_ROLE` to a role the account lacks → login is denied
   with the explicit role message, and an `auth.denied` row appears.
9. Stop the provider and restart Argus → agents still connect and heartbeat;
   only login is unavailable. This is §10's guarantee, and it is the one most
   likely to regress silently.

## 13. Frontend

- `/api/me` behind TanStack Query.
- A shared fetch wrapper maps any 401 to a typed `Unauthenticated` error; the app
  shell renders a full-page sign-in view in place of the routes. One gate, not a
  guard per route.
- The sign-in view is a single button setting `window.location = "/auth/login"`.
  No SPA routing is involved, because the flow leaves the SPA entirely.
- Signed-in identity and a Sign out control sit in the sidebar footer beside
  `ThemeToggle`.
- The vite dev proxy gains `/auth`; `/api` keeps `ws: true` (the trap from the
  terminal slice — the string shorthand does not forward upgrades).
- Session expiry needs no special handling: the next `/api` call 401s and the app
  flips to the sign-in view. A rejected terminal upgrade closes the socket, and
  the blur overlay added in PR #10 surfaces the reason.

## 14. Error handling

Callback failures — state mismatch, expired or missing flow cookie, failed token
exchange, invalid ID token — render a small error page with a retry link, with
detail logged server-side rather than returned to the browser.

**Role denial is the deliberate exception**: it states plainly that the account
lacks the required role. A generic "login failed" there is undiagnosable and
sends the operator hunting through code for what is really a one-line
configuration mismatch between `ARGUS_OIDC_ROLES_CLAIM` and their provider.

## 15. Risks

- **Established WebSocket/SSE connections outlive a revoked session (first
  follow-up).** As §7.1 details, `AuthUser` is resolved once at upgrade time;
  a terminal WebSocket or a log SSE stream opened before logout (or before
  expiry, or before a role change at the provider) keeps running until it
  closes for an unrelated reason. The first follow-up to this slice is
  periodic re-validation of long-lived connections against the `sessions`
  table (e.g. re-checking on the existing idle/keepalive tick), so a
  revocation reaches an already-open root PTY within one tick instead of only
  at natural connection close.
- **The roles claim is the integration risk, not the protocol.** Providers must
  often be configured to emit roles at all. Mitigation: the first implementation
  task dumps the real merged claims for the maintainer's provider and sets
  `ARGUS_OIDC_ROLES_CLAIM` from observed fact, not documentation.
- **Cookies and `Secure` on plain-HTTP localhost.** Browsers treat `localhost` as
  a trustworthy origin, but the interaction is subtle enough that §5.3 derives
  the attribute from `ARGUS_PUBLIC_URL` instead of assuming.
- **Existing sessions across a deploy.** Rows survive; the field key does not
  encrypt them, so a `ARGUS_FIELD_KEY` rotation invalidates only in-flight
  logins, not established sessions.
