//! machopo: elise, machopo's GC, as the collector, through the host's API: GC types
//! registered and let go with the engine's, objects laid out and read and
//! written as their types say (`i31ref`s, GC references, `externref`s'
//! host data), a pooling allocator's heap given to the next store afresh,
//! and a store moved between threads with its heap. Its GCs, given the
//! store's roots (the host's, globals', tables'), keep what they reach and
//! give an `externref`'s host data back once it dies; one an allocation
//! finds due is elise's to choose, the embedder's is a full GC; a marking
//! runs a step at a time between the host's writes. Compiled code's writes
//! keep the young objects old ones refer to (`struct.set`, `array.set`,
//! `array.fill`, `array.copy`, a large array's initial elements), its locals
//! follow what a GC moves, its globals' and tables' writes hold what they
//! refer to, and a marking runs a step at a time between its writes. Every
//! heap of these checks itself after each GC.

// Stores that borrow the embedder's heap.
mod shared;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wasmtime::elise::{HeapOptions, InlinePlatform, Job, JobDelegate, JobHandle, Platform};
use wasmtime::*;

fn engine(config: &mut Config) -> Result<Engine> {
    engine_with(config, verified(), Arc::new(InlinePlatform))
}

fn engine_with(
    config: &mut Config,
    options: HeapOptions,
    platform: Arc<dyn Platform>,
) -> Result<Engine> {
    config.wasm_function_references(true);
    config.wasm_gc(true);
    config.collector(Collector::Elise);
    config.elise_heap(options, platform);
    Engine::new(config)
}

/// The heap checks itself after each GC.
fn verified() -> HeapOptions {
    HeapOptions {
        verify: true,
        ..HeapOptions::default()
    }
}

/// ... and GCs at each allocation.
fn stressed() -> HeapOptions {
    HeapOptions {
        stress: true,
        ..verified()
    }
}

fn stressed_store() -> Result<Store<()>> {
    let engine = engine_with(&mut Config::new(), stressed(), Arc::new(InlinePlatform))?;
    Ok(Store::new(&engine, ()))
}

fn store() -> Result<Store<()>> {
    Ok(Store::new(&engine(&mut Config::new())?, ()))
}

/// `(struct (mut i8) (mut i64) (mut anyref) (mut externref))`.
fn struct_ty(engine: &Engine) -> Result<StructType> {
    StructType::new(
        engine,
        [
            FieldType::new(Mutability::Var, StorageType::I8),
            FieldType::new(Mutability::Var, StorageType::ValType(ValType::I64)),
            FieldType::new(Mutability::Var, StorageType::ValType(ValType::ANYREF)),
            FieldType::new(Mutability::Var, StorageType::ValType(ValType::EXTERNREF)),
        ],
    )
}

#[test]
fn a_structs_fields_are_read_as_written() -> Result<()> {
    let mut store = store()?;
    let ty = struct_ty(store.engine())?;
    let pre = StructRefPre::new(&mut store, ty);
    let inner = StructRef::new(
        &mut store,
        &pre,
        &[
            Val::I32(7),
            Val::I64(-1),
            Val::null_any_ref(),
            Val::null_extern_ref(),
        ],
    )?;
    let i31 = AnyRef::from_i31(&mut store, I31::new_i32(-42).unwrap());
    let host = ExternRef::new(&mut store, 1234_u32)?;
    let outer = StructRef::new(
        &mut store,
        &pre,
        &[
            Val::I32(0x1ff),
            Val::I64(i64::MAX),
            inner.to_anyref().into(),
            host.into(),
        ],
    )?;

    assert_eq!(
        outer.field(&mut store, 0)?.unwrap_i32(),
        0xff,
        "an i8, truncated, read zero-extended"
    );
    assert_eq!(outer.field(&mut store, 1)?.unwrap_i64(), i64::MAX);
    let field = outer.field(&mut store, 2)?;
    let back = field.unwrap_anyref().unwrap().unwrap_struct(&store)?;
    assert_eq!(back.field(&mut store, 1)?.unwrap_i64(), -1);
    let field = outer.field(&mut store, 3)?;
    let data = field.unwrap_externref().unwrap().data(&store)?.unwrap();
    assert_eq!(data.downcast_ref::<u32>(), Some(&1234));

    outer.set_field(&mut store, 2, i31.into())?;
    outer.set_field(&mut store, 1, Val::I64(5))?;
    let field = outer.field(&mut store, 2)?;
    assert_eq!(
        field.unwrap_anyref().unwrap().unwrap_i31(&store)?.get_i32(),
        -42
    );
    assert_eq!(outer.field(&mut store, 1)?.unwrap_i64(), 5);
    outer.set_field(&mut store, 2, Val::null_any_ref())?;
    assert!(outer.field(&mut store, 2)?.unwrap_anyref().is_none());
    let other = ExternRef::new(&mut store, 5678_u32)?;
    outer.set_field(&mut store, 3, other.into())?;
    let field = outer.field(&mut store, 3)?;
    let data = field.unwrap_externref().unwrap().data(&store)?.unwrap();
    assert_eq!(data.downcast_ref::<u32>(), Some(&5678));
    outer.set_field(&mut store, 3, Val::null_extern_ref())?;
    assert!(outer.field(&mut store, 3)?.unwrap_externref().is_none());
    Ok(())
}

#[test]
fn an_arrays_elements_are_read_as_written() -> Result<()> {
    let mut store = store()?;
    let engine = store.engine().clone();
    let bytes = ArrayType::new(&engine, FieldType::new(Mutability::Var, StorageType::I8));
    let words = ArrayType::new(
        &engine,
        FieldType::new(Mutability::Var, StorageType::ValType(ValType::I64)),
    );
    let refs = ArrayType::new(
        &engine,
        FieldType::new(Mutability::Var, StorageType::ValType(ValType::ANYREF)),
    );
    let pre = ArrayRefPre::new(&mut store, bytes);
    let bytes = ArrayRef::new(&mut store, &pre, &Val::I32(3), 5)?;
    let pre = ArrayRefPre::new(&mut store, words);
    let words = ArrayRef::new(&mut store, &pre, &Val::I64(-9), 3)?;
    let pre = ArrayRefPre::new(&mut store, refs);
    let refs = ArrayRef::new(&mut store, &pre, &Val::null_any_ref(), 4)?;

    assert_eq!(bytes.len(&store)?, 5);
    assert_eq!(words.len(&store)?, 3);
    assert_eq!(refs.len(&store)?, 4);
    bytes.set(&mut store, 4, Val::I32(0x180))?;
    assert_eq!(bytes.get(&mut store, 4)?.unwrap_i32(), 0x80);
    assert_eq!(bytes.get(&mut store, 3)?.unwrap_i32(), 3);
    words.set(&mut store, 2, Val::I64(11))?;
    assert_eq!(words.get(&mut store, 2)?.unwrap_i64(), 11);
    assert_eq!(words.get(&mut store, 1)?.unwrap_i64(), -9);
    refs.set(&mut store, 3, words.to_anyref().into())?;
    let element = refs.get(&mut store, 3)?;
    let back = element.unwrap_anyref().unwrap().unwrap_array(&store)?;
    assert_eq!(back.get(&mut store, 2)?.unwrap_i64(), 11);
    assert!(refs.get(&mut store, 0)?.unwrap_anyref().is_none());
    assert!(bytes.get(&mut store, 5).is_err(), "out of bounds");
    Ok(())
}

