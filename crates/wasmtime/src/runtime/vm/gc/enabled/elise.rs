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
//! It doesn't compile GC instructions yet: only the host's API reaches its
//! objects.

use super::*;
use crate::{
    Engine, Trap, bail_bug,
    prelude::*,
    vm::{
        ExternRefHostDataId, GarbageCollection, GcHeap, GcProgress, GcRoot, GcRootsIter, GcRuntime,
        GcStoreTraceState, SendSyncUnsafeCell, VMGcHeader, VMGcObjectData, VMGcRef,
        VMMemoryDefinition,
    },
};
use ::elise::raw::{
    AllocError, ArrayLayout, CAGE_BYTES, EngineHeap, Layout as EliseLayout, RegisterError,
    TypeRange,
};
use ::elise::{HeapOptions, Id, InlinePlatform, Platform};
use core::ptr::NonNull;
use core::sync::atomic::AtomicUsize;
use core::{alloc::Layout, any::Any, mem, slice};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use wasmtime_environ::elise::{ARRAY_LENGTH_OFFSET, EliseTypeLayouts};
use wasmtime_environ::{
    GcArrayLayout, GcLayout, GcStructLayout, GcTypeLayouts, VMGcKind, VMSharedTypeIndex,
};

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
}

impl EliseCollector {
    pub fn new(heap: Option<(HeapOptions, Arc<dyn Platform>)>) -> Result<Self> {
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
            memory: None,
            types: self.types.clone(),
            cache: Vec::new(),
            externref: self.externref,
            externrefs: Vec::new(),
            options: self.options,
            platform: self.platform.clone(),
            no_gc_count: 0,
            vmctx_data: SendSyncUnsafeCell::new(0),
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

/// A GC heap of elise's: an `EngineHeap` while it is attached.
struct EliseHeap {
    /// Dropped as it is detached: a pooling allocator gives the next store
    /// this `EliseHeap`, which must have none of the last one's objects.
    heap: Option<EngineHeap>,
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
    /// What compiled Wasm would reach through `vmctx_gc_heap_data`: nothing
    /// yet.
    vmctx_data: SendSyncUnsafeCell<u64>,
}

impl EliseHeap {
    fn heap(&self) -> &EngineHeap {
        self.heap.as_ref().expect("an attached heap")
    }

    fn heap_mut(&mut self) -> &mut EngineHeap {
        self.heap.as_mut().expect("an attached heap")
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
        alloc: impl FnOnce(&mut EngineHeap) -> Result<u32, AllocError>,
        bytes: usize,
    ) -> Result<Result<VMGcRef, u64>> {
        let bits = match alloc(self.heap_mut()) {
            Ok(bits) => bits,
            Err(AllocError::GcDue) => return Ok(Err(u64::try_from(bytes)?)),
            Err(AllocError::TooLarge) => return Err(Trap::AllocationTooLarge.into()),
        };
        let gc_ref = VMGcRef::from_raw_u32(bits).expect("an object's bits");
        // SAFETY: the object just allocated, zeroed, its data 8-aligned and
        // at least a `VMGcHeader`'s bytes (its layout's first).
        unsafe {
            self.heap()
                .cage()
                .add(usize::try_from(bits)?)
                .cast::<VMGcHeader>()
                .write(header);
        }
        Ok(Ok(gc_ref))
    }

    fn object_bytes(&self, gc_ref: &VMGcRef) -> Result<(*mut u8, usize)> {
        let offset = Self::offset(gc_ref)?;
        let heap = self.heap();
        // SAFETY: as the caller of the `GcHeap` method promises, an object
        // of this heap, live (the store roots what it reaches).
        let size = unsafe { heap.size(u32::try_from(offset)?) };
        Ok((heap.cage().wrapping_add(offset), size))
    }
}

unsafe impl GcHeap for EliseHeap {
    fn is_attached(&self) -> bool {
        self.memory.is_some()
    }

    fn attach(&mut self, memory: crate::vm::Memory) {
        assert!(!self.is_attached());
        self.memory = Some(memory);
        self.heap = Some(EngineHeap::new(self.options, self.platform.clone()));
    }

    fn detach(&mut self) -> crate::vm::Memory {
        assert!(self.is_attached());
        self.heap = None;
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
        unsafe { self.heap_mut().write_bits(object, offset, bits) };
        Ok(())
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
        self.gc_object_data_mut(&gc_ref)?
            .write_u32(EXTERNREF_HOST_DATA_OFFSET, host_data.into_raw())?;
        // SAFETY: the object just allocated.
        let id = unsafe { self.heap_mut().new_id(gc_ref.as_raw_u32()) };
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
        // data starts with its `VMGcHeader`, 8-aligned.
        let header = unsafe { &*self.heap().cage().add(offset).cast::<VMGcHeader>() };
        debug_assert!(header.kind().is_some(), "header: invalid VMGcKind");
        Ok(header)
    }

    fn header_mut(&mut self, gc_ref: &VMGcRef) -> Result<&mut VMGcHeader> {
        let offset = Self::offset(gc_ref)?;
        // SAFETY: as `header`'s; `&mut self` borrows the heap's objects.
        let header = unsafe { &mut *self.heap_mut().cage().add(offset).cast::<VMGcHeader>() };
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
        self.heap().used_bytes()
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

    unsafe fn vmctx_gc_heap_data(&self) -> NonNull<u8> {
        // SAFETY: only a pointer: nothing reads or writes it yet.
        let data = unsafe { self.vmctx_data.get() };
        NonNull::new(data).unwrap().cast()
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
        let cage = NonNull::new(self.heap().cage()).expect("the cage isn't null");
        VMMemoryDefinition {
            base: cage.into(),
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
        // SAFETY: a live object's bytes (`object_bytes`), committed; only
        // `&mut self` writes them.
        Ok(unsafe { slice::from_raw_parts(data, size) }.into())
    }

    fn gc_object_data_mut(&mut self, gc_ref: &VMGcRef) -> Result<&mut VMGcObjectData> {
        let (data, size) = self.object_bytes(gc_ref)?;
        // SAFETY: as `gc_object_data`'s; `&mut self` borrows the heap's
        // objects, and no worker runs (an `InlinePlatform`).
        Ok(unsafe { slice::from_raw_parts_mut(data, size) }.into())
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
        let heap = self.heap.heap_mut();
        // SAFETY: the store's roots, every GC reference to the heap off it
        // (the stack's, globals', tables', the host's), each the bits of a
        // live object or an `i31ref`; given twice alike, as `bits` is.
        unsafe {
            if heap.should_collect() {
                heap.safepoint_with_roots(give);
            } else {
                heap.collect_with_roots(give);
            }
        }
        for (root, bits) in roots.iter_mut().zip(bits) {
            match VMGcRef::from_raw_u32(bits) {
                Some(gc_ref) => root.set(gc_ref),
                None => bail_bug!("elise gave a root back as none"),
            }
        }
        let EliseHeap {
            heap, externrefs, ..
        } = &mut *self.heap;
        let heap = heap.as_mut().expect("an attached heap");
        let mut dead = Vec::new();
        externrefs.retain(|&(id, host_data)| {
            let lives = heap.id_lives(id);
            if !lives {
                dead.push(host_data);
            }
            lives
        });
        for host_data in dead {
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
        let collector = EliseCollector::new(None).unwrap();
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
