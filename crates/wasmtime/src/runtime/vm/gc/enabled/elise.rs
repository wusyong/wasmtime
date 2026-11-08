//! kun: elise, kun's GC, as a collector.
//!
//! elise's heap is its own 4 GiB cage (`EngineHeap`), and the GC heap: the
//! memory Wasmtime gives a heap is only held, and given back. A `VMGcRef`
//! is an object's data's offset in the cage, the bits elise's edges hold;
//! the data starts with the `VMGcHeader` (`EliseTypeLayouts`), after
//! elise's own header. An `i31ref` is encoded as elise's `I31` is.
//!
//! Each GC type of an engine is registered in elise as the engine's type
//! registry registers it, and retired as the registry unregisters it: a
//! store holds every type it allocates while it lives, so no heap has a
//! reachable object of a type the registry lets go. `externref`s are a
//! type of their own, an object holding its host data's id.
//!
//! The runtime writes an object's GC references through `write_gc_ref_in`
//! and `init_gc_ref_in`, with elise's barriers (the generational one and
//! the marking's); a root's (a global's, a table's) is a plain write, a GC
//! being given them all.
//!
//! A GC is elise's, given the store's roots: what elise is due to do (a
//! scavenge, a marking's start or end) once an allocation found one due,
//! else a full GC (the embedder's `Store::gc`). Each `externref` has an id
//! of elise's: after a GC, those whose object died give their host data
//! back.
//!
//! A heap may instead be an embedder's, borrowed (`Config::elise_heap_provider`):
//! kun's DOM's, whose wasm GC objects live beside its nodes. Its stores are a
//! `StoreSet`'s, which gives elise their roots at every GC (`SetSource`), and
//! counts their frames while it lends them. They stay on the heap's thread:
//! each use checks it, and that the heap still lives.
//!
//! Compiled code reaches the heap through elise's barrier state
//! (`vmctx_gc_heap_data`): its write barriers read it inline, and call
//! `elise_satb` and `elise_remember` when they must
//! (`crates/cranelift/src/func_environ/gc/elise.rs`).

use super::*;
use crate::{
    Engine, Trap, bail_bug,
    prelude::*,
    vm::{
        ExternRefHostDataId, GarbageCollection, GcHeap, GcProgress, GcRoot, GcRootsIter, GcRuntime,
        GcStoreTraceState, VMGcHeader, VMGcObjectData, VMGcRef, VMMemoryDefinition,
    },
};
use ::elise::raw::{
    self as raw, AllocError, ArrayLayout, CAGE_BYTES, EngineHeap, HeapAlive, Layout as EliseLayout,
    RegisterError, TypeRange,
};
use ::elise::{Heap, HeapOptions, Id, InlinePlatform, Platform, RootsSource};
use core::cell::RefCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{alloc::Layout, any::Any, mem};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use wasmtime_environ::elise::{
    ARRAY_LENGTH_OFFSET, BARRIER_YOUNG_PAGES_OFFSET, EliseTypeLayouts, PAGE_SHIFT,
    barrier_marking_offset,
};
use wasmtime_environ::{
    GcArrayLayout, GcLayout, GcStructLayout, GcTypeLayouts, VMGcKind, VMSharedTypeIndex,
};

// Compiled code reads elise's barrier state where `wasmtime_environ::elise`
// says it is (no dependency on elise there).
#[expect(clippy::cast_possible_truncation, reason = "a pointer's size, 4 or 8")]
const _: () = assert!(
    ::elise::raw::BarrierState::YOUNG_PAGES == BARRIER_YOUNG_PAGES_OFFSET as usize
        && ::elise::raw::BarrierState::MARKING
            == barrier_marking_offset(mem::size_of::<usize>() as u8) as usize
        && ::elise::raw::PAGE_SHIFT == PAGE_SHIFT
);

/// How many ids a range of elise's type table holds: an engine reserves
/// ranges as its types need them, so that many engines (each test's) share
/// the table.
const RANGE_IDS: u32 = 256;

/// The bytes of an `externref`: its `VMGcHeader`, then its host data's id.
const EXTERNREF_BYTES: u32 = 12;

/// The offset of an `externref`'s host data's id.
const EXTERNREF_HOST_DATA_OFFSET: u32 = 8;

/// A GC type as elise has it.
#[derive(Clone, Copy, Debug)]
struct EliseType {
    /// Its index in elise's type table.
    index: u32,
    /// Its id in the engine's ranges.
    id: u32,
    /// An array's fixed bytes and element bytes.
    array: Option<(u32, u32)>,
}

/// An engine's GC types in elise.
#[derive(Default)]
struct TypeTable {
    /// By `VMSharedTypeIndex::bits`.
    by_type: Vec<Option<EliseType>>,
    /// Ids `0..RANGE_IDS` of each, then the next's.
    ranges: Vec<TypeRange>,
    /// Ids retired, to register again once elise frees them.
    retired: VecDeque<u32>,
    /// The ids below were given.
    next: u32,
}

impl TypeTable {
    /// Registers `layout` at an id of its own: a retired one elise freed,
    /// else a new one.
    fn register(&mut self, layout: &EliseLayout<'_>) -> Result<(u32, u32), OutOfMemory> {
        for _ in 0..self.retired.len() {
            let id = self.retired.pop_front().expect("as many as its length");
            match self.register_at(id, layout) {
                Ok(index) => return Ok((index, id)),
                Err(RegisterError::Occupied) => self.retired.push_back(id),
                Err(e) => panic!("elise refused a GC type: {e}"),
            }
        }
        let id = self.next;
        if id / RANGE_IDS == u32::try_from(self.ranges.len()).unwrap() {
            let range = TypeRange::reserve(RANGE_IDS).ok_or(OutOfMemory::new(0))?;
            self.ranges.push(range);
        }
        self.next = id.checked_add(1).ok_or(OutOfMemory::new(0))?;
        match self.register_at(id, layout) {
            Ok(index) => Ok((index, id)),
            Err(e) => panic!("elise refused a new GC type: {e}"),
        }
    }

