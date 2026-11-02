//! kun: layout of Wasm GC objects in elise, kun's GC.
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

    fn array_layout(&self, ty: &WasmArrayType) -> GcArrayLayout {
        common_array_layout(ty, HEADER_SIZE, HEADER_ALIGN, ARRAY_LENGTH_OFFSET)
    }

    fn struct_layout(&self, ty: &WasmStructType) -> Result<GcStructLayout, OutOfMemory> {
        common_struct_layout(ty, HEADER_SIZE, HEADER_ALIGN)
    }

    fn exn_layout(&self, ty: &WasmExnType) -> Result<GcStructLayout, OutOfMemory> {
        common_exn_layout(ty, HEADER_SIZE, HEADER_ALIGN)
    }
}
