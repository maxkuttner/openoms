# Access grants (Open WebUI style)

## Intent

Admin access should work like Open WebUI's permissions and groups, using the word
**grant**: default grants every user gets, groups that add grants on top, and
per-portfolio access lists of groups or single people. Approved design: canvas boards
"Admin · Groups & grants" and "Admin · Portfolio access"
(https://claude.ai/artifact/9fAqHFNfqDFQHRuhUAZYeR).

## Decided

- Option 1: grants are **switches for what a person may use**; portfolios get an
  **access list** (groups or people, View / Trade / Manage, highest wins). Venues stay a
  property of the portfolio (built: `trading_grant`).
- Groups stay under **People**. The switch list as shown is approved.
- Combining: default grants ∪ every group's grants; any "on" wins; nothing removes a
  grant. A person may do an action on a portfolio only with **both** the access level
  and the grant.

## Grants (keys)

| Key | Label | Checked where |
|---|---|---|
| `orders` | Place and cancel own orders | submit, cancel own (with Trade access) |
| `cancel_any` | Cancel any order | cancel others' orders (with Trade access) |
| `allocate` | Allocate block orders | allocation (with Trade access) |
| `options` | Trade options and combos | submit of an option/combo instrument |
| `strategies` | Create strategies | reported strategy create |
| `strategy_tokens` | Rotate and revoke strategy tokens | rotate / revoke / archive (own strategies) |
| `limits_view` | See risk limits | limits read (with View access) |
| `limits_set` | Set risk limits | limits write (with Manage access) |
| `api_keys` | Personal API keys | create own API key |

"Export orders and fills" from the board is left out: no export exists yet (my call).

## Design

- **Table** `access_grant(subject_type CHECK IN ('default','group'), subject_id uuid NULL
  (NULL for default, group id otherwise), grant_key text, PRIMARY KEY-like unique)`.
  Group delete cascades.
- **Person access** `portfolio_person_access(portfolio_id, principal_id, access
  view|trade|manage)`, unioned into the `portfolio_access` view (highest level wins
  with group access).
- **Effective grants** `grants_for(principal) -> BTreeSet<GrantKey>` = default ∪ groups.
  **Admins skip grant checks** (all keys) but still need portfolio access to trade
  (existing rule: admin does not imply trading).
- **Enforcement** in `authz::allowed`: each `Permission` maps to an optional grant key;
  allowed = access level permits AND (no key OR key in grants OR admin). Non-portfolio
  checks (strategies, api_keys, options) call `authz::require_grant`.
- **Machine credentials** (strategy tokens, host token) are not grant-checked.
- **Migration keeps today's behavior**: default grants on for `orders`, `allocate`,
  `options`, `strategies`, `strategy_tokens`, `limits_view`, `api_keys`; every group
  that has Manage on any portfolio gets `cancel_any` and `limits_set`. Admins then trim.
- `/auth/me` returns `grants: string[]` so the cockpit hides what a person can't use.
- **API**: `GET/PUT /admin/grants/default`, `GET/PUT /admin/groups/:id/grants`,
  `GET /admin/groups/:id/preview` (resolved grants), `GET/PUT
  /admin/portfolios/:id/access` (`[{kind:"group"|"person", id, access}]`). Audited, MFA.
- **UI** per the boards: Groups page = list (Default grants first) + editor tabs General /
  Users / Portfolios / Grants / Preview. Portfolios page = table (Access, Venues, Status)
  + editor tabs Access / Venues / General (replaces the Grants dialog).

## Non-goals

Parent/nested groups, "who can share", per-resource read/write beyond portfolios,
export grant.
