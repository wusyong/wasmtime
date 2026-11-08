//! kun: stores that borrow the embedder's elise heap
//! (`Config::elise_heap_provider`), a `StoreSet`'s. The embedder's
//! safepoints' GCs and each store's keep every store's objects (their
//! globals'), moved; none that reads the roots runs while another store is
//! lent, nor the embedder's while any is; a store outside a set gets no GC
//! heap, one off the heap's thread or after the heap is replaced fails, and
//! a heap takes one set's source only.

use std::cell::RefCell;
use std::sync::Arc;
use wasmtime::elise::raw::{self, ArrayLayout, Layout, TypeRange};
use wasmtime::elise::{Heap, HeapOptions};
use wasmtime::*;

thread_local! {
    /// The embedder's heap, under its key.
    static DOM: RefCell<Option<(u64, Heap)>> = const { RefCell::new(None) };
}

/// Lends the thread's `DOM`, as kun's provider lends its DOM's heap.
struct Lender;

impl EliseHeapProvider for Lender {
    fn key(&self) -> Result<u64> {
        DOM.with_borrow(|dom| dom.as_ref().map(|(key, _)| *key))
            .ok_or_else(|| format_err!("no heap on this thread"))
    }

    fn with_heap(&self, key: u64, f: &mut dyn FnMut(&mut Heap)) -> Result<()> {
        DOM.with(|dom| {
            let mut dom = dom
                .try_borrow_mut()
                .map_err(|_| format_err!("the heap is in use"))?;
            match dom.as_mut() {
                Some((at, heap)) if *at == key => {
                    f(heap);
                    Ok(())
                }
                _ => bail!("no such heap on this thread"),
            }
        })
    }
}

/// The thread's `DOM`, checking itself after each GC, under `key`.
fn lend(key: u64) {
    let heap = Heap::new(HeapOptions {
        verify: true,
        ..HeapOptions::default()
    });
    DOM.set(Some((key, heap)));
}

fn dom<R>(f: impl FnOnce(&mut Heap) -> R) -> R {
    DOM.with_borrow_mut(|dom| f(&mut dom.as_mut().expect("a heap").1))
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_function_references(true);
    config.wasm_gc(true);
    config.collector(Collector::Elise);
    config.elise_heap_provider(Arc::new(Lender));
    Engine::new(&config)
}

