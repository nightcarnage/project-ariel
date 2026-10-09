# BC-250 snapshot tool — port spec

Status: **PROPOSAL, implemented — kept as the design record.** This is the
original port spec, written before any code existed; the live documentation is
`docs/snapshot-tool.md`. The branch it arrived on (`design/snapshot-tool-spec`)
was folded into this one when the branch set was reduced to two.

Source being ported: `bc-250-snapshotter/snapshot` at `d8abfb6` — 2675 lines of
bash, 45 functions, 24 `cmd_*` verbs, no test harness.

Destination: a new workspace member, `arieltune/crates/snapshot/`.

---

## 1. Why this is worth doing

Three facts from this repo, not a preference for Rust.

1. **`apu/src/initramfs.rs` already shells out to the bash tool.**
   `Command::new("snapshot").args(["list", "--ids"])`, wrapped in a four-way
   `SnapshotterVerdict` (`Known` / `Unknown` / `Unusable` / `Unavailable`). Part
   of that enum's reason to exist is that `/usr/bin/snapshot` on a workstation is
   an unrelated program answering `Unknown option --ids`. In-workspace this
   becomes a library call and the enum's ambiguity disappears.

2. **arieltune already rebuilds snapshot images.** The same module documents that
   a snapshot-booted blade uses `/boot/initramfs-snap-<hash>.img`, that no
   mkinitcpio preset produces it, and that `mkinitcpio -P` reports success while
   leaving it untouched. It already derives the snapshot id from `BOOT_IMAGE=`.
   The snapshot domain is leaking into arieltune; this consolidates it.

3. **The bash tool is at the size where its bug classes repeat.** Three real
   defects were found in it during the week of 2026-10-08 alone:
   - `verify` failed its own success path (source-tree check gated on
     `registered: true`, so a create-path snapshot on a tree-less blade reported
     `FAIL`);
   - `hide` could remove the menu entry of the booted / golden / pinned snapshot,
     after which GRUB falls back to entry 0 and boots a different kernel;
   - `40_custom` blocks are deleted with two *different* `sed` ranges —
     `,/^}/d` in `grub_remove_entry`, `/^$/d` in `cmd_prune`.

---

## 2. Port surface

| | count |
|---|---|
| lines | 2675 |
| functions | 45 |
| `cmd_*` verbs | 24 |
| external invocations | ~200 |

External-call census, by frequency: `grep` 78, `sudo` 77, `sed` 48, `stat` 17,
`md5sum` 17, `grub-reboot` 15, `grub-mkconfig` 11, `zstd` 10, `strings` 10,
`awk` 10, `modinfo` 7, `make` 7, `grub-editenv` 7, `cpio` 7, `mkinitcpio` 6,
`lsinitcpio` 5, `nproc` 4, `grub-set-default` 4, `pacman` 2, `modprobe` 2,
`findmnt` 1.

The 78 `grep`, 48 `sed`, 17 `stat` and 10 `awk` are pure computation. Only GRUB
(37 calls) genuinely has to stay a subprocess.

---

## 3. Hard invariants — these are the whole risk

Get any of these wrong and existing snapshots break.

### 3.1 Snapshot identity: three formulas, not one

`snap-` + `md5(<components>)[0:8]`. Which components depends on how the
snapshot was made. **Verified against real snapshots on blade 104 on 2026-10-09**
(read-only reproduction of the bash logic):

**F1 — `create` with a source tree** (`compute_snap_id`, tree branch)

```
H  = md5(concat(gmc_v10_0.c, gfx_v10_0.c, amdgpu_gmc.c,
                amdgpu_amdkfd_gfx_v10.c))[0:8]     # concatenation, order matters
S  = md5(Module.symvers)[0:8]                      # "" when absent
id = "snap-" + md5("H-S" + "\n")[0:8]              # echo appends a newline
```

**F2 — `create` with no tree** (`compute_snap_id`, treeless branch)

```
M  = md5(/usr/lib/modules/<running_kver>/kernel/drivers/gpu/drm/amd/amdgpu/amdgpu.ko.zst)[0:8]
id = "snap-" + md5("<running_kver>-<M>")[0:8]      # printf: NO trailing newline
```

**F3 — `register` / `import`** (`compute_register_id`)

```
M, I, V = md5(module), md5(initramfs), md5(vmlinuz)  → [0:8] each
S       = md5(Module.symvers)[0:8]                   # "" when absent
id = "snap-" + md5("M-I-V-S" + "\n")[0:8]
```

Reproduction evidence:

| snapshot | formula | computed | match |
|---|---|---|---|
| `snap-0848359a` | F3 | `ecdb619e-f21daf6a-ddb9f7cc-` → `0848359a` | **YES** |
| `snap-67d5ebcf` | F3 | `2b55d5a3-a033475e-176f3286-` → `67d5ebcf` | **YES** |
| `snap-da794092` | F2 | `7.0.9-1-cachyos` + `df096b91` → `da794092` | **YES** |
| `snap-8b8eb10c` | F1 | not reproducible | **NO** |
| `snap-677fd8d0` | F1 | not reproducible | **NO** |
| `snap-2d2b1734` | F1 | not reproducible | **NO** |

