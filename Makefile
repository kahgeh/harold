.PHONY: build install reinstall deploy

build:
	cargo build --locked --release --workspace

install deploy:
	./scripts/install.sh $(INSTALL_ARGS)

reinstall:
	./scripts/install.sh --reinstall $(INSTALL_ARGS)
