# Snaptune: Fail-Safe Kernel Snapshots and Module Management

Snaptune takes the guesswork out of BC-250 kernel development by managing your
snapshots, boot entries, and module verification in one place.

It manages the kernel snapshots of a BC-250 blade: what is on the disk, which one this
boot came from, and the GRUB menu entries that boot them. It is the SNAP tab
(`arieltune snap`), and it is also a standalone binary.

## Key Features

**Fail-safe deployments.** An untested module is never the entry the machine boots
again. `boot` arms a single boot through `next_entry` — which the generated GRUB config
consults whatever `GRUB_DEFAULT` is set to — and the armed value is read back out of
grubenv before anything reboots. An entry GRUB cannot resolve is refused before it is
written, because an unresolvable value does not fail: it boots entry 0, silently. The
worst case for a bad snapshot is a power cycle, not a reflash.

**A module in about a minute, not a kernel build.** `dev` relinks only what changed and
then derives a boot image from an existing one, so iterating on a driver patch does not
cost a full kernel build per attempt.

**Provenance stamped on the artefact.** A snapshot's id is a hash of its module, its
initramfs and its kernel, so the name can be recomputed from the bytes and checked. The
manifest records the build, the source tree and the liberation series the snapshot
carries.

**Verified, not assumed.** `verify` re-derives each id from stored content; `validate`
compares the stored module with the one the kernel actually loaded. The check that earns
its place is the one with no visible symptom otherwise: a boot that succeeds and is not
the snapshot you asked for.

**Portable between blades.** A snapshot moves as a single module: `create --module …
--from …` derives the target blade's own image with that one member replaced, so nothing
of the source tree's state travels with it.

**Scriptable.** Every report verb takes `--json`, and `verify` and `doctor --verify` exit
2 when something is wrong, so a post-reboot script can gate on the result instead of
parsing prose.

## Safety Guarantees

Three things no snaptune verb does, whatever it is passed:

- **The live module tree is never mutated.** Nothing is installed into
  `/usr/lib/modules/<kver>/`. A snapshot's module is stored in the snapshot, and reaches
  a boot image only inside a copy of that image.
- **The running initramfs is never touched.** No verb regenerates the machine's own
  images, and no `mkinitcpio -P` sweep runs over them.
- **The permanent GRUB default is never changed automatically.** Only `pin` sets it, and
  it is a separate, deliberate command.

Together these keep a change inside the snapshot: the running system is never the thing
being modified, so a snapshot that turns out to be bad is discarded rather than
repaired.

---

## 1. Installing & Quick Start

### 1.1 Build and install

Build from the `arieltune` workspace:

```bash
cargo build -p snapshot --bin snaptune              # dev build
cargo build -p snapshot --release --bin snaptune    # release
```

Install on a blade. `snapshot` is another name for this binary (§2):

```bash
scp target/debug/snaptune blade:/var/tmp/snaptune.new
ssh blade 'sudo install -m755 -o root -g root /var/tmp/snaptune.new /usr/local/bin/snaptune \
           && rm -f /var/tmp/snaptune.new && snaptune --version'
```

`./install.sh` in the workspace installs the suite; for the standalone binary the
completion script goes alongside it (§5.10):

```bash
sudo install -d /etc/bash_completion.d
sudo install -m 0644 completions/snaptune.bash /etc/bash_completion.d/snaptune
```

Note `cargo test` and `cargo clippy` do **not** refresh `target/debug/snaptune` — only a
real `cargo build` does. Installing a binary that `cargo test` left behind silently ships
the previous version's behaviour.

Replace a binary that may be running by uploading to `<name>.new` and `mv -f`-ing it over
the target; an in-place `scp` fails with "Text file busy".

### 1.2 Quick start — the 3-step loop

```bash
# 1. Build the module and register it as a bootable snapshot.
sudo snaptune dev "patch 25 variant" \
    --tree ~/linux-cachyos/src/cachyos-7.2.9-2 --run --register

# 2. Boot it once. If it does not come up, the machine reverts by itself.
sudo snaptune boot snap-1a2b3c4d
snaptune validate                 # is the module I am running the one I meant?

# 3. Keep it, once it has earned it.
sudo snaptune pin snap-1a2b3c4d
```