    fn register_at(&self, id: u32, layout: &EliseLayout<'_>) -> Result<u32, RegisterError> {
        let range = &self.ranges[usize::try_from(id / RANGE_IDS).unwrap()];
        range.register(id % RANGE_IDS, layout, Box::new(()))
    }

    /// Retires the type at `id`.
    ///
    /// # Safety
    ///
    /// As `TypeRange::retire`: nothing allocates it from now on, and no
    /// heap has a reachable object of it.
    unsafe fn retire(&mut self, id: u32) {
        let range = &self.ranges[usize::try_from(id / RANGE_IDS).unwrap()];
        // SAFETY: as the caller promises.
        unsafe { range.retire(id % RANGE_IDS) };
        self.retired.push_back(id);
    }
}

/// elise's layout of a GC type of `layout`: the same bytes (the
/// `VMGcHeader` first), its GC references its edges.
fn elise_layout(layout: &GcLayout) -> (u32, Vec<u32>, Option<ArrayLayout>) {
    match layout {
        GcLayout::Struct(s) => {
            let edges = s
                .fields
                .iter()
                .filter(|f| f.is_gc_ref)
                .map(|f| f.offset)
                .collect();
            (s.size, edges, None)
        }
        GcLayout::Array(a) => (
            a.base_size,
            Vec::new(),
            Some(ArrayLayout {
                length_at: ARRAY_LENGTH_OFFSET,
                element_bytes: a.elem_size,
                element_edges: a.elems_are_gc_refs,
            }),
        ),
    }
}

/// elise as a collector.
pub struct EliseCollector {
    layouts: EliseTypeLayouts,
    types: Arc<Mutex<TypeTable>>,
    /// The `externref` type's index in elise's table.
    externref: u32,
    /// Each heap's options and platform (`Config::elise_heap`).
    options: HeapOptions,
    platform: Arc<dyn Platform>,
    /// The embedder's heap the stores borrow, if they don't own one
    /// (`Config::elise_heap_provider`).
    provider: Option<Arc<dyn EliseHeapProvider>>,
}

impl EliseCollector {
    pub fn new(
        heap: Option<(HeapOptions, Arc<dyn Platform>)>,
        provider: Option<Arc<dyn EliseHeapProvider>>,
    ) -> Result<Self> {
        let (options, platform) =
            heap.unwrap_or_else(|| (HeapOptions::default(), Arc::new(InlinePlatform)));
        let mut types = TypeTable::default();
        let (externref, _) = types.register(&EliseLayout {
            size: EXTERNREF_BYTES,
            edges: &[],
            array: None,
        })?;
        Ok(Self {
            layouts: EliseTypeLayouts,
            types: Arc::new(Mutex::new(types)),
            externref,
            options,
            platform,
            provider,
        })
    }
}

unsafe impl GcRuntime for EliseCollector {
    fn layouts(&self) -> &dyn GcTypeLayouts {
        &self.layouts
    }

    fn new_gc_heap(&self, _: &Engine) -> Result<Box<dyn GcHeap>> {
        Ok(Box::new(EliseHeap {
            heap: None,
            cage: None,
            lends: self.provider.is_some(),
            memory: None,
            types: self.types.clone(),
            cache: Vec::new(),
            externref: self.externref,
            externrefs: Vec::new(),
            options: self.options,
            platform: self.platform.clone(),
            no_gc_count: 0,
        }) as _)
    }

    fn register_type(&self, ty: VMSharedTypeIndex, layout: &GcLayout) -> Result<(), OutOfMemory> {
        let (size, edges, array) = elise_layout(layout);
        let layout = EliseLayout {
            size,
            edges: &edges,
            array,
        };
        let mut types = self.types.lock().unwrap();
        let (index, id) = types.register(&layout)?;
        let at = usize::try_from(ty.bits()).unwrap();
        if types.by_type.len() <= at {
            types.by_type.resize(at + 1, None);
        }
        debug_assert!(types.by_type[at].is_none());
        types.by_type[at] = Some(EliseType {
            index,
            id,
            array: array.map(|a| (size, a.element_bytes)),
        });
        Ok(())
    }

    fn unregister_type(&self, ty: VMSharedTypeIndex) {
        let mut types = self.types.lock().unwrap();
        let at = usize::try_from(ty.bits()).unwrap();
        let Some(ty) = types.by_type.get_mut(at).and_then(Option::take) else {
            return;
        };
        // SAFETY: the registry lets a type go once nothing holds it: no
        // store, since one holds every type it allocates (its modules', and
        // its host types) as long as it lives. So no heap allocates it any
        // more, nor has a reachable object of it; a heap's cache of it is a
        // store's that held it, and gone.
        unsafe { types.retire(ty.id) };
    }
}

impl Drop for EliseCollector {
    fn drop(&mut self) {
        let mut types = self.types.lock().unwrap();
        // SAFETY: the engine goes, so did every store and its heap: none
        // allocates `externref`s, or has one.
        unsafe { types.retire(0) };
    }
}

/// `elise_satb`: `old` is the old value of a reference field compiled code
/// overwrites, as a marking runs.
pub fn elise_satb(heap: &mut dyn GcHeap, old: u32) -> Result<()> {
    let Some(heap) = heap.as_any_mut().downcast_mut::<EliseHeap>() else {
        bail_bug!("elise's libcall on another collector's heap");
    };
    // SAFETY: a field's value, read in the heap, which holds the bits of
    // live objects only (elise's heap trusts compiled code, as its GC does).
    heap.bits(|bits| unsafe { bits.satb(old) })
}

