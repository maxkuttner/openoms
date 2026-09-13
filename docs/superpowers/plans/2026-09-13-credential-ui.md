# Credential UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Configure broker and market-data credentials from the cockpit instead of a file — enter them, test them, see what is set without seeing the secret, and have the change apply without a restart.

**Architecture:** Three write endpoints on the existing admin router (`PUT`/`DELETE` credentials, `POST` test), returning the `Redacted` view Plan 2 built and nothing else has consumed. A credentials panel on the existing `BrokerConnections` page and a rewritten `DataFeeds` page, both using the cockpit's existing `api`/`useApiMutation` layer. A `GET /admin/setup-status` checklist answers "what is left to do", which nothing in the system currently can.

**Plan 4 of 4** — the last for this spec, and the one the user originally asked for. Plans 1-3 are merged: `oms.toml` bootstrap, the encrypted store, and live reload.

**Tech Stack:** Rust 2021, axum, sqlx 0.8, utoipa; cockpit is React + Mantine + TanStack Query + Vite.

**Spec:** `docs/superpowers/specs/2026-08-20-connection-config-gui-design.md`

## Global Constraints

- **Rust edition 2021.** Doc comments explain *why*, not *what*.
- **A plaintext secret must never leave the server.** Every read path returns `credentials::Redacted` — secret fields present by name with no value, non-secret fields visible. A configuration UI that hands API secrets back to a browser is a mistake regardless of how well they are stored.
- **Test before save is the gate, not a convenience.** The order is decrypt → build adapter → test → persist → reload. A failed test must leave both the database and the running registry untouched.
- **Never echo a submitted secret**, not in a response, not in a validation error, not in a log.
- **Renaming a connection orphans its credentials** — the connection `code` is the AES-GCM associated data. This plan must not add a rename path. **Verified: `UpdateBrokerConnection` has no `code` field, so the existing `PATCH` cannot rename.** Keep it that way.
- **A save applies via the existing reload**, which is serialized and already handles FIX correctly. Do not add a second reload path.
- **Tests needing Postgres are `#[ignore]`d**, matching `credentials.rs` and `reload_tests.rs`.
- **Never call `config::load()` or write an `oms.toml` from a test**; the repo root has a real one.
- **Do not run the server or connect to a database from a task.** `cargo run` loads the repo's `.env`, which points at the user's live production database. The controller does live checks.

---

## What already exists (verified — do not rebuild)

| Piece | Where | State |
|---|---|---|
| `Redacted`, `Redact`, per-variant redaction | `src/credentials.rs` | Built in Plan 2, **zero consumers** — this plan is its first |
| `save_broker`, `save_feed`, `load_brokers`, `load_feeds` | `src/credentials.rs` | Working, `#[ignore]`-tested against Postgres |
| `POST /admin/connections/reload` | `src/admin.rs` | Serialized, FIX-aware, returns a per-connection report |
| Broker connection CRUD | `src/admin.rs`, `/admin/broker-connections` | Create/list/get/update all exist |
| `CrudResource` generic table + edit modal | `cockpit/src/components/CrudResource.tsx` | `BrokerConnections` already uses it with `editable` |
| `api.get/post/put/patch/del`, `useList`, `useApiMutation`, `notifyOk/notifyError` | `cockpit/src/api/` | The whole data layer is in place |

**The cockpit's connection pages are not read-only.** `BrokerConnections` is already an editable CRUD table; what is missing is specifically credentials. `DataFeeds` is genuinely read-only and gets rewritten.

---

## File Structure

**Create:**
- `src/credentials_api.rs` — request shapes, the parse-into-`BrokerCredentials` step, and the connection test. Kept out of `admin.rs`, which is already ~1800 lines.
- `cockpit/src/components/CredentialsPanel.tsx` — the per-connection credential form, used by both pages.

**Modify:**
- `src/credentials.rs` — a `Redacted` accessor for a single connection.
- `src/admin.rs` — the three handlers plus `setup_status`.
- `src/main.rs` — routes and utoipa registration.
- `cockpit/src/api/types.ts` — the new response types.
- `cockpit/src/pages/BrokerConnections.tsx` — mount the panel.
- `cockpit/src/pages/DataFeeds.tsx` — rewrite as configurable.
- `readme.md`.

---

### Task 1: The redacted read path