const WAT: &str = r#"
    (module
        (type $s (struct (field (mut i64)) (field (mut anyref))))
        (global $g (mut anyref) (ref.null any))
        (func (export "set_global") (param $v i64)
            (global.set $g (struct.new $s (local.get $v) (ref.null any))))
        (func (export "global") (result i64)
            (struct.get $s 0 (ref.cast (ref $s) (global.get $g))))
        ;; `n` structs, garbage.
        (func (export "allocate") (param $n i32)
            (block $done
                (loop $more
                    (br_if $done (i32.eqz (local.get $n)))
                    (drop (struct.new $s (i64.const 0) (ref.null any)))
                    (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                    (br $more))))
    )
"#;

/// A store of `set` with `WAT` instantiated, its global's struct holding
/// `value`: its index and instance.
fn store(set: &mut StoreSet<()>, value: i64) -> Result<(usize, Instance)> {
    let engine = engine_of(set)?;
    let module = Module::new(&engine, WAT)?;
    let index = set.insert(Store::new(&engine, ()))?;
    let instance = set.with(index, |mut cx| -> Result<Instance> {
        let instance = Instance::new(&mut cx, &module, &[])?;
        let set_global = instance.get_typed_func::<i64, ()>(&mut cx, "set_global")?;
        set_global.call(&mut cx, value)?;
        Ok(instance)
    })??;
    Ok((index, instance))
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// The engine the test's sets are of.
fn engine_of(_: &StoreSet<()>) -> Result<Engine> {
    ENGINE.with_borrow_mut(|engine| {
        if engine.is_none() {
            *engine = Some(self::engine()?);
        }
        Ok(engine.clone().unwrap())
    })
}

/// A set of a new engine's.
fn set() -> Result<StoreSet<()>> {
    let engine = engine()?;
    ENGINE.set(Some(engine.clone()));
    StoreSet::new(&engine)
}

/// The value of the struct the global of store `index` holds.
fn global(set: &StoreSet<()>, index: usize, instance: Instance) -> Result<i64> {
    set.with(index, |mut cx| {
        let global = instance.get_typed_func::<(), i64>(&mut cx, "global")?;
        global.call(&mut cx, ())
    })?
}

/// Store `index` allocates `n` structs.
fn allocate(set: &StoreSet<()>, index: usize, instance: Instance, n: i32) -> Result<()> {
    set.with(index, |mut cx| {
        let allocate = instance.get_typed_func::<i32, ()>(&mut cx, "allocate")?;
        allocate.call(&mut cx, n)
    })?
}

/// The embedder's objects: arrays of 4 KiB (the nursery is 1 MiB at
/// first, and shrinks).
fn embedders_garbage(count: usize) {
    thread_local! {
        static WORDS: (TypeRange, u32) = {
            let range = TypeRange::reserve(1).unwrap();
            let layout = Layout {
                size: 8,
                edges: &[],
                array: Some(ArrayLayout {
                    length_at: 0,
                    element_bytes: 8,
                    element_edges: false,
                }),
            };
            let index = range.register(0, &layout, Box::new(())).unwrap();
            (range, index)
        };
    }
    let ty = WORDS.with(|(_, index)| *index);
    for _ in 0..count {
        dom(|heap| {
            heap.mutate(|cx| {
                raw::alloc_array(cx, ty, 511).unwrap();
            })
        });
    }
}

#[test]
fn the_embedders_gcs_keep_every_stores_objects() -> Result<()> {
    lend(1);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let (b, b_instance) = store(&mut set, 2)?;
    let before = dom(|heap| heap.stats());
    embedders_garbage(1024);
    dom(|heap| heap.collect());
    let after = dom(|heap| heap.stats());
    assert!(after.scavenges >= before.scavenges + 2, "{after:?}");
    assert!(after.gcs > before.gcs);
    assert_eq!(global(&set, a, a_instance)?, 1);
    assert_eq!(global(&set, b, b_instance)?, 2);
    drop(set);
    DOM.set(None);
    Ok(())
}

#[test]
fn a_stores_gcs_keep_the_other_stores_objects() -> Result<()> {
    lend(2);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let (b, b_instance) = store(&mut set, 2)?;
    let before = dom(|heap| heap.stats()).scavenges;
    allocate(&set, a, a_instance, 100_000)?;
    assert!(dom(|heap| heap.stats()).scavenges > before);
    assert_eq!(global(&set, b, b_instance)?, 2);
    assert_eq!(global(&set, a, a_instance)?, 1);
    // The embedder's GCs after keep `a`'s too: it runs no GC now.
    embedders_garbage(1024);
    dom(|heap| heap.collect());
    assert_eq!(global(&set, a, a_instance)?, 1);
    assert_eq!(global(&set, b, b_instance)?, 2);
    drop(set);
    DOM.set(None);
    Ok(())
}

#[test]
fn no_gc_reads_the_roots_while_another_store_is_lent() -> Result<()> {
    lend(3);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let (b, b_instance) = store(&mut set, 2)?;
    let before = dom(|heap| heap.stats()).scavenges;
    // Store `b` lent, `a`'s allocations run no GC: they go to the old
    // generation.
    set.with(b, |_| allocate(&set, a, a_instance, 100_000))??;
    assert_eq!(dom(|heap| heap.stats()).scavenges, before);
    assert!(dom(|heap| heap.should_collect()));
    allocate(&set, a, a_instance, 100)?;
    assert!(dom(|heap| heap.stats()).scavenges > before);
    assert_eq!(global(&set, a, a_instance)?, 1);
    assert_eq!(global(&set, b, b_instance)?, 2);
    drop(set);
    DOM.set(None);
    Ok(())
}

#[test]
fn the_embedders_safepoints_run_no_gc_while_a_store_is_lent() -> Result<()> {
    lend(4);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let before = dom(|heap| heap.stats()).scavenges;
    set.with(a, |_| {
        assert!(dom(|heap| heap.in_frames()));
        embedders_garbage(1024);
    })?;
    assert_eq!(dom(|heap| heap.stats()).scavenges, before);
    assert!(!dom(|heap| heap.in_frames()), "left as the lend ends");
    embedders_garbage(1);
    assert!(dom(|heap| heap.stats()).scavenges > before);
    assert_eq!(global(&set, a, a_instance)?, 1);
    // A lend unwound leaves its frames too.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        set.with(a, |_| panic!("unwinding")).unwrap();
    }));
    assert!(unwound.is_err());
    assert!(!dom(|heap| heap.in_frames()));
    drop(set);
    DOM.set(None);
    Ok(())
}

