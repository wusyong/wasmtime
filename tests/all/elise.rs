//! kun: elise, kun's GC, as the collector, through the host's API: GC types
//! registered and let go with the engine's, objects laid out and read and
//! written as their types say (`i31ref`s, GC references, `externref`s'
//! host data), a pooling allocator's heap given to the next store afresh,
//! and a store moved between threads with its heap. Its GCs, given the
//! store's roots (the host's, globals', tables'), keep what they reach and
//! give an `externref`'s host data back once it dies; one an allocation
//! finds due is elise's to choose, the embedder's is a full GC; a marking
//! runs a step at a time between the host's writes. Every heap of these
//! checks itself after each GC.

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