Four traps the port must reproduce rather than "fix":

1. **A missing component is the empty string, not an error.**
   `md5sum <missing> | cut -c1-8` prints nothing and still yields a valid id
   input. A port that returns an error where bash returned `""` computes a
   different id for every treeless or symvers-less snapshot.
2. **`echo` vs `printf` is load-bearing.** F1/F3 hash a newline-terminated
   string; F2 does not. Switching either changes every id it produces.
3. **F2 hashes the *live installed* module**, not the snapshot's copy, so it is
   only reproducible while that module is unchanged.
4. **F1 records none of its inputs.** `Module.symvers` and the four `.c` files
   are gone once the tree is, so build-from-source ids cannot be re-derived or
   audited afterwards. The three `NO` rows above are this, and only this.

**Proposed improvement (additive, safe):** record `id_formula` plus the raw
components (`module_md5`, `initramfs_md5`, `vmlinuz_md5`, `symvers_md5` or the
tree hash) in the manifest. That makes every id re-derivable and turns a
three-branch algorithm into something auditable. Existing manifests keep
working: absent fields simply mean "not recorded".

### 3.2 Manifest schema

Field names must survive, because other tooling reads them and bash and Rust
must interoperate during the transition. Observed shape (`snap-da794092`):

```
snapshot:   id, timestamp, description, status, host, parent, git_commit
kernel:     version, vmlinuz_source, vmlinuz_snapshot
initramfs:  snapshot, preserve_copy
module:     path, srcversion, vermagic, size
patches:    applied, markers, files[]
cmdline:    (string)
grub:       entry_id, entry_title, root_uuid
boot_files: vmlinuz, initramfs, cmdline_file
source_tree: path, mode, bc250_markers
config:     mkinitcpio_conf_md5
firmware:   cyan_skillfish_md5
test_results: boot_test, gpu_alive, rocminfo, dmesg_errors
registered: true            # register path only
```

Two known data defects to preserve *understanding* of, even if they are
eventually corrected:

- **The stored `cmdline` is the live one at capture time and is not safe to boot
  with.** `snap-da794092`'s manifest contains `BOOT_IMAGE=` **twice**, and both
  point at `/boot/vmlinuz-snap-2d2b1734` — a *different snapshot*. Any code path
  that rebuilds an entry from this must rewrite `BOOT_IMAGE=` (as
  `grub_write_entry_from_manifest` now does).
- **Three manifests record `kernel.version: "7.0.9"`** where the id was computed
  with the full `7.0.9-1-cachyos`. A lossy record of an id input.

### 3.3 Path conventions

```
/var/lib/kernel-snapshots/snap-<id>/manifest.yaml
/var/lib/kernel-snapshots/snap-<id>/{modules,config,firmware}/
/var/lib/kernel-snapshots/.golden-snapshot          # golden marker (id as text)
/var/lib/kernel-snapshots/snap-<id>/manifest.yaml.hidden   # hide = rename
/boot/vmlinuz-snap-<id>
/boot/initramfs-snap-<id>.img                        # mode 0600 → root to hash
/boot/snap-<id>.cmdline
/etc/grub.d/40_custom                                # our menu blocks
```

### 3.4 GRUB entries: parse, never `sed`

Current blocks:

```
# === <id>: <description> ===
# Created: <ts>          (or "# Restored by: snapshot unhide")
# Kernel: <ver>
menuentry '[<id>] <description>' --id '<id>' {
    insmod gzio | insmod part_gpt | insmod ext2
    search --no-floppy --fs-uuid --set=root <uuid>
    linux <vmlinuz> BOOT_IMAGE=<vmlinuz> <cmdline>
    initrd [<acpi_override.cpio> ]<initramfs>
}
```

The port should model this as a list of blocks with a deterministic serializer.
That is what makes nesting (L3) safe and removes the two-inconsistent-`sed`-ranges
bug class. `grub.cfg` reading must stay brace-depth based, not
indentation-based — the EFI entries are indented inside an `if/fi` block yet are
top-level menu lines.

### 3.5 CLI compatibility

- `snapshot list --ids` is an existing machine-readable contract that arieltune's
  `initramfs.rs` depends on. Keep it until that call site is replaced by the
  library. It matters that the dependency is *removed deliberately*, not broken.
- `blade-onboard.sh` and the fleet scripts invoke `snapshot` by name.
- **`snapshot` is taken on the workstation** by an unrelated binary. Proposed:
  primary binary `snaptune`, plus a `snapshot` compat symlink on blades only —
  the same trick arieltune already uses for `aputune`/`memtune` via argv[0]
  namespace injection.

---

## 4. Placement and shape

One crate, two consumers:

```
arieltune/crates/snapshot/
    src/lib.rs        # core + Screen impl  → linked into `arieltune` as a tab
    src/main.rs       # `[[bin]] name = "snaptune"` → standalone for blades
```

