# apu shaders

## `kat.comp` — CU health-test known-answer kernel

One shader: a deterministic integer chain, seeded by global invocation id, that
each invocation runs over its own slot. All arithmetic is `u32` and wraps, and
there is no floating point, so a CPU reference reproduces the result exactly —
which is the whole point. A defective ALU in the routed shader array shows up as
a checksum mismatch, a hang as device-lost, and a weak CU as low throughput.

The CPU reference lives in `../src/cutest.rs` and must match this kernel
bit-for-bit. Changing the arithmetic here without changing it there will make the
test report a fault that does not exist, so treat the two as one unit.

## Building

```sh
glslc -fshader-stage=comp -O kat.comp -o kat.spv
```

`make shaders` from the `arieltune/` root rebuilds it; `make shaders-check`
verifies the committed blob still matches.

**The `-O` is load-bearing, not a preference.** With it, `glslc` runs the output
through `spirv-opt`, which is what produces the `OpPhi`-based body the committed
blob has. Compiling without it yields different bytes from identical source. Note
this differs from `crates/mem/src/shaders/`, whose kernels are committed
*unoptimized* — each blob reproduces only with the flags it was built with,
which is why the Makefile carries them per directory.

`kat.spv` is embedded by `src/cutest.rs` via `include_bytes!("../shaders/kat.spv")`
— deliberately from here rather than a second copy under `src/`, so the source
and the artifact it produces cannot drift apart unnoticed.
