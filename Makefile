DEPLOY_DIR := $(HOME)/bin/harold
BINARY     := target/release/harold

-include .env
export

.PHONY: build deploy setup-codesign

build:
	cargo build --release

setup-codesign .env:
	@bash scripts/setup-codesign.sh

deploy: build .env
	mkdir -p $(DEPLOY_DIR)
	pkill -f "$(DEPLOY_DIR)/harold" || true
	sleep 1
	cp $(BINARY) $(DEPLOY_DIR)/harold
	codesign --force --sign "$(CODESIGN_IDENTITY)" $(DEPLOY_DIR)/harold
	cp harold-api/proto/harold.proto $(DEPLOY_DIR)/harold.proto
	mkdir -p $(DEPLOY_DIR)/hooks
	cp hooks/shared/harold_turn_complete.py $(DEPLOY_DIR)/hooks/harold_turn_complete.py
	mkdir -p $(DEPLOY_DIR)/config
	cp harold/config/default.toml $(DEPLOY_DIR)/config/default.toml
	cp harold/config/local.template.toml $(DEPLOY_DIR)/config/local.template.toml
	cd $(DEPLOY_DIR) && nohup $(DEPLOY_DIR)/harold </dev/null >> harold.log 2>&1 &
