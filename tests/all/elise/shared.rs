//! machopo: stores that borrow the embedder's elise heap
//! (`Config::elise_heap_provider`), a `StoreSet`'s. The embedder's
//! safepoints' GCs and each store's keep every store's objects (their
//! globals'), moved; none that reads the roots runs while another store is
//! lent, nor the embedder's while any is; a store with no GC heap runs the
//! set's heap's; a store outside a set gets no GC
//! heap, one off the heap's thread or after the heap is replaced fails, and
//! a heap takes one set's source only. A marking of the heap a few objects
//! at a time keeps what two stores' compiled code moves meanwhile, and the
//! SATB barrier what compiled code cut from an object the marker hadn't
//! reached.

use std::cell::RefCell;
use std::sync::Arc;
use wasmtime::elise::raw::{self, ArrayLayout, Layout, TypeRange};
use wasmtime::elise::{Heap, HeapOptions, Job, JobDelegate, JobHandle, Platform};
use wasmtime::*;

thread_local! {
    /// The embedder's heap, under its key.
    static DOM: RefCell<Option<(u64, Heap)>> = const { RefCell::new(None) };
}

/// Lends the thread's `DOM`, as machopo's provider lends its DOM's heap.
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

/// `lend`, with `options` and `platform`.
fn lend_with(key: u64, options: HeapOptions, platform: Arc<dyn Platform>) {
    DOM.set(Some((key, Heap::with_platform(options, platform))));
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
fn a_store_with_no_gc_heap_gcs_the_sets_heap() -> Result<()> {
    lend(10);
    let mut set = set()?;
    let (a, a_instance) = store(&mut set, 1)?;
    let engine = engine_of(&set)?;
    let plain = set.insert(Store::new(&engine, ()))?;
    let gcs = dom(|heap| heap.stats()).gcs;
    set.with(plain, |mut cx| cx.gc(None))??;
    assert_eq!(dom(|heap| heap.stats()).gcs, gcs + 1, "a full GC, none due");
    assert_eq!(global(&set, a, a_instance)?, 1);
    // Another store lent, none.
    set.with(a, |_| set.with(plain, |mut cx| cx.gc(None)))???;
    assert_eq!(dom(|heap| heap.stats()).gcs, gcs + 1);
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

/// A module whose `run` calls the host's `dispatch`, which calls the
/// module's `listener` back (as an embedder dispatches an event to the
/// guest's own listener, within the guest's call), then gives its count.
const CALLS_BACK: &str = r#"
    (module
        (import "" "dispatch" (func $dispatch (param i32)))
        (global $count (mut i32) (i32.const 0))
        (func (export "run") (param $depth i32) (result i32)
            (call $dispatch (local.get $depth))
            (global.get $count))
        (func (export "listener") (param $depth i32)
            (global.set $count (i32.add (global.get $count) (i32.const 1)))
            (if (i32.gt_u (local.get $depth) (i32.const 1))
                (then (call $dispatch (i32.sub (local.get $depth) (i32.const 1)))))))
"#;

/// The host's `dispatch`: a sync call of the caller's `listener`.
fn dispatch(mut caller: Caller<'_, ()>, depth: i32) -> Result<()> {
    let listener = caller
        .get_export("listener")
        .and_then(Extern::into_func)
        .expect("the module's listener");
    listener.call(&mut caller, &[Val::I32(depth)], &mut [])
}

/// `future` polled until it is done (nothing it waits for is outside it).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

/// On a set's store's fiber (it was entered with `call_async`) a host
/// function calls the guest that called it, with a sync `call`, each
/// call on the fiber's stack under the one before.
#[test]
fn a_host_function_calls_its_guest_back_on_a_sets_fiber() -> Result<()> {
    lend(11);
    let mut set = set()?;
    let engine = engine_of(&set)?;
    let module = Module::new(&engine, CALLS_BACK)?;
    let index = set.insert(Store::new(&engine, ()))?;
    let count = set.with(index, |mut cx| -> Result<i32> {
        let host = Func::wrap(&mut cx, dispatch);
        let instance = Instance::new(&mut cx, &module, &[host.into()])?;
        let run = instance.get_typed_func::<i32, i32>(&mut cx, "run")?;
        block_on(run.call_async(&mut cx, 3))
    })??;
    assert_eq!(count, 3);
    drop(set);
    DOM.set(None);
    Ok(())
}

/// A store of no set doesn't: its fiber may resume on any thread.
#[test]
fn a_store_of_no_set_takes_no_sync_call_on_its_fiber() -> Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, CALLS_BACK)?;
    let mut store = Store::new(&engine, ());
    let host = Func::wrap(&mut store, dispatch);
    let instance = Instance::new(&mut store, &module, &[host.into()])?;
    let run = instance.get_typed_func::<i32, i32>(&mut store, "run")?;
    let error = block_on(run.call_async(&mut store, 1)).unwrap_err();
    assert!(
        format!("{error:?}").contains("cannot do blocking `call` from an async context"),
        "{error:?}"
    );
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

const SLOTS_WAT: &str = r#"
    (module
        (type $s (struct (field (mut i64)) (field (mut anyref))))
        (type $a (array (mut anyref)))
        (global $slots (mut (ref null $a)) (ref.null $a))
        (global $ballast (mut anyref) (ref.null any))
        (func (export "init") (param $n i32)
            (global.set $slots (array.new $a (ref.null any) (i32.const 8)))
            ;; `n` structs in a list, live: a marking of them takes steps.
            (block $done
                (loop $more
                    (br_if $done (i32.eqz (local.get $n)))
                    (global.set $ballast
                        (struct.new $s (i64.const 0) (global.get $ballast)))
                    (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                    (br $more))))
        (func $at (param $i i32) (result (ref $s))
            (ref.cast (ref $s) (array.get $a (global.get $slots) (local.get $i))))
        ;; A new struct (a GC first, under stress) into slot `i`.
        (func (export "put") (param $i i32) (param $v i64)
            (array.set $a (global.get $slots) (local.get $i)
                (struct.new $s (local.get $v) (ref.null any))))
        ;; Slot `i`'s to slot `j`, `i` cleared.
        (func (export "move") (param $i i32) (param $j i32)
            (local $v anyref)
            (local.set $v (array.get $a (global.get $slots) (local.get $i)))
            (array.set $a (global.get $slots) (local.get $i) (ref.null any))
            (array.set $a (global.get $slots) (local.get $j) (local.get $v)))
        ;; Slot `j`'s hung on slot `i`'s second field, `j` cleared.
        (func (export "hang") (param $i i32) (param $j i32)
            (struct.set $s 1 (call $at (local.get $i))
                (array.get $a (global.get $slots) (local.get $j)))
            (array.set $a (global.get $slots) (local.get $j) (ref.null any)))
        ;; What slot `i`'s struct hangs into slot `j`, cut from it: the array
        ;; may be scanned, and the struct not yet (SATB keeps it).
        (func (export "unhang") (param $i i32) (param $j i32)
            (local $v anyref)
            (local.set $v (struct.get $s 1 (call $at (local.get $i))))
            (array.set $a (global.get $slots) (local.get $j) (local.get $v))
            (struct.set $s 1 (call $at (local.get $i)) (ref.null any)))
        ;; Slot `i`'s value, -1 for none; with `hung`, its hung struct's.
        (func (export "value") (param $i i32) (param $hung i32) (result i64)
            (local $v anyref)
            (local.set $v (array.get $a (global.get $slots) (local.get $i)))
            (if (local.get $hung)
                (then (local.set $v
                    (struct.get $s 1 (ref.cast (ref $s) (local.get $v))))))
            (if (result i64) (ref.is_null (local.get $v))
                (then (i64.const -1))
                (else (struct.get $s 0 (ref.cast (ref $s) (local.get $v))))))
    )
"#;

/// A struct as the model has it: its value, and what it hangs.
#[derive(Clone)]
struct Node {
    value: i64,
    hung: Option<Box<Node>>,
}

/// A store of `SLOTS_WAT`, and what its slots hold.
struct Slots {
    index: usize,
    instance: Instance,
    model: [Option<Node>; 8],
}

impl Slots {
    fn new(set: &mut StoreSet<()>, ballast: i32) -> Result<Self> {
        let engine = engine_of(set)?;
        let module = Module::new(&engine, SLOTS_WAT)?;
        let index = set.insert(Store::new(&engine, ()))?;
        let instance = set.with(index, |mut cx| -> Result<Instance> {
            let instance = Instance::new(&mut cx, &module, &[])?;
            let init = instance.get_typed_func::<i32, ()>(&mut cx, "init")?;
            init.call(&mut cx, ballast)?;
            Ok(instance)
        })??;
        Ok(Self {
            index,
            instance,
            model: Default::default(),
        })
    }

    fn call<P: WasmParams, R: WasmResults>(
        &self,
        set: &StoreSet<()>,
        name: &str,
        params: P,
    ) -> Result<R> {
        let instance = self.instance;
        set.with(self.index, |mut cx| {
            instance
                .get_typed_func::<P, R>(&mut cx, name)?
                .call(&mut cx, params)
        })?
    }

    /// Every slot, and what it hangs, read by compiled code, as the model
    /// says.
    fn check(&self, set: &StoreSet<()>, round: i64) -> Result<()> {
        for (i, slot) in self.model.iter().enumerate() {
            let at = i32::try_from(i)?;
            let value: i64 = self.call(set, "value", (at, 0))?;
            assert_eq!(
                value,
                slot.as_ref().map_or(-1, |n| n.value),
                "round {round}, slot {i}"
            );
            if let Some(node) = slot {
                let value: i64 = self.call(set, "value", (at, 1))?;
                let hung = node.hung.as_ref().map_or(-1, |n| n.value);
                assert_eq!(value, hung, "round {round}, slot {i}'s hung");
            }
        }
        Ok(())
    }
}

/// A platform whose posted jobs (a marking's) run only in `step`, each run
/// asked to yield (after 64 objects, a marking's).
#[derive(Default)]
struct Yielding {
    posted: std::sync::Mutex<Vec<(usize, Arc<dyn Job + Send>)>>,
    next: std::sync::Mutex<usize>,
    ran: std::sync::atomic::AtomicUsize,
}

impl Yielding {
    fn step(&self) {
        let jobs: Vec<_> = self
            .posted
            .lock()
            .unwrap()
            .iter()
            .map(|(_, job)| job.clone())
            .collect();
        for job in jobs {
            job.run(&YieldingWorker);
            self.ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

struct YieldingWorker;

impl JobDelegate for YieldingWorker {
    fn notify_concurrency_increase(&self) {}

    fn is_joining_thread(&self) -> bool {
        false
    }

    fn should_yield(&self) -> bool {
        true
    }
}

struct YieldingHandle(Arc<Yielding>, usize);

impl JobHandle for YieldingHandle {
    fn notify_concurrency_increase(&self) {}

    fn cancel(&self) {
        self.0
            .posted
            .lock()
            .unwrap()
            .retain(|&(id, _)| id != self.1);
    }
}

struct YieldingSteps(Arc<Yielding>);

impl Platform for YieldingSteps {
    fn run(&self, job: &dyn Job) {
        wasmtime::elise::InlinePlatform.run(job);
    }

    fn post_job(&self, job: Arc<dyn Job + Send>) -> Option<Box<dyn JobHandle>> {
        let mut next = self.0.next.lock().unwrap();
        *next += 1;
        self.0.posted.lock().unwrap().push((*next, job));
        Some(Box::new(YieldingHandle(self.0.clone(), *next)))
    }

    fn posts_jobs(&self) -> bool {
        true
    }
}

/// A marking of the shared heap a few objects at a time: two stores'
/// compiled code moves objects between slots, hangs them on each other and
/// takes them back (`array.set`, `struct.set`, the slots in a global), and
/// the embedder allocates; under stress each allocation GCs (a scavenge, a
/// marking's start or end), the marker running a step (64 objects) between
/// each. What each store holds is read back as its model says: the
/// barriers on the DOM's heap (SATB, the generational one) keep what the
/// marker hadn't reached.
#[test]
fn a_marking_of_the_shared_heap_steps_between_compiled_writes() -> Result<()> {
    let steps = Arc::new(Yielding::default());
    lend_with(
        11,
        HeapOptions {
            stress: true,
            verify: true,
            ..HeapOptions::default()
        },
        Arc::new(YieldingSteps(steps.clone())),
    );
    let mut set = set()?;
    let mut stores = [Slots::new(&mut set, 300)?, Slots::new(&mut set, 300)?];
    let mut seed = 7_u64;
    let mut random = |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for round in 0..600_i64 {
        let which = random(5);
        if which == 4 {
            embedders_garbage(1);
        } else {
            let store = &mut stores[usize::try_from(which % 2)?];
            let (i, j) = (random(8), random(8));
            let (i_at, j_at) = (usize::try_from(i)?, usize::try_from(j)?);
            let (i, j) = (i32::try_from(i)?, i32::try_from(j)?);
            match random(4) {
                0 => {
                    store.call::<_, ()>(&set, "put", (i, round))?;
                    store.model[i_at] = Some(Node {
                        value: round,
                        hung: None,
                    });
                }
                1 if i != j => {
                    store.call::<_, ()>(&set, "move", (i, j))?;
                    store.model[j_at] = store.model[i_at].take();
                }
                2 if i != j && store.model[i_at].is_some() => {
                    store.call::<_, ()>(&set, "hang", (i, j))?;
                    let hung = store.model[j_at].take().map(Box::new);
                    store.model[i_at].as_mut().unwrap().hung = hung;
                }
                3 if i != j && store.model[i_at].is_some() => {
                    store.call::<_, ()>(&set, "unhang", (i, j))?;
                    let hung = store.model[i_at].as_mut().unwrap().hung.take();
                    store.model[j_at] = hung.map(|node| *node);
                }
                _ => {}
            }
        }
        steps.step();
        for store in &stores {
            store.check(&set, round)?;
        }
    }
    assert!(
        steps.ran.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "a marking ran a step"
    );
    assert!(dom(|heap| heap.stats()).concurrent_markings > 0);
    drop(set);
    DOM.set(None);
    Ok(())
}

const CUT_WAT: &str = r#"
    (module
        (type $s (struct (field (mut i64)) (field (mut anyref))))
        (global $list (mut anyref) (ref.null any))
        (global $keep (mut anyref) (ref.null any))
        ;; `n` structs (value 0) in a list, its last `P` (1) hanging `H` (42):
        ;; a marking reaches `P` last.
        (func (export "init") (param $n i32)
            (global.set $list
                (struct.new $s (i64.const 1) (struct.new $s (i64.const 42) (ref.null any))))
            (block $done
                (loop $more
                    (br_if $done (i32.eqz (local.get $n)))
                    (global.set $list (struct.new $s (i64.const 0) (global.get $list)))
                    (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                    (br $more))))
        ;; `H` into a global (a root: no barrier), cut from `P`.
        (func (export "cut")
            (local $node (ref $s))
            (local.set $node (ref.cast (ref $s) (global.get $list)))
            (block $found
                (loop $next
                    (br_if $found (i64.eq (struct.get $s 0 (local.get $node)) (i64.const 1)))
                    (local.set $node
                        (ref.cast (ref $s) (struct.get $s 1 (local.get $node))))
                    (br $next)))
            (global.set $keep (struct.get $s 1 (local.get $node)))
            (struct.set $s 1 (local.get $node) (ref.null any)))
        (func (export "kept") (result i64)
            (struct.get $s 0 (ref.cast (ref $s) (global.get $keep))))
    )
"#;

/// Compiled code cuts an edge as a marking of the shared heap starts,
/// before the marker reached it, the object it held put in a global: the
/// marking's end reads no root (SATB's snapshot was taken), so only the
/// SATB barrier on the cut (`struct.set`) keeps the object.
#[test]
fn the_satb_barrier_on_the_shared_heap_keeps_what_compiled_code_cut() -> Result<()> {
    let steps = Arc::new(Yielding::default());
    lend_with(
        12,
        HeapOptions {
            stress: true,
            verify: true,
            ..HeapOptions::default()
        },
        Arc::new(YieldingSteps(steps.clone())),
    );
    let mut set = set()?;
    let engine = engine_of(&set)?;
    let module = Module::new(&engine, CUT_WAT)?;
    let index = set.insert(Store::new(&engine, ()))?;
    let instance = set.with(index, |mut cx| -> Result<Instance> {
        let instance = Instance::new(&mut cx, &module, &[])?;
        instance
            .get_typed_func::<i32, ()>(&mut cx, "init")?
            .call(&mut cx, 300)?;
        Ok(instance)
    })??;
    let call = |name: &str| -> Result<()> {
        set.with(index, |mut cx| {
            instance
                .get_typed_func::<(), ()>(&mut cx, name)?
                .call(&mut cx, ())
        })?
    };
    // The embedder's safepoints GC (under stress, each); none runs in a
    // store's call that allocates nothing.
    for _ in 0..20 {
        if dom(|heap| heap.is_marking()) {
            break;
        }
        embedders_garbage(1);
    }
    assert!(dom(|heap| heap.is_marking()), "a marking started");
    call("cut")?;
    for _ in 0..20 {
        if !dom(|heap| heap.is_marking()) {
            break;
        }
        steps.step();
        embedders_garbage(1);
    }
    assert!(!dom(|heap| heap.is_marking()), "the marking ended");
    let kept = set.with(index, |mut cx| {
        instance
            .get_typed_func::<(), i64>(&mut cx, "kept")?
            .call(&mut cx, ())
    })??;
    assert_eq!(kept, 42);
    drop(set);
    DOM.set(None);
    Ok(())
}
