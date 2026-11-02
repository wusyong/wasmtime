//! kun: compiler for elise, kun's GC, and its barriers.
//!
//! Allocation goes through the `gc_alloc_raw` libcall, as the DRC
//! collector's: elise's nursery isn't bumped inline. Writes of a GC
//! object's reference fields take elise's barriers, inline checks of its
//! barrier state (what `VMContext::gc_heap_data` points at), and a libcall
//! when they must:
//!
//! - while a marking runs, the field's old value goes to `elise_satb`
//!   (snapshot at the beginning);
//! - an old object's field written a young object goes to `elise_remember`,
//!   for the next scavenge to find (generational).
//!
//! A field initialized in an object just allocated takes the generational
//! barrier only: it held nothing. A root's (a table's, a global's) takes
//! neither: elise's GCs are given the roots. Reads take no barrier; the
//! references are in stack maps, elise moving objects.

use super::*;
use crate::func_environ::FuncEnvironment;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{self, InstBuilder};
use cranelift_frontend::FunctionBuilder;
use smallvec::SmallVec;
use wasmtime_environ::elise::{
    BARRIER_YOUNG_PAGES_OFFSET, EXCEPTION_TAG_DEFINED_OFFSET, EXCEPTION_TAG_INSTANCE_OFFSET,
    EliseTypeLayouts, PAGE_SHIFT, barrier_marking_offset,
};
use wasmtime_environ::{
    GcTypeLayouts, I31_DISCRIMINANT, TypeIndex, VMGcKind, WasmHeapType, WasmRefType, WasmResult,
    WasmStorageType, WasmValType,
};

#[derive(Default)]
pub struct EliseCompiler {
    layouts: EliseTypeLayouts,
}

impl EliseCompiler {
    /// Load the pointer to elise's barrier state from vmctx.
    fn load_barrier_state(
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
    ) -> ir::Value {
        let vmctx = func_env.vmctx_val(&mut builder.cursor());
        func_env
            .alias_regions
            .vmctx()
            .gc_heap_data()
            .load(&mut builder.cursor(), vmctx)
    }

    /// Whether the young pages' flag of `offset`'s page (an `i32` offset in
    /// the GC heap) is set, as an `i32` (0 or 1).
    fn is_young(
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
        young_pages: ir::Value,
        offset: ir::Value,
    ) -> ir::Value {
        let page = builder.ins().ushr_imm_u(offset, i64::from(PAGE_SHIFT));
        let page = uextend_i32_to_pointer_type(builder, func_env.pointer_type(), page);
        let flag = builder.ins().iadd(young_pages, page);
        let region = func_env
            .alias_regions
            .elise_young_pages_region(builder.func);
        let flags = ir::MemFlagsData::trusted().with_alias_region(Some(region));
        // Any `u32`'s page is among the flags: no bounds check.
        builder.ins().uload8(ir::types::I32, flags, flag, 0)
    }

