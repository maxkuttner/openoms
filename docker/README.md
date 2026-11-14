# docker

Config for the containers the root `docker-compose.yml` can bring up — currently just `authentik/blueprints/` (the dev blueprint creating the `oms` OIDC provider and application, used by the opt-in `oidc` profile as a local SSO test provider). The OMS works with any OIDC issuer (Entra, Google, Okta, Keycloak, Authentik, ...) via `[auth.oidc]`; Authentik here is a dev-only fixture with a public client secret, which the OMS refuses off-loopback, and the deploy stack does not run it.
