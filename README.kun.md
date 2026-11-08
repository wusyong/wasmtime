# Wasmtime (kun's fork)

| | |
| --- | --- |
| Upstream | https://github.com/bytecodealliance/wasmtime (fetched by URL; no remote) |
| Fork | https://github.com/wusyong/wasmtime (remote `origin`) |
| Branch | `kun`, from upstream's tag `v50.0.1` (`c307ec81de`); the fork's `main` is upstream's, untouched |
| License | Apache-2.0 WITH LLVM-exception (`LICENSE`) |
| Used by | kun's `dom_wasm` (by path, version pinned), with `gc-elise` on and the default collector: kun doesn't choose elise yet. Elise is beside it (`../elise`, by path), the one kun builds too |

## Why a fork

To find out whether Wasmtime can take elise, kun's GC, as its collector,
and whether a store's GC heap can be the DOM's heap: wasm GC objects and
DOM nodes in one heap, one GC. Upstream has no way to plug in a collector
from outside; the collectors are modules of the `wasmtime` crate.

## Status

- Elise is a collector (`Collector::Elise`, cargo feature `gc-elise`): the
  host's API (`StructRef`, `ArrayRef`, `ExternRef`, `AnyRef::from_i31`, GC
  references in globals and tables) and compiled code (Cranelift's, native
  or for Pulley; Winch refuses elise) allocate, read and write its objects.
  Upstream's GC tests (`tests/all`'s and the spec tests of GC types) pass
  with it.
- It collects, keeping what the store's roots (the host's `Rooted`s,
  globals, tables, wasm frames' stack maps) reach. An `externref`'s host
  data is dropped once elise finds its object dead.
- There is no heap limit: elise panics when its cage is full, and a store's
  `ResourceLimiter` doesn't apply.
- Works with elise's commit noted in the last commit of `kun` that changed
  `crates/wasmtime/src/runtime/vm/gc/enabled/elise.rs` (elise's
  `raw::EngineHeap`, uncommitted as this is written).
- Built with the toolchain elise pins or newer (1.99; Wasmtime's own
  `rust-version` is 1.97).

## How elise sits in Wasmtime

- elise's heap (`raw::EngineHeap`) is its own 4 GiB cage, and the GC heap:
  the memory Wasmtime gives `GcHeap::attach` is only held, and given back.
  `vmmemory()` is the cage, 4 GiB long; a `VMGcRef` is an object's offset
  in it, the bits elise's edges hold, and an `i31ref` is encoded as elise's
  `I31` is. Most of the cage is reserved, not committed, so no slice of all
  of it is made: the heap reads and writes an object at a time.
- The data of an object starts with its `VMGcHeader`, after elise's own
  header (`crates/environ/src/gc/elise.rs`): 8 bytes more per object than
  one header would take.
- Elise trusts the GC references in its heap, as its GC does: a corrupt
  one is read where it points, not checked against the heap's bounds as
  the other collectors' are (it must be in the cage and 8-aligned).
- Each GC type is registered in elise as the engine's type registry
  registers it, and retired as the registry lets it go; ids come from
  ranges of 256 an engine reserves as it needs them.
- A pooling allocator's heap drops its elise heap when detached, and the
  next store's attach makes a new one.
- `GcHeap::gc` runs one GC of elise's, given the roots: what elise is due
  to do (`safepoint_with_roots`: a scavenge, or a step of its concurrent
  marking) if it wants a GC (an allocation failed), a full GC if not (the
  embedder asked: `Store::gc`). Each root is
  written back, objects having moved. One increment does it all.
- Each `externref` gets an elise id; after each GC the heap drops the host
  data of those whose ids died. Asking doesn't keep an object (unlike
  `by_id`), so an `externref` dies at the end of a marking as other objects
  do.
- `Config::elise_heap` gives elise's heap options and platform (where its
  concurrent marking runs); by default they're elise's defaults and
  `InlinePlatform`. `wasmtime::elise` re-exports elise for them.