/// `elise_remember`: compiled code wrote `value` at `slot`, the offset of a
/// field of an object.
pub fn elise_remember(heap: &mut dyn GcHeap, slot: u32, value: u32) -> Result<()> {
    let Some(heap) = heap.as_any_mut().downcast_mut::<EliseHeap>() else {
        bail_bug!("elise's libcall on another collector's heap");
    };
    // SAFETY: as `elise_satb`: compiled code gives a live object's field's
    // offset, and what it just wrote there.
    heap.bits(|bits| unsafe { bits.remember(slot, value) })
}

/// kun: an embedder's heap of elise's that stores borrow
/// (`Config::elise_heap_provider`), kun's DOM's: a thread's, found by a
/// key, as Blink finds cppgc's heap through its thread's `ThreadState`.
///
/// It is trusted with nothing: a store checks that what it lends is the
/// heap it was bound to (`raw::HeapAlive::is`), on the thread it was bound
/// on, and that the heap lives, so a provider that lends none, or another,
/// only makes the store's calls fail.
pub trait EliseHeapProvider: Send + Sync {
    /// The key of the heap this thread lends now.
    ///
    /// # Errors
    ///
    /// If this thread has none.
    fn key(&self) -> Result<u64>;

    /// Lends the heap `key` names to `f`.
    ///
    /// # Errors
    ///
    /// If this thread has no such heap, or can't lend it now (it is in
    /// use).
    fn with_heap(&self, key: u64, f: &mut dyn FnMut(&mut Heap)) -> Result<()>;
}

/// What a GC heap of elise's does through bits, its own (`EngineHeap`) or
/// an embedder's (`Heap`, through the `raw` functions).
trait Bits {
    fn alloc(&mut self, index: u32) -> Result<u32, AllocError>;
    fn alloc_array(&mut self, index: u32, length: u32) -> Result<u32, AllocError>;
    /// As `raw::write_bits_in`.
    unsafe fn write_bits(&mut self, object: u32, at: u32, bits: u32);
    /// As `raw::new_id_in`.
    unsafe fn new_id(&mut self, object: u32) -> Id;
    fn id_lives(&mut self, id: Id) -> bool;
    /// As `raw::satb_in`.
    unsafe fn satb(&mut self, old: u32);
    /// As `raw::remember_in`.
    unsafe fn remember(&mut self, slot: u32, value: u32);
    fn should_collect(&mut self) -> bool;
    /// As `Heap::safepoint_with_roots`.
    unsafe fn safepoint_with_roots(&mut self, roots: &mut dyn FnMut(&mut dyn FnMut(&mut u32)));
    /// As `Heap::collect_with_roots`.
    unsafe fn collect_with_roots(&mut self, roots: &mut dyn FnMut(&mut dyn FnMut(&mut u32)));
}

impl Bits for EngineHeap {
    fn alloc(&mut self, index: u32) -> Result<u32, AllocError> {
        EngineHeap::alloc(self, index)
    }
    fn alloc_array(&mut self, index: u32, length: u32) -> Result<u32, AllocError> {
        EngineHeap::alloc_array(self, index, length)
    }
    unsafe fn write_bits(&mut self, object: u32, at: u32, bits: u32) {
        // SAFETY: as the caller promises.
        unsafe { EngineHeap::write_bits(self, object, at, bits) }
    }
    unsafe fn new_id(&mut self, object: u32) -> Id {
        // SAFETY: as the caller promises.
        unsafe { EngineHeap::new_id(self, object) }
    }
    fn id_lives(&mut self, id: Id) -> bool {
        EngineHeap::id_lives(self, id)
    }
    unsafe fn satb(&mut self, old: u32) {
        // SAFETY: as the caller promises.
        unsafe { EngineHeap::satb(self, old) }
    }
    unsafe fn remember(&mut self, slot: u32, value: u32) {
        // SAFETY: as the caller promises.
        unsafe { EngineHeap::remember(self, slot, value) }
    }
    fn should_collect(&mut self) -> bool {
        EngineHeap::should_collect(self)
    }
    unsafe fn safepoint_with_roots(&mut self, roots: &mut dyn FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { EngineHeap::safepoint_with_roots(self, roots) }
    }
    unsafe fn collect_with_roots(&mut self, roots: &mut dyn FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { EngineHeap::collect_with_roots(self, roots) }
    }
}

impl Bits for Heap {
    fn alloc(&mut self, index: u32) -> Result<u32, AllocError> {
        raw::alloc_bits(self, index)
    }
    fn alloc_array(&mut self, index: u32, length: u32) -> Result<u32, AllocError> {
        raw::alloc_array_bits(self, index, length)
    }
    unsafe fn write_bits(&mut self, object: u32, at: u32, bits: u32) {
        // SAFETY: as the caller promises.
        unsafe { raw::write_bits_in(self, object, at, bits) }
    }
    unsafe fn new_id(&mut self, object: u32) -> Id {
        // SAFETY: as the caller promises.
        unsafe { raw::new_id_in(self, object) }
    }
    fn id_lives(&mut self, id: Id) -> bool {
        raw::id_lives_in(self, id)
    }
    unsafe fn satb(&mut self, old: u32) {
        // SAFETY: as the caller promises.
        unsafe { raw::satb_in(self, old) }
    }
    unsafe fn remember(&mut self, slot: u32, value: u32) {
        // SAFETY: as the caller promises.
        unsafe { raw::remember_in(self, slot, value) }
    }
    fn should_collect(&mut self) -> bool {
        Heap::should_collect(self)
    }
    unsafe fn safepoint_with_roots(&mut self, roots: &mut dyn FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { Heap::safepoint_with_roots(self, roots) }
    }
    unsafe fn collect_with_roots(&mut self, roots: &mut dyn FnMut(&mut dyn FnMut(&mut u32))) {
        // SAFETY: as the caller promises.
        unsafe { Heap::collect_with_roots(self, roots) }
    }
}

