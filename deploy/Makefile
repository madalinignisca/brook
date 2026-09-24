# Brook server stack — homelab ops. Run from deploy/.
COMPOSE := docker compose
ENV := .env

.DEFAULT_GOAL := help

.PHONY: help init up down reset restart logs ps rebuild storage media migrate dbrev

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n",$$1,$$2}'

init: ## Create .env with random secrets — only if it doesn't exist yet
	@if [ -f $(ENV) ]; then \
	  echo "$(ENV) already exists — leaving it untouched."; \
	else \
	  cp .env.example $(ENV); \
	  key=$$(head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 48); \
	  pw=$$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 24); \
	  mpw=$$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 24); \
	  tmp=$$(mktemp); \
	  sed "s|^BROOK_JWT_SIGNING_KEY=.*|BROOK_JWT_SIGNING_KEY=$$key|;\
	       s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=$$pw|;\
	       s|^MINIO_ROOT_PASSWORD=.*|MINIO_ROOT_PASSWORD=$$mpw|" $(ENV) > $$tmp && mv $$tmp $(ENV); \
	  echo "Generated $(ENV) with random secrets — review it."; \
	fi

up: ## Start the core stack (postgres + api + caddy)
	@[ -f $(ENV) ] || $(MAKE) init
	$(COMPOSE) up -d --build
	@echo "Brook API is up on $$(grep -E '^BROOK_HTTP_BIND=' $(ENV) | cut -d= -f2 | grep . || echo 127.0.0.1):$$(grep -E '^BROOK_HTTP_PORT=' $(ENV) | cut -d= -f2) (BROOK_HTTP_BIND / BROOK_HTTP_PORT in .env)."

down: ## Stop the stack (data is kept)
	$(COMPOSE) down

reset: ## Stop and WIPE all data (postgres + minio volumes), then rebuild the core stack
	$(COMPOSE) down -v --remove-orphans
	$(COMPOSE) up -d --build

restart: ## Restart services
	$(COMPOSE) restart

logs: ## Follow logs (last 100 lines)
	$(COMPOSE) logs -f --tail=100

ps: ## Show running services
	$(COMPOSE) ps

rebuild: ## Rebuild & restart only the api (entrypoint applies migrations on start)
	$(COMPOSE) up -d --build api

migrate: ## Apply DB migrations to head in the running api (no rebuild)
	$(COMPOSE) exec api alembic upgrade head

dbrev: ## Show the current DB revision and migration history
	$(COMPOSE) exec api alembic current
	$(COMPOSE) exec api alembic history --verbose | head -40

storage: ## Start, also bringing up MinIO (Phase 2)
	@[ -f $(ENV) ] || $(MAKE) init
	$(COMPOSE) --profile storage up -d --build

media: ## Start, also bringing up Janus (Phase 4)
	@[ -f $(ENV) ] || $(MAKE) init
	$(COMPOSE) --profile media up -d --build