- An object's align is 8, its data's (elise's objects are): a `v128` field
  stays where the common layouts put it, 16-aligned from the data's start,
  8-aligned in memory. Compiled code's GC field accesses carry no `aligned`
  flag and the host's copy bytes, so neither needs more.

## Elise's compiled code

The compiler is `crates/cranelift/src/func_environ/gc/elise.rs`.

- Allocation goes through the `gc_alloc_raw` libcall, as the DRC
  collector's: elise's nursery isn't bumped inline.
- Reads take no barrier; GC references live across a call are in stack
  maps, and elise moves objects, writing the frames' references back.
- `VMContext::gc_heap_data` points at elise's barrier state
  (`elise::raw::BarrierState`: the young pages' flags' address and the
  marking flag; offsets in `crates/environ/src/gc/elise.rs`, checked against
  elise's at compile time). A write of a GC object's reference field checks
  them inline:
  - while a marking runs, the field's old value goes to the `elise_satb`
    libcall (snapshot at the beginning);
  - if the new value is a young object and the field isn't on a young page,
    the field goes to the `elise_remember` libcall (generational).
  A field initialized in an object just allocated takes the generational
  barrier only (a large array is old when allocated); an `i31` field takes
  neither.
- A root's write (a table's, a global's) takes no barrier: elise's GCs are
  given the roots. Upstream compiles fields' and roots' writes with one
  hook, `GcCompiler::translate_write_gc_reference`; the fork adds
  `translate_write_gc_field`, which `write_field_at_addr` calls, defaulting
  to the first for the other collectors.
  **On every rebase, look for new paths that write a GC object's reference
  field**: one calling `translate_write_gc_reference` instead of going
  through `write_field_at_addr` takes no barrier with elise, which loses
  objects silently.

## Local modifications

Each is marked with a `kun:` comment.

- `crates/environ`: `Collector::Elise`; the feature `gc-elise`; elise's
  layouts and barrier state's offsets (`src/gc/elise.rs`); the builtins
  `elise_satb` and `elise_remember`, and `gc_alloc_raw` for elise too
  (`src/builtin.rs`).
- `crates/cranelift`:
  - `src/func_environ/gc.rs`: `GcCompiler::translate_write_gc_field`,
    which `write_field_at_addr` calls; elise's compiler.
  - `src/func_environ/gc/elise.rs` (new): elise's compiler.
  - `src/alias_region.rs`: alias regions for elise's barrier state and
    young pages' flags.
  - `build.rs`, `Cargo.toml`: the `gc-elise` builtins, elise's layouts.
- `winch/codegen/src/codegen/mod.rs`, `gc.rs`: Winch refuses to compile
  for elise; `build.rs`: the `gc-elise` builtins.
- `crates/wasmtime`:
  - `Cargo.toml`: the feature `gc-elise`, and elise by path.
  - `src/config.rs`: `Collector::Elise` (never `Auto`), its runtime,
    `Config::elise_heap`.
  - `src/lib.rs`: `pub use elise`.
  - `src/runtime/vm/gc/enabled/elise.rs` (new), `enabled.rs`: the
    collector, and unit tests of its type table.
  - `src/runtime/vm/gc/gc_runtime.rs`: `GcRuntime::register_type` and
    `unregister_type`; `GcHeap::write_gc_ref_in`, `init_gc_ref_in` (a
    barrier given the object, which a generational collector needs) and
    `capacity`. All have defaults the other collectors keep.
  - `src/runtime/vm/gc/gc_ref.rs`: `write_val` and `initialize_val` write
    an object's GC references through `write_gc_ref_in` and
    `init_gc_ref_in`.
  - `src/runtime/vm/gc.rs`: `gc_heap_capacity` asks the heap.
  - `src/runtime/vm/libcalls.rs`: the libcalls `elise_satb` and
    `elise_remember`, and `gc_alloc_raw` for elise too.
  - `src/runtime/type_registry.rs`, `src/engine.rs`: the registry holds
    the engine's GC runtime, and tells it of GC types it registers and
    unregisters.
- `Cargo.toml`: the feature `gc-elise`.
- `tests/all/main.rs`: `gc_engine()` takes elise with
  `WASMTIME_TEST_COLLECTOR=elise` (checking the heap after each GC) or
  `elise-stress` (a GC at every allocation, too); `tests/all/elise.rs`
  (new).
- `tests/wast.rs`: with the same variable, the spec tests of GC types
  only, with elise, compiled by Cranelift natively and for Pulley.
- `crates/test-util`: `Collector::Elise` for the spec tests;
  `crates/test-macros`: the collector's name.
- `crates/fuzzing`: the feature `gc-elise`, which fuzz configs can choose
  elise with (checking its heap after each GC, and stressed or not);
  `oracles::gc_ops_outcome`, how a `gc_ops` case ended; tests of `gc_ops`
  with elise for fixed seeds, and comparing elise's runs with the copying
  collector's.
- `kun/`: running upstream's GC tests with elise.

## Tests

```sh
cargo test --features gc-elise --test all elise::               # elise's own
cargo test -p wasmtime --features gc-elise --lib elise::        # its type table
python3 kun/elise_tests.py                                      # upstream's GC tests, with elise
cargo test -p wasmtime-fuzzing --no-default-features --features gc-elise --lib -- \
    gc_ops_with_elise gc_ops_elise_agrees                       # fixed-seed fuzzing
```

`kun/elise_tests.py` runs the modules whose tests take their engine from
`gc_engine()` or `gc_store()` (`structs`, `arrays`, `gc`, `exnrefs`,
`eqref`), and the spec tests of GC types, with
`WASMTIME_TEST_COLLECTOR=elise` and `elise-stress`, and fails if a test not
in `kun/elise-expected-failures.txt` fails, or one in it passes. The list
is empty now.

The spec tests and `crates/fuzzing`'s build script need the
`tests/spec_testsuite` and `tests/component-model` submodules
(`git submodule update --init tests/spec_testsuite tests/component-model`);
without them, an empty directory `test` in `tests/component-model` lets
them run `tests/misc_testsuite` alone. `crates/fuzzing`'s default feature `v8`
downloads a prebuilt V8, hence `--no-default-features`.

The fixed-seed tests are what fuzzing elise is for now: cargo-fuzz (and
Miri over the fork) wait for a macOS or Linux machine.

The test programs (`crates/test-programs`) need the `wasm32-wasip1` and
`wasm32-unknown-unknown` targets.