    /// The generational barrier for `new_val`, just written at `dst`, a GC
    /// object's field: if `new_val` is a young object and `dst` isn't on a
    /// young page, call `elise_remember`.
    ///
    /// ```ignore
    /// current_block:
    ///     young_pages = load state.young_pages
    ///     is_null_or_i31 = (new_val == 0) | (new_val & I31_DISCRIMINANT)
    ///     brif is_null_or_i31, continue_block, check_value_block
    ///
    /// check_value_block:
    ///     brif young_pages[new_val >> PAGE_SHIFT], check_slot_block, continue_block
    ///
    /// check_slot_block:
    ///     slot = ireduce.i32 (dst - gc_heap_base)
    ///     brif young_pages[slot >> PAGE_SHIFT], continue_block, remember_block
    ///
    /// remember_block:
    ///     call elise_remember(vmctx, slot, new_val)
    ///     jump continue_block
    ///
    /// continue_block:
    ///     ...
    /// ```
    fn emit_generational_barrier(
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
        state: ir::Value,
        dst: ir::Value,
        new_val: ir::Value,
    ) -> WasmResult<()> {
        let check_value_block = builder.create_block();
        let check_slot_block = builder.create_block();
        let remember_block = builder.create_block();
        let continue_block = builder.create_block();

        builder.set_cold_block(remember_block);

        let pointer_type = func_env.pointer_type();
        let region = func_env
            .alias_regions
            .elise_barrier_state_region(builder.func);
        let young_pages = builder.ins().load(
            pointer_type,
            ir::MemFlagsData::trusted().with_alias_region(Some(region)),
            state,
            i32::try_from(BARRIER_YOUNG_PAGES_OFFSET).unwrap(),
        );
        let is_null = builder.ins().icmp_imm_u(IntCC::Equal, new_val, 0);
        let is_i31 = builder
            .ins()
            .band_imm_u(new_val, i64::from(I31_DISCRIMINANT));
        let is_null = builder.ins().uextend(ir::types::I32, is_null);
        let is_null_or_i31 = builder.ins().bor(is_null, is_i31);
        builder
            .ins()
            .brif(is_null_or_i31, continue_block, &[], check_value_block, &[]);

        builder.switch_to_block(check_value_block);
        builder.seal_block(check_value_block);
        let value_is_young = Self::is_young(func_env, builder, young_pages, new_val);
        builder
            .ins()
            .brif(value_is_young, check_slot_block, &[], continue_block, &[]);

        builder.switch_to_block(check_slot_block);
        builder.seal_block(check_slot_block);
        let base = func_env.get_gc_heap_base(builder)?;
        let slot = builder.ins().isub(dst, base);
        let slot = if pointer_type == ir::types::I32 {
            slot
        } else {
            builder.ins().ireduce(ir::types::I32, slot)
        };
        let slot_is_young = Self::is_young(func_env, builder, young_pages, slot);
        builder
            .ins()
            .brif(slot_is_young, continue_block, &[], remember_block, &[]);

        builder.switch_to_block(remember_block);
        builder.seal_block(remember_block);
        let remember = func_env.builtin_functions.elise_remember(builder.func);
        let vmctx = func_env.vmctx_val(&mut builder.cursor());
        builder.ins().call(remember, &[vmctx, slot, new_val]);
        builder.ins().jump(continue_block, &[]);

        builder.switch_to_block(continue_block);
        builder.seal_block(continue_block);
        Ok(())
    }
}

impl GcCompiler for EliseCompiler {
    fn layouts(&self) -> &dyn GcTypeLayouts {
        &self.layouts
    }

    fn is_moving_collector(&self) -> bool {
        true
    }

    fn alloc_uninit_array(
        &mut self,
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder<'_>,
        array_type_index: TypeIndex,
        len: ir::Value,
    ) -> WasmResult<ir::Value> {
        let interned_type_index =
            func_env.module.types[array_type_index].unwrap_module_type_index();

        let len_offset = self.layouts.array_length_field_offset();
        let array_layout = func_env.array_layout(interned_type_index)?.clone();
        let align = array_layout.align;

        let size = emit_array_size(func_env, builder, &array_layout, len);
        let array_ref = emit_gc_raw_alloc(
            func_env,
            builder,
            VMGcKind::ArrayRef,
            interned_type_index,
            size,
            align,
            0,
        );

        // Write the array's length. No bounds check: the allocation
        // libcall's result is trusted.
        let base = func_env.get_gc_heap_base(builder)?;
        let extended_array_ref =
            uextend_i32_to_pointer_type(builder, func_env.pointer_type(), array_ref);
        let object_addr = builder.ins().iadd(base, extended_array_ref);
        let len_addr = builder.ins().iadd_imm_s(object_addr, i64::from(len_offset));
        let flags = func_env.gc_memflags(&mut builder.func, GcAccess::ArrayLength);
        builder.ins().store(flags, len, len_addr, 0);
        Ok(array_ref)
    }

    fn alloc_struct(
        &mut self,
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder<'_>,
        struct_type_index: TypeIndex,
        field_vals: &[ir::Value],
    ) -> WasmResult<ir::Value> {
        let interned_type_index =
            func_env.module.types[struct_type_index].unwrap_module_type_index();
        let struct_layout = func_env.struct_or_exn_layout(interned_type_index);

        // Copied out of the layout, which borrows `func_env`.
        let struct_size = struct_layout.size;
        let struct_align = struct_layout.align;
        let field_offsets: SmallVec<[_; 8]> = struct_layout.fields.iter().copied().collect();
        assert_eq!(field_vals.len(), field_offsets.len());

        let struct_size_val = builder.ins().iconst(ir::types::I32, i64::from(struct_size));
        let struct_ref = emit_gc_raw_alloc(
            func_env,
            builder,
            VMGcKind::StructRef,
            interned_type_index,
            struct_size_val,
            struct_align,
            0,
        );

        // No bounds check: the allocation libcall's result is trusted.
        let base = func_env.get_gc_heap_base(builder)?;
        let extended_struct_ref =
            uextend_i32_to_pointer_type(builder, func_env.pointer_type(), struct_ref);
        let raw_ptr_to_struct = builder.ins().iadd(base, extended_struct_ref);
        initialize_struct_fields(
            func_env,
            builder,
            interned_type_index,
            raw_ptr_to_struct,
            field_vals,
        )?;

        Ok(struct_ref)
    }

