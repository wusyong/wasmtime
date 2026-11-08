//! Unchecked methods for working with the data inside GC objects.

use crate::{Result, V128, bail_bug};
use core::cell::UnsafeCell;
use core::mem;
use core::ops::Range;
use core::ptr;

/// A plain-old-data type that can be stored in a `ValType` or a `StorageType`.
pub trait PodValType<const SIZE: usize>: Copy {
    /// Read an instance of `Self` from the given native-endian bytes.
    fn read_le(le_bytes: &[u8; SIZE]) -> Self;

    /// Write `self` into the given memory location, as native-endian bytes.
    fn write_le(&self, into: &mut [u8; SIZE]);
}

macro_rules! impl_pod_val_type {
    ( $( $t:ty , )* ) => {
        $(
            impl PodValType<{mem::size_of::<$t>()}> for $t {
                fn read_le(le_bytes: &[u8; mem::size_of::<$t>()]) -> Self {
                    <$t>::from_le_bytes(*le_bytes)
                }
                fn write_le(&self, into: &mut [u8; mem::size_of::<$t>()]) {
                    *into = self.to_le_bytes();
                }
            }
        )*
    };
}

impl_pod_val_type! {
    u8,
    u16,
    u32,
    u64,
    i8,
    i16,
    i32,
    i64,
}

impl PodValType<{ mem::size_of::<V128>() }> for V128 {
    fn read_le(le_bytes: &[u8; mem::size_of::<V128>()]) -> Self {
        u128::from_le_bytes(*le_bytes).into()
    }
    fn write_le(&self, into: &mut [u8; mem::size_of::<V128>()]) {
        *into = self.as_u128().to_le_bytes();
    }
}

/// The backing storage for a GC-managed object.
///
/// Methods on this type do not, generally, check against things like type
/// mismatches or that the given offset to read from even falls on a field
/// boundary. Omitting these checks is memory safe, due to our untrusted,
/// indexed GC heaps. Providing incorrect offsets will result in general
/// incorrectness, such as wrong answers or even panics, however.
///
/// Finally, these methods *will* panic on out-of-bounds accesses, either out of
/// the GC heap's bounds or out of this object's bounds. The former is necessary
/// for preserving the memory safety of indexed GC heaps in the face of (for
/// example) collector bugs, but the latter is just a defensive technique to
/// catch bugs early and prevent action at a distance as much as possible.
///
/// kun: its bytes are cells, written through a shared reference, never a
/// `&mut` (as V8's `TaggedField` reads and writes a heap's fields through
/// raw pointers): a collector whose marker reads an object's GC references
/// on another thread while the runtime writes its other fields (elise's)
/// can't have a `&mut` cover the object. Only one writes at a time all the
/// same: a `GcStore` hands it out borrowed mutably, and it isn't `Sync`.
#[repr(transparent)]
pub struct VMGcObjectData {
    data: [UnsafeCell<u8>],
}

macro_rules! impl_pod_methods {
    ( $( $T:ty, $read:ident, $write:ident; )* ) => {
        $(
            /// Read a `
            #[doc = stringify!($T)]
            /// ` field this object.
            ///
            /// Returns an error on out-of-bounds accesses.
            #[inline]
            pub fn $read(&self, offset: u32) -> Result<$T>
            {
                self.read_pod::<{ mem::size_of::<$T>() }, $T>(offset)
            }

            /// Write a `
            #[doc = stringify!($T)]
            /// ` into this object.
            ///
            /// Returns an error on out-of-bounds accesses.
            #[inline]
            pub fn $write(&self, offset: u32, val: $T) -> Result<()>
            {
                self.write_pod::<{ mem::size_of::<$T>() }, $T>(offset, val)
            }
        )*
    };
}

impl VMGcObjectData {
    /// kun: the object's `len` bytes at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` is valid for reads and writes of `len` bytes for `'a`, which
    /// no reference but other `VMGcObjectData`s covers meanwhile (a
    /// collector's marker may read them through raw pointers, atomically).
    #[inline]
    pub unsafe fn from_raw_parts<'a>(ptr: *mut u8, len: usize) -> &'a Self {
        let cells = ptr::slice_from_raw_parts(ptr.cast::<UnsafeCell<u8>>(), len);
        // SAFETY: `UnsafeCell<u8>` is `u8`'s layout, and `VMGcObjectData` is
        // transparent over a slice of them; valid for `'a` as the caller
        // promises.
        unsafe { &*(cells as *const VMGcObjectData) }
    }