#[test]
fn a_store_outside_a_set_gets_no_gc_heap() -> Result<()> {
    lend(5);
    let engine = engine()?;
    let mut store = Store::new(&engine, ());
    let module = Module::new(&engine, WAT)?;
    // Its global's initial value is its first GC reference.
    let error = Instance::new(&mut store, &module, &[]).unwrap_err();
    assert!(format!("{error:?}").contains("`StoreSet`"), "{error:?}");
    DOM.set(None);
    Ok(())
}

#[test]
fn a_store_is_used_on_its_heaps_thread_only() -> Result<()> {
    lend(6);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let error = set.with(a, |mut cx| {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let global = a_instance.get_typed_func::<(), i64>(&mut cx, "global")?;
                    global.call(&mut cx, ())
                })
                .join()
                .unwrap()
        })
    })?;
    let error = error.unwrap_err();
    assert!(
        format!("{error:?}").contains("off the heap's thread"),
        "{error:?}"
    );
    assert_eq!(global(&set, a, a_instance)?, 1);
    drop(set);
    DOM.set(None);
    Ok(())
}

#[test]
fn a_store_whose_heap_went_fails() -> Result<()> {
    lend(7);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let error = set.with(a, |mut cx| {
        // The heap dropped while the store is lent: inside its frames, it
        // leaks.
        DOM.set(None);
        let global = a_instance.get_typed_func::<(), i64>(&mut cx, "global")?;
        global.call(&mut cx, ())
    })?;
    let error = error.unwrap_err();
    assert!(format!("{error:?}").contains("is gone"), "{error:?}");
    // Another heap under the same key (it may be where the first was) is
    // not the store's.
    lend(7);
    let error = set.with(a, |_| ()).unwrap_err();
    assert!(
        format!("{error:?}").contains("another elise heap"),
        "{error:?}"
    );
    drop(set);
    DOM.set(None);
    Ok(())
}

#[test]
fn a_store_lent_another_heap_fails() -> Result<()> {
    lend(9);
    let mut set = set()?;
    let (a, _) = store(&mut set, 1)?;
    // Another heap under the same key, the store's kept alive: the set
    // lends the store no more, nor touches the other (its frames, its
    // roots source).
    let kept = DOM.take();
    lend(9);
    let error = set.with(a, |_| ()).unwrap_err();
    assert!(
        format!("{error:?}").contains("another elise heap"),
        "{error:?}"
    );
    let other = StoreSet::<()>::new(&engine_of(&set)?)?;
    drop(set);
    assert!(dom(|heap| heap.has_roots_source()), "the other set's stays");
    drop(other);
    DOM.set(None);
    drop(kept);
    Ok(())
}

#[test]
fn a_heap_takes_one_sets_source() -> Result<()> {
    lend(8);
    let set = set()?;
    let error = StoreSet::<()>::new(&engine_of(&set)?).err().unwrap();
    assert!(
        format!("{error:?}").contains("roots source already"),
        "{error:?}"
    );
    // Once the first goes, its source goes with it.
    drop(set);
    StoreSet::<()>::new(&engine_of_new()?)?;
    DOM.set(None);
    Ok(())
}

fn engine_of_new() -> Result<Engine> {
    let engine = engine()?;
    ENGINE.set(Some(engine.clone()));
    Ok(engine)
}