    fn alloc_exn(
        &mut self,
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder<'_>,
        tag_index: TagIndex,
        field_vals: &[ir::Value],
        instance_id: ir::Value,
        tag: ir::Value,
    ) -> WasmResult<ir::Value> {
        let interned_type_index = func_env.module.tags[tag_index]
            .exception
            .unwrap_module_type_index();
        let exn_layout = func_env.struct_or_exn_layout(interned_type_index);

        // Copied out of the layout, which borrows `func_env`.
        let exn_size = exn_layout.size;
        let exn_align = exn_layout.align;
        let field_offsets: SmallVec<[_; 8]> = exn_layout.fields.iter().copied().collect();
        assert_eq!(field_vals.len(), field_offsets.len());

        let exn_size_val = builder.ins().iconst(ir::types::I32, i64::from(exn_size));
        let exn_ref = emit_gc_raw_alloc(
            func_env,
            builder,
            VMGcKind::ExnRef,
            interned_type_index,
            exn_size_val,
            exn_align,
            0,
        );

        // No bounds check: the allocation libcall's result is trusted.
        let base = func_env.get_gc_heap_base(builder)?;
        let extended_exn_ref =
            uextend_i32_to_pointer_type(builder, func_env.pointer_type(), exn_ref);
        let raw_ptr_to_exn = builder.ins().iadd(base, extended_exn_ref);
        initialize_struct_fields(
            func_env,
            builder,
            interned_type_index,
            raw_ptr_to_exn,
            field_vals,
        )?;

        let instance_id_addr = builder
            .ins()
            .iadd_imm_s(raw_ptr_to_exn, i64::from(EXCEPTION_TAG_INSTANCE_OFFSET));
        self.init_field(
            func_env,
            builder,
            WasmStorageType::Val(WasmValType::I32),
            instance_id_addr,
            GcAccess::ExnTagInstance,
            instance_id,
        )?;
        let tag_addr = builder
            .ins()
            .iadd_imm_s(raw_ptr_to_exn, i64::from(EXCEPTION_TAG_DEFINED_OFFSET));
        self.init_field(
            func_env,
            builder,
            WasmStorageType::Val(WasmValType::I32),
            tag_addr,
            GcAccess::ExnTagDefined,
            tag,
        )?;

        Ok(exn_ref)
    }

    fn translate_read_gc_reference(
        &mut self,
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
        ty: WasmRefType,
        src: ir::Value,
        flags: ir::MemFlagsData,
    ) -> WasmResult<ir::Value> {
        assert!(ty.is_vmgcref_type());

        let (reference_type, _) = func_env.reference_type(ty.heap_type);

        // References to uninhabited bottom types are null.
        if let WasmHeapType::None = ty.heap_type {
            let null = builder.ins().iconst(reference_type, 0);
            if flags.trap_code().is_some() {
                let _ = builder.ins().load(reference_type, flags, src, 0);
            }
            if !ty.nullable {
                let zero = builder.ins().iconst(ir::types::I32, 0);
                builder.ins().trapz(zero, TRAP_INTERNAL_ASSERT);
            }
            return Ok(null);
        };

        // No read barrier; stack maps, for elise to find and update the
        // references it moves (`i31`s need none).
        unbarriered_load_gc_ref(builder, ty.heap_type, src, flags)
    }

    fn translate_write_gc_reference(
        &mut self,
        _func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
        ty: WasmRefType,
        dst: ir::Value,
        new_val: ir::Value,
        flags: ir::MemFlagsData,
    ) -> WasmResult<()> {
        // A root's: elise's GCs are given the roots, no barrier.
        unbarriered_store_gc_ref(builder, ty.heap_type, dst, new_val, flags)
    }