#[test]
fn an_externrefs_host_data_is_its_own() -> Result<()> {
    let mut store = store()?;
    let a = ExternRef::new(&mut store, String::from("a"))?;
    let b = ExternRef::new(&mut store, 2_u64)?;
    assert_eq!(
        a.data(&store)?.unwrap().downcast_ref::<String>().unwrap(),
        "a"
    );
    assert_eq!(b.data(&store)?.unwrap().downcast_ref::<u64>(), Some(&2));
    *b.data_mut(&mut store)?
        .unwrap()
        .downcast_mut::<u64>()
        .unwrap() = 3;
    assert_eq!(b.data(&store)?.unwrap().downcast_ref::<u64>(), Some(&3));
    Ok(())
}

#[test]
fn a_type_let_go_and_made_again_is_allocated() -> Result<()> {
    let engine = engine(&mut Config::new())?;
    // The registry gives a type's index to the next type once the type
    // goes; each round's type is another, a struct or an array by turns.
    for round in 0..600 {
        let mut store = Store::new(&engine, ());
        if round % 2 == 0 {
            let pre = StructRefPre::new(&mut store, struct_ty(&engine)?);
            let s = StructRef::new(
                &mut store,
                &pre,
                &[
                    Val::I32(1),
                    Val::I64(round),
                    Val::null_any_ref(),
                    Val::null_extern_ref(),
                ],
            )?;
            assert_eq!(s.field(&mut store, 1)?.unwrap_i64(), round);
        } else {
            let ty = ArrayType::new(&engine, FieldType::new(Mutability::Var, StorageType::I8));
            let pre = ArrayRefPre::new(&mut store, ty);
            let a = ArrayRef::new(&mut store, &pre, &Val::I32(1), 300)?;
            assert_eq!(a.len(&store)?, 300);
        }
    }
    Ok(())
}

#[test]
fn a_pooled_heap_is_the_next_stores_afresh() -> Result<()> {
    let mut pool = PoolingAllocationConfig::new();
    pool.total_memories(1);
    pool.total_gc_heaps(1);
    pool.max_memory_size(1 << 16);
    let mut config = Config::new();
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    let engine = engine(&mut config)?;
    // Each round's type goes with its store, a struct or an array by turns:
    // the next may take its index, in the pool's one heap.
    for round in 0..20 {
        let mut store = Store::new(&engine, ());
        if round % 2 == 0 {
            let pre = StructRefPre::new(&mut store, struct_ty(&engine)?);
            let s = StructRef::new(
                &mut store,
                &pre,
                &[
                    Val::I32(1),
                    Val::I64(round),
                    Val::null_any_ref(),
                    Val::null_extern_ref(),
                ],
            )?;
            assert_eq!(s.field(&mut store, 1)?.unwrap_i64(), round);
        } else {
            let ty = ArrayType::new(&engine, FieldType::new(Mutability::Var, StorageType::I8));
            let pre = ArrayRefPre::new(&mut store, ty);
            let a = ArrayRef::new(&mut store, &pre, &Val::I32(2), 300)?;
            assert_eq!(a.len(&store)?, 300);
            assert_eq!(a.get(&mut store, 299)?.unwrap_i32(), 2);
        }
    }
    Ok(())
}

#[test]
fn an_array_too_large_for_elise_traps() -> Result<()> {
    let mut store = store()?;
    let ty = ArrayType::new(
        store.engine(),
        FieldType::new(Mutability::Var, StorageType::I8),
    );
    let pre = ArrayRefPre::new(&mut store, ty);
    // Its bytes fit a `u32`, not elise's largest object.
    let error = ArrayRef::new(&mut store, &pre, &Val::I32(0), 1 << 31).unwrap_err();
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::AllocationTooLarge)
    );
    Ok(())
}

/// The barriers given the object default to what the other collectors did
/// before: an object's GC references keep what they refer to.
#[test]
fn the_other_collectors_objects_keep_what_they_refer_to() -> Result<()> {
    for collector in [
        Collector::DeferredReferenceCounting,
        Collector::Copying,
        Collector::Null,
    ] {
        let mut config = Config::new();
        config.wasm_function_references(true);
        config.wasm_gc(true);
        config.collector(collector);
        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, ());
        let pre = StructRefPre::new(&mut store, struct_ty(&engine)?);
        let fields = |value| {
            [
                Val::I32(0),
                Val::I64(value),
                Val::null_any_ref(),
                Val::null_extern_ref(),
            ]
        };
        let outer = StructRef::new(&mut store, &pre, &fields(1))?.to_owned_rooted(&mut store)?;
        {
            let mut scope = RootScope::new(&mut store);
            let inner = StructRef::new(&mut scope, &pre, &fields(7))?;
            outer.set_field(&mut scope, 2, inner.to_anyref().into())?;
            let host = ExternRef::new(&mut scope, 9_u32)?;
            outer.set_field(&mut scope, 3, host.into())?;
            // Overwritten: the first goes.
            let inner = StructRef::new(&mut scope, &pre, &fields(8))?;
            outer.set_field(&mut scope, 2, inner.to_anyref().into())?;
        }
        store.gc(None)?;
        let mut scope = RootScope::new(&mut store);
        let field = outer.field(&mut scope, 2)?;
        let inner = field.unwrap_anyref().unwrap().unwrap_struct(&scope)?;
        assert_eq!(inner.field(&mut scope, 1)?.unwrap_i64(), 8, "{collector:?}");
        let field = outer.field(&mut scope, 3)?;
        let data = field.unwrap_externref().unwrap().data(&scope)?.unwrap();
        assert_eq!(data.downcast_ref::<u32>(), Some(&9), "{collector:?}");
    }
    Ok(())
}

