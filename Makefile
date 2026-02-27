DEPLOY_DIR := $(HOME)/bin/harold
BINARY     := target/release/harold

.PHONY: build deploy restart

build:
	cargo build --release

deploy: build
	mkdir -p $(DEPLOY_DIR)
	cp $(BINARY) $(DEPLOY_DIR)/harold
	codesign --force --sign "Kah Geh Tan" $(DEPLOY_DIR)/harold
	cp harold/proto/harold.proto $(DEPLOY_DIR)/harold.proto
	mkdir -p $(DEPLOY_DIR)/config
	cp harold/config/default.toml $(DEPLOY_DIR)/config/default.toml
	cp harold/config/local.template.toml $(DEPLOY_DIR)/config/local.template.toml

restart: deploy
	pkill -f "$(DEPLOY_DIR)/harold" || true
	sleep 1
	nohup $(DEPLOY_DIR)/harold >> $(DEPLOY_DIR)/harold.log 2>&1 &