std::thread_local! {
    /// Its address names the thread (`this_thread`).
    static THREAD: u8 = const { 0 };
}

/// The current thread, as cheaply as a thread-local's address: a thread
/// that took another's after it ended has its address, but the heaps of
/// the one that ended are gone (`HeapAlive`).
fn this_thread() -> usize {
    THREAD.with(|thread| core::ptr::from_ref(thread) as usize)
}

/// A store's place in a `StoreSet` (`StoreOpaque::elise_slot`): its GC heap
/// borrows the set's heap, and gives the set's roots source its own.
#[derive(Clone)]
pub struct EliseSlot {
    pub(crate) shared: Arc<SetShared>,
    pub(crate) index: usize,
}

/// What a `StoreSet`'s stores' GC heaps share: how they borrow the heap,
/// and which of them runs a GC now.
pub struct SetShared {
    pub(crate) provider: Arc<dyn EliseHeapProvider>,
    pub(crate) key: u64,
    /// The heap: what the key lends later is it only if it is
    /// (`SetShared::with_heap`), and it lives while this says so.
    pub(crate) heap: HeapAlive,
    /// The index of the store whose GC runs now (`EliseCollection`), whose
    /// roots wasmtime gives: `usize::MAX` for none.
    pub(crate) gcing: AtomicUsize,
}

impl SetShared {
    pub(crate) fn new(provider: Arc<dyn EliseHeapProvider>, key: u64, heap: &Heap) -> Self {
        Self {
            provider,
            key,
            heap: raw::alive_in(heap),
            gcing: AtomicUsize::new(usize::MAX),
        }
    }

    /// `f` given the set's heap, if the key lends it (not another under
    /// the same key).
    ///
    /// # Errors
    ///
    /// If it doesn't.
    pub(crate) fn with_heap<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> Result<R> {
        let (mut f, mut out) = (Some(f), None);
        self.provider.with_heap(self.key, &mut |heap| {
            if self.heap.is(heap)
                && let Some(f) = f.take()
            {
                out = Some(f(heap));
            }
        })?;
        out.ok_or_else(|| format_err!("the embedder lent another elise heap, or none"))
    }
}

/// A store of a `StoreSet`, as its roots source sees it: lent while it is
/// borrowed.
pub(crate) trait SetStore {
    fn lent(&self) -> bool;
    /// Gives `visit` the store's roots, it not lent.
    fn roots(&self, visit: &mut dyn FnMut(&mut u32));
}

/// A `StoreSet`'s stores, by index (`None` once removed): the set and its
/// heap's roots source own them both (the source is the heap's, the
/// embedder's to drop), on the heap's thread.
pub(crate) type SetStores = Rc<RefCell<Vec<Option<Rc<dyn SetStore>>>>>;

/// A `StoreSet`'s stores' roots, for its heap's every GC that reads the
/// roots: none while one of them is lent (its frames, or the embedder's
/// `&mut` to it, may be on the stack) but the one whose GC runs, whose
/// roots wasmtime gives.
pub(crate) struct SetSource {
    pub(crate) shared: Arc<SetShared>,
    pub(crate) stores: SetStores,
}

// SAFETY: each store's roots are its GC references to the heap off it (its
// globals', tables', the host's; its stack's are none while it isn't lent),
// read where wasmtime's own GC reads them (`trace_roots`): 0, `i31ref`s and
// live objects. It gives them all, or (one lent) none; and the same ones
// again, as nothing runs between (no store's lend starts or ends in a GC).
unsafe impl RootsSource for SetSource {
    fn roots(&mut self, visit: &mut dyn FnMut(&mut u32)) -> bool {
        let gcing = self.shared.gcing.load(Ordering::Relaxed);
        let stores = self.stores.borrow();
        let others = || {
            stores
                .iter()
                .enumerate()
                .filter_map(move |(index, store)| store.as_ref().filter(|_| index != gcing))
        };
        if others().any(|store| store.lent()) {
            return false;
        }
        for store in others() {
            store.roots(visit);
        }
        true
    }
}

/// A heap borrowed from the embedder, as a store's GC heap has it.
struct Lent {
    slot: EliseSlot,
    thread: usize,
}

impl Lent {
    /// The heap's thread, the heap alive.
    fn check(&self) -> Result<()> {
        if self.thread != this_thread() {
            bail!("a store borrowing an embedder's elise heap is used off the heap's thread");
        }
        if !self.slot.shared.heap.get() {
            bail!("a store's embedder's elise heap is gone");
        }
        Ok(())
    }

    /// `f` given the heap, the one this store was bound to.
    fn with<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> Result<R> {
        self.check()?;
        self.slot.shared.with_heap(f)
    }
}

/// A GC heap's elise heap: its own, or the embedder's it borrows.
enum Backing {
    Owned(EngineHeap),
    Lent(Lent),
}

/// A GC heap of elise's: its heap while it is attached.
struct EliseHeap {
    /// Dropped as it is detached: a pooling allocator gives the next store
    /// this `EliseHeap`, which must have none of the last one's objects.
    /// A borrowed heap is bound after it is attached (`elise_bind`).
    heap: Option<Backing>,
    /// The heap's cage's start and its barrier state, where they stay while
    /// it lives.
    cage: Option<(crate::vm::SendSyncPtr<u8>, NonNull<u8>)>,
    /// Whether it borrows the embedder's heap (`Config::elise_heap_provider`).
    lends: bool,
    /// What Wasmtime gave, held and given back.
    memory: Option<crate::vm::Memory>,
    types: Arc<Mutex<TypeTable>>,
    /// The engine's types this heap allocated, by `VMSharedTypeIndex::bits`:
    /// the store holds them as long as the heap is attached.
    cache: Vec<Option<EliseType>>,
    externref: u32,
    /// Each `externref`'s id and host data, to give back once it dies.
    externrefs: Vec<(Id, ExternRefHostDataId)>,
    options: HeapOptions,
    platform: Arc<dyn Platform>,
    no_gc_count: usize,
}

