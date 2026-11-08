//! machopo: layout of Wasm GC objects in elise, machopo's GC.
//!
//! elise's own header comes before an object's data, out of these layouts:
//! the data starts with the `VMGcHeader`, and a `VMGcRef` is the data's
//! offset in elise's cage, the bits elise's edges hold.

use super::*;

/// The size of the header for GC objects: the `VMGcHeader` alone.
pub const HEADER_SIZE: u32 = 8;

/// The align of the header for GC objects.
pub const HEADER_ALIGN: u32 = 8;

/// The offset of the length field in an array.
pub const ARRAY_LENGTH_OFFSET: u32 = HEADER_SIZE;

/// The offset of the tag-instance-index field in an exception header.
pub const EXCEPTION_TAG_INSTANCE_OFFSET: u32 = HEADER_SIZE;

/// The offset of the tag-defined-index field in an exception header.
pub const EXCEPTION_TAG_DEFINED_OFFSET: u32 = HEADER_SIZE + 4;

/// Where elise's barrier state (`elise::raw::BarrierState`, what
/// `VMContext::gc_heap_data` points at) has the young pages' flags' address,
/// a pointer.
pub const BARRIER_YOUNG_PAGES_OFFSET: u32 = 0;

/// Where elise's barrier state has its marking flag, a byte: after the
/// pointer, `ptr_size` bytes.
pub const fn barrier_marking_offset(ptr_size: u8) -> u32 {
    ptr_size as u32
}

/// A GC heap offset shifted right by it is its page's index among the young
/// pages' flags, a byte each (`elise::raw::PAGE_SHIFT`).
pub const PAGE_SHIFT: u32 = 18;

/// The layout of Wasm GC objects in elise.
#[derive(Default)]
pub struct EliseTypeLayouts;

impl GcTypeLayouts for EliseTypeLayouts {
    fn array_length_field_offset(&self) -> u32 {
        ARRAY_LENGTH_OFFSET
    }

    fn exception_tag_instance_offset(&self) -> u32 {
        EXCEPTION_TAG_INSTANCE_OFFSET
    }

    fn exception_tag_defined_offset(&self) -> u32 {
        EXCEPTION_TAG_DEFINED_OFFSET
    }

    // A `v128` field makes the common layouts' align 16, but elise's objects'
    // data is 8-aligned: the align is what elise gives, the field's offset
    // stays where the common layout put it. Compiled code's and the host's
    // field accesses don't need more (no `aligned` flag; bytes copied).

    fn array_layout(&self, ty: &WasmArrayType) -> GcArrayLayout {
        let mut layout = common_array_layout(ty, HEADER_SIZE, HEADER_ALIGN, ARRAY_LENGTH_OFFSET);
        layout.align = HEADER_ALIGN;
        layout
    }

    fn struct_layout(&self, ty: &WasmStructType) -> Result<GcStructLayout, OutOfMemory> {
        let mut layout = common_struct_layout(ty, HEADER_SIZE, HEADER_ALIGN)?;
        layout.align = HEADER_ALIGN;
        Ok(layout)
    }

    fn exn_layout(&self, ty: &WasmExnType) -> Result<GcStructLayout, OutOfMemory> {
        let mut layout = common_exn_layout(ty, HEADER_SIZE, HEADER_ALIGN)?;
        layout.align = HEADER_ALIGN;
        Ok(layout)
    }
}
