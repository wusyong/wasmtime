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
- A store's GC heap can borrow the embedder's elise heap instead
  (`Config::elise_heap_provider`), its stores a `StoreSet`'s ("A heap
  borrowed from the embedder"): the DOM's and wasm's objects in one heap.
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

## A heap borrowed from the embedder

With `Config::elise_heap_provider`, a store's GC heap owns no elise heap:
it borrows the embedder's (kun's DOM's), which holds the embedder's
objects too, through the provider, which lends a thread's heap by a key
(`EliseHeapProvider::with_heap`). The heap's GCs run in the embedder's
calls (its safepoints) and in each store's, so every GC that reads the
roots needs every store's roots. Nothing of it relies on the embedder's
code for memory safety:

- Such stores are a `StoreSet`'s, which owns them (behind `Rc`, so it
  isn't `Send`: they stay on the heap's thread) and is the heap's roots
  source: at each GC that reads the roots it gives elise the roots of the
  stores it doesn't lend now, as their own GC finds them (`trace_roots`),
  and none at all while another than the one whose GC runs is lent (its
  wasm frames, or the embedder's `&mut` to it, may be on the stack): that
  GC doesn't run, as inside frames. The store whose GC runs gives its own
  roots, as wasmtime does. A store with a provider outside a set gets no
  GC heap (attaching it fails).
- While the set lends a store (`StoreSet::with`, a `StoreContextMut`), it
  counts the heap's frames (`Heap::enter_frames`): the embedder's
  safepoints then run no GC that reads the roots, which they can't see on
  the stack. A lend that unwinds leaves them too.
- A GC heap keeps the cage's start and the barrier state (where they stay
  while the heap lives), reads objects there without borrowing the heap,
  and checks first that it is on the heap's thread, and that the heap
  lives (`raw::HeapAlive`); so does entering wasm
  (`invoke_wasm_and_catch_traps`) and resuming a fiber (aborting there:
  no error can be given), as compiled code reads and writes the heap
  without asking. Each time a store or its set borrows the heap it checks
  it is the set's (`HeapAlive::is`: another heap under the same key, even
  at the same cage, isn't). A provider that lends none, or another, only
  makes the calls fail.
- elise's side: removing or replacing a roots source, and leaving frames,
  are `unsafe`; a heap dropped inside frames leaks (compiled code may
  still use its cage).

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

## Writes alongside elise's marker

Elise's concurrent marking reads an object's GC references (and an array's
length) atomically on its workers, while the runtime reads and writes the
object on the store's thread. So no `&mut` may cover an object's bytes:

- `VMGcObjectData`'s bytes are cells (`UnsafeCell<u8>`), read and written
  through a shared reference with raw pointers, as V8's `TaggedField`
  reads and writes a heap's fields; nothing hands out a `&mut
  VMGcObjectData`, or a `&[u8]` of an object (`copy_to_slice` copies out).
  A `GcStore` still lends it borrowed mutably, and it isn't `Sync`: one
  writer at a time, the store's.
- A GC reference field is written only through `write_gc_ref_in` and
  `init_gc_ref_in` (the runtime's) or `GcCompiler` (compiled code's), which
  elise does atomically with its barriers (`raw::EngineHeap::write_bits`);
  the runtime's other writes are of other bytes (numbers, a funcref's id in
  the func-ref table, an `i8` array's elements), which the marker doesn't
  read. `header_mut`'s `&mut VMGcHeader` covers the header, which the
  marker doesn't read either.
- Compiled code's plain stores are outside Rust's memory model: an aligned
  32-bit store is one write on x86-64 and ARM64, so the marker's relaxed
  atomic read finds the old value or the new one, and the barrier keeps
  what the old one referred to.

**On every rebase, look for new runtime writes of a GC object**: one that
writes a GC reference field other than through `write_gc_ref_in` and
`init_gc_ref_in`, or makes a `&mut` (or `&[u8]`) of an object's bytes,
races with elise's marker.

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
    `Config::elise_heap`, `Config::elise_heap_provider`.
  - `src/lib.rs`: `pub use elise`.
  - `src/runtime/vm/gc/enabled/elise.rs` (new), `enabled.rs`: the
    collector, its own heap or a borrowed one (`EliseHeapProvider`, the
    `StoreSet`'s roots source), and unit tests of its type table.
  - `src/runtime/store/elise.rs` (new), `src/runtime/store.rs`,
    `src/runtime.rs`: `StoreSet`, a store's place in it
    (`StoreOpaque::elise_slot`); `src/runtime/store/gc.rs`: a GC heap bound
    to the set's heap as it is allocated, a store's roots for the set
    (`elise_trace_roots`).
  - `src/runtime/func.rs` (`invoke_wasm_and_catch_traps`),
    `src/runtime/fiber.rs` (`resume_fiber`): `GcHeap::check_thread`.
  - `src/runtime/vm/gc/gc_runtime.rs`: `GcRuntime::register_type` and
    `unregister_type`; `GcHeap::write_gc_ref_in`, `init_gc_ref_in` (a
    barrier given the object, which a generational collector needs) and
    `capacity`, `check_thread`. All have defaults the other collectors keep. No
    `gc_object_data_mut`: `gc_object_data`'s default makes the object's
    cells from the memory's base pointer, not from `heap_slice`.
  - `src/runtime/vm/gc/data.rs`: `VMGcObjectData`'s bytes are cells, its
    methods take `&self`, `copy_to_slice` in place of `slice`
    ("Writes alongside elise's marker").
  - `src/runtime/vm/gc.rs`: `GcStore::gc_object_data` gives a shared
    `VMGcObjectData`; `src/runtime/gc/enabled/arrayref.rs`:
    `copy_to_i8_slice` copies out with `copy_to_slice`.
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
  and `tests/all/elise/shared.rs` (new, a borrowed heap's).
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
MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-permissive-provenance" \
  cargo +nightly miri test --features gc-elise --test all -- elise::a_marking_on_workers
                                                                 # its marker beside the host's writes, under Miri
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

Not tested: `resume_fiber`'s thread check, which aborts (reaching it takes
an async call in a `StoreSet::with`, moved to another thread and polled
there); reviewed only.
