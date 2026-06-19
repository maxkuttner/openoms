.DEFAULT_GOAL := help
.PHONY: help db-provision db-migrate db-access db-seed db-fixtures db-setup seed-instruments sync-brokers

# Load .env into the recipe shell (one shell per recipe line, so chain with &&).
ENV := set -a && . ./.env && set +a

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# --- one-shot standalone bootstrap (no data-provider creds needed) ---
db-setup: db-provision db-migrate db-access db-seed db-fixtures ## Full standalone DB bootstrap (roles, schema, ref data, SPY fixture)
	@echo "✅ openoms DB ready — start the service and place a paper order."

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

# --- live universe (on-demand, NOT scheduled; the seeders write as market_user) ---
seed-instruments: ## Seed the instrument universe from Databento (needs DATABENTO_API_KEY)
	@$(ENV) && DB_USER=market_user DB_PASSWORD="$$MARKET_USER_PASSWORD" \
		python3 scripts/seed_instruments.py --symbol $${SYMBOL:-SPY}

sync-brokers: ## Sync broker_instrument from the broker catalog (needs ALPACA_PAPER_*)
	@$(ENV) && DB_USER=market_user DB_PASSWORD="$$MARKET_USER_PASSWORD" \
		python3 scripts/broker_sync.py
