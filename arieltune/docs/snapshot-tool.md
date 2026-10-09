# snaptune — the snapshot tool: model, contracts, and workflows

`snaptune` (also reachable as `arieltune snapshot`) manages the kernel snapshots
on a BC-250 blade: what is on the disk, which one this boot came from, and the
GRUB menu entries that boot them. It is a port of `bc-250-snapshotter/snapshot`,
and it is now the authority for everything the shell tool used to do.

This document is the design and invariants reference. The module headers assume
it; §3 in particular is the part that must not be "improved" while snapshots
written by the shell tool still exist.

---

## 1. The mental model

**A snapshot is a promise that a named triple — one `amdgpu` module, one
initramfs, one kernel — boots.** Everything else in this tool exists to keep that
promise checkable.

Three separate things identify a snapshot, and it is worth keeping them apart:

| | what it is | how you see it |
|---|---|---|
| **the id** | `snap-` + a hash of the *content* | the directory name, the `/boot` filenames, the GRUB entry id |
| **the manifest** | what the writer *claimed* at the moment it wrote | `/var/lib/kernel-snapshots/<id>/manifest.yaml` |
| **the entry** | what GRUB will actually run | the block in `/etc/grub.d/40_custom` |

The whole reason the id is content-addressed is that it is the only one of the
three that cannot drift. A manifest is written once and never read back; a
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

---

## 2. What is on disk

Three locations, and every verb is about relating them.

```text
/var/lib/kernel-snapshots/            the store
├── .golden-snapshot                  one id, as text — the baseline
├── known-good-amdgpu.ko.zst          the shell tool's safety copy (not written here)
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
└── initramfs-rescue-<kver>-<ts>.img  the shell tool's preserved copies

/etc/grub.d/40_custom                 one `menuentry` block per snapshot
/boot/grub/grub.cfg                   generated; never edited by hand here
```

A snapshot is **self-contained**: its kernel, its initramfs and its module are
copies. That is why `prune` can delete one without touching anything else, and why
`boot` can be one-shot — the next boot simply falls back to another entry.

---

## 3. The contracts with existing snapshots

Everything in this section is a **contract with snapshots that already exist on
the blades**. The ids on disk, the manifest field names and the path conventions
cannot be changed without orphaning real snapshots, so this tool reproduces them
byte for byte rather than tidying them up.

### 3.1 The three id formulas

There are three, they are not interchangeable, and which one produced an id is
recorded in the manifest as `snapshot.kind`.

| | made by | formula |
|---|---|---|
| **F1** | the shell's `create`, with a source tree | `md5("<hash of 4 .c files>-<symvers8>" + "\n")` |
| **F2** | the shell's `create`, no source tree | `md5("<kver>-<module8>")` — **no** trailing newline |
| **F3** | `register` / `import`, and this tool's `dev` / `create` / `rebuild` | `md5("<mod8>-<initrd8>-<vmlinuz8>-<symvers8>" + "\n")` |

Three consequences worth stating plainly:

1. **F1 and F2 hash inputs that are not the artefact.** F1 hashes *source files*,
   F2 hashes the *installed* module. Neither is reproducible once the tree or the
   installed module changes. That is why the shell's own comment admits "a stale
   source tree used to be able to recompute an OLD snapshot's id and clobber it",
   and why this tool uses F3 everywhere.
2. **The same content can carry two ids.** Blade 115's `snap-11e61755` (a `create`)
   and blade 104's `snap-0848359a` (a `register`) hold byte-identical content —
   module `ecdb619e`, initramfs `f21daf6a`, kernel `ddb9f7cc` — and differ only
   because the formulas differ. Neither is wrong and neither has drifted.
3. **An id can only be re-derived for content-addressed kinds.** `verify` recomputes
   the id only when `kind` is one this tool writes (`dev-build`, `created`,
   `rebuilt`); for a shell snapshot it says "not checked" and means it. Reporting
   that as a mismatch would be a false alarm on a perfectly good snapshot.

`manifest::id_is_content_addressed` is the single place that decides this.

### 3.2 Manifest field names