// SAFETY: its barrier state's pointer is an `EngineHeap`'s (which is
// `Send` and `Sync`, and moves with it) or a borrowed heap's, which it
// reads, and gives compiled code, only on the heap's thread (`Lent::check`,
// and `check_thread` as wasm is entered).
unsafe impl Send for EliseHeap {}
// SAFETY: as above; its `&self` methods read objects through the cage's
// start only on the heap's thread, or its own `EngineHeap`'s (`Sync`).
unsafe impl Sync for EliseHeap {}

/// Binds a store's GC heap, just attached, to the heap its `StoreSet`
/// borrows: a GC heap of elise's that borrows the embedder's heap needs
/// one (`slot`), the others none.
pub fn elise_bind(heap: &mut dyn GcHeap, slot: Option<&EliseSlot>) -> Result<()> {
    let Some(heap) = heap.as_any_mut().downcast_mut::<EliseHeap>() else {
        if slot.is_some() {
            bail!("a `StoreSet`'s store's GC heap isn't elise's");
        }
        return Ok(());
    };
    match (heap.lends, slot) {
        (false, None) => Ok(()),
        (false, Some(_)) => bail!("a `StoreSet` needs `Config::elise_heap_provider`"),
        (true, None) => {
            bail!("a store whose GC heap borrows the embedder's elise heap is a `StoreSet`'s")
        }
        (true, Some(slot)) => {
            let (cage, barrier) = slot
                .shared
                .with_heap(|heap| (raw::cage_in(heap), raw::barrier_state_in(heap).cast::<u8>()))?;
            heap.cage = Some((
                crate::vm::SendSyncPtr::new(NonNull::new(cage).unwrap()),
                barrier,
            ));
            heap.heap = Some(Backing::Lent(Lent {
                slot: slot.clone(),
                thread: this_thread(),
            }));
            Ok(())
        }
    }
}

/// The GC of a `StoreSet`'s heap that a store with no GC heap asks for
/// (`Store::gc`): what the heap is due to do, else a full GC, given none of
/// the store's roots (it has no GC reference), the other stores' from the
/// set's source.
pub fn elise_gc_without_heap(slot: &EliseSlot) -> Result<()> {
    let shared = &slot.shared;
    shared.gcing.store(slot.index, Ordering::Relaxed);
    // SAFETY: none of the store's roots: it has none.
    let collected = shared.with_heap(|heap| unsafe {
        if heap.should_collect() {
            heap.safepoint_with_roots(|_| {});
        } else {
            heap.collect_with_roots(|_| {});
        }
    });
    shared.gcing.store(usize::MAX, Ordering::Relaxed);
    collected
}

impl EliseHeap {
    /// The cage's start, the heap checked first (a borrowed one's thread,
    /// and that it lives).
    fn cage(&self) -> Result<*mut u8> {
        match &self.heap {
            Some(Backing::Owned(_)) => {}
            Some(Backing::Lent(lent)) => lent.check()?,
            None => bail_bug!("no heap bound"),
        }
        Ok(self.cage.expect("a bound heap's cage").0.as_ptr())
    }

    /// `f` given the heap.
    fn bits<R>(&mut self, f: impl FnOnce(&mut dyn Bits) -> R) -> Result<R> {
        self.cage()?;
        match self.heap.as_mut().expect("a bound heap") {
            Backing::Owned(heap) => Ok(f(heap)),
            Backing::Lent(lent) => lent.with(|heap| f(heap)),
        }
    }

    /// The type `ty` is in elise.
    fn elise_type(&mut self, ty: VMSharedTypeIndex) -> Result<EliseType> {
        let at = usize::try_from(ty.bits())?;
        if let Some(Some(ty)) = self.cache.get(at) {
            return Ok(*ty);
        }
        let Some(found) = self
            .types
            .lock()
            .unwrap()
            .by_type
            .get(at)
            .copied()
            .flatten()
        else {
            bail_bug!("no elise type for {ty:?}");
        };
        if self.cache.len() <= at {
            self.cache.resize(at + 1, None);
        }
        self.cache[at] = Some(found);
        Ok(found)
    }

    /// The offset of the object `gc_ref` refers to.
    fn offset(gc_ref: &VMGcRef) -> Result<usize> {
        let offset = usize::try_from(gc_ref.heap_index()?.get())?;
        if offset >= CAGE_BYTES || offset % mem::align_of::<VMGcHeader>() != 0 {
            bail_bug!("gc object out of elise's cage, or misaligned");
        }
        Ok(offset)
    }

    /// A new object from elise, its `VMGcHeader` written.
    fn alloc(
        &mut self,
        header: VMGcHeader,
        alloc: impl FnOnce(&mut dyn Bits) -> Result<u32, AllocError>,
        bytes: usize,
    ) -> Result<Result<VMGcRef, u64>> {
        let bits = match self.bits(alloc)? {
            Ok(bits) => bits,
            Err(AllocError::GcDue) => return Ok(Err(u64::try_from(bytes)?)),
            Err(AllocError::TooLarge) => return Err(Trap::AllocationTooLarge.into()),
        };
        let gc_ref = VMGcRef::from_raw_u32(bits).expect("an object's bits");
        // SAFETY: the object just allocated, zeroed, its data 8-aligned and
        // at least a `VMGcHeader`'s bytes (its layout's first).
        unsafe {
            self.cage()?
                .add(usize::try_from(bits)?)
                .cast::<VMGcHeader>()
                .write(header);
        }
        Ok(Ok(gc_ref))
    }