**Files:**
- Modify: `src/credentials.rs`, `src/admin.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  - `GET /admin/broker-connections/:code/credentials` → `200` `RedactedCredentials`, `404` if no such connection
  - `pub struct RedactedCredentials { pub code: String, pub state: String, pub fields: Vec<RedactedField>, pub updated_at: Option<DateTime<Utc>> }`
  - `pub struct RedactedField { pub name: String, pub value: Option<String>, pub secret: bool }`

`state` is `"configured" | "unconfigured" | "error"`, mirroring `CredentialState` so the UI can distinguish "needs setup" from "stored but unusable" — a distinction the whole store was built to preserve.

- [ ] **Step 1: Write the failing tests**

In `src/credentials.rs`'s existing test module — `Redact` is already implemented, so this tests the *wire shape*:

```rust
    /// The wire form must mark which fields are secret, so the UI can render a
    /// "leave blank to keep" input rather than an empty text box that looks like
    /// the value was lost.
    #[test]
    fn the_wire_form_marks_secret_fields() {
        let fields = super::redacted_fields(&alpaca());
        let secret: Vec<_> = fields.iter().filter(|f| f.secret).map(|f| f.name.as_str()).collect();
        assert_eq!(secret, vec!["secret"], "only the secret is secret; the key id is shown");

        let shown = fields.iter().find(|f| f.name == "key").expect("key present");
        assert!(shown.value.is_some(), "the key id must be visible so the operator can tell which is installed");
    }

    /// A secret field must never carry a value on the wire — this is the whole
    /// point of the type.
    #[test]
    fn no_secret_field_carries_a_value() {
        for creds in [alpaca(), binance(), ibkr()] {
            for f in super::redacted_fields(&creds) {
                if f.secret {
                    assert!(f.value.is_none(), "{} leaked a value", f.name);
                }
            }
        }
        for f in super::redacted_fields_feed(&FeedCredentials::Databento { api_key: "db-key".into() }) {
            assert!(f.value.is_none() || !f.secret);
        }
    }

    /// Every field of every variant must appear — a field silently missing from
    /// the wire form is a field the UI cannot offer to set.
    #[test]
    fn every_field_appears_on_the_wire() {
        let names: Vec<_> = super::redacted_fields(&ibkr()).into_iter().map(|f| f.name).collect();
        for expected in ["host", "port", "sender_comp_id", "target_comp_id", "password", "ssl"] {
            assert!(names.contains(&expected.to_string()), "{expected} missing from {names:?}");
        }
    }
```

- [ ] **Step 2: Run them, watch them fail, then implement**

Add `redacted_fields(&BrokerCredentials) -> Vec<RedactedField>` and `redacted_fields_feed(&FeedCredentials) -> Vec<RedactedField>` to `src/credentials.rs`, built from the existing `Redact` impl — `Redacted.fields` is already `Vec<(String, Option<String>)>` where `None` means secret, so this is a mapping, not new redaction logic. Do **not** write a second redaction path; if the existing one is inconvenient, adapt it and say so.

- [ ] **Step 3: The handler**

In `src/admin.rs`, load the connection via `credentials::load_brokers` filtered by code (or a single-row query in the same shape), resolve the master key the way `reload_connections` does, and map `CredentialState` to `state` + `fields`. `Error` returns `state: "error"` with the reason in a `message` field — never the payload.

404 when the connection row does not exist. A connection with no credentials is **200** with `state: "unconfigured"`, not 404 — it exists, it just needs setup.

- [ ] **Step 4: Register the route and utoipa**

Beside the other `/admin/broker-connections/:code` routes, inside the auth layer. Add to `ApiDoc`'s `paths(...)` and the response types to `components(schemas(...))` — the previous plan shipped an annotation that documented nothing because it was never registered.

- [ ] **Step 5: Verify and commit**

`cargo test`, `cargo clippy --bin oms`.

```bash
git add src/credentials.rs src/admin.rs src/main.rs
git commit -m "feat(admin): serve the redacted credential view