#[test]
fn a_store_moves_to_another_thread_with_its_heap() -> Result<()> {
    let mut store = store()?;
    let ty = struct_ty(store.engine())?;
    let pre = StructRefPre::new(&mut store, ty);
    let s = StructRef::new(
        &mut store,
        &pre,
        &[
            Val::I32(1),
            Val::I64(2),
            Val::null_any_ref(),
            Val::null_extern_ref(),
        ],
    )?;
    let s = s.to_owned_rooted(&mut store)?;
    std::thread::spawn(move || -> Result<()> {
        s.set_field(&mut store, 1, Val::I64(3))?;
        assert_eq!(s.field(&mut store, 1)?.unwrap_i64(), 3);
        Ok(())
    })
    .join()
    .unwrap()
}

#[test]
fn the_heaps_capacity_is_the_cage() -> Result<()> {
    let mut store = store()?;
    let ty = struct_ty(store.engine())?;
    let pre = StructRefPre::new(&mut store, ty);
    StructRef::new(
        &mut store,
        &pre,
        &[
            Val::I32(1),
            Val::I64(2),
            Val::null_any_ref(),
            Val::null_extern_ref(),
        ],
    )?;
    assert_eq!(store.gc_heap_capacity(), 1 << 32);
    Ok(())
}

/// The fields of a `struct_ty` holding `value`.
fn fields(value: i64) -> [Val; 4] {
    [
        Val::I32(0),
        Val::I64(value),
        Val::null_any_ref(),
        Val::null_extern_ref(),
    ]
}

/// Host data that counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_gc_keeps_what_the_roots_reach() -> Result<()> {
    let mut store = stressed_store()?;
    let ty = struct_ty(store.engine())?;
    let pre = StructRefPre::new(&mut store, ty);
    // A list, each node allocated (and the heap GCed, moving them) with
    // only the head rooted: the rest are reached through it.
    let head = StructRef::new(&mut store, &pre, &fields(0))?.to_owned_rooted(&mut store)?;
    for value in 1..100 {
        let mut scope = RootScope::new(&mut store);
        let node = StructRef::new(&mut scope, &pre, &fields(value))?;
        let next = head.field(&mut scope, 2)?;
        node.set_field(&mut scope, 2, next)?;
        head.set_field(&mut scope, 2, node.to_anyref().into())?;
    }
    store.gc(None)?;
    let mut scope = RootScope::new(&mut store);
    let mut node = head.to_rooted(&mut scope);
    for value in (1..100).rev() {
        let next = node.field(&mut scope, 2)?;
        node = next.unwrap_anyref().unwrap().unwrap_struct(&scope)?;
        assert_eq!(node.field(&mut scope, 1)?.unwrap_i64(), value);
    }
    assert!(node.field(&mut scope, 2)?.unwrap_anyref().is_none());
    Ok(())
}

#[test]
fn an_externrefs_host_data_goes_with_it() -> Result<()> {
    let mut store = store()?;
    let ty = struct_ty(store.engine())?;
    let pre = StructRefPre::new(&mut store, ty);
    let drops = Arc::new(AtomicUsize::new(0));
    let holder = StructRef::new(&mut store, &pre, &fields(0))?.to_owned_rooted(&mut store)?;
    let owned = {
        let mut scope = RootScope::new(&mut store);
        ExternRef::new(&mut scope, Counted(drops.clone()))?;
        let held = ExternRef::new(&mut scope, Counted(drops.clone()))?;
        holder.set_field(&mut scope, 3, held.into())?;
        ExternRef::new(&mut scope, Counted(drops.clone()))?.to_owned_rooted(&mut scope)?
    };
    store.gc(None)?;
    assert_eq!(drops.load(Ordering::SeqCst), 1, "the one let go");
    holder.set_field(&mut store, 3, Val::null_extern_ref())?;
    drop(owned);
    store.gc(None)?;
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    Ok(())
}

