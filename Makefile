.PHONY: build install reinstall deploy release install-release

PYTHON ?= python3

build:
	cargo build --locked --release --workspace

install deploy:
	./scripts/install.sh $(INSTALL_ARGS)

reinstall:
	./scripts/install.sh --reinstall $(INSTALL_ARGS)

# make exports VERSION, NOTES, PREFIX and SIGNING_IDENTITY from its command line to the shell,
# which passes each one that is set as one argument, spaces and all.
release:
	$(PYTHON) scripts/release.py release $${VERSION:+--version "$$VERSION"} $${NOTES:+--notes "$$NOTES"} $${PREFIX:+--prefix "$$PREFIX"} $${SIGNING_IDENTITY:+--signing-identity "$$SIGNING_IDENTITY"}

install-release:
	$(PYTHON) scripts/release.py install $${VERSION:+--version "$$VERSION"} $${PREFIX:+--prefix "$$PREFIX"} $${SIGNING_IDENTITY:+--signing-identity "$$SIGNING_IDENTITY"}