    fn object_bytes(&self, gc_ref: &VMGcRef) -> Result<(*mut u8, usize)> {
        let offset = Self::offset(gc_ref)?;
        let cage = self.cage()?;
        // SAFETY: as the caller of the `GcHeap` method promises, an object
        // of this heap, live (the store roots what it reaches); the heap's
        // cage, checked.
        let size = unsafe { raw::size_at(cage, u32::try_from(offset)?) };
        Ok((cage.wrapping_add(offset), size))
    }
}

unsafe impl GcHeap for EliseHeap {
    fn is_attached(&self) -> bool {
        self.memory.is_some()
    }

    fn attach(&mut self, memory: crate::vm::Memory) {
        assert!(!self.is_attached());
        self.memory = Some(memory);
        if !self.lends {
            let heap = EngineHeap::new(self.options, self.platform.clone());
            let cage = NonNull::new(heap.cage()).expect("the cage isn't null");
            self.cage = Some((cage.into(), heap.barrier_state().cast()));
            self.heap = Some(Backing::Owned(heap));
        }
    }

    fn detach(&mut self) -> crate::vm::Memory {
        assert!(self.is_attached());
        self.heap = None;
        self.cage = None;
        self.cache.clear();
        // The store's host data table goes with it.
        self.externrefs.clear();
        self.no_gc_count = 0;
        self.memory.take().unwrap()
    }

    fn as_any(&self) -> &dyn Any {
        self as _
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self as _
    }

    fn enter_no_gc_scope(&mut self) {
        self.no_gc_count += 1;
    }

    fn exit_no_gc_scope(&mut self) {
        self.no_gc_count -= 1;
    }

    fn clone_gc_ref(&mut self, gc_ref: &VMGcRef) -> VMGcRef {
        gc_ref.unchecked_copy()
    }

    fn write_gc_ref(
        &mut self,
        destination: &mut Option<VMGcRef>,
        source: Option<&VMGcRef>,
    ) -> Result<()> {
        // A root's: a GC is given every root.
        *destination = source.map(|s| s.unchecked_copy());
        Ok(())
    }

    fn write_gc_ref_in(
        &mut self,
        object: &VMGcRef,
        offset: u32,
        source: Option<&VMGcRef>,
    ) -> Result<()> {
        let object = u32::try_from(Self::offset(object)?)?;
        let bits = source.map_or(0, |s| s.as_raw_u32());
        // SAFETY: as the caller promises, `object` is a live object of this
        // heap, `offset` one of its GC references (an edge of its type's
        // layout), and `source` none, an `i31ref` or a live object of it.
        self.bits(|heap| unsafe { heap.write_bits(object, offset, bits) })
    }

    fn init_gc_ref_in(
        &mut self,
        object: &VMGcRef,
        offset: u32,
        source: Option<&VMGcRef>,
    ) -> Result<()> {
        // The barrier too: in frames, elise allocates in the old generation,
        // whose edge to a young object it must record.
        self.write_gc_ref_in(object, offset, source)
    }

    fn expose_gc_ref_to_wasm(&mut self, _gc_ref: VMGcRef) -> Result<()> {
        Ok(())
    }

    fn alloc_externref(
        &mut self,
        host_data: ExternRefHostDataId,
    ) -> Result<Result<VMExternRef, u64>> {
        let index = self.externref;
        let gc_ref = match self.alloc(
            VMGcHeader::externref(),
            |heap| heap.alloc(index),
            usize::try_from(EXTERNREF_BYTES)?,
        )? {
            Ok(gc_ref) => gc_ref,
            Err(bytes_needed) => return Ok(Err(bytes_needed)),
        };
        self.gc_object_data(&gc_ref)?
            .write_u32(EXTERNREF_HOST_DATA_OFFSET, host_data.into_raw())?;
        let bits = gc_ref.as_raw_u32();
        // SAFETY: the object just allocated.
        let id = self.bits(|heap| unsafe { heap.new_id(bits) })?;
        self.externrefs.push((id, host_data));
        Ok(Ok(gc_ref.into_externref_unchecked()))
    }

    fn externref_host_data(&self, externref: &VMExternRef) -> Result<ExternRefHostDataId> {
        let data = self.gc_object_data(externref.as_gc_ref())?;
        match ExternRefHostDataId::from_raw(data.read_u32(EXTERNREF_HOST_DATA_OFFSET)?) {
            Some(id) => Ok(id),
            None => bail_bug!("invalid `ExternRefHostDataId`"),
        }
    }

    fn header(&self, gc_ref: &VMGcRef) -> Result<&VMGcHeader> {
        let offset = Self::offset(gc_ref)?;
        // SAFETY: as the caller promises, a live object of this heap, whose
        // data starts with its `VMGcHeader`, 8-aligned; its cage, checked.
        let header = unsafe { &*self.cage()?.add(offset).cast::<VMGcHeader>() };
        debug_assert!(header.kind().is_some(), "header: invalid VMGcKind");
        Ok(header)
    }

    fn header_mut(&mut self, gc_ref: &VMGcRef) -> Result<&mut VMGcHeader> {
        let offset = Self::offset(gc_ref)?;
        // SAFETY: as `header`'s; `&mut self` borrows the heap's objects.
        let header = unsafe { &mut *self.cage()?.add(offset).cast::<VMGcHeader>() };
        debug_assert!(header.kind().is_some(), "header_mut: invalid VMGcKind");
        Ok(header)
    }

    fn object_size(&self, gc_ref: &VMGcRef) -> Result<usize> {
        Ok(self.object_bytes(gc_ref)?.1)
    }