#[test]
fn globals_and_tables_hold_what_they_refer_to() -> Result<()> {
    let mut store = stressed_store()?;
    let ty = struct_ty(store.engine())?;
    let pre = StructRefPre::new(&mut store, ty);
    let drops = Arc::new(AtomicUsize::new(0));
    let (global, table) = {
        let mut scope = RootScope::new(&mut store);
        let mut held = |value| -> Result<Rooted<StructRef>> {
            let s = StructRef::new(&mut scope, &pre, &fields(value))?;
            let host = ExternRef::new(&mut scope, Counted(drops.clone()))?;
            s.set_field(&mut scope, 3, host.into())?;
            Ok(s)
        };
        let first = held(1)?;
        let second = held(2)?;
        let ty = GlobalType::new(ValType::ANYREF, Mutability::Var);
        let global = Global::new(&mut scope, ty, first.to_anyref().into())?;
        let ty = TableType::new(RefType::ANYREF, 1, None);
        let table = Table::new(&mut scope, ty, second.to_anyref().into())?;
        (global, table)
    };
    for _ in 0..10 {
        let mut scope = RootScope::new(&mut store);
        StructRef::new(&mut scope, &pre, &fields(0))?;
    }
    store.gc(None)?;
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    {
        let mut scope = RootScope::new(&mut store);
        let value = global.get(&mut scope);
        let s = value.unwrap_anyref().unwrap().unwrap_struct(&scope)?;
        assert_eq!(s.field(&mut scope, 1)?.unwrap_i64(), 1);
        let element = table.get(&mut scope, 0).unwrap();
        let s = element.unwrap_any().unwrap().unwrap_struct(&scope)?;
        assert_eq!(s.field(&mut scope, 1)?.unwrap_i64(), 2);
    }
    global.set(&mut store, Val::null_any_ref())?;
    table.set(&mut store, 0, Ref::Any(None))?;
    store.gc(None)?;
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn an_allocations_gc_is_elises_the_embedders_is_full() -> Result<()> {
    // A nursery of 64 KiB, and no full GC due for long.
    let options = HeapOptions {
        nursery_bytes: 64 << 10,
        max_nursery_bytes: 64 << 10,
        min_allocation_bytes: 64 << 20,
        ..verified()
    };
    let engine = engine_with(&mut Config::new(), options, Arc::new(InlinePlatform))?;
    let mut store = Store::new(&engine, ());
    let drops = Arc::new(AtomicUsize::new(0));
    let old = {
        let mut scope = RootScope::new(&mut store);
        ExternRef::new(&mut scope, Counted(drops.clone()))?.to_owned_rooted(&mut scope)?
    };
    for _ in 0..3 {
        store.gc(None)?;
    }
    drop(old);
    let young = Arc::new(AtomicUsize::new(0));
    {
        let mut scope = RootScope::new(&mut store);
        ExternRef::new(&mut scope, Counted(young.clone()))?;
    }
    // Until an allocation's GC (a scavenge) finds the young one dead.
    let ty = ArrayType::new(&engine, FieldType::new(Mutability::Var, StorageType::I8));
    let pre = ArrayRefPre::new(&mut store, ty);
    for _ in 0..10_000 {
        if young.load(Ordering::SeqCst) == 1 {
            break;
        }
        let mut scope = RootScope::new(&mut store);
        ArrayRef::new(&mut scope, &pre, &Val::I32(0), 1024)?;
    }
    assert_eq!(young.load(Ordering::SeqCst), 1, "a scavenge ran");
    assert_eq!(drops.load(Ordering::SeqCst), 0, "it left the old alone");
    store.gc(None)?;
    assert_eq!(drops.load(Ordering::SeqCst), 1, "the embedder's GC is full");
    Ok(())
}

#[test]
fn a_pooled_heap_keeps_no_externref_of_the_last_store() -> Result<()> {
    let mut pool = PoolingAllocationConfig::new();
    pool.total_memories(1);
    pool.total_gc_heaps(1);
    pool.max_memory_size(1 << 16);
    let mut config = Config::new();
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    let engine = engine(&mut config)?;
    {
        let mut store = Store::new(&engine, ());
        ExternRef::new(&mut store, 1_u32)?;
        ExternRef::new(&mut store, 2_u32)?;
    }
    let mut store = Store::new(&engine, ());
    let e = ExternRef::new(&mut store, 3_u32)?;
    store.gc(None)?;
    assert_eq!(e.data(&store)?.unwrap().downcast_ref::<u32>(), Some(&3));
    Ok(())
}

/// A platform whose posted jobs (a marking's) run only in `step`.
#[derive(Default)]
struct Stepping {
    posted: Mutex<Vec<(usize, Arc<dyn Job + Send>)>>,
    next: Mutex<usize>,
    /// The jobs `step` ran.
    ran: AtomicUsize,
}

impl Stepping {
    fn step(&self) {
        let jobs: Vec<_> = self
            .posted
            .lock()
            .unwrap()
            .iter()
            .map(|(_, job)| job.clone())
            .collect();
        for job in jobs {
            job.run(&Worker);
            self.ran.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct Worker;

impl JobDelegate for Worker {
    fn notify_concurrency_increase(&self) {}

    fn is_joining_thread(&self) -> bool {
        false
    }
}

/// A posted job's handle: cancelling takes the job back.
struct Handle(Arc<Stepping>, usize);

impl JobHandle for Handle {
    fn notify_concurrency_increase(&self) {}

    fn cancel(&self) {
        self.0
            .posted
            .lock()
            .unwrap()
            .retain(|&(id, _)| id != self.1);
    }
}

struct Steps(Arc<Stepping>);

impl Platform for Steps {
    fn run(&self, job: &dyn Job) {
        InlinePlatform.run(job);
    }

    fn post_job(&self, job: Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        let mut next = self.0.next.lock().unwrap();
        *next += 1;
        self.0.posted.lock().unwrap().push((*next, job));
        Some(Box::new(Handle(self.0.clone(), *next)))
    }

    fn posts_jobs(&self) -> bool {
        true
    }
}

#[test]
fn a_marking_steps_between_the_hosts_writes() -> Result<()> {
    // Each allocation GCs: by turns a scavenge, and a marking's start or
    // end; the marker runs a step between the host's writes.
    let steps = Arc::new(Stepping::default());
    let engine = engine_with(
        &mut Config::new(),
        stressed(),
        Arc::new(Steps(steps.clone())),
    )?;
    let mut store = Store::new(&engine, ());
    let pre = StructRefPre::new(&mut store, struct_ty(&engine)?);
    let element = FieldType::new(Mutability::Var, StorageType::ValType(ValType::ANYREF));
    let array_pre = ArrayRefPre::new(&mut store, ArrayType::new(&engine, element));
    let mut arrays = Vec::new();
    for _ in 0..4 {
        let array = ArrayRef::new(&mut store, &array_pre, &Val::null_any_ref(), 8)?;
        arrays.push(array.to_owned_rooted(&mut store)?);
    }
    // What each slot holds: none, or the value of the struct in it.
    let mut model = [[None::<i64>; 8]; 4];
    let mut seed = 1_u64;
    let mut random = |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for round in 0..400_i64 {
        let (a, i) = (random(4), random(8));
        let (a_at, i_at) = (usize::try_from(a)?, usize::try_from(i)?);
        let i = u32::try_from(i)?;
        let mut scope = RootScope::new(&mut store);
        if random(3) == 0 {
            // A new struct (a GC first) into a slot.
            let s = StructRef::new(&mut scope, &pre, &fields(round))?;
            arrays[a_at].set(&mut scope, i, s.to_anyref().into())?;
            model[a_at][i_at] = Some(round);
        } else {
            // Moved to another slot, its own cleared: the marker may have
            // scanned where it goes and not where it was.
            let (b, j) = (usize::try_from(random(4))?, random(8));
            let j_at = usize::try_from(j)?;
            let value = arrays[a_at].get(&mut scope, i)?;
            arrays[a_at].set(&mut scope, i, Val::null_any_ref())?;
            arrays[b].set(&mut scope, u32::try_from(j)?, value)?;
            let moved = model[a_at][i_at].take();
            model[b][j_at] = moved;
        }
        drop(scope);
        steps.step();
        let mut scope = RootScope::new(&mut store);
        for (a, slots) in model.iter().enumerate() {
            for (i, &value) in slots.iter().enumerate() {
                let field = arrays[a].get(&mut scope, u32::try_from(i)?)?;
                let read = match field.unwrap_anyref() {
                    Some(r) => {
                        let s = r.unwrap_struct(&scope)?;
                        Some(s.field(&mut scope, 1)?.unwrap_i64())
                    }
                    None => None,
                };
                assert_eq!(read, value, "round {round}, slot {a}.{i}");
            }
        }
    }
    assert!(steps.ran.load(Ordering::SeqCst) > 0, "a marking ran a step");
    Ok(())
}

/// A marking on a pool's workers (elise's `ThreadPool`) runs alongside the
/// host's writes: the runtime writes a struct's fields (its numbers, through
/// `VMGcObjectData`'s cells; its GC references, through elise's atomic
/// writes) while the marker reads its GC references on another thread. Under
/// Miri, which reports a data race or a `&mut` covering what the marker
/// reads, fewer rounds.
/// A pool's workers (elise's `ThreadPool`), counting the posted jobs' runs
/// on other threads than the test's.
struct Workers {
    pool: wasmtime::elise::ThreadPool,
    test: std::thread::ThreadId,
    ran: Arc<AtomicUsize>,
}

/// A posted job, its runs on a worker counted.
struct CountedJob {
    job: Arc<dyn Job + Send>,
    test: std::thread::ThreadId,
    ran: Arc<AtomicUsize>,
}

impl Job for CountedJob {
    fn run(&self, delegate: &dyn JobDelegate) {
        if std::thread::current().id() != self.test {
            self.ran.fetch_add(1, Ordering::SeqCst);
        }
        self.job.run(delegate);
    }

    fn max_concurrency(&self, active: usize) -> usize {
        self.job.max_concurrency(active)
    }
}

impl Platform for Workers {
    fn run(&self, job: &dyn Job) {
        self.pool.run(job);
    }

    fn post_job(&self, job: Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        self.pool.post_job(Arc::new(CountedJob {
            job,
            test: self.test,
            ran: self.ran.clone(),
        }))
    }

    fn posts_jobs(&self) -> bool {
        true
    }
}

#[test]
fn a_marking_on_workers_runs_alongside_the_hosts_writes() -> Result<()> {
    let ran = Arc::new(AtomicUsize::new(0));
    let engine = engine_with(
        &mut Config::new(),
        stressed(),
        Arc::new(Workers {
            pool: wasmtime::elise::ThreadPool::new(2),
            test: std::thread::current().id(),
            ran: ran.clone(),
        }),
    )?;
    let mut store = Store::new(&engine, ());
    let pre = StructRefPre::new(&mut store, struct_ty(&engine)?);
    let element = FieldType::new(Mutability::Var, StorageType::ValType(ValType::ANYREF));
    let array_pre = ArrayRefPre::new(&mut store, ArrayType::new(&engine, element));
    let mut arrays = Vec::new();
    for _ in 0..4 {
        let array = ArrayRef::new(&mut store, &array_pre, &Val::null_any_ref(), 8)?;
        arrays.push(array.to_owned_rooted(&mut store)?);
    }
    // What each slot holds: none, or the value of the struct in it.
    let mut model = [[None::<i64>; 8]; 4];
    let mut seed = 7_u64;
    let mut random = |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let rounds = if cfg!(miri) { 30 } else { 2000 };
    for round in 0..rounds {
        let (a, i) = (usize::try_from(random(4))?, random(8));
        let i_at = usize::try_from(i)?;
        let i = u32::try_from(i)?;
        let mut scope = RootScope::new(&mut store);
        match (random(3), model[a][i_at]) {
            (0, _) | (_, None) => {
                // A new struct (each allocation GCs) into the slot.
                let s = StructRef::new(&mut scope, &pre, &fields(round))?;
                arrays[a].set(&mut scope, i, s.to_anyref().into())?;
                model[a][i_at] = Some(round);
            }
            (1, Some(_)) => {
                // Its numbers written while the marker may be scanning it.
                let field = arrays[a].get(&mut scope, i)?;
                let s = field.unwrap_anyref().unwrap().unwrap_struct(&scope)?;
                s.set_field(&mut scope, 0, Val::I32(i32::try_from(round)?))?;
                s.set_field(&mut scope, 1, Val::I64(-round))?;
                let i31 = AnyRef::from_i31(&mut scope, I31::wrapping_i32(i32::try_from(round)?));
                s.set_field(&mut scope, 2, i31.into())?;
                model[a][i_at] = Some(-round);
            }
            (_, Some(_)) => {
                // Moved to another slot, its own cleared.
                let (b, j) = (usize::try_from(random(4))?, random(8));
                let j_at = usize::try_from(j)?;
                let value = arrays[a].get(&mut scope, i)?;
                arrays[a].set(&mut scope, i, Val::null_any_ref())?;
                arrays[b].set(&mut scope, u32::try_from(j)?, value)?;
                let moved = model[a][i_at].take();
                model[b][j_at] = moved;
            }
        }
        drop(scope);
        let mut scope = RootScope::new(&mut store);
        for (a, slots) in model.iter().enumerate() {
            for (i, &value) in slots.iter().enumerate() {
                let field = arrays[a].get(&mut scope, u32::try_from(i)?)?;
                let read = match field.unwrap_anyref() {
                    Some(r) => {
                        let s = r.unwrap_struct(&scope)?;
                        Some(s.field(&mut scope, 1)?.unwrap_i64())
                    }
                    None => None,
                };
                assert_eq!(read, value, "round {round}, slot {a}.{i}");
            }
        }
    }
    store.gc(None)?;
    assert!(ran.load(Ordering::SeqCst) > 0, "the marker ran on a worker");
    Ok(())
}

/// Compiled code's reads and writes, its barriers', of `$s` and `$a`
/// objects, and of a global's and a table's.
const WAT: &str = r#"
    (module
        (type $s (struct (field (mut i64)) (field (mut anyref))))
        (type $a (array (mut anyref)))
        (import "" "gc" (func $gc))
        (global $g (mut anyref) (ref.null any))
        (table $t 1 anyref)

        (func $s (param $v i64) (result (ref $s))
            (struct.new $s (local.get $v) (ref.null any)))
        (func (export "new_s") (param $v i64) (result anyref)
            (call $s (local.get $v)))
        (func (export "new_a") (param $len i32) (result anyref)
            (array.new $a (ref.null any) (local.get $len)))
        (func (export "new_a_of") (param $len i32) (param $v i64) (result anyref)
            (array.new $a (call $s (local.get $v)) (local.get $len)))

        (func (export "value") (param $o anyref) (result i64)
            (struct.get $s 0 (ref.cast (ref $s) (local.get $o))))
        (func (export "next") (param $o anyref) (result anyref)
            (struct.get $s 1 (ref.cast (ref $s) (local.get $o))))
        (func (export "element") (param $o anyref) (param $i i32) (result anyref)
            (array.get $a (ref.cast (ref $a) (local.get $o)) (local.get $i)))

        (func (export "struct_set") (param $o anyref) (param $v i64)
            (struct.set $s 1 (ref.cast (ref $s) (local.get $o)) (call $s (local.get $v))))
        (func (export "array_set") (param $o anyref) (param $i i32) (param $v i64)
            (array.set $a (ref.cast (ref $a) (local.get $o)) (local.get $i)
                (call $s (local.get $v))))
        (func (export "array_fill") (param $o anyref) (param $v i64)
            (local $a (ref $a))
            (local.set $a (ref.cast (ref $a) (local.get $o)))
            (array.fill $a (local.get $a) (i32.const 0) (call $s (local.get $v))
                (array.len (local.get $a))))
        (func (export "array_copy") (param $o anyref) (param $v i64)
            (local $a (ref $a))
            (local $from (ref $a))
            (local.set $a (ref.cast (ref $a) (local.get $o)))
            (local.set $from
                (array.new $a (call $s (local.get $v)) (array.len (local.get $a))))
            (array.copy $a $a (local.get $a) (i32.const 0) (local.get $from) (i32.const 0)
                (array.len (local.get $a))))
        ;; What `from[i]` holds is cleared there, then written at `to[j]`.
        (func (export "move") (param $from anyref) (param $i i32) (param $to anyref) (param $j i32)
            (local $v anyref)
            (local.set $v (array.get $a (ref.cast (ref $a) (local.get $from)) (local.get $i)))
            (array.set $a (ref.cast (ref $a) (local.get $from)) (local.get $i) (ref.null any))
            (array.set $a (ref.cast (ref $a) (local.get $to)) (local.get $j) (local.get $v)))
        (func (export "spread") (param $from anyref) (param $i i32) (param $len i32) (result anyref)
            (local $v anyref)
            (local $to anyref)
            (local.set $v (array.get $a (ref.cast (ref $a) (local.get $from)) (local.get $i)))
            (local.set $to (array.new $a (local.get $v) (local.get $len)))
            (array.set $a (ref.cast (ref $a) (local.get $from)) (local.get $i) (ref.null any))
            (local.get $to))

        (func (export "set_global") (param $v i64)
            (global.set $g (call $s (local.get $v))))
        (func (export "global") (result anyref)
            (global.get $g))
        (func (export "set_table") (param $v i64)
            (table.set $t (i32.const 0) (call $s (local.get $v))))
        (func (export "table") (result anyref)
            (table.get $t (i32.const 0)))

        ;; A local held across the host's GC, and across `n` allocations
        ;; (a list), each a GC under stress.
        (func (export "across_a_gc") (param $v i64) (result i64)
            (local $l (ref $s))
            (local.set $l (call $s (local.get $v)))
            (call $gc)
            (struct.get $s 0 (local.get $l)))
        (func (export "across_allocations") (param $v i64) (param $n i32) (result i64)
            (local $l (ref $s))
            (local $head anyref)
            (local.set $l (call $s (local.get $v)))
            (block $done
                (loop $more
                    (br_if $done (i32.eqz (local.get $n)))
                    (local.set $head (struct.new $s (i64.const 0) (local.get $head)))
                    (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                    (br $more)))
            (struct.get $s 0 (local.get $l)))
    )
"#;

type AnyRefArg = Option<Rooted<AnyRef>>;

/// `WAT`'s exports.
struct Wasm {
    new_s: TypedFunc<i64, AnyRefArg>,
    new_a: TypedFunc<i32, AnyRefArg>,
    new_a_of: TypedFunc<(i32, i64), AnyRefArg>,
    value: TypedFunc<AnyRefArg, i64>,
    next: TypedFunc<AnyRefArg, AnyRefArg>,
    element: TypedFunc<(AnyRefArg, i32), AnyRefArg>,
    struct_set: TypedFunc<(AnyRefArg, i64), ()>,
    array_set: TypedFunc<(AnyRefArg, i32, i64), ()>,
    array_fill: TypedFunc<(AnyRefArg, i64), ()>,
    array_copy: TypedFunc<(AnyRefArg, i64), ()>,
    moves: TypedFunc<(AnyRefArg, i32, AnyRefArg, i32), ()>,
    /// A new `$a` of a length, each element an element of another's, which
    /// is cleared after.
    spread: TypedFunc<(AnyRefArg, i32, i32), AnyRefArg>,
    set_global: TypedFunc<i64, ()>,
    global: TypedFunc<(), AnyRefArg>,
    set_table: TypedFunc<i64, ()>,
    table: TypedFunc<(), AnyRefArg>,
    across_a_gc: TypedFunc<i64, i64>,
    across_allocations: TypedFunc<(i64, i32), i64>,
}

impl Wasm {
    fn new(store: &mut Store<()>) -> Result<Self> {
        let module = Module::new(store.engine(), WAT)?;
        let gc = Func::wrap(&mut *store, |mut caller: Caller<'_, ()>| caller.gc(None));
        let instance = Instance::new(&mut *store, &module, &[gc.into()])?;
        let mut store = &mut *store;
        Ok(Self {
            new_s: instance.get_typed_func(&mut store, "new_s")?,
            new_a: instance.get_typed_func(&mut store, "new_a")?,
            new_a_of: instance.get_typed_func(&mut store, "new_a_of")?,
            value: instance.get_typed_func(&mut store, "value")?,
            next: instance.get_typed_func(&mut store, "next")?,
            element: instance.get_typed_func(&mut store, "element")?,
            struct_set: instance.get_typed_func(&mut store, "struct_set")?,
            array_set: instance.get_typed_func(&mut store, "array_set")?,
            array_fill: instance.get_typed_func(&mut store, "array_fill")?,
            array_copy: instance.get_typed_func(&mut store, "array_copy")?,
            moves: instance.get_typed_func(&mut store, "move")?,
            spread: instance.get_typed_func(&mut store, "spread")?,
            set_global: instance.get_typed_func(&mut store, "set_global")?,
            global: instance.get_typed_func(&mut store, "global")?,
            set_table: instance.get_typed_func(&mut store, "set_table")?,
            table: instance.get_typed_func(&mut store, "table")?,
            across_a_gc: instance.get_typed_func(&mut store, "across_a_gc")?,
            across_allocations: instance.get_typed_func(&mut store, "across_allocations")?,
        })
    }

    /// The value of the `$s` that `object` is, or (with `element`) that
    /// that element of the `$a` `object` is, read by compiled code.
    fn value_of(
        &self,
        store: &mut Store<()>,
        object: &OwnedRooted<AnyRef>,
        element: Option<u32>,
    ) -> Result<i64> {
        let mut scope = RootScope::new(store);
        let mut object = Some(object.to_rooted(&mut scope));
        if let Some(i) = element {
            object = self.element.call(&mut scope, (object, i32::try_from(i)?))?;
        }
        self.value.call(&mut scope, object)
    }

    /// The value of the `$s` that the `$s` `object` refers to.
    fn next_value(&self, store: &mut Store<()>, object: &OwnedRooted<AnyRef>) -> Result<i64> {
        let mut scope = RootScope::new(store);
        let object = Some(object.to_rooted(&mut scope));
        let next = self.next.call(&mut scope, object)?;
        self.value.call(&mut scope, next)
    }

    /// A new `$a` of `len` nulls, owned.
    fn owned_array(&self, store: &mut Store<()>, len: i32) -> Result<OwnedRooted<AnyRef>> {
        let mut scope = RootScope::new(store);
        let array = self.new_a.call(&mut scope, len)?.unwrap();
        array.to_owned_rooted(&mut scope)
    }

    /// `object` written by compiled code: `write` given it.
    fn write(
        &self,
        store: &mut Store<()>,
        object: &OwnedRooted<AnyRef>,
        write: impl FnOnce(&Self, &mut RootScope<&mut Store<()>>, AnyRefArg) -> Result<()>,
    ) -> Result<()> {
        let mut scope = RootScope::new(store);
        let object = Some(object.to_rooted(&mut scope));
        write(self, &mut scope, object)
    }
}

/// A heap whose allocations' GCs are scavenges: a 64 KiB nursery, and no
/// full GC due for long.
fn generational() -> HeapOptions {
    HeapOptions {
        nursery_bytes: 64 << 10,
        max_nursery_bytes: 64 << 10,
        min_allocation_bytes: 64 << 20,
        ..verified()
    }
}

/// Allocates until an allocation's GC, a scavenge in a `generational`
/// heap, ran: a young `externref` let go is dropped.
fn scavenge(store: &mut Store<()>) -> Result<()> {
    let young = Arc::new(AtomicUsize::new(0));
    {
        let mut scope = RootScope::new(&mut *store);
        ExternRef::new(&mut scope, Counted(young.clone()))?;
    }
    let ty = ArrayType::new(
        store.engine(),
        FieldType::new(Mutability::Var, StorageType::I8),
    );
    let pre = ArrayRefPre::new(&mut *store, ty);
    for _ in 0..10_000 {
        if young.load(Ordering::SeqCst) == 1 {
            return Ok(());
        }
        let mut scope = RootScope::new(&mut *store);
        ArrayRef::new(&mut scope, &pre, &Val::I32(0), 1024)?;
    }
    bail!("no scavenge ran")
}

#[test]
fn compiled_writes_keep_the_young_objects_old_ones_refer_to() -> Result<()> {
    let engine = engine_with(&mut Config::new(), generational(), Arc::new(InlinePlatform))?;
    let mut store = Store::new(&engine, ());
    let wasm = Wasm::new(&mut store)?;
    let s = {
        let mut scope = RootScope::new(&mut store);
        let s = wasm.new_s.call(&mut scope, 0)?.unwrap();
        s.to_owned_rooted(&mut scope)?
    };
    let set = wasm.owned_array(&mut store, 4)?;
    let fill = wasm.owned_array(&mut store, 4)?;
    let copy = wasm.owned_array(&mut store, 4)?;
    // All old, then each written a young `$s` by compiled code.
    store.gc(None)?;
    wasm.write(&mut store, &s, |w, scope, o| {
        w.struct_set.call(scope, (o, 1))
    })?;
    wasm.write(&mut store, &set, |w, scope, o| {
        w.array_set.call(scope, (o, 2, 2))
    })?;
    wasm.write(&mut store, &fill, |w, scope, o| {
        w.array_fill.call(scope, (o, 3))
    })?;
    wasm.write(&mut store, &copy, |w, scope, o| {
        w.array_copy.call(scope, (o, 4))
    })?;
    scavenge(&mut store)?;
    assert_eq!(wasm.next_value(&mut store, &s)?, 1, "struct.set");
    assert_eq!(wasm.value_of(&mut store, &set, Some(2))?, 2, "array.set");
    for i in 0..4 {
        assert_eq!(wasm.value_of(&mut store, &fill, Some(i))?, 3, "array.fill");
        assert_eq!(wasm.value_of(&mut store, &copy, Some(i))?, 4, "array.copy");
    }
    Ok(())
}

#[test]
fn a_large_arrays_initial_elements_survive_a_scavenge() -> Result<()> {
    let engine = engine_with(&mut Config::new(), generational(), Arc::new(InlinePlatform))?;
    let mut store = Store::new(&engine, ());
    let wasm = Wasm::new(&mut store)?;
    // Larger than a page: allocated old, its elements initialized to a
    // young `$s`.
    let len = 1 << 17;
    let array = {
        let mut scope = RootScope::new(&mut store);
        let array = wasm.new_a_of.call(&mut scope, (len, 7))?.unwrap();
        array.to_owned_rooted(&mut scope)?
    };
    scavenge(&mut store)?;
    let last = u32::try_from(len - 1)?;
    assert_eq!(wasm.value_of(&mut store, &array, Some(0))?, 7);
    assert_eq!(wasm.value_of(&mut store, &array, Some(last))?, 7);
    Ok(())
}

#[test]
fn compiled_codes_locals_follow_the_objects_a_gc_moves() -> Result<()> {
    let mut store = stressed_store()?;
    let wasm = Wasm::new(&mut store)?;
    assert_eq!(wasm.across_a_gc.call(&mut store, 5)?, 5);
    assert_eq!(wasm.across_allocations.call(&mut store, (6, 200))?, 6);
    Ok(())
}

#[test]
fn compiled_writes_of_globals_and_tables_hold_what_they_refer_to() -> Result<()> {
    let engine = engine_with(&mut Config::new(), generational(), Arc::new(InlinePlatform))?;
    let mut store = Store::new(&engine, ());
    let wasm = Wasm::new(&mut store)?;
    wasm.set_global.call(&mut store, 1)?;
    wasm.set_table.call(&mut store, 2)?;
    scavenge(&mut store)?;
    store.gc(None)?;
    let mut scope = RootScope::new(&mut store);
    let global = wasm.global.call(&mut scope, ())?;
    assert_eq!(wasm.value.call(&mut scope, global)?, 1);
    let table = wasm.table.call(&mut scope, ())?;
    assert_eq!(wasm.value.call(&mut scope, table)?, 2);
    Ok(())
}

#[test]
fn a_marking_steps_between_compiled_writes() -> Result<()> {
    // Each allocation GCs: by turns a scavenge, and a marking's start or
    // end; the marker runs a step between compiled code's calls.
    let steps = Arc::new(Stepping::default());
    let engine = engine_with(
        &mut Config::new(),
        stressed(),
        Arc::new(Steps(steps.clone())),
    )?;
    let mut store = Store::new(&engine, ());
    let wasm = Wasm::new(&mut store)?;
    let mut arrays = Vec::new();
    for _ in 0..4 {
        arrays.push(wasm.owned_array(&mut store, 8)?);
    }
    // What each slot holds: none, or the value of the `$s` in it.
    let mut model = [[None::<i64>; 8]; 4];
    let mut seed = 1_u64;
    let mut random = |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for round in 0..400_i64 {
        let (a, i) = (usize::try_from(random(4))?, random(8));
        let i_at = usize::try_from(i)?;
        let i = i32::try_from(i)?;
        if random(3) == 0 {
            // A new `$s` (a GC first) into a slot.
            wasm.write(&mut store, &arrays[a], |w, scope, o| {
                w.array_set.call(scope, (o, i, round))
            })?;
            model[a][i_at] = Some(round);
        } else {
            // Moved to another slot, its own cleared: the marker may have
            // scanned where it goes and not where it was.
            let (b, j) = (usize::try_from(random(4))?, random(8));
            let j_at = usize::try_from(j)?;
            let j = i32::try_from(j)?;
            let to = &arrays[b];
            wasm.write(&mut store, &arrays[a], |w, scope, o| {
                let to = Some(to.to_rooted(&mut *scope));
                w.moves.call(scope, (o, i, to, j))
            })?;
            let moved = model[a][i_at].take();
            model[b][j_at] = moved;
        }
        steps.step();
        for (a, slots) in model.iter().enumerate() {
            for (i, &value) in slots.iter().enumerate() {
                let i = u32::try_from(i)?;
                let read = match value {
                    Some(_) => Some(wasm.value_of(&mut store, &arrays[a], Some(i))?),
                    None => {
                        let mut scope = RootScope::new(&mut store);
                        let array = Some(arrays[a].to_rooted(&mut scope));
                        let element = wasm.element.call(&mut scope, (array, i32::try_from(i)?))?;
                        element.map(|_| -1)
                    }
                };
                assert_eq!(read, value, "round {round}, slot {a}.{i}");
            }
        }
    }
    assert!(steps.ran.load(Ordering::SeqCst) > 0, "a marking ran a step");
    Ok(())
}

#[test]
fn compiled_writes_keep_what_a_marking_hasnt_reached() -> Result<()> {
    // The marker never runs on its own: a marking's work is all done at its
    // end. A large array allocated while a marking runs is black, never
    // scanned, and its initial elements take no SATB (they held nothing):
    // an old `$s` made its elements, then cleared from a slot the marker
    // hasn't reached, is held by the clearing's SATB alone. Each allocation
    // GCs: by turns a scavenge, and a marking's start or end, so the rounds'
    // extra allocations put the clearing at each point of the cycle.
    let steps = Arc::new(Stepping::default());
    let engine = engine_with(
        &mut Config::new(),
        stressed(),
        Arc::new(Steps(steps.clone())),
    )?;
    let mut store = Store::new(&engine, ());
    let wasm = Wasm::new(&mut store)?;
    let from = wasm.owned_array(&mut store, 1)?;
    for round in 0..16_i64 {
        wasm.write(&mut store, &from, |w, scope, o| {
            w.array_set.call(scope, (o, 0, round))
        })?;
        // The `$s` old, and the cycle started afresh.
        store.gc(None)?;
        for _ in 0..round % 4 {
            wasm.owned_array(&mut store, 1)?;
        }
        let to = {
            let mut scope = RootScope::new(&mut store);
            let o = Some(from.to_rooted(&mut scope));
            let to = wasm.spread.call(&mut scope, (o, 0, 1 << 16))?.unwrap();
            to.to_owned_rooted(&mut scope)?
        };
        // The marking, if one runs, ends.
        for _ in 0..4 {
            wasm.owned_array(&mut store, 1)?;
        }
        assert_eq!(
            wasm.value_of(&mut store, &to, Some(0))?,
            round,
            "round {round}"
        );
    }
    assert_eq!(steps.ran.load(Ordering::SeqCst), 0, "the marker never ran");
    Ok(())
}

#[test]
fn an_exception_of_a_v128_is_thrown_and_caught() -> Result<()> {
    // Its layout's align is elise's 8, the `v128` 16-aligned in the data.
    let mut config = Config::new();
    config.wasm_exceptions(true);
    let mut store = Store::new(&engine(&mut config)?, ());
    let module = Module::new(
        store.engine(),
        r#"(module
            (tag $t (param v128))
            (func (export "throw_and_catch") (param $v i64) (result i64)
                (block $caught (result v128)
                    (try_table (catch $t $caught)
                        (throw $t (i64x2.splat (local.get $v))))
                    unreachable)
                (i64x2.extract_lane 1)))"#,
    )?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let throw_and_catch = instance.get_typed_func::<i64, i64>(&mut store, "throw_and_catch")?;
    assert_eq!(throw_and_catch.call(&mut store, 7)?, 7);
    Ok(())
}

#[test]
fn compiled_code_allocates_an_array_of_its_length() -> Result<()> {
    // Over 64 MiB of `v128`s: taken for bytes, the length would make the
    // array over elise's largest object, 1 GiB.
    let mut store = store()?;
    let module = Module::new(
        store.engine(),
        r#"(module
            (type $v (array (mut v128)))
            (func (export "length") (param $len i32) (result i32)
                (array.len (array.new_default $v (local.get $len)))))"#,
    )?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let length = instance.get_typed_func::<i32, i32>(&mut store, "length")?;
    let len = (1 << 22) + 1;
    assert_eq!(length.call(&mut store, len)?, len);
    Ok(())
}