Redacted was built in Plan 2 and has had no consumer until now. Secret
fields go on the wire by name with no value so the UI can render
\"leave blank to keep\" rather than an empty box that looks like data loss;
non-secret fields stay visible so an operator can see which key is
installed without being handed it."
```

---

### Task 2: Parsing and testing a submitted credential

**Files:**
- Create: `src/credentials_api.rs`
- Modify: `src/main.rs` (add `mod credentials_api;`)

**Interfaces:**
- Produces:
  - `pub struct CredentialSubmission { pub fields: HashMap<String, String> }`
  - `pub fn parse_broker(kind: &str, existing: Option<&BrokerCredentials>, sub: &CredentialSubmission) -> Result<BrokerCredentials, CredentialError>`
  - `pub async fn test_broker(creds: &BrokerCredentials) -> Result<(), String>`
  - `pub enum CredentialError { UnknownKind(String), MissingField(&'static str), BadField { name: &'static str, why: String } }`

**The merge rule is the subtle part.** A submission may omit a secret field, meaning "keep what is stored". `parse_broker` takes the existing decrypted credential for exactly that. Omitting a secret when nothing is stored is `MissingField`. A submitted empty string means "keep", not "set empty" — an empty API secret is never a real value, and treating it as one would silently break a working connection.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::BrokerCredentials;

    fn sub(pairs: &[(&str, &str)]) -> CredentialSubmission {
        CredentialSubmission {
            fields: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn existing_alpaca() -> BrokerCredentials {
        BrokerCredentials::Alpaca { key: "OLDKEY".into(), secret: "OLDSECRET".into() }
    }

    #[test]
    fn a_full_submission_replaces_everything() {
        match parse_broker("ALPACA", None, &sub(&[("key", "NEWKEY"), ("secret", "NEWSECRET")])).expect("parse") {
            BrokerCredentials::Alpaca { key, secret } => {
                assert_eq!(key, "NEWKEY");
                assert_eq!(secret, "NEWSECRET");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// The core of the edit experience: change the key id, leave the secret
    /// blank, keep the stored secret. Without this an operator cannot edit any
    /// non-secret field without re-typing the secret they cannot see.
    #[test]
    fn an_omitted_secret_keeps_the_stored_one() {
        match parse_broker("ALPACA", Some(&existing_alpaca()), &sub(&[("key", "NEWKEY")])).expect("parse") {
            BrokerCredentials::Alpaca { key, secret } => {
                assert_eq!(key, "NEWKEY");
                assert_eq!(secret, "OLDSECRET", "the stored secret must survive an omission");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// An empty string is an omission, not a value. Treating "" as "set the
    /// secret to empty" would silently break a working connection.
    #[test]
    fn an_empty_secret_is_treated_as_omitted() {
        match parse_broker("ALPACA", Some(&existing_alpaca()), &sub(&[("key", "NEWKEY"), ("secret", "")])).expect("parse") {
            BrokerCredentials::Alpaca { secret, .. } => assert_eq!(secret, "OLDSECRET"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_omitted_secret_with_nothing_stored_is_an_error() {
        assert!(matches!(
            parse_broker("ALPACA", None, &sub(&[("key", "NEWKEY")])),
            Err(CredentialError::MissingField("secret"))
        ));
    }

    #[test]
    fn an_unparseable_port_is_a_field_error_not_a_panic() {
        let r = parse_broker("IBKR", None, &sub(&[
            ("host", "h"), ("port", "not-a-number"), ("password", "p"),
        ]));
        assert!(matches!(r, Err(CredentialError::BadField { name: "port", .. })));
    }

    #[test]
    fn an_unknown_broker_kind_is_rejected() {
        assert!(matches!(parse_broker("NASDAQ", None, &sub(&[])), Err(CredentialError::UnknownKind(_))));
    }

    /// THE regression that matters: an error message is shown to a user and
    /// logged. It must never contain what they submitted.
    #[test]
    fn no_error_message_echoes_a_submitted_value() {
        let r = parse_broker("IBKR", None, &sub(&[
            ("host", "h"), ("port", "SUPERSECRETVALUE"), ("password", "SUPERSECRETVALUE"),
        ]));
        let msg = format!("{:?}", r.unwrap_err());
        assert!(!msg.contains("SUPERSECRETVALUE"), "submitted value leaked into {msg}");
    }
}
```

- [ ] **Step 2: Run them, watch them fail, then implement `parse_broker`**

