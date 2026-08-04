# detox-parser — сборка, инфраструктура и управление длительным сбором.
# `make` или `make help` — список целей.

SHELL := /bin/bash
REPO  := $(CURDIR)
BIN   := $(REPO)/target/release/detox-parser
CONFIG ?= config/seeds.youtube.toml
SYSTEMD_DIR := $(HOME)/.config/systemd/user
UNITS := detox-collect.service detox-collect.timer

.DEFAULT_GOAL := help

## help: показать этот список
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed -e 's/## //'

# ─── Сборка / проверки ───────────────────────────────────────────────────────
## build: release-бинарь detox-parser
build:
	cargo build --release -p detox-parser-cli

## check: быстрая проверка компиляции воркспейса
check:
	cargo check --workspace

## test: тесты воркспейса
test:
	cargo test --workspace

## fmt: форматирование
fmt:
	cargo fmt --all

# ─── Инфраструктура (docker: Postgres + MinIO) ───────────────────────────────
## infra-up: поднять Postgres + MinIO
infra-up:
	docker compose up -d

## infra-down: остановить инфраструктуру (данные в volume сохраняются)
infra-down:
	docker compose down

## infra-logs: логи docker-инфраструктуры
infra-logs:
	docker compose logs -f

# ─── Разовые команды пайплайна ───────────────────────────────────────────────
## discover: Stage 0 — найти кандидатов по сидам (без harvest)
discover: build
	$(BIN) discover --config $(CONFIG)

## run: полный прогон discover→harvest→media один раз
run: build
	$(BIN) run --config $(CONFIG)

## status: сводка по стадиям из манифеста
status: build
	$(BIN) status --config $(CONFIG)

# ─── systemd: длительный сбор по таймеру (user-сервис, без root) ──────────────
## env: создать config/detox.env из примера (если ещё нет)
env:
	@test -f config/detox.env \
	  || { cp config/detox.env.example config/detox.env; \
	       echo ">> создан config/detox.env — впиши YTDLP_BIN и креды MinIO"; }

## install: установить и включить таймер (пути в юнитах → этот репозиторий)
install: build env
	@mkdir -p $(SYSTEMD_DIR)
	@for u in $(UNITS); do \
	  sed 's#%h/detox-parser#$(REPO)#g' deploy/$$u > $(SYSTEMD_DIR)/$$u; \
	  echo ">> установлен $(SYSTEMD_DIR)/$$u"; \
	done
	loginctl enable-linger "$$USER"
	systemctl --user daemon-reload
	systemctl --user enable --now detox-collect.timer
	@echo ">> таймер включён. Следующий прогон: make timers"

## uninstall: выключить и удалить таймер+сервис (данные и config НЕ трогаем)
uninstall:
	-systemctl --user disable --now detox-collect.timer
	-systemctl --user stop detox-collect.service
	@for u in $(UNITS); do rm -f $(SYSTEMD_DIR)/$$u && echo ">> удалён $(SYSTEMD_DIR)/$$u"; done
	systemctl --user daemon-reload
	-systemctl --user reset-failed detox-collect.service
	@echo ">> таймер и сервис удалены."
	@echo ">> linger оставлен (влияет на все user-сервисы). Убрать вручную:"
	@echo ">>   loginctl disable-linger $$USER"

## start: прогнать сбор прямо сейчас (разово, через сервис)
start:
	systemctl --user start detox-collect.service

## stop: остановить текущий прогон сервиса
stop:
	systemctl --user stop detox-collect.service

## timers: когда следующий прогон по таймеру
timers:
	systemctl --user list-timers detox-collect.timer --no-pager

## svc-status: результат последнего прогона сервиса
svc-status:
	systemctl --user status detox-collect.service --no-pager

## logs: live-лог сбора (journalctl)
logs:
	journalctl --user -u detox-collect.service -f

.PHONY: help build check test fmt infra-up infra-down infra-logs \
        discover run status env install uninstall start stop timers svc-status logs