`manifest.yaml` is emitted by the shell as a heredoc, so **field types vary**:
`size: 5823372 bytes` is a string, `markers: 0` an integer, `files: []` a list,
and `kernel.version` is sometimes the short `7.0.9` where the id was computed with
`7.0.9-1-cachyos`. `Manifest` therefore parses into a `Value` tree and exposes
*coercing* accessors, so a snapshot captured correctly is never rejected over its
formatting. Unknown fields are preserved.

Two accessors deliberately behave differently from the rest:

- `source_tree_path()` has **no line-scan fallback**. The fallback takes the first
  line *starting* with the key, and `module.path` is written before
  `source_tree.path` in every manifest the shell produces — so a falling-back scan
  would answer with the module's own path, plausibly and wrongly.
- `cmdline()` is **not safe to boot with**. It is the *live* cmdline at capture
  time, can name whichever kernel was booted then, and can contain `BOOT_IMAGE=`
  twice (`snap-da794092` does). Regenerate it; do not reuse it.

### 3.3 Path conventions

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

### 3.4 The GRUB entry shape

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

And one trap: **`grub-reboot` exits 0 while changing nothing.** `next_entry` is
only honoured when `grub.cfg` was generated with `GRUB_DEFAULT=saved`. `boot` reads
the armed value back out of grubenv before it reports success, and will not reboot
unless that check passed.

### 3.5 The name: `snaptune`, not `snapshot`

The binary is `snaptune` for two reasons: `snapshot` is an unrelated program on the
workstation (the GNOME screenshot tool answers `Unknown option --ids`), and the name
joins the fleet's per-namespace convention.

**`/usr/local/bin/snapshot` on the blades is the shell tool, and stays that way.**
Checked on 104 and 115: both have a 108 KB `snapshot` there (with
`/etc/bash_completion.d/snapshot` on 104 and the repo checkout at
`/srv/data/bc-250-snapshotter/`), so a compat symlink to this binary cannot be
created without displacing the tool it would be replacing. `snaptune` is installed
alongside it instead, as its own binary, and the two coexist:

```text
/usr/local/bin/snapshot    the shell tool, unchanged
/usr/local/bin/snaptune    this tool
```

The fleet convention is one multi-call `arieltune` plus symlinks per namespace
(`aputune`, `biostune`, `memtune`, `wikitune`, `at` all point at it). This tool is
a **standalone binary** rather than another such symlink, because the installed
`arieltune` has no `snapshot` namespace yet — building and installing a new
`arieltune` would replace the binary the other modules are served from. When that
happens, `snaptune` becomes a symlink to `arieltune` and `arieltune snapshot <verb>`
starts working; until then, `snaptune <verb>` is the only entry point.

Retiring the shell tool is therefore two deliberate steps, not one: install
`arieltune` with the namespace, then replace `/usr/local/bin/snapshot` with the
compat symlink and check the fleet scripts. §6 lists the verbs that would need
porting before that step is safe.

---

## 4. The verbs

| verb | what it does | writes | root |
|---|---|---|---|
| `list [--ids] [--all]` | snapshots, newest first | — | no |
| `show <id>` | one snapshot in full | — | no |
| `current` | the snapshot this boot came from | — | no |
| `lineage` | follow `snapshot.parent` from the booted snapshot | — | no |
| `grub-list` | GRUB menu entries, marking the nested ones | — | no |
| `diff <a> <b>` | manifest fields and boot artefacts that differ | — | no |
| `golden [<id>]` | report the baseline, or mark one | marker | with id |
| `doctor [--verify]` | is the *store* coherent? | — | no |
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
| `grub-flatten` / `--restore` | a flat, unthemed menu — or the distro's submenus back | `grub_defaults` + `grub_cfg` | yes |
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

## 5. Workflows

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

There are two routes, and neither needs the shell tool.

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

The id is content-addressed over the module, the image and the kernel, using the
formula the shell tool already used (§3.1), so a snapshot registered here carries
the id the shell would have given it and `verify` can re-derive it. That was checked
on blade 115: registering `snap-11e61755`'s artefacts reproduced the shell's
`snap-0848359a` byte for byte.

The kernel release is read **out of the image**, not from `uname -r` — registering
artefacts for a kernel you are not running is the normal case, and the running kernel
is the wrong answer exactly then. `--vmlinuz` pairs a kernel with the initramfs and
is also where the label comes from: `vmlinuz-snap-<id>-v3-ttm-fix` yields
`[v3-ttm-fix] <desc>`, because that filename is the only place the variant is
written down.

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
The shell tool took copies like this internally, before it overwrote a live module —
this one never overwrites, so a copy is only ever made on purpose.

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

