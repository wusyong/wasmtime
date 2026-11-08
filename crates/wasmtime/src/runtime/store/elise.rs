//! kun: the stores that borrow an embedder's elise heap
//! (`Config::elise_heap_provider`), a `StoreSet`'s.
//!
//! The heap is the embedder's (kun's DOM's), and its GCs run in the
//! embedder's calls and in each store's: every GC that reads the roots
//! needs each store's. The set owns its stores, so it knows them all, and
//! which it lends now: its roots source (`SetSource`) gives elise the roots
//! of those it doesn't, and none at all while another than the one whose
//! GC runs is lent (its frames, or the embedder's use of it, may be on the
//! stack). While it lends a store it counts the heap's frames
//! (`Heap::enter_frames`), so that the embedder's safepoints run no GC
//! that reads the roots meanwhile. It isn't `Send`: its stores stay on the
//! heap's thread.

use crate::prelude::*;
use crate::runtime::vm::{EliseSlot, SetShared, SetSource, SetStore, SetStores};
use crate::store::StoreOpaque;
use crate::{AsContextMut, Engine, Store, StoreContextMut};
use alloc::rc::Rc;
use alloc::sync::Arc;
use core::cell::RefCell;

/// A store of a set: lent while borrowed, gone once `None`.
type Cell<T> = Rc<RefCell<Option<Store<T>>>>;

impl<T: 'static> SetStore for RefCell<Option<Store<T>>> {
    fn lent(&self) -> bool {
        self.try_borrow_mut().is_err()
    }

    fn roots(&self, visit: &mut dyn FnMut(&mut u32)) {
        if let Some(store) = self.borrow_mut().as_mut() {
            let opaque: &mut StoreOpaque = &mut store.inner;
            opaque.elise_trace_roots(visit);
        }
    }
}

/// kun: the stores that borrow the embedder's elise heap
/// (`Config::elise_heap_provider`) its thread lends now: a document's, as
/// kun's. A store of an engine with a provider allocates its GC heap only
/// in a set (else that fails), and the set owns it: it lends it
/// (`with`) and drops it (`remove`). Not `Send` (its stores are behind
/// `Rc`, shared with the heap's roots source): they stay on the heap's
/// thread.
pub struct StoreSet<T: 'static> {
    shared: Arc<SetShared>,
    /// By index; the source's table holds them too.
    stores: Vec<Cell<T>>,
    /// The source's table.
    table: SetStores,
    engine: Engine,
}

impl<T: 'static> StoreSet<T> {
    /// A set for the heap this thread lends now, whose roots source it
    /// becomes.
    ///
    /// # Errors
    ///
    /// If `engine` has no `Config::elise_heap_provider`, the thread lends
    /// no heap, or the heap has a roots source already (another set's).
    pub fn new(engine: &Engine) -> Result<Self> {
        let Some(provider) = engine.config().elise_provider() else {
            bail!("a `StoreSet` needs `Config::elise_heap_provider`");
        };
        let key = provider.key()?;
        let table = SetStores::default();
        let mut made = Err(format_err!("the embedder lent no elise heap"));
        provider.with_heap(key, &mut |heap| {
            made = if heap.has_roots_source() {
                Err(format_err!(
                    "the embedder's elise heap has a roots source already"
                ))
            } else {
                let shared = Arc::new(SetShared::new(provider.clone(), key, heap));
                let source = SetSource {
                    shared: shared.clone(),
                    stores: table.clone(),
                };
                // SAFETY: it replaces none.
                unsafe { heap.set_roots_source(Some(Box::new(source))) };
                Ok(shared)
            };
        })?;
        let shared = made?;
        Ok(Self {
            shared,
            stores: Vec::new(),
            table,
            engine: engine.clone(),
        })
    }

    /// Takes `store`, its GC heap to borrow the set's heap: its index.
    ///
    /// # Errors
    ///
    /// If its engine isn't the set's, or it has a GC heap already (made
    /// with no set, it owns one).
    pub fn insert(&mut self, mut store: Store<T>) -> Result<usize> {
        if !Engine::same(store.engine(), &self.engine) {
            bail!("a store of another engine than its `StoreSet`'s");
        }
        let index = self.stores.len();
        let opaque: &mut StoreOpaque = &mut store.inner;
        if opaque.gc_store.is_some() || opaque.elise_slot.is_some() {
            bail!("a store with a GC heap of its own, or another `StoreSet`'s");
        }
        opaque.elise_slot = Some(EliseSlot {
            shared: self.shared.clone(),
            index,
        });
        let cell = Rc::new(RefCell::new(Some(store)));
        self.table.borrow_mut().push(Some(cell.clone()));
        self.stores.push(cell);
        Ok(index)
    }

    /// Lends the store at `index` to `f`: the heap counts frames meanwhile
    /// (the embedder's safepoints run no GC that reads the roots), and the
    /// set's roots source gives none (but in its own GCs, wasmtime giving
    /// its roots). Lending another one in `f` is allowed; the same one
    /// isn't.
    ///
    /// # Errors
    ///
    /// If there is no store at `index`, it is lent already, or the
    /// embedder can't lend the heap now (it is in use).
    pub fn with<R>(&self, index: usize, f: impl FnOnce(StoreContextMut<'_, T>) -> R) -> Result<R> {
        let Some(cell) = self.stores.get(index) else {
            bail!("no store at {index}");
        };
        let Ok(mut store) = cell.try_borrow_mut() else {
            bail!("the store at {index} is lent already");
        };
        let Some(store) = store.as_mut() else {
            bail!("no store at {index}");
        };
        self.with_heap(|heap| heap.enter_frames())?;
        /// The lend ended, returned from or unwound: its frames are off
        /// the stack.
        struct Lend<'a, T: 'static>(&'a StoreSet<T>);
        impl<T: 'static> Drop for Lend<'_, T> {
            fn drop(&mut self) {
                // SAFETY: the frames this lend entered: the store's wasm,
                // called in it, returned or unwound. If the heap can't be
                // lent now, its frames stay counted: it runs fewer GCs.
                let _ = self.0.with_heap(|heap| unsafe { heap.leave_frames() });
            }
        }
        let _lend = Lend(self);
        Ok(f(store.as_context_mut()))
    }

    /// Drops the store at `index`: its objects are garbage.
    ///
    /// # Errors
    ///
    /// If there is no store at `index`.
    pub fn remove(&mut self, index: usize) -> Result<()> {
        let Some(store) = self
            .stores
            .get(index)
            .and_then(|cell| cell.borrow_mut().take())
        else {
            bail!("no store at {index}");
        };
        // Its cell, empty, gives the source no roots.
        drop(store);
        Ok(())
    }

    /// Whether there is a store at `index`.
    pub fn contains(&self, index: usize) -> bool {
        self.stores
            .get(index)
            .is_some_and(|cell| cell.try_borrow().map_or(true, |store| store.is_some()))
    }

    /// `f` given the set's heap (not another the key lends now).
    fn with_heap(&self, f: impl FnOnce(&mut crate::elise::Heap)) -> Result<()> {
        self.shared.with_heap(f)
    }
}

impl<T: 'static> Drop for StoreSet<T> {
    fn drop(&mut self) {
        // Its source gives no roots once its stores go (their cells empty).
        for cell in &self.stores {
            drop(cell.borrow_mut().take());
        }
        // SAFETY: its stores are gone. If the heap can't be lent now, the
        // source stays, giving none.
        let _ = self.with_heap(|heap| unsafe { heap.set_roots_source(None) });
    }
}