That is the whole shape of the tool: build, boot once, pin. Step 1 relinks only the
module rather than rebuilding a kernel, which is what makes iterating on a driver patch
practical. Nothing reaches the live system, and nothing becomes permanent until step 3.
§5 walks each workflow in full.

### 1.3 Which verbs need root

Everything read-only works as an ordinary user, including `doctor` and `verify`
(confirmed on 104 and 115). Only the writers need `sudo`: `create`, `rebuild`,
`dev --register`, `register`, `boot`, `pin`, `unpin`, `hide`, `unhide`, `golden <id>`,
`prune`.

### 1.4 Pointing at a different store

`--root <dir>` (or `SNAPSHOT_ROOT=`) reads a store from anywhere, with the boot, GRUB and
cmdline paths taken from under it — so a fixture is readable end to end and a whole
workflow can be exercised without touching the real one. That is how `create` and
`rebuild` were validated: a fixture root under `/var/tmp`, which writes nothing outside
it.

---

## 2. Architecture & Core Concepts

**A snapshot is one `amdgpu` module, one initramfs and one kernel, kept as copies
under a name derived from their content.**

Three separate things identify a snapshot, and it is worth keeping them apart:

| | what it is | how you see it |
|---|---|---|
| **the id** | `snap-` + a hash of the *content* | the directory name, the `/boot` filenames, the GRUB entry id |
| **the manifest** | what the writer *claimed* at the moment it wrote | `/var/lib/kernel-snapshots/<id>/manifest.yaml` |
| **the entry** | what GRUB will actually run | the block in `/etc/grub.d/40_custom` |

The id is the only one of the three that cannot drift, which is why it is
content-addressed. A manifest is written once and never read back; a
`40_custom` block is a file that can be edited. The id is a hash, so it can be
recomputed — and `snaptune verify` does exactly that.

### Why this matters on a BC-250

The `amdgpu` module lives in the **early** cpio archive of the initramfs, because
early KMS needs it before the real root is mounted. So "which module did this boot
come from?" is not answerable from the running system: the kernel has no record of
which file in which archive it loaded. It is answerable only from the artefacts,
which is what a snapshot is.

That also fixes the shape of the safe edit: you cannot change the module on a
running system and hope. You make a *different* artefact, name it, and boot it
once.

### Read-only, and not

| | verbs |
|---|---|
| **read-only, safe anywhere** | `list` `show` `current` `lineage` `grub-list` `diff` `golden`(report) `doctor` `verify` |
| **write** (need root) | `create` `rebuild` `dev --run --register` `register` `boot` `pin` `unpin` `hide` `unhide` `golden <id>` `prune` |

Every writing verb takes `--dry-run`, which prints the action plan and touches
nothing. `--json` is global and puts one JSON value on stdout and nothing else — an
object for the report verbs, an array for `list` and `lineage`.

### The names

`snaptune`, `snapshot` and `at` are all the same program. `snapshot <verb>` is the
spelling older scripts use, and it keeps working — every verb in §4 is reachable under
any of the three names.

`snapshot list --ids` is machine-readable on purpose: `arieltune apu` reads it to learn
which snapshot a boot came from, so its output shape is a contract.

---

## 3. Filesystem Layout & Artifacts

Three locations, and every verb is about relating them.

```text
/var/lib/kernel-snapshots/            the store
├── .golden-snapshot                  one id, as text — the baseline
├── known-good-amdgpu.ko.zst          a rescue copy, not written here
├── snap-<id>/
│   ├── manifest.yaml                 the claim
│   ├── manifest.yaml.hidden          `hide` renames it — that IS the hidden flag
│   ├── modules/<name>.ko.zst         this snapshot's own module
│   ├── config/                       mkinitcpio.conf + modprobe.d, shell snapshots
│   ├── firmware/                     cyan_skillfish* blobs
│   └── source/                       the .c/.h it was built from, when there was a tree

/boot/
├── vmlinuz-snap-<id>                 its own copy of a kernel
├── initramfs-snap-<id>.img           its own initramfs
├── snap-<id>.cmdline                 its own command line
├── vmlinuz-linux-cachyos-bore        the distro's kernel, untouched
├── initramfs-linux-cachyos-bore.img  the distro's initramfs, untouched
└── initramfs-rescue-<kver>-<ts>.img  rescue copies, never automatic

/etc/grub.d/40_custom                 one `menuentry` block per snapshot
/boot/grub/grub.cfg                   generated; never edited by hand here
```