    fn alloc_raw(&mut self, header: VMGcHeader, layout: Layout) -> Result<Result<VMGcRef, u64>> {
        if layout.align() > 8 {
            return Err(Trap::AllocationTooLarge.into());
        }
        let bytes = layout.size();
        if header.kind() == Some(VMGcKind::ExternRef) {
            let index = self.externref;
            return self.alloc(header, |heap| heap.alloc(index), bytes);
        }
        let Some(ty) = header.ty() else {
            bail_bug!("a GC object's header without a type");
        };
        let ty = self.elise_type(ty)?;
        match ty.array {
            None => self.alloc(header, |heap| heap.alloc(ty.index), bytes),
            Some((base, element)) => {
                let length = (u32::try_from(bytes)? - base) / element;
                self.alloc(header, |heap| heap.alloc_array(ty.index, length), bytes)
            }
        }
    }

    fn alloc_uninit_struct_or_exn(
        &mut self,
        ty: VMSharedTypeIndex,
        layout: &GcStructLayout,
    ) -> Result<Result<VMGcRef, u64>> {
        let kind = if layout.is_exception {
            VMGcKind::ExnRef
        } else {
            VMGcKind::StructRef
        };
        let index = self.elise_type(ty)?.index;
        self.alloc(
            VMGcHeader::from_kind_and_index(kind, ty),
            |heap| heap.alloc(index),
            usize::try_from(layout.size)?,
        )
    }

    fn dealloc_uninit_struct_or_exn(&mut self, _structref: VMGcRef) -> Result<()> {
        // Garbage, its fields zero (none or 0): the GC frees it.
        Ok(())
    }

    fn alloc_uninit_array(
        &mut self,
        ty: VMSharedTypeIndex,
        length: u32,
        layout: &GcArrayLayout,
    ) -> Result<Result<VMArrayRef, u64>> {
        let index = self.elise_type(ty)?.index;
        let bytes = layout
            .size_for_len(length)
            .ok_or(Trap::AllocationTooLarge)?;
        // elise writes the length.
        let gc_ref = match self.alloc(
            VMGcHeader::from_kind_and_index(VMGcKind::ArrayRef, ty),
            |heap| heap.alloc_array(index, length),
            usize::try_from(bytes)?,
        )? {
            Ok(gc_ref) => gc_ref,
            Err(bytes_needed) => return Ok(Err(bytes_needed)),
        };
        Ok(Ok(gc_ref.into_arrayref_unchecked()))
    }

    fn dealloc_uninit_array(&mut self, _arrayref: VMArrayRef) -> Result<()> {
        Ok(())
    }

    fn array_len(&self, arrayref: &VMArrayRef) -> Result<u32> {
        self.gc_object_data(arrayref.as_gc_ref())?
            .read_u32(ARRAY_LENGTH_OFFSET)
    }

    fn allocated_bytes(&self) -> usize {
        if self.cage().is_err() {
            return 0;
        }
        match self.heap.as_ref().expect("a bound heap") {
            Backing::Owned(heap) => heap.used_bytes(),
            Backing::Lent(lent) => lent.with(|heap| heap.used_bytes()).unwrap_or(0),
        }
    }

    fn gc<'a, 'b>(
        &'a mut self,
        roots: GcRootsIter<'a>,
        trace_state: &'a mut GcStoreTraceState<'b>,
    ) -> Box<dyn GarbageCollection + 'a>
    where
        'b: 'a,
    {
        assert_eq!(self.no_gc_count, 0, "Cannot GC inside a no-GC scope!");
        Box::new(EliseCollection {
            heap: self,
            roots: Some(roots),
            trace_state,
        })
    }

    /// elise's barrier state, which stays where it is while the heap is
    /// attached: compiled code runs only then, its store's instances
    /// having the attached heap's.
    unsafe fn vmctx_gc_heap_data(&self) -> NonNull<u8> {
        self.cage.expect("a bound heap's barrier state").1
    }

    fn check_thread(&self) -> Result<()> {
        self.cage().map(|_| ())
    }

    fn take_memory(&mut self) -> crate::vm::Memory {
        debug_assert!(self.is_attached());
        self.memory.take().unwrap()
    }

    unsafe fn replace_memory(&mut self, memory: crate::vm::Memory, _delta_bytes_grown: u64) {
        debug_assert!(self.memory.is_none());
        self.memory = Some(memory);
    }

    fn vmmemory(&self) -> VMMemoryDefinition {
        // Only its address: compiled code reads it, on the heap's thread
        // (`check_thread`).
        let cage = self.cage.expect("a bound heap's cage").0;
        VMMemoryDefinition {
            base: cage.as_non_null().into(),
            current_length: AtomicUsize::new(CAGE_BYTES),
        }
    }

    fn capacity(&self) -> usize {
        CAGE_BYTES
    }

    fn heap_slice(&self) -> &[u8] {
        // Most of the cage is reserved, not committed: no slice of it.
        unreachable!("elise's heap is read an object at a time")
    }

    fn heap_slice_mut(&mut self) -> &mut [u8] {
        unreachable!("elise's heap is written an object at a time")
    }

    fn gc_object_data(&self, gc_ref: &VMGcRef) -> Result<&VMGcObjectData> {
        let (data, size) = self.object_bytes(gc_ref)?;
        // SAFETY: a live object's bytes (`object_bytes`), committed, which
        // nothing moves or frees while `&self` lasts (elise moves objects
        // only in the GCs given the roots: this store's take `&mut self`; a
        // borrowed heap's others run in the embedder's and other stores'
        // calls, not while this store's runtime reads it, on the heap's
        // thread, `object_bytes` checked). No
        // reference covers them: the runtime's are these cells, and elise's
        // marker reads the object's GC references atomically, on its
        // workers too. Those references are written only through
        // `write_gc_ref_in` and `init_gc_ref_in` (elise's `write_bits`, also
        // atomic); the runtime writes the other bytes, which the marker
        // doesn't read (README.kun.md, "Writes alongside elise's marker").
        Ok(unsafe { VMGcObjectData::from_raw_parts(data, size) })
    }
}

