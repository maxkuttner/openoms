# scripts

Standalone dev-workflow shell scripts that don't belong to any one crate — currently `dev-trade.sh`, which brings up the full local stack (Postgres, the optional Authentik local SSO test provider, the OMS, the cockpit dev server) for testing the trader webapp end to end. Authentik is only a dev fixture; `[auth.oidc]` accepts any OIDC issuer.