A snapshot is **self-contained**: its kernel, its initramfs and its module are
copies. That is why `prune` can delete one without touching anything else, and why
`boot` can be one-shot — the next boot simply falls back to another entry.

---

## 4. Command Reference

| verb | what it does | writes | root |
|---|---|---|---|
| `list [--ids] [--all]` | snapshots, newest first | — | no |
| `show <id>` | one snapshot in full | — | no |
| `current` | the snapshot this boot came from | — | no |
| `lineage [<id>]` | `snapshot.parent` for every snapshot, or one — works un-booted | — | no |
| `grub-list` | GRUB menu entries, marking the nested ones | — | no |
| `diff <a> <b>` | manifest fields and boot artefacts that differ | — | no |
| `golden [<id>]` | report the baseline, or mark one | marker | with id |
| `doctor [--verify]` | is the *store* coherent? Reports a flattened menu, too | — | no |
| `verify [<id>]` | is one *snapshot* what its manifest claims? | — | no |
| `validate [<module>]` | is this *module* patched, and is it the one running? | — | no |
| `extract-modules <id> [dir]` (`extract`) | the amdgpu module out of a snapshot's image | `[dir]` | no |
| `dev <desc>` | plan a dev build; `--run` builds, `--register` registers | `--register` | `--register` |
| `create <desc>` | build or adopt a module, make it a bootable snapshot | store + `/boot` + `40_custom` | yes |
| `rebuild <id>` | rebuild that snapshot's module as a **new** snapshot | same | yes |
| `register --module … --initramfs … --vmlinuz …` | register artefacts you built yourself (`import`) | store + `/boot` + `40_custom` | yes |
| `test-result <id> <name> <value>` (`result`) | record a test result in a manifest | manifest | yes |
| `preserve [label]` (`save`, `backup`) | copy the current initramfs aside as a rescue image | `/boot` | yes |
| `boot <id>` | arm a one-shot boot; `--reboot` also restarts | grubenv | yes |
| `boot --entry <grub-id>` | the same, into *any* GRUB entry — the distro's kernel, LTS, rescue | grubenv | yes |
| `grub-flatten` / `--nest` | a flat, unthemed menu — or the nested one, which is the layout to want | `grub_defaults` + `grub_cfg` | yes |
| `pin <id>` / `unpin` | the permanent GRUB default | grubenv | yes |
| `hide <id>` / `unhide <id>` | off/on the list and the menu; files kept | manifest rename | yes |
| `prune <id>` (`delete`, `rm`) | delete files, directory and entry | everything | yes |

Exit codes: `verify` returns **2** when anything failed; `doctor --verify` returns
2 when anything is broken; `create`/`rebuild`/`dev` return 2 when they refuse to
build. A refusal always writes nothing.

`validate` is the exception that proves the rule: it returns **0** however bad its
news, because the fleet scripts call it for information and a report is not a gate.
Its only hard error is a module it cannot read. `verify` and `doctor --verify` are
the verbs that fail a run.

---

## 5. Operational Workflows

### 5.1 The dev loop — iterate on a module without a kernel build

The loop is seconds, not minutes, because `make M=... modules` relinks only what
changed. Nothing here installs into the live module tree.

```bash
# 1. What would this build? Nothing runs.
snaptune dev "patch 25 variant" --tree ~/linux-cachyos/src/cachyos-7.2.9-2

# 2. Build it, derive an image from the booted snapshot, verify the member bytes.
snaptune dev "patch 25 variant" --tree ~/... --run

# 3. Make it a real snapshot: id, manifest, /boot artefacts, GRUB entry.
sudo snaptune dev "patch 25 variant" --tree ~/... --run --register

# 4. Boot it once. It reverts by itself on the next boot.
sudo snaptune boot snap-1a2b3c4d
```

Step 2's check is the part that matters: the derived image is opened and the member
is compared byte for byte with what the build produced. "The image carries the
module I built" is a fact, not an inference.

`dev` refuses when the tree builds a *different* kernel from the one the parent
image boots, because that module would be refused at load.

### 5.2 A from-source snapshot