    /// Both barriers:
    ///
    /// ```ignore
    /// current_block:
    ///     state = load vmctx.gc_heap_data
    ///     brif state.marking, satb_block, store_block
    ///
    /// satb_block:
    ///     old_val = load dst
    ///     call elise_satb(vmctx, old_val)
    ///     jump store_block
    ///
    /// store_block:
    ///     store dst, new_val
    ///     ;; the generational barrier, `emit_generational_barrier`
    /// ```
    fn translate_write_gc_field(
        &mut self,
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
        ty: WasmRefType,
        dst: ir::Value,
        new_val: ir::Value,
        flags: ir::MemFlagsData,
    ) -> WasmResult<()> {
        assert!(ty.is_vmgcref_type());

        // An `i31` field only ever holds `i31`s (and null): no object, old or
        // new.
        if let WasmHeapType::I31 = ty.heap_type {
            return unbarriered_store_gc_ref(builder, ty.heap_type, dst, new_val, flags);
        }

        let satb_block = builder.create_block();
        let store_block = builder.create_block();
        builder.set_cold_block(satb_block);

        let state = Self::load_barrier_state(func_env, builder);
        let marking_offset =
            barrier_marking_offset(u8::try_from(func_env.pointer_type().bytes()).unwrap());
        let region = func_env
            .alias_regions
            .elise_barrier_state_region(builder.func);
        let marking = builder.ins().uload8(
            ir::types::I32,
            ir::MemFlagsData::trusted().with_alias_region(Some(region)),
            state,
            i32::try_from(marking_offset).unwrap(),
        );
        builder
            .ins()
            .brif(marking, satb_block, &[], store_block, &[]);

        builder.switch_to_block(satb_block);
        builder.seal_block(satb_block);
        // Not in any stack map: not live after the call, which doesn't GC.
        let old_val = builder.ins().load(ir::types::I32, flags, dst, 0);
        let satb = func_env.builtin_functions.elise_satb(builder.func);
        let vmctx = func_env.vmctx_val(&mut builder.cursor());
        builder.ins().call(satb, &[vmctx, old_val]);
        builder.ins().jump(store_block, &[]);

        builder.switch_to_block(store_block);
        builder.seal_block(store_block);
        unbarriered_store_gc_ref(builder, ty.heap_type, dst, new_val, flags)?;
        Self::emit_generational_barrier(func_env, builder, state, dst, new_val)
    }

    fn translate_init_gc_reference(
        &mut self,
        _func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder,
        ty: WasmRefType,
        dst: ir::Value,
        new_val: ir::Value,
        flags: ir::MemFlagsData,
    ) -> WasmResult<()> {
        // A root's (a table's, a global's, an element segment's), as
        // `translate_write_gc_reference`.
        unbarriered_store_gc_ref(builder, ty.heap_type, dst, new_val, flags)
    }

    fn init_field(
        &mut self,
        func_env: &mut FuncEnvironment<'_>,
        builder: &mut FunctionBuilder<'_>,
        ty: WasmStorageType,
        field_addr: ir::Value,
        access: GcAccess,
        val: ir::Value,
    ) -> WasmResult<()> {
        // Data inside GC objects is always little endian.
        let flags = func_env
            .gc_memflags(&mut builder.func, access)
            .with_endianness(ir::Endianness::Little);

        match ty {
            WasmStorageType::Val(WasmValType::Ref(r)) => match r.heap_type.top() {
                WasmHeapTopType::Func => {
                    write_func_ref_at_addr(func_env, builder, r, flags, field_addr, val)?
                }
                WasmHeapTopType::Extern | WasmHeapTopType::Any | WasmHeapTopType::Exn => {
                    unbarriered_store_gc_ref(builder, r.heap_type, field_addr, val, flags)?;
                    // The field held nothing: no SATB. The object may be old
                    // (a large array is), so the generational barrier.
                    if r.heap_type != WasmHeapType::I31 {
                        let state = Self::load_barrier_state(func_env, builder);
                        Self::emit_generational_barrier(func_env, builder, state, field_addr, val)?;
                    }
                }
                WasmHeapTopType::Cont => {
                    write_field_at_addr(func_env, builder, ty, field_addr, access, val)?
                }
            },
            WasmStorageType::I8 => {
                assert_eq!(builder.func.dfg.value_type(val), ir::types::I32);
                builder.ins().istore8(flags, val, field_addr, 0);
            }
            WasmStorageType::I16 => {
                assert_eq!(builder.func.dfg.value_type(val), ir::types::I32);
                builder.ins().istore16(flags, val, field_addr, 0);
            }
            WasmStorageType::Val(_) => {
                let size_of_access = wasmtime_environ::byte_size_of_wasm_ty_in_gc_heap(&ty);
                assert_eq!(builder.func.dfg.value_type(val).bytes(), size_of_access);
                builder.ins().store(flags, val, field_addr, 0);
            }
        }

        Ok(())
    }
}