One arm per `BrokerCredentials` variant. Non-secret fields (`host`, `port`, comp IDs, `ssl`, Alpaca's `key`) are required on a create and may be carried from `existing` on an edit. Secret fields (`secret`, `password`, `api_key`, `private_key`) follow the merge rule above.

**`BadField.why` must describe the problem, never quote the input** — the test enforces it.

- [ ] **Step 3: Implement `test_broker`**

Per variant, and honest about cost:

- **Alpaca** — `GET /v2/account` with the submitted credential. Sub-second. `AlpacaAdapter` exists; check whether it already has a suitable call before adding one.
- **IBKR / Binance FIX** — there is no cheap test; the only real one is attempting a logon. **Do not attempt it.** Return `Ok(())` with the understanding that FIX is validated at session start, and make the API surface say so (Task 3 returns `tested: false` for these). A fake "passed" for an untestable credential is worse than an honest "not tested".
- **Databento** — an auth handshake if the client offers one cheaply; otherwise the same honest not-tested answer.

Say in the doc comment why FIX is not tested — it is the same reason FIX is not reloadable.

- [ ] **Step 4: Verify and commit**

`cargo test`, `cargo clippy --bin oms`.

---

### Task 3: The write endpoints

**Files:**
- Modify: `src/admin.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  - `PUT /admin/broker-connections/:code/credentials` → `200 { redacted, tested, reload }`
  - `DELETE /admin/broker-connections/:code/credentials` → `200` redacted (now `unconfigured`)
  - `POST /admin/broker-connections/:code/credentials/test` → `200 { tested, ok, message }` — tests what is **stored**, changing nothing

**The order is the requirement:** decrypt existing → parse merged → test → persist → reload. A failed test returns **422** with the broker's own message and **must not** write or reload.

- [ ] **Step 1: Write the failing tests**

The handler needs a database, so test the decision logic as a pure function plus one `#[ignore]`d round trip:

```rust
    /// A failed test must stop before the write. This is the gate the whole
    /// feature rests on: a credential that cannot authenticate must never
    /// replace one that can.
    #[test]
    fn a_failed_test_blocks_the_write() {
        assert!(!should_persist(&Err("401 unauthorized".into())));
        assert!(should_persist(&Ok(())));
    }

    /// A save response must carry the redacted view, never the submission.
    #[test]
    fn the_save_response_carries_no_submitted_secret() {
        let body = serde_json::to_string(&SaveResponse {
            redacted: RedactedCredentials {
                code: "alpaca-paper".into(),
                state: "configured".into(),
                fields: vec![RedactedField { name: "secret".into(), value: None, secret: true }],
                updated_at: None,
            },
            tested: true,
            reload: None,
        })
        .expect("serialize");
        assert!(!body.contains("SUPERSECRET"));
        assert!(body.contains("alpaca-paper"));
    }
```

Then an `#[ignore]`d test in the style of `credentials.rs`'s round trip: PUT-equivalent save → read back redacted → assert `state == "configured"` and no secret value present.

- [ ] **Step 2: Implement**

`reload` in the response is the outcome for **this** connection taken from the existing reload report — so the UI can say "applied" or "restart required" without a second call. Call `reload_connections`' underlying function rather than duplicating it; if it is not factored for reuse, factor it, and say so in the report.

A `DELETE` must also reload — removing a credential should disarm the adapter, and leaving it live would be the same silent-mismatch class this project has hit repeatedly.

- [ ] **Step 3: Register routes + utoipa, verify, commit**

---

### Task 4: `GET /admin/setup-status`

**Files:**
- Modify: `src/admin.rs`, `src/main.rs`

Returns the checklist the spec describes:

```jsonc
{"database": "ok",
 "admin_password": "default",
 "master_key": "configured",
 "brokers":  [{"code": "alpaca-paper", "state": "configured"}],
 "feeds":    [{"code": "databento-opra", "state": "unconfigured"}],
 "catalog":  {"instruments": 0, "state": "empty"},
 "portfolios": 0}
```

- [ ] **Step 1: Write the failing tests**

Test the classification, which is pure:

```rust
    #[test]
    fn catalog_state_reflects_the_count() {
        assert_eq!(catalog_state(0), "empty");
        assert_eq!(catalog_state(14_000), "ok");
    }

    /// The checklist exists to tell an operator what is left. A default admin
    /// password must be reported, because it is the one thing a fresh install
    /// has that a finished one must not.
    #[test]
    fn a_default_admin_password_is_reported() {
        assert_eq!(admin_password_state("openoms-dev"), "default");
        assert_eq!(admin_password_state("something-else"), "set");
    }
```

Import `DEFAULT_ADMIN_PASSWORD` rather than repeating the literal.

- [ ] **Step 2: Implement, register, verify, commit**

Counts come from `instrument` and `oms.portfolio`. Reuse `credentials::load_brokers`/`load_feeds` for the connection states so this cannot disagree with what the reload path sees.

---

### Task 5: Validate `broker_code` on write

**Files:** `src/admin.rs`, plus a test.

A pre-existing hazard that this plan makes materially more reachable, because Task 6 puts a credentials panel beside the field that triggers it.

- The order path resolves an adapter with `registry.get(&broker_code, &environment)` (`src/handlers.rs:663`) — keyed by the **database's** `broker_code`.
- Registration hardcodes the canonical literal: `registry.register("IBKR", ...)`, `registry.register("BINANCE", ...)` (`src/reload.rs`).
- The cockpit exposes `broker_code` as an **editable, required** field (`cockpit/src/pages/BrokerConnections.tsx`), and `UpdateBrokerConnection` accepts it.

So an operator who edits `broker_code` from `BINANCE` to `binance` gets no error at save, no error at reload, and a silent 503 on the next order — the adapter is registered under one key and looked up under another. `broker_code` is unconstrained `TEXT`: no CHECK, no FK.

**Fix: validate on write.** Reject a `broker_code` outside the known set in both `create_broker_connection` and `update_broker_connection`, with a 400 naming the accepted values. Derive the set from one place — `Broker::ALL` plus the FIX-only brokers that have no `Broker` variant — rather than a second hardcoded list that can drift from the registration literals.

- [ ] **Step 1: Write the failing test** — a canonical code is accepted; a differently-cased or unknown one is rejected with 400; the error names the accepted values but does not echo arbitrary user input back verbatim beyond the offending code itself.
- [ ] **Step 2: Implement, verify, commit.**

Do **not** change the order path or the registration keys — that is a wider refactor and this validation closes the reachable hole.

---

### Task 6: The cockpit credentials panel

**Files:**
- Create: `cockpit/src/components/CredentialsPanel.tsx`
- Modify: `cockpit/src/api/types.ts`, `cockpit/src/pages/BrokerConnections.tsx`

**This is the deliverable the user asked for at the start.** Read `CrudResource.tsx` and `api/hooks.ts` first and follow their patterns — `useList`, `useApiMutation`, `notifyOk`/`notifyError`, Mantine components. Do not introduce a new data-fetching approach.

The panel, given a connection code and kind:

- Fetches the redacted credentials and renders one input per field.
- **A secret field renders as a password input with placeholder "leave blank to keep"** when `state === "configured"`, and as a required field when `unconfigured`. This is the single most important interaction detail: an operator must be able to change a host without re-typing a secret they cannot see.
- **Test** calls the test endpoint and shows the result. When the API reports `tested: false` (FIX), say "not testable before save" rather than implying success.
- **Save** calls `PUT`, then surfaces the returned reload outcome: "applied" or "needs a restart".
- **Clear** calls `DELETE`, behind a confirmation.
- On `state === "error"`, show the reason prominently — that is the "stored but the key does not match" case, and it is actionable.

Mount it in `BrokerConnections.tsx`. That page already uses `CrudResource` with `editable`, so add the panel per row (an expanded row or a drawer) rather than rewriting the page.

- [ ] **Steps: types → panel → mount → `npm run build` → commit**

There is no frontend test setup in this repo; do not add one. Verify by building and by reading the component against the API's actual response shape.

---

### Task 7: Data feeds page + docs

**Files:**
- Modify: `cockpit/src/pages/DataFeeds.tsx`, `readme.md`

`DataFeeds.tsx` is genuinely read-only today and its own comment says so. Give it the same credentials panel for `feed_connection` rows, keeping the existing stream-health strip and coverage table.

Then document the flow in `readme.md`: configure credentials in the cockpit, test, save, and the change applies immediately for Alpaca and Databento while FIX needs a restart. **Verify every claim against the code** — several earlier tasks in this project shipped documentation that described intent rather than behaviour.

Note what is still environment-only: Kafka and OpenFIGI.

---

## Verification

```bash
cargo build && cargo test && cargo clippy --bin oms
cd cockpit && npm run build
```

End-to-end, by the controller, against a throwaway Postgres:

1. Start with an unconfigured Alpaca connection; the panel shows "needs setup".
2. Enter a deliberately wrong key; **Test** fails and **Save** is refused with the broker's message; the store is unchanged.
3. Enter a real key; Test passes; Save persists and the response says applied.
4. Re-read: the key id is visible, the secret is not.
5. Edit the key id with the secret left blank; the stored secret survives.
6. Clear; the adapter is disarmed and `setup-status` shows `unconfigured`.
7. A FIX connection reports `tested: false` and, on save, "restart required".

## Out of scope

- **Kafka and OpenFIGI credentials** — same shape, deliberately deferred since Plan 2.
- **User accounts.** `credentials_updated_by` stays null; the cockpit is still one shared admin password.
- **Renaming a connection.** The `code` is the AES-GCM AAD, so a rename orphans the credential. This plan adds no rename path.
- **A first-run wizard.** `setup-status` returns the data; sequencing it into a guided flow is a separate piece of work.
