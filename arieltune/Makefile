# arieltune -- unified BC-250 tuning suite (WIKI | BIOS | APU | MEM)
BIN     := arieltune
PREFIX  ?= /usr/local
BINDIR  := $(DESTDIR)$(PREFIX)/bin
COMPAT  := aputune memtune biostune wikitune

# Compute shaders. The .spv is embedded with include_bytes!, so the build never
# regenerates it; these targets are the explicit path to do so.
#
# glslc (shaderc), not glslangValidator: glslc enables the two GL_GOOGLE_*
# extensions by default, so its output carries extra OpSourceExtension entries
# and a different generator word, and byte-identity with the committed blobs
# depends on that.
#
# The flags are per-directory and are NOT cosmetic. Each committed blob
# reproduces only with the set it was originally built with: mem's kernels are
# unoptimized, apu's KAT went through spirv-opt (-O), which is what gives it its
# OpPhi-based body. Building either with the other's flags changes the bytes.
MEM_SHADERS := $(wildcard crates/mem/src/shaders/*.comp)
MEM_FLAGS   :=
APU_SHADERS := $(wildcard crates/apu/shaders/*.comp)
APU_FLAGS   := -O
GLSLC       ?= glslc

.PHONY: build release install install-units install-driver uninstall test check fmt \
        shaders shaders-check clean

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

# Rebuild every .spv from its .comp. Only needed after editing a shader;
# `cargo build` embeds the existing blob.
shaders:
	@command -v $(GLSLC) >/dev/null 2>&1 || { echo "$(GLSLC) (shaderc) not found"; exit 1; }
	@for s in $(MEM_SHADERS); do \
	  $(GLSLC) -fshader-stage=comp $(MEM_FLAGS) "$$s" -o "$${s%.comp}.spv" || exit 1; \
	  echo "  built $${s%.comp}.spv"; \
	done
	@for s in $(APU_SHADERS); do \
	  $(GLSLC) -fshader-stage=comp $(APU_FLAGS) "$$s" -o "$${s%.comp}.spv" || exit 1; \
	  echo "  built $${s%.comp}.spv"; \
	done

# Prove the committed blobs match their sources. Kept out of `check` on purpose:
# it needs shaderc installed, and the generator word it writes tracks the shaderc
# version, so a mismatch here may mean an upgraded tool rather than an edited
# shader. See the shader READMEs before acting on a failure.
shaders-check:
	@command -v $(GLSLC) >/dev/null 2>&1 || { echo "$(GLSLC) (shaderc) not found"; exit 1; }
	@fail=0; \
	check() { \
	  out=$$(mktemp); \
	  if $(GLSLC) -fshader-stage=comp $$1 "$$2" -o "$$out" 2>/dev/null && cmp -s "$$out" "$${2%.comp}.spv"; then \
	    echo "  ok   $${2}"; \
	  else \
	    echo "  DIFF $${2} - see the shader README"; fail=1; \
	  fi; \
	  rm -f "$$out"; \
	}; \
	for s in $(MEM_SHADERS); do check "$(MEM_FLAGS)" "$$s"; done; \
	for s in $(APU_SHADERS); do check "$(APU_FLAGS)" "$$s"; done; \
	exit $$fail

clean:
	cargo clean
