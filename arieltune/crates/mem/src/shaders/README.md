# mem shaders

The five Vulkan compute kernels the memory benchmark dispatches. `bench.rs`
embeds each `.spv` with `include_bytes!`, so the compiled blob is what actually
runs on the GPU; the `.comp` file beside it is its source.

| source | what it does |
|---|---|
| `bandwidth.comp` | grid-stride `vec4` read of an SSBO far larger than L2, repeated `iters` times. Only lane 0 writes, so traffic is ~pure read. |
| `random.comp` | scattered reads over a shuffled index — the figure that actually responds to timing changes |
| `latency.comp` | pointer-chase over a random permutation |
| `stab_write.comp` | writes a known pattern across the buffer |
| `stab_check.comp` | reads it back and counts mismatches |

**Editing a `.comp` does not change the binary.** The `.spv` is embedded, not
produced by the build: compiling SPIR-V at build time would make `cargo build`
require shaderc, which is a steep dependency for a tuning tool that most users
install to read sensors. Rebuild deliberately instead.

## Rebuilding

Compiled with **`glslc`** (shaderc):

```sh
glslc -fshader-stage=comp bandwidth.comp -o bandwidth.spv
```

`make shaders` runs that across every `.comp` here. `make shaders-check`
recompiles and compares against the committed `.spv`.

## Why glslc and not glslangValidator

`glslc` enables `GL_GOOGLE_include_directive` and
`GL_GOOGLE_cpp_style_line_directive` by default, so its output carries two extra
`OpSourceExtension` entries and a different generator word. `glslangValidator -V`
produces the *same instructions* from the same source but different bytes, so a
byte comparison against the committed blobs only passes with `glslc`.

The generator word also records the tool version, so a different shaderc release
will produce different bytes for identical input. If `make shaders-check` fails
after a shaderc upgrade, compare structurally before assuming the source drifted:

```sh
spirv-dis a.spv -o - | grep -v '^; Generator' | grep -v OpSourceExtension
```

## Provenance

These sources came from `bc250-memtune`, the standalone predecessor of the `mem`
tab. They were never present in this repository's history: the `.spv` blobs were
committed without their source, leaving the kernels unrebuildable and
unauditable. Recovered 2026-10-06 and verified by recompiling — every `.comp`
here reproduces its committed `.spv` byte for byte, which is what makes this
directory's source authoritative rather than approximate.