/// A GC of elise's: one increment, a single call into elise.
struct EliseCollection<'a, 'b> {
    heap: &'a mut EliseHeap,
    roots: Option<GcRootsIter<'a>>,
    trace_state: &'a mut GcStoreTraceState<'b>,
}

impl GarbageCollection for EliseCollection<'_, '_> {
    fn collect_increment(&mut self) -> Result<GcProgress> {
        let Some(roots) = self.roots.take() else {
            return Ok(GcProgress::Complete);
        };
        // Given to elise twice (before the GC, after it moved objects),
        // alike: so a list.
        let mut roots: Vec<GcRoot<'_>> = roots.collect();
        let mut bits = roots
            .iter()
            .map(|root| Ok(root.get()?.as_raw_u32()))
            .collect::<Result<Vec<u32>>>()?;
        let give = |visit: &mut dyn FnMut(&mut u32)| {
            for slot in bits.iter_mut() {
                visit(slot);
            }
        };
        let mut give = give;
        // A borrowed heap's other stores' roots come from its `StoreSet`'s
        // source, which skips this one's: wasmtime gives them.
        let gcing = match &self.heap.heap {
            Some(Backing::Lent(lent)) => Some(lent.slot.clone()),
            _ => None,
        };
        if let Some(slot) = &gcing {
            slot.shared.gcing.store(slot.index, Ordering::Relaxed);
        }
        // SAFETY: the store's roots, every GC reference to the heap off it
        // (the stack's, globals', tables', the host's), each the bits of a
        // live object or an `i31ref`; given twice alike, as `bits` is.
        let collected = self.heap.bits(|heap| unsafe {
            if heap.should_collect() {
                heap.safepoint_with_roots(&mut give);
            } else {
                heap.collect_with_roots(&mut give);
            }
        });
        if let Some(slot) = &gcing {
            slot.shared.gcing.store(usize::MAX, Ordering::Relaxed);
        }
        collected?;
        for (root, bits) in roots.iter_mut().zip(bits) {
            match VMGcRef::from_raw_u32(bits) {
                Some(gc_ref) => root.set(gc_ref),
                None => bail_bug!("elise gave a root back as none"),
            }
        }
        let externrefs = mem::take(&mut self.heap.externrefs);
        let lives = self.heap.bits(|heap| {
            externrefs
                .iter()
                .map(|&(id, _)| heap.id_lives(id))
                .collect::<Vec<bool>>()
        });
        let lives = match lives {
            Ok(lives) => lives,
            Err(e) => {
                self.heap.externrefs = externrefs;
                return Err(e);
            }
        };
        let (kept, dead): (Vec<_>, Vec<_>) = externrefs
            .into_iter()
            .zip(lives)
            .partition(|&(_, lives)| lives);
        self.heap.externrefs = kept.into_iter().map(|(entry, _)| entry).collect();
        for ((_, host_data), _) in dead {
            self.trace_state.host_data_table.dealloc(host_data)?;
        }
        Ok(GcProgress::Complete)
    }
}

// elise frees a retired type once each heap has done a full GC since (at
// once if there is none, or once the heaps go): these hold that no other
// test of this binary makes an elise heap.
#[cfg(test)]
mod tests {
    use super::*;

    /// Two GC references after the `VMGcHeader`.
    const PAIR: EliseLayout<'static> = EliseLayout {
        size: 16,
        edges: &[8, 12],
        array: None,
    };

    #[test]
    fn a_retired_id_is_given_again_once_elise_frees_it() {
        let mut types = TypeTable::default();
        let (_, first) = types.register(&PAIR).unwrap();
        let (_, second) = types.register(&PAIR).unwrap();
        assert_ne!(first, second);
        // A heap keeps it from being freed.
        let heap = EngineHeap::new(HeapOptions::default(), Arc::new(InlinePlatform));
        // SAFETY: nothing allocated it.
        unsafe { types.retire(first) };
        let (_, third) = types.register(&PAIR).unwrap();
        assert_ne!(third, first, "not freed yet");
        drop(heap);
        let (_, fourth) = types.register(&PAIR).unwrap();
        assert_eq!(fourth, first, "freed, and given again");
        for id in [second, third, fourth] {
            // SAFETY: as above.
            unsafe { types.retire(id) };
        }
    }

    #[test]
    fn ids_past_a_range_are_the_next_ranges() {
        let mut types = TypeTable::default();
        let ids: Vec<u32> = (0..=RANGE_IDS)
            .map(|_| types.register(&PAIR).unwrap().1)
            .collect();
        assert_eq!(ids.last(), Some(&RANGE_IDS));
        assert_eq!(types.ranges.len(), 2);
        for id in ids {
            // SAFETY: as above.
            unsafe { types.retire(id) };
        }
    }

    #[test]
    fn a_type_unregistered_is_retired() {
        let collector = EliseCollector::new(None, None).unwrap();
        let ty = VMSharedTypeIndex::new(3);
        let layout = GcLayout::Array(GcArrayLayout {
            base_size: 12,
            align: 8,
            elem_size: 4,
            elems_are_gc_refs: true,
        });
        collector.register_type(ty, &layout).unwrap();
        let id = {
            let types = collector.types.lock().unwrap();
            let found = types.by_type[3].expect("registered");
            assert_eq!(found.array, Some((12, 4)));
            found.id
        };
        collector.unregister_type(ty);
        let types = collector.types.lock().unwrap();
        assert!(types.by_type[3].is_none());
        assert_eq!(types.retired.back(), Some(&id));
    }
}