## 6. What this deliberately does not do

Three of the shell tool's behaviours are **not** reproduced, on purpose (§6.1–6.3).
The fourth item is not a difference — it is a property both tools share, written
down because it is the one that matters most if you are about to trust a snapshot
you have not booted (§6.4). §6.5 maps every verb the shell tool had onto its
equivalent here — which is the thing to read before retiring it.

### 6.1 No live-system mutation in `create`

The shell's `create` installs the new module into `/usr/lib/modules/<kver>/`, runs
`depmod`, generates the snapshot's initramfs with `mkinitcpio -g`, then restores the
known-good module and rebuilds *every* image on the machine with `mkinitcpio -P`.
Between the install and the restore, the running system's module tree holds a module
that has never booted; if that command dies in the window, the machine is left that
way. Here the module is put into a *copy* of an image and the result is verified
against the build. The live module tree and the live initramfs are never written.

### 6.2 No in-place `rebuild`

The shell writes the rebuilt module into the snapshot it names and keeps the id — so
the id stops describing the content, which is exactly the mismatch `verify` reports.
It then tells you to run `create`, which makes a new id anyway. This refuses to
stage: the original is untouched and the rebuild is a new snapshot.

### 6.3 No `known-good-amdgpu.ko.zst` backup, and no automatic rescue copy

The shell keeps both because it mutates the live initramfs and the live module, so a
failure part-way through leaves them wrong and needs a way back. This does not mutate
either, so it has nothing to rescue on its own behalf, and it does not make copies
nobody asked for.

`preserve` (§5.11) is the deliberate version of the rescue copy: it exists, it is just
never automatic. The `known-good-amdgpu.ko.zst` backup has no equivalent, because the
module a snapshot was built from is already kept as that snapshot's own
`modules/amdgpu.ko.zst`, and `extract-modules` will hand it back.

### 6.4 Nothing sets the GRUB default

`boot` tries a snapshot exactly once and reverts by itself; `pin` makes it the
permanent default. An untested snapshot never becomes the entry the machine falls
back to.

### 6.5 Every shell verb, and where it went

Read from the shell tool's own `snapshot --help`, so this is its list and not a
recollection of it. This is the table to work from before retiring `snapshot`.

| shell | here | note |
|---|---|---|
| `list`, `ls` | same + `ls` | |
| `current`, `status`, `which` | same, all three | |
| `show`, `info` | same + `info` | |
| `lineage`, `tree` | same + `tree` | |
| `diff` | same | reads the manifests, so it works with no source tree |
| `boot`, `reboot <id>` | `boot <id> [--reboot]` | **differs**: see below |
| `boot --entry <grub-id>` | same | nested entries by path, which the shell refused |
| `grub-flatten` | same (+ `--restore`) | re-reads `grub.cfg` and counts what is left |
| `grub-list`, `grublist`, `grub-entries`, `grub` | same, all four | |
| `pin`, `default`, `set-default` | same, all three | |
| `unpin` | same | |
| `verify`, `check` | same + `check` | exits 2 on failure |
| `validate`, `markers`, `check-module` | same, all three | plus the loaded-`srcversion` comparison |
| `create <desc>` | same | **differs**: see below |
| `register` | same (+ `import`) | same content-addressed id |
| `rebuild <id>` | same | **differs**: see below |
| `golden`, `gold` | same + `gold` | |
| `preserve`, `save`, `backup [label]` | same, all three | |
| `extract`, `extract-modules`, `extract-module <id> [dir]` | same, all three | |
| `prune`, `delete`, `rm <id>` | same, all three | |
| `hide` / `unhide` | same | |
| `test-result`, `result <id> <t> <r>` | same + `result` | plus the read-back check |
| `help`, `usage`, `--help`, `-h` | `--help`, `-h` | |

Environment, from the shell's own help and its `README.md`:

