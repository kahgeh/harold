DEPLOY_DIR := $(HOME)/bin/harold
BINARY     := target/release/harold

.PHONY: build deploy

build:
	cargo build --release

deploy: build
	mkdir -p $(DEPLOY_DIR)
	cp $(BINARY) $(DEPLOY_DIR)/harold
	codesign --force --sign "Kah Geh Tan" $(DEPLOY_DIR)/harold
	mkdir -p $(DEPLOY_DIR)/config
	cp harold/config/default.toml $(DEPLOY_DIR)/config/default.toml
	cp harold/config/local.template.toml $(DEPLOY_DIR)/config/local.template.toml
