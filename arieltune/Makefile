# arieltune -- unified BC-250 tuning suite (WIKI | BIOS | APU | MEM)
BIN     := arieltune
PREFIX  ?= /usr/local
BINDIR  := $(DESTDIR)$(PREFIX)/bin
COMPAT  := aputune memtune biostune wikitune

.PHONY: build release install install-units install-driver uninstall test check fmt clean

build release:
	cargo build --release --locked

# Install the one binary + the `at` alias + the four compat symlinks.
# Use `sudo make install` (writes to $(PREFIX)/bin).
install: release
	install -d $(BINDIR)
	install -m755 target/release/$(BIN) $(BINDIR)/$(BIN)
	ln -sf $(BIN) $(BINDIR)/at
	for n in $(COMPAT); do ln -sf $(BIN) $(BINDIR)/$$n; done
	@echo "installed $(BINDIR)/$(BIN) (+ at + compat: $(COMPAT))"

# Lay + enable the APU GPU power unit. The mode command writes the unit itself,
# so this just points you at it (the unit name is now arieltune-gpu.service).
install-units: install
	@echo "pick the APU power mode to lay + enable the unit:"
	@echo "  sudo $(BIN) apu gpu governor-on | autosleep-on | force <mhz>"
	@echo "service routing (optional): sudo $(BIN) apu cu route-save && systemctl enable arieltune-route"

# Build + install the BIOS smiflash DKMS driver (needs a board + kernel headers).
install-driver: install
	$(BIN) bios driver build

uninstall:
	$(BIN) uninstall || rm -f $(BINDIR)/$(BIN) $(BINDIR)/at $(foreach n,$(COMPAT),$(BINDIR)/$(n))

test:
	cargo test --workspace --locked

# Full local gate. Keep this in step with .github/workflows/ci.yml: a
# contributor's local gate being weaker than CI is what lets a change pass
# here and still fail review.
check: fmt
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo test --workspace --locked
	RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links -D rustdoc::invalid_html_tags" \
		cargo doc --workspace --no-deps --locked

fmt:
	cargo fmt --check

clean:
	cargo clean