| shell | here |
|---|---|
| `SOURCE_TREE=<path>` | still honoured, and `SNAPSHOT_TREE` is the name to prefer. An empty value is ignored, because `bootstrap.sh` sets `SOURCE_TREE=""` before filling it in |
| `SNAPSHOT_KEEP_THEME=1` | same |
| `SNAPSHOT_KEEP_FLAT=1` | **no equivalent, and no need** — see below |
| `PINNED_VMLINUZ=<path>` | **no equivalent** — see below |

Four verbs are ported but **deliberately not identical**, and none of the four is a
gap:

- **`boot` does not reboot.** The shell's `boot` and `reboot` both arm *and* restart.
  Here `boot <id>` arms only, and `--reboot` restarts — because arming is verified
  against grubenv first, and a machine that goes down before that check is a machine
  that boots whatever the default happened to be. `reboot` is therefore not offered as
  an alias: the name would promise a restart the verb does not perform.
- **`create` does not mutate the live system** (§6.1), so its initramfs is derived
  rather than regenerated. Same tree, same toolchain, same config, one member changed.
- **`rebuild` does not overwrite** (§6.2): it registers a new snapshot and leaves the
  original alone, so the id keeps describing the content.
- **Nothing here changes the menu's shape on its own.** The shell's `pin` also restores
  the distro's GRUB submenus and comments out the theme, as a side effect of pinning.
  Those are `grub-flatten` and `grub-flatten --restore` here, so a pin pins. That is
  also why `SNAPSHOT_KEEP_FLAT=1` has no meaning: it exists to stop the shell's implicit
  un-flattening, and there is no implicit un-flattening to stop.

  Flattening deserves the warning the shell learned the hard way: `10_linux` enumerates
  every `/boot/vmlinuz-*` with no exclusion list, so it emits an entry for each
  `vmlinuz-snap-*` as well, and those live inside the distro's "Advanced options"
  submenu. Flattening promotes all of them to the top level — on a store with 26
  snapshots that is 26 extra rows, and the `10_linux` twin of each snapshot is the
  *worse* of the pair, with no `--id` and no `BOOT_IMAGE=`. Blade 115's menu is
  already in that state, which is why it lists each snapshot twice. `--restore` is the
  way back.

`PINNED_VMLINUZ` is the one environment variable with no home here. It names the
kernel image a snapshot boots; the kernel is derived from the release's `pkgbase`,
which is what the shell's *default* did too. Only a non-default override has nowhere
to go, and expressing it would mean a `--vmlinuz` on `create` — a change to what
`create` guarantees about the kernel and image it pairs, not an alias. Named here so
it is a decision rather than an oversight.

---

## 7. Reading a `verify` report

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
content (§3.1) and says "not checked" otherwise — on most existing snapshots, which
the shell wrote, that is the expected answer rather than a problem.

---

## 8. Installing

Build from the `arieltune` workspace:

```bash
cargo build -p snapshot --bin snaptune              # dev build
cargo build -p snapshot --release --bin snaptune    # release
```

Install on a blade — `snaptune` only, never `/usr/local/bin/snapshot` (§3.5):

```bash
scp target/debug/snaptune blade:/var/tmp/snaptune.new
ssh blade 'sudo install -m755 -o root -g root /var/tmp/snaptune.new /usr/local/bin/snaptune \
           && rm -f /var/tmp/snaptune.new && snaptune --version'
```

Note `cargo test` and `cargo clippy` do **not** refresh `target/debug/snaptune` —
only a real `cargo build` does. Installing a binary that `cargo test` left behind
silently ships the previous version's behaviour.

Replace a binary that may be running by uploading to `<name>.new` and `mv -f`-ing it
over the target; an in-place `scp` fails with "Text file busy".

### Which verbs need root

Everything read-only works as an ordinary user, including `doctor` and `verify`
(confirmed on 104 and 115). Only the writers need `sudo`: `create`, `rebuild`,
`dev --register`, `register`, `boot`, `pin`, `unpin`, `hide`, `unhide`,
`golden <id>`, `prune`.

### Pointing at a different store

`--root <dir>` (or `SNAPSHOT_ROOT=`) reads a store from anywhere, with the boot,
GRUB and cmdline paths taken from under it — so a fixture is readable end to end and
a whole workflow can be exercised without touching the real one. That is how
`create` and `rebuild` were validated: a fixture root under `/var/tmp`, which writes
nothing outside it.