```bash
snaptune create "golden-base: patches 1-3" --tree ~/linux-cachyos/src/... --dry-run
sudo snaptune create "golden-base: patches 1-3" --tree ~/linux-cachyos/src/...
```

The snapshot's initramfs is the base image's members with the module member
replaced — not a fresh `mkinitcpio` run. If `/etc/mkinitcpio.conf` gained a hook
since the base image was built, the snapshot will not carry it; choose the base
with `--from` if that matters.

### 5.3 Snapshot the running system, on a blade with no source tree

The normal case for a blade provisioned from packages: the patched module is
already installed and running.

```bash
snaptune create "running: 26/26 applied, 40 CU live" --dry-run
sudo snaptune create "running: 26/26 applied, 40 CU live"
```

The module is adopted **byte for byte** — not re-stripped, not re-compressed.
It is the artefact that has actually booted, and recompressing would make the
snapshot differ from the thing that was validated. The base image is the running
kernel's own initramfs, so the snapshot differs from the running system in exactly
one member.

The first snapshot a store ever holds becomes its baseline (`status: golden`, and
the store's marker names it).

### 5.4 Rebuild a snapshot's module

"I have `snap-11e61755`; my tree has moved on; give me that snapshot's module
rebuilt."

```bash
snaptune rebuild snap-11e61755 --dry-run        # plans against the recorded tree
sudo snaptune rebuild snap-11e61755             # registers a NEW snapshot
```

The original is never written to. The new snapshot's `parent` is the one it came
from, so `lineage` shows the relationship.

It **refuses when the tree already produces the module the snapshot holds**: there
is nothing to rebuild, and saying so beats a second snapshot with identical
content (which the id would refuse as a duplicate anyway). On blade 115 this
distinguished the two cases cleanly — rebuilding `snap-f59f7152` reported "nothing
to rebuild", while rebuilding `snap-11e61755` found real drift
(`ecdb619e…` → `39a2eab4…`).

### 5.5 Test, then promote

Nothing here ever sets the GRUB default on your behalf, because an untested
snapshot must not become the entry the machine falls back to.

```bash
sudo snaptune boot snap-1a2b3c4d     # one boot; reverts by itself
snaptune validate                    # is the module I am running the one I meant?
sudo snaptune pin snap-1a2b3c4d      # now it is the permanent default
```

`validate` earns its place *after* the boot, not before: it is the only check that
compares the file with the running kernel, so it can tell you the boot did not
actually take even when the GRUB entry says it did. Record what you saw with
`test-result` and the snapshot carries its own test history (§5.11). Two
`srcversion`s that differ mean the loaded module is some other build:

```
  live         amdgpu is loaded, srcversion 11F98DC7EE7590D51AA9C9C
  verdict      MISMATCH — the loaded module is NOT this file
               loaded 11F98DC7EE7590D51AA9C9C
               this   B073B0B14D2B2A2BCD3F67A
```

`boot` is one-shot (`grub-reboot` sets `next_entry`), so a snapshot that does not
come up reverts on the following boot with no intervention.

`boot --entry` does the same for an entry that is not a snapshot — the distro's own
kernel, an LTS kernel, a rescue entry:

```bash
sudo snaptune boot --entry 'CachyOS Linux, with Linux linux-cachyos-lts'
```

It takes the `--id` or the exact title. The entry is looked up in `grub.cfg`
**before** anything is written, because `grub-reboot` writes `next_entry` whether or
not GRUB can resolve what it was handed — and an unresolvable value does not fail,
it boots **entry 0** instead, silently, taking the place of a correctly-saved
default. That is not hypothetical: arming a bare nested id on blade 115 booted
"CachyOS Linux".

A nested entry must therefore be named by its full `Parent>Child` path, which is what
GRUB's `get_entry_number()` needs. Its bare `--id` — a real id, present in the file —
is exactly the value that boots entry 0, so it is refused with the path to use
instead. `grub-list` marks the nested ones and prints those paths. An ambiguous name
is refused rather than guessed: a title that appears in two submenus would otherwise
arm whichever comes first in the file.

### 5.6 Triage: `doctor`, then `verify`

`doctor` asks whether the *store* is coherent. `verify` asks whether one
*snapshot* is what its manifest claims. Run them in that order.

```bash
snaptune doctor                      # the whole store's health
snaptune doctor --verify; echo $?    # 2 if anything is broken — usable in a script
snaptune doctor --json               # one object, for a caller

snaptune verify                      # every snapshot; lists only the unclean ones
snaptune verify snap-0848359a        # all five checks for one
snaptune verify --json               # one object, incl. per-check findings
```

On the archive blade this reads as: 28 snapshots, `verify` exits 2, five of them
fail `manifest` — and `doctor` names the same five. Two independently written
checks agreeing is the point.

### 5.7 Retire a snapshot

```bash
snaptune show snap-1a2b3c4d          # confirm it is the one you mean
sudo snaptune prune snap-1a2b3c4d --dry-run
sudo snaptune prune snap-1a2b3c4d
```

`prune` refuses the **booted**, **golden** and **pinned** snapshots. To retire a
booted one, boot something else first; to retire the golden one, move the marker
with `golden <other-id>`.

`hide` is the reversible middle ground: off the list and off the menu, files kept.
It is a rename of `manifest.yaml` to `manifest.yaml.hidden`, so it is also what
"hidden" means when reading a store.

### 5.8 Moving a snapshot between blades

There are two routes.

**Same tree, same config, only the module differs** — which is the rule the whole
fast path rests on. Build the module once and give it to `create`:

```bash
scp blade-a:/path/to/amdgpu.ko.zst /var/tmp/
sudo snaptune create "vanilla twin of the RC" \
    --module /var/tmp/amdgpu.ko.zst \
    --from /boot/initramfs-linux-cachyos-bore.img
```

The snapshot's image is that base image with the module member replaced, so it is
the target blade's own initramfs plus one changed member — no rebuild of anything
else, and none of the source tree's state comes along.

**The running module on a blade that has no tree** — `create` with no `--module`
adopts what the kernel package installed (§5.3).

### 5.9 Handing over artefacts you built yourself

`create` always *derives* an image: it takes a base image and replaces one member.
When the initramfs itself was generated by hand — which is what the fleet's
`FAST-MODULE-SNAPSHOTS.md` describes — there is nothing to derive from, so the
artefacts are registered as they stand:

```bash
sudo snaptune register \
    --module /var/tmp/amdgpu.ko.zst \
    --initramfs /boot/initramfs-linux-cachyos-bore.img \
    --vmlinuz /boot/vmlinuz-linux-cachyos-bore \
    --desc "liberation-7.2.9: 26/26 applied, navi12 SDMA fw"
```

The id is content-addressed over the module, the image and the kernel (§8.1), so
`verify` can re-derive it from the artefacts alone. That was checked on blade 115:
registering `snap-11e61755`'s artefacts reproduced `snap-0848359a` byte for byte.

The kernel release is read **out of the image**, not from `uname -r` — registering
artefacts for a kernel you are not running is the normal case, and the running kernel
is the wrong answer exactly then. `--vmlinuz` pairs a kernel with the initramfs and
is also where the label comes from: `vmlinuz-snap-<id>-v3-ttm-fix` yields
`[v3-ttm-fix] <desc>`, because that filename is the only place the variant is
written down.

### 5.10 Typing snapshot ids: TAB completion

Snapshot ids are `snap-` plus eight hex characters, which nobody wants to type when
`snap-` plus three would do. `completions/snaptune.bash` completes them:

```bash
sudo install -m 0644 completions/snaptune.bash /etc/bash_completion.d/snaptune
exec bash                                    # or: source /etc/bash_completion.d/snaptune
snaptune show snap-a<TAB>                    # -> snap-a7c41d02
snaptune unhide <TAB>                        # every id, hidden ones included
arieltune snapshot show <TAB>                # and through the suite's name
```

The script holds **no list of verbs of its own** — it asks the tool, which answers
from `clap`, so a verb or alias added to the CLI is completable without the script
changing and one removed cannot go on being offered. The hidden `__complete` verb is
that protocol; it is not documented as a verb because it is not one to type.

Two consequences worth knowing:

- **Hidden snapshots complete too.** They have to: `unhide` and `prune` name one, and
  an id that cannot be typed is one that cannot be restored. (`list --ids` shows only
  the visible ones.)
- **A position that wants something else is not given ids.** `dev` and `create` take a
  `DESCRIPTION`, `validate` a `MODULE`, `preserve` a `LABEL` — offering ids there
  would be a wrong answer rather than a helpful one. A test derives the id-typed
  positions from each verb's own `--help` usage (`<ID>` / `[ID]`) and asserts the
  completion agrees with it.

The script registers `snapshot` as well as `snaptune`, because `snapshot` is another
name for this binary, so its completion is this function's job (§2).

### 5.11 Rescue copies, test results, and getting the module back out

Three verbs that are useful around the edges of the loop.

**Before a change you did not build with this tool** — a kernel package update, a
hand-edited `mkinitcpio.conf` — take a copy of the initramfs that is currently proven
to boot:

```bash
sudo snaptune preserve pre-7.3
#   /boot/initramfs-preserve-pre-7.3-20261009-142657.img, 54945380 bytes
```

Nothing ever reads the copy automatically; it is insurance, and the verb reports how
many are already in `/boot` because each is tens of megabytes and nothing prunes them.
Nothing is copied automatically — a copy is only ever made on purpose.

```bash
sudo snaptune test-result snap-11e61755 rocminfo PASS
sudo snaptune test-result snap-11e61755 dmesg_errors INCONCLUSIVE
```

`PASS`, `FAIL` and `INCONCLUSIVE` are the three values, because those are the three the
workflow has a meaning for; `create`'s `NOT TESTED` and `NOT CHECKED` mean there is *no*
result yet, which is not something to record by hand. The manifest is edited, then
**parsed back** and the value read out of the parsed tree before anything is written —
so an edit that broke the YAML, or that did not land at all, refuses instead of
reporting success. Diffing a manifest before and after shows only the line that
changed: the header comment, the field order and every other value survive.

```bash
snaptune extract-modules snap-11e61755 /var/tmp/modules
```

The module inside a snapshot's image is the one that actually booted, which makes it the
right thing to compare against, to hand to `validate`, or to copy to another blade and
give to `create --module`. It comes out of the early archive, which is where mkinitcpio
keeps the modules on these blades.

---

## 6. Rules, Environment, and the Menu Layout

§6.1–6.4 are rules: the things this deliberately does not do, stated because they are
the ones that matter when you are about to trust a snapshot you have not booted. §6.5
and §6.6 are the environment it reads and the menu shape to aim for.

### 6.1 No live-system mutation in `create`

`create` never writes the live module tree or the live initramfs. The module goes into
a *copy* of an image, and the result is verified against the build. So there is no
window in which the running system holds a module that has never booted, and no
`depmod` plus `mkinitcpio -P` sweep over every image on the machine.

### 6.2 No in-place `rebuild`

`rebuild` never writes into the snapshot it names. The original is untouched and the
rebuild is a **new** snapshot, so an id keeps describing the content it was computed
from — which is the whole basis of `verify`.

### 6.3 No automatic rescue copy

Nothing is copied aside unless you ask. `preserve` (§5.11) is the deliberate version:
it exists, it is just never automatic. There is no separate `known-good-amdgpu.ko.zst`
backup to keep in sync either, because the module a snapshot was built from is already
that snapshot's own `modules/amdgpu.ko.zst`, and `extract-modules` hands it back.

### 6.4 Nothing sets the GRUB default

`boot` tries a snapshot exactly once and reverts by itself; `pin` makes it the
permanent default. An untested snapshot never becomes the entry the machine falls
back to.

### 6.5 Environment

| variable | effect |
|---|---|
| `SNAPSHOT_TREE=<path>` | the source tree to build against — the name to prefer |
| `SOURCE_TREE=<path>` | still honoured as an alias. An empty value is ignored, so a script that sets `SOURCE_TREE=""` before filling it in does not blank the setting |
| `SNAPSHOT_KEEP_THEME=1` | `pin` leaves `GRUB_THEME` alone |
| `SNAPSHOT_KEEP_FLAT=1` | accepted and meaningless — nothing here re-shapes the menu implicitly |

There is deliberately **no** variable naming the kernel image a snapshot boots. The
kernel is read off the artefact: `register::vmlinuz_for` derives it from the base
image's own name, and the image's member path states which release that image belongs
to. An override could not be honoured safely — pairing an image with a kernel that is
not its own is precisely the mismatch the rest of the tool refuses, since the module's
`vermagic` would not match the kernel that loads it — and it could not be checked
either, because a `vmlinuz` here carries no greppable `Linux version` banner. Use
`--for <release>` and `--from <image>` instead: both are statements about the snapshot
that `verify` can go back and check.

One ordering fact worth knowing: **`list --ids` prints newest-first.** The order is not
meaningful, but a script that pipes it to `head -1` gets a different snapshot than glob
order would give. `arieltune apu` reads the whole list looking for one id, so it is
unaffected.

### 6.6 The menu layout: nested, not flat

**The nested layout is the one to want**, and the one to run:

```text
CachyOS Linux                                   <- the distro's default kernel
[snap-11e61755] liberation-7.2.9: 26/26 …       <- ours, described, top level
[snap-f59f7152] rebuild of snap-11e61755: …     <- ours, described, top level
Advanced options for CachyOS Linux >
    with Linux linux-cachyos-bore               <- 10_linux's twins, out of the way
    with Linux snap-11e61755                    <- one per vmlinuz-snap-*, hidden
```

Flattening puts every `10_linux` twin at the top level instead, so each snapshot
appears **twice** and the twin is the worse of the pair — no usable `--id`, no
`BOOT_IMAGE=`, and nothing to tell you which is which. `10_linux` enumerates every
`/boot/vmlinuz-*` with no exclusion list, which is why there is a twin per snapshot at
all; with 26 snapshots, a flattened menu is 52 rows of snapshot entries.

Nothing re-shapes the menu on its own. `grub-flatten` is the explicit opt-in,
`grub-flatten --nest` is the way back, and `doctor` will name a flattened menu, count
the duplicates it promoted, and tell you which command to run.

---

## 7. Reading a `verify` Report

Five checks, in the order they appear. An `ok` means the check ran and passed; a
`warn` can also mean *it could not run*, and the detail says which.

| check | the question | a failure means |
|---|---|---|
| `manifest` | does it parse? | everything below would come from a line scan, so it stops here |
| `files` | are the files it names present? | the manifest and the disk disagree; an entry will point at nothing |
| `module` | is the stored module the one recorded? | the code in the snapshot is not the code the manifest claims |
| `entry` | does the GRUB entry boot *this* snapshot? | a boot that succeeds and is not the one you asked for — the failure with no symptom |
| `identity` | does the id match the content? | the module, image or kernel has been swapped since it was named |

`identity` is the strongest and the only one that can notice a quiet module swap: it
recomputes the id from the stored bytes. It runs only for kinds whose id *is* the
content (§8.1) and says "not checked" otherwise — which on an older snapshot is the
expected answer, not a problem.

---

## 8. Backward Compatibility & Legacy Contracts

The ids on disk, the manifest field names and the path conventions predate this tool,
and they are contracts with snapshots that already exist on real blades. They cannot be
changed without orphaning every one of them, so this section records them as they are
and this tool reproduces them byte for byte rather than tidying them up - including the
forms it would not write today.

This is why adding a snapshot kind, a manifest field or a boot path means reading here
first. Nothing in §1–§7 depends on it.

### 8.1 The three id formulas

There are three, they are not interchangeable, and which one produced an id is
recorded in the manifest as `snapshot.kind`.

| | made by | formula |
|---|---|---|
| **F1** | a `create` that had a source tree | `md5("<hash of 4 .c files>-<symvers8>" + "\n")` |
| **F2** | a `create` with no source tree | `md5("<kver>-<module8>")` — **no** trailing newline |
| **F3** | `register` / `import`, and this tool's `dev` / `create` / `rebuild` | `md5("<mod8>-<initrd8>-<vmlinuz8>-<symvers8>" + "\n")` |

Three consequences worth stating plainly:

1. **F1 and F2 hash inputs that are not the artefact.** F1 hashes *source files*,
   F2 hashes the *installed* module. Neither is reproducible once the tree or the
   installed module changes. That is why a stale source tree could previously
   recompute an OLD snapshot's id and clobber it, and why this tool uses F3
   everywhere.
2. **The same content can carry two ids.** Blade 115's `snap-11e61755` (a `create`)
   and blade 104's `snap-0848359a` (a `register`) hold byte-identical content —
   module `ecdb619e`, initramfs `f21daf6a`, kernel `ddb9f7cc` — and differ only
   because the formulas differ. Neither is wrong and neither has drifted.
3. **An id can only be re-derived for content-addressed kinds.** `verify` recomputes
   the id only when `kind` is one this tool writes (`dev-build`, `created`,
   `rebuilt`); for any other kind it says "not checked" and means it. Reporting
   that as a mismatch would be a false alarm on a perfectly good snapshot.

`manifest::id_is_content_addressed` is the single place that decides this.

### 8.2 Manifest field names

`manifest.yaml` **field types vary**:
`size: 5823372 bytes` is a string, `markers: 0` an integer, `files: []` a list,
and `kernel.version` is sometimes the short `7.0.9` where the id was computed with
`7.0.9-1-cachyos`. `Manifest` therefore parses into a `Value` tree and exposes
*coercing* accessors, so a snapshot captured correctly is never rejected over its
formatting. Unknown fields are preserved.

Two accessors deliberately behave differently from the rest:

- `source_tree_path()` has **no line-scan fallback**. The fallback takes the first
  line *starting* with the key, and `module.path` is written before
  `source_tree.path` in every manifest written this way — so a falling-back scan
  would answer with the module's own path, plausibly and wrongly.
- `cmdline()` is **not safe to boot with**. It is the *live* cmdline at capture
  time, can name whichever kernel was booted then, and can contain `BOOT_IMAGE=`
  twice — `snap-da794092` does, and so do 19 of the 28 snapshots in the archive
  store, so this is the normal shape of the field rather than a quirk of one
  manifest. Regenerate it; do not reuse it.

### 8.3 Path conventions

The names are load-bearing in both directions:

```text
/var/lib/kernel-snapshots/snap-<id>/manifest.yaml
/var/lib/kernel-snapshots/snap-<id>/manifest.yaml.hidden   # hide = rename
/var/lib/kernel-snapshots/.golden-snapshot                 # id as text
/boot/vmlinuz-snap-<id>
/boot/initramfs-snap-<id>.img
/boot/snap-<id>.cmdline
```

An id already starts with `snap-`: the artefacts are `/boot/vmlinuz-<id>` and
`/boot/initramfs-<id>.img`, with **no second `snap-`**. The pairing runs both ways
(`register::vmlinuz_for`), which is why the kernel for a snapshot is derived from
its initramfs name rather than guessed.

The distro's own files are named after the **package**, not the release: the kernel
in `/boot` for release `7.2.9-1-cachyos-bore` is
`initramfs-linux-cachyos-bore.img`, which `/usr/lib/modules/<kver>/pkgbase` names.
Guessing `initramfs-<release>.img` finds nothing.

### 8.4 The GRUB entry shape

One `menuentry` per snapshot in `/etc/grub.d/40_custom`, with `--id '<snap-id>'`,
a `search --fs-uuid` line, a `linux` line naming this snapshot's own vmlinuz, and
an `initrd` line chaining `/boot/acpi_override.cpio` when it exists.

Two rules:

- **Append, never replace.** The file holds every other snapshot's entry;
  writing only the new one deletes them all.
- **Artefacts before `grub-mkconfig`.** The distro's `10_linux` generates a menu
  entry for *every* `/boot/vmlinuz-*`, so a snapshot's files must be deleted
  *before* the config is regenerated, or the menu keeps an entry pointing at a
  kernel that is gone. `prune` encodes that order and is tested on it.

And two traps.

**`grub-reboot` exits 0 while changing nothing GRUB will honour.** It writes
`next_entry` whether or not the value resolves, and an unresolvable value does not
fail: GRUB boots **entry 0** instead, silently, taking the place of a correctly-saved
default. Confirmed on blade 115, where arming a bare nested id booted "CachyOS Linux".
`boot` therefore resolves the entry against `grub.cfg` before writing anything, and
reads the armed value back out of grubenv before it reports success.

**`GRUB_DEFAULT=saved` is what `pin` needs, not what `boot` needs.** `/etc/grub.d/00_header`
emits the `next_entry` block whatever `GRUB_DEFAULT` is set to, so a one-shot `boot` is
honoured either way. `pin` writes `saved_entry`, and without `GRUB_DEFAULT=saved`
grub.cfg ignores it: `pin` reports success and changes nothing. `doctor` checks for
exactly this and names it.
