# Brook server stack — homelab ops. Run from deploy/.
COMPOSE := docker compose
ENV := .env

.DEFAULT_GOAL := help

.PHONY: help init keyring up start stop down reset restart logs ps rebuild storage media migrate dbrev

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
	  jsec=$$(head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 40); \
	  skey=$$(head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'); \
	  tmp=$$(mktemp); \
	  sed "s|^BROOK_JWT_SIGNING_KEY=.*|BROOK_JWT_SIGNING_KEY=$$key|;\
	       s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=$$pw|;\
	       s|^MINIO_ROOT_PASSWORD=.*|MINIO_ROOT_PASSWORD=$$mpw|;\
	       s|^JANUS_API_SECRET=.*|JANUS_API_SECRET=$$jsec|;\
	       s|^BROOK_SECRET_KEYS=.*|BROOK_SECRET_KEYS=1:$$skey|;\
	       s|^BROOK_SECRET_PRIMARY_KEY_ID=.*|BROOK_SECRET_PRIMARY_KEY_ID=1|" $(ENV) > $$tmp && mv $$tmp $(ENV); \
	  echo "Generated $(ENV) with random secrets — review it."; \
	fi

keyring: ## Add a secret-encryption keyring to an existing .env that lacks one (never replaces)
	@if ! grep -qE '^BROOK_SECRET_KEYS=[0-9]+:.' $(ENV); then \
	  skey=$$(head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'); \
	  sed -i.bak '/^BROOK_SECRET_KEYS=/d; /^BROOK_SECRET_PRIMARY_KEY_ID=/d' $(ENV) && rm -f $(ENV).bak; \
	  printf '\n# Added by `make keyring`: encryption of stored secrets. Never regenerate.\nBROOK_SECRET_KEYS=1:%s\nBROOK_SECRET_PRIMARY_KEY_ID=1\n' "$$skey" >> $(ENV); \
	  echo "Added a secret keyring to $(ENV)."; \
	fi

up: ## Start the core stack (postgres + api + caddy)
	@[ -f $(ENV) ] || $(MAKE) init
	@$(MAKE) --no-print-directory keyring
	$(COMPOSE) up -d --build
	@echo "Brook API is up on $$(grep -E '^BROOK_HTTP_BIND=' $(ENV) | cut -d= -f2 | grep . || echo 127.0.0.1):$$(grep -E '^BROOK_HTTP_PORT=' $(ENV) | cut -d= -f2) (BROOK_HTTP_BIND / BROOK_HTTP_PORT in .env)."

stop: ## End the work session — stop containers but keep them (data kept); `make start` resumes
	$(COMPOSE) stop

start: ## Resume a stopped session without rebuilding
	$(COMPOSE) start

down: ## Stop and remove containers (data volumes are kept; use `reset` to wipe)
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
	@$(MAKE) --no-print-directory keyring
	$(COMPOSE) up -d --build api

migrate: ## Apply DB migrations to head in the running api (no rebuild)
	$(COMPOSE) exec api alembic upgrade head

dbrev: ## Show the current DB revision and migration history
	$(COMPOSE) exec api alembic current
	$(COMPOSE) exec api alembic history --verbose | head -40

storage: ## Start, also bringing up MinIO (Phase 2)
	@[ -f $(ENV) ] || $(MAKE) init
	@$(MAKE) --no-print-directory keyring
	$(COMPOSE) --profile storage up -d --build

media: ## Start, also bringing up Janus (Phase 4)
	@[ -f $(ENV) ] || $(MAKE) init
	@$(MAKE) --no-print-directory keyring
	@# `make init` only creates .env when it is missing, so a .env from before calls
	@# has no Janus secret. Add one (random, never printed) rather than fail.
	@grep -qE '^JANUS_API_SECRET=.+' $(ENV) || { \
	  jsec=$$(head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 40); \
	  tmp=$$(mktemp); grep -v '^JANUS_API_SECRET=' $(ENV) > $$tmp && mv $$tmp $(ENV); \
	  printf '\n# Added by `make media`: shared secret between api and Janus.\nJANUS_API_SECRET=%s\n' "$$jsec" >> $(ENV); \
	  echo "Added JANUS_API_SECRET to $(ENV)."; }
	$(COMPOSE) --profile media up -d --build