    /// kun: the address of `len` bytes at `offset`, checked to be in the
    /// object.
    #[inline]
    fn bytes(&self, offset: u32, len: usize) -> Result<*mut u8> {
        let start = usize::try_from(offset)?;
        match start.checked_add(len) {
            Some(end) if end <= self.data.len() => {
                // In bounds: a pointer into the cells, for reads and writes.
                Ok(UnsafeCell::raw_get(self.data.as_ptr()).wrapping_add(start))
            }
            _ => bail_bug!("out of bounds access"),
        }
    }

    /// Read a POD field out of this object.
    ///
    /// Returns an error on out-of-bounds accesses.
    ///
    /// Don't generally use this method, use `read_u8`, `read_i64`,
    /// etc... instead.
    #[inline]
    fn read_pod<const N: usize, T>(&self, offset: u32) -> Result<T>
    where
        T: PodValType<N>,
    {
        assert_eq!(N, mem::size_of::<T>());
        let from = self.bytes(offset, N)?;
        // SAFETY: `N` bytes of the object (`bytes`), not written meanwhile
        // (one writer at a time, on this thread).
        let bytes = unsafe { from.cast::<[u8; N]>().read_unaligned() };
        Ok(T::read_le(&bytes))
    }

    /// Read a POD field out of this object.
    ///
    /// Returns an error on out-of-bounds accesses.
    ///
    /// Don't generally use this method, use `write_u8`, `write_i64`,
    /// etc... instead.
    #[inline]
    fn write_pod<const N: usize, T>(&self, offset: u32, val: T) -> Result<()>
    where
        T: PodValType<N>,
    {
        assert_eq!(N, mem::size_of::<T>());
        let into = self.bytes(offset, N)?;
        let mut bytes = [0; N];
        val.write_le(&mut bytes);
        // SAFETY: `N` bytes of the object (`bytes`), cells no reference
        // covers but this one's.
        unsafe { into.cast::<[u8; N]>().write_unaligned(bytes) };
        Ok(())
    }

    /// kun: copy `into.len()` bytes of this object's data at `offset` into
    /// `into` (which a slice of the data, upstream's `slice`, can't be: the
    /// data's cells may be written through another `VMGcObjectData`).
    ///
    /// Note that GC data is always stored in little-endian order, and this
    /// method does not do any conversions to/from host endianness for you.
    ///
    /// Returns an error on out-of-bounds accesses.
    #[inline]
    pub fn copy_to_slice(&self, offset: u32, into: &mut [u8]) -> Result<()> {
        let from = self.bytes(offset, into.len())?;
        // SAFETY: `into.len()` bytes of the object (`bytes`), which `into`,
        // a `&mut` of its own, doesn't overlap.
        unsafe { ptr::copy_nonoverlapping(from, into.as_mut_ptr(), into.len()) };
        Ok(())
    }

    /// Copy the given slice into this object's data at the given offset.
    ///
    /// Note that GC data is always stored in little-endian order, and this
    /// method does not do any conversions to/from host endianness for you.
    ///
    /// Returns an error on out-of-bounds accesses.
    #[inline]
    pub fn copy_from_slice(&self, offset: u32, src: &[u8]) -> Result<()> {
        let into = self.bytes(offset, src.len())?;
        // SAFETY: `src.len()` bytes of the object (`bytes`), which `src`, a
        // `&[u8]` (no `VMGcObjectData`'s cells), doesn't overlap.
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), into, src.len()) };
        Ok(())
    }

    /// Copy within this this object's data.
    ///
    /// Note that GC data is always stored in little-endian order, and this
    /// method does not do any conversions to/from host endianness for you.
    ///
    /// Returns an error on out-of-bounds accesses.
    #[inline]
    pub fn copy_within(&self, src: Range<u32>, dest: u32) -> Result<()> {
        let Some(len) = src.end.checked_sub(src.start) else {
            bail_bug!("invalid copy range");
        };
        let len = usize::try_from(len)?;
        let from = self.bytes(src.start, len)?;
        let into = self.bytes(dest, len)?;
        // SAFETY: two ranges of `len` bytes of the object (`bytes`), which
        // may overlap (a `memmove`).
        unsafe { ptr::copy(from, into, len) };
        Ok(())
    }

    impl_pod_methods! {
        u8, read_u8, write_u8;
        u16, read_u16, write_u16;
        u32, read_u32, write_u32;
        u64, read_u64, write_u64;
        i8, read_i8, write_i8;
        i16, read_i16, write_i16;
        i32, read_i32, write_i32;
        i64, read_i64, write_i64;
        V128, read_v128, write_v128;
    }
}