Added to the workspace `members` list. `arieltune` gains a `SNAPSHOTS` tab and a
`snapshot` CLI namespace (alongside `apu`/`mem`/`bios`/`wiki`).

Alternatives considered:

- **Standalone crate, own workspace** — loses the reuse below; the tab is what
  makes the archive blade usable.
- **Separate repo (llmtune-style)** — loses the reuse *and* would mean a third
  copy of the "which image did this boot come from" logic.

---

## 5. Reuse map

| arieltune today | replaces in the bash tool |
|---|---|
| `arieltune-tui-kit` (`Screen`, `Outcome`, theme, widgets) | a hand-rolled TUI |
| `ariel-hal` (`require_root`, `running_kernel`) | 77 `sudo` calls |
| `apu::initramfs` (+ `boot_provenance`, `snapshot_id_for_boot_image`) | `mkinitcpio`/`lsinitcpio` invocation, id derivation |
| `apu::kbuild` | 7 `make M=…` module builds, toolchain selection |
| `apu::patches`, `bc250-catalog` | `zstd -d \| strings \| grep BC-250` marker counting |
| `zstd`, `cpio`, `md-5` crates | 10 `zstd`, 7 `cpio`, 17 `md5sum` |

---

## 6. Proposed module layout

```
model.rs      Snapshot, Manifest (serde), state enum, the three id formulas
store.rs      discovery over /var/lib/kernel-snapshots, hidden/golden markers
manifest.rs   typed read/write, schema-version tolerant
grub.rs       40_custom block parse/serialize; grub.cfg reader by brace depth
bootenv.rs    grub-editenv / grub-reboot / grub-set-default wrappers
image.rs      initramfs inspection (in-process cpio+zstd, so two-segment
              mkinitcpio images stop being a footgun)
create.rs     module build (kbuild), mkinitcpio invocation, rescue retention
cli.rs        clap surface, `--json` on read verbs
screen.rs     the SNAPSHOTS tab
```

---

## 7. CLI surface

All 24 verbs ported, with `--json` added to the read verbs (`list`, `show`,
`current`, `lineage`, `grub-list`, `verify`) for agent consumption — the same
convention the suite already uses.

---

## 8. TUI screen and safety boundary

`apu` documents a deliberate boundary: the GPU governor and `apply-boot` are
CLI-only and never spawned from the TUI, because a second SMU writer is
dangerous. The snapshot equivalent is that **`prune` deletes kernels**, and from
a list view that is one keystroke away.

Proposed split:

- **TUI may**: list/filter, show detail, pin/unpin, boot (one-shot), hide/unhide,
  inspect GRUB entries.
- **CLI-only**: `create`, `rebuild`, `prune`, `register`, `golden`, `preserve`,
  `extract`.

Destructive-but-reversible verbs (`hide`) are TUI-safe; anything that deletes a
boot artefact is not.

---

## 9. Phases

**P0 — read-only.** Model, discovery, and `list/show/current/lineage/grub-list`
plus `--json` and a read-only screen. No mutating verb.
*Acceptance:* output matches the bash tool's on blade 104 (28 snapshots) and
blade 115 (3), including the three id formulas; `cargo fmt --check`,
`clippy -D warnings`, tests green.

**P1 — menu writes.** `pin`/`unpin`/`boot`/`hide`/`unhide`, then nesting (L3).
This is where the current pain is, and where the `sed` fragility lives.
*Acceptance:* boot test on blade 115 proving a nested entry resolves and
`saved_entry` is not overridden.

**P2 — capture.** `create`/`register`/`rebuild` on the `kbuild` reuse, with the
rescue-retention rules preserved.

**P3 — destructive + retire.** `prune`/`golden`/`preserve`/`extract`, then park
the bash tool on a branch.

Bash stays authoritative until P0's output is provably identical — it is what is
deployed, and the fleet scripts call it by name.

---

## 10. Test plan

- **Pure unit tests**: three id formulas against the verified vectors in §3.1
  (including the empty-component and newline traps); `40_custom` parse→serialize
  round-trip; `grub.cfg` brace-depth nesting; manifest round-trip.
- **Golden comparison**: `list --json` vs the bash tool on 104 and 115.
- **Gates**: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`.

---

## 11. Risks

| risk | mitigation |
|---|---|
| Two writers on `40_custom` / grubenv during transition | single-writer discipline, as with the SMU rule: one tool on PATH per blade |
| A destructive verb reachable from the TUI | §8 split; typed confirm at minimum |
| Rewrite stalls before parity | P0 provides value alone (kills the subprocess dependency, gives JSON) |
| Silent id drift | §3.1 golden vectors in CI, not just self-consistency |
| `snapshot` name collision | `snaptune` primary + argv[0] compat symlink |

---

## 12. Open questions

1. Placement: workspace member (proposed) vs standalone crate vs separate repo?
2. Should the manifest record id inputs (§3.1 improvement), or stay byte-frozen?
3. Does the bash tool get retired, or kept for blades without a Rust toolchain?
4. TUI verb split as proposed in §8?
