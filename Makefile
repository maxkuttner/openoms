.DEFAULT_GOAL := help
.PHONY: help db-provision db-migrate db-access db-seed db-fixtures db-setup db-reset seed-instruments sync-brokers

# Load .env into the recipe shell (one shell per recipe line, so chain with &&).
ENV := set -a && . ./.env && set +a

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# --- one-shot standalone bootstrap (no data-provider creds needed) ---
db-setup: db-provision db-migrate db-access db-seed db-fixtures ## Full standalone DB bootstrap (roles, schema, ref data, SPY fixture)
	@echo "✅ openoms DB ready — start the service and place a paper order."

db-reset: ## Drop the ods database and rebuild it from scratch (roles are kept)
	@$(ENV) && PGPASSWORD="$$ADMIN_PASSWORD" psql -v ON_ERROR_STOP=1 \
		-h "$$DB_HOST" -p "$$DB_PORT" -U "$$ADMIN_USER" -d postgres \
		-c "DROP DATABASE IF EXISTS $${ODS_DB:-ods} WITH (FORCE);"
	@$(MAKE) db-setup

# --- individual steps ---
db-provision: ## Create roles + the ods database (needs ADMIN superuser creds)
	@$(ENV) && ./db/scripts/provision.sh

db-migrate: ## Apply the ods schema migrations
	@$(ENV) && ./db/scripts/migrate.sh up

db-access: ## Apply role attributes + grants (run after db-migrate)
	@$(ENV) && ./db/scripts/access.sh

db-seed: ## Seed reference data (currencies, venues)
	@$(ENV) && ./db/scripts/seed.sh

db-fixtures: ## Load the minimal no-creds fixture (SPY)
	@$(ENV) && PGPASSWORD="$$ADMIN_PASSWORD" psql -v ON_ERROR_STOP=1 \
		-h "$$DB_HOST" -p "$$DB_PORT" -U "$$ADMIN_USER" -d "$${ODS_DB:-ods}" \
		-f scripts/fixtures/minimal_seed.sql

# --- live universe (on-demand, NOT scheduled) ---
# Default to the admin role (like every other seeder) so standalone works out of
# the box. Note .env sets DB_USER=oms_user for the app (read-only on public), so we
# use a separate SEED_DB_USER var: a managed deployment can export it to point the
# seeders at a least-privilege write role (e.g. market_user) without touching DB_USER.
seed-instruments: ## Seed instrument universes from Databento (interactive; UNIVERSE=CODE for one)
	@$(ENV) && DB_USER="$${SEED_DB_USER:-$$ADMIN_USER}" DB_PASSWORD="$${SEED_DB_PASSWORD:-$$ADMIN_PASSWORD}" \
		python3 scripts/seed_instruments.py \
		$${UNIVERSE:+--universe $$UNIVERSE} $${UNIVERSE:---interactive}

sync-brokers: ## Sync broker_instrument from the broker catalog (needs ALPACA_PAPER_*)
	@$(ENV) && DB_USER="$${SEED_DB_USER:-$$ADMIN_USER}" DB_PASSWORD="$${SEED_DB_PASSWORD:-$$ADMIN_PASSWORD}" \
		python3 scripts/broker_sync.py
