// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// trans.rs: Translate the completed AST to the LLVM IR.
//
// Some functions here, such as trans_block and trans_expr, return a value --
// the result of the translation to LLVM -- while others, such as trans_fn,
// trans_impl, and trans_item, are called only for the side effect of adding a
// particular definition to the LLVM IR output we're producing.
//
// Hopefully useful general knowledge about trans:
//
//   * There's no way to find out the ty::t type of a ValueRef.  Doing so
//     would be "trying to get the eggs out of an omelette" (credit:
//     pcwalton).  You can, instead, find out its TypeRef by calling val_ty,
//     but one TypeRef corresponds to many `ty::t`s; for instance, tup(int, int,
//     int) and rec(x=int, y=int, z=int) will have the same TypeRef.

#![allow(non_camel_case_types)]

use back::link::{mangle_exported_name};
use back::{link, abi};
use driver::session;
use driver::session::{Session, NoDebugInfo, FullDebugInfo};
use driver::driver::OutputFilenames;
use driver::driver::{CrateAnalysis, CrateTranslation};
use lib::llvm::{ModuleRef, ValueRef, BasicBlockRef};
use lib::llvm::{llvm, Vector};
use lib;
use metadata::{csearch, encoder};
use middle::astencode;
use middle::lang_items::{LangItem, ExchangeMallocFnLangItem, StartFnLangItem};
use middle::lang_items::{MallocFnLangItem, ClosureExchangeMallocFnLangItem};
use middle::trans::_match;
use middle::trans::adt;
use middle::trans::build::*;
use middle::trans::builder::{Builder, noname};
use middle::trans::callee;
use middle::trans::cleanup;
use middle::trans::cleanup::CleanupMethods;
use middle::trans::common::*;
use middle::trans::consts;
use middle::trans::controlflow;
use middle::trans::datum;
// use middle::trans::datum::{Datum, Lvalue, Rvalue, ByRef, ByValue};
use middle::trans::debuginfo;
use middle::trans::expr;
use middle::trans::foreign;
use middle::trans::glue;
use middle::trans::inline;
use middle::trans::machine;
use middle::trans::machine::{llalign_of_min, llsize_of};
use middle::trans::meth;
use middle::trans::monomorphize;
use middle::trans::tvec;
use middle::trans::type_::Type;
use middle::trans::type_of;
use middle::trans::type_of::*;
use middle::trans::value::Value;
use middle::ty;
use middle::typeck;
use util::common::indenter;
use util::ppaux::{Repr, ty_to_str};
use util::sha2::Sha256;
use util::nodemap::NodeMap;

use arena::TypedArena;
use std::c_str::ToCStr;
use std::cell::{Cell, RefCell};
use std::libc::c_uint;
use std::local_data;
use syntax::abi::{X86, X86_64, Arm, Mips, Rust, RustIntrinsic};
use syntax::ast_util::{local_def, is_local};
use syntax::attr::AttrMetaMethods;
use syntax::attr;
use syntax::codemap::Span;
use syntax::parse::token::InternedString;
use syntax::visit::Visitor;
use syntax::visit;
use syntax::{ast, ast_util, ast_map};

use time;

local_data_key!(task_local_insn_key: Vec<&'static str> )

pub fn with_insn_ctxt(blk: |&[&'static str]|) {
    local_data::get(task_local_insn_key, |c| {
        match c {
            Some(ctx) => blk(ctx.as_slice()),
            None => ()
        }
    })
}

pub fn init_insn_ctxt() {
    local_data::set(task_local_insn_key, Vec::new());
}

pub struct _InsnCtxt { _x: () }

#[unsafe_destructor]
impl Drop for _InsnCtxt {
    fn drop(&mut self) {
        local_data::modify(task_local_insn_key, |c| {
            c.map(|mut ctx| {
                ctx.pop();
                ctx
            })
        })
    }
}

pub fn push_ctxt(s: &'static str) -> _InsnCtxt {
    debug!("new InsnCtxt: {}", s);
    local_data::modify(task_local_insn_key, |c| {
        c.map(|mut ctx| {
            ctx.push(s);
            ctx
        })
    });
    _InsnCtxt { _x: () }
}

pub struct StatRecorder<'a> {
    ccx: &'a CrateContext,
    name: Option<~str>,
    start: u64,
    istart: uint,
}

impl<'a> StatRecorder<'a> {
    pub fn new(ccx: &'a CrateContext, name: ~str) -> StatRecorder<'a> {
        let start = if ccx.sess().trans_stats() {
            time::precise_time_ns()
        } else {
            0
        };
        let istart = ccx.stats.n_llvm_insns.get();
        StatRecorder {
            ccx: ccx,
            name: Some(name),
            start: start,
            istart: istart,
        }
    }
}

#[unsafe_destructor]
impl<'a> Drop for StatRecorder<'a> {
    fn drop(&mut self) {
        if self.ccx.sess().trans_stats() {
            let end = time::precise_time_ns();
            let elapsed = ((end - self.start) / 1_000_000) as uint;
            let iend = self.ccx.stats.n_llvm_insns.get();
            self.ccx.stats.fn_stats.borrow_mut().push((self.name.take_unwrap(),
                                                       elapsed,
                                                       iend - self.istart));
            self.ccx.stats.n_fns.set(self.ccx.stats.n_fns.get() + 1);
            // Reset LLVM insn count to avoid compound costs.
            self.ccx.stats.n_llvm_insns.set(self.istart);
        }
    }
}

// only use this for foreign function ABIs and glue, use `decl_rust_fn` for Rust functions
fn decl_fn(llmod: ModuleRef, name: &str, cc: lib::llvm::CallConv,
           ty: Type, output: ty::t) -> ValueRef {
    let llfn: ValueRef = name.with_c_str(|buf| {
        unsafe {
            llvm::LLVMGetOrInsertFunction(llmod, buf, ty.to_ref())
        }
    });

    match ty::get(output).sty {
        // functions returning bottom may unwind, but can never return normally
        ty::ty_bot => {
            unsafe {
                llvm::LLVMAddFunctionAttr(llfn, lib::llvm::NoReturnAttribute as c_uint)
            }
        }
        // `~` pointer return values never alias because ownership is transferred
        // FIXME #6750 ~Trait cannot be directly marked as
        // noalias because the actual object pointer is nested.
        ty::ty_uniq(..) | // ty::ty_trait(_, _, ty::UniqTraitStore, _, _) |
        ty::ty_vec(_, ty::vstore_uniq) | ty::ty_str(ty::vstore_uniq) => {
            unsafe {
                llvm::LLVMAddReturnAttribute(llfn, lib::llvm::NoAliasAttribute as c_uint);
            }
        }
        _ => {}
    }

    lib::llvm::SetFunctionCallConv(llfn, cc);
    // Function addresses in Rust are never significant, allowing functions to be merged.
    lib::llvm::SetUnnamedAddr(llfn, true);

    llfn
}

// only use this for foreign function ABIs and glue, use `decl_rust_fn` for Rust functions
pub fn decl_cdecl_fn(llmod: ModuleRef,
                     name: &str,
                     ty: Type,
                     output: ty::t) -> ValueRef {
    decl_fn(llmod, name, lib::llvm::CCallConv, ty, output)
}

// only use this for foreign function ABIs and glue, use `get_extern_rust_fn` for Rust functions
pub fn get_extern_fn(externs: &mut ExternMap, llmod: ModuleRef,
                     name: &str, cc: lib::llvm::CallConv,
                     ty: Type, output: ty::t) -> ValueRef {
    match externs.find_equiv(&name) {
        Some(n) => return *n,
        None => {}
    }
    let f = decl_fn(llmod, name, cc, ty, output);
    externs.insert(name.to_owned(), f);
    f
}

fn get_extern_rust_fn(ccx: &CrateContext, inputs: &[ty::t], output: ty::t,
                      name: &str, did: ast::DefId) -> ValueRef {
    match ccx.externs.borrow().find_equiv(&name) {
        Some(n) => return *n,
        None => ()
    }

    let f = decl_rust_fn(ccx, false, inputs, output, name);
    csearch::get_item_attrs(&ccx.sess().cstore, did, |meta_items| {
        set_llvm_fn_attrs(meta_items.iter().map(|&x| attr::mk_attr(x)).collect::<~[_]>(), f)
    });

    ccx.externs.borrow_mut().insert(name.to_owned(), f);
    f
}

pub fn decl_rust_fn(ccx: &CrateContext, has_env: bool,
                    inputs: &[ty::t], output: ty::t,
                    name: &str) -> ValueRef {
    use middle::ty::{BrAnon, ReLateBound};

    let llfty = type_of_rust_fn(ccx, has_env, inputs, output);
    let llfn = decl_cdecl_fn(ccx.llmod, name, llfty, output);

    let uses_outptr = type_of::return_uses_outptr(ccx, output);
    let offset = if uses_outptr { 1 } else { 0 };
    let offset = if has_env { offset + 1 } else { offset };

    for (i, &arg_ty) in inputs.iter().enumerate() {
        let llarg = unsafe { llvm::LLVMGetParam(llfn, (offset + i) as c_uint) };
        match ty::get(arg_ty).sty {
            // `~` pointer parameters never alias because ownership is transferred
            // FIXME #6750 ~Trait cannot be directly marked as
            // noalias because the actual object pointer is nested.
            ty::ty_uniq(..) | // ty::ty_trait(_, _, ty::UniqTraitStore, _, _) |
            ty::ty_vec(_, ty::vstore_uniq) | ty::ty_str(ty::vstore_uniq) |
            ty::ty_closure(~ty::ClosureTy {sigil: ast::OwnedSigil, ..}) => {
                unsafe {
                    llvm::LLVMAddAttribute(llarg, lib::llvm::NoAliasAttribute as c_uint);
                }
            },
            // When a reference in an argument has no named lifetime, it's
            // impossible for that reference to escape this function(ie, be
            // returned).
            ty::ty_rptr(ReLateBound(_, BrAnon(_)), _) => {
                debug!("marking argument of {} as nocapture because of anonymous lifetime", name);
                unsafe {
                    llvm::LLVMAddAttribute(llarg, lib::llvm::NoCaptureAttribute as c_uint);
                }
            },
            _ => {
                // For non-immediate arguments the callee gets its own copy of
                // the value on the stack, so there are no aliases
                if !type_is_immediate(ccx, arg_ty) {
                    unsafe {
                        llvm::LLVMAddAttribute(llarg, lib::llvm::NoAliasAttribute as c_uint);
                        llvm::LLVMAddAttribute(llarg, lib::llvm::NoCaptureAttribute as c_uint);
                    }
                }
            }
        }
    }

    // The out pointer will never alias with any other pointers, as the object only exists at a
    // language level after the call. It can also be tagged with SRet to indicate that it is
    // guaranteed to point to a usable block of memory for the type.
    if uses_outptr {
        unsafe {
            let outptr = llvm::LLVMGetParam(llfn, 0);
            llvm::LLVMAddAttribute(outptr, lib::llvm::StructRetAttribute as c_uint);
            llvm::LLVMAddAttribute(outptr, lib::llvm::NoAliasAttribute as c_uint);
        }
    }

    llfn
}

pub fn decl_internal_rust_fn(ccx: &CrateContext, has_env: bool,
                             inputs: &[ty::t], output: ty::t,
                             name: &str) -> ValueRef {
    let llfn = decl_rust_fn(ccx, has_env, inputs, output, name);
    lib::llvm::SetLinkage(llfn, lib::llvm::InternalLinkage);
    llfn
}

pub fn get_extern_const(externs: &mut ExternMap, llmod: ModuleRef,
                        name: &str, ty: Type) -> ValueRef {
    match externs.find_equiv(&name) {
        Some(n) => return *n,
        None => ()
    }
    unsafe {
        let c = name.with_c_str(|buf| {
            llvm::LLVMAddGlobal(llmod, ty.to_ref(), buf)
        });
        externs.insert(name.to_owned(), c);
        return c;
    }
}

// Returns a pointer to the body for the box. The box may be an opaque
// box. The result will be casted to the type of body_t, if it is statically
// known.
pub fn at_box_body(bcx: &Block, body_t: ty::t, boxptr: ValueRef) -> ValueRef {
    let _icx = push_ctxt("at_box_body");
    let ccx = bcx.ccx();
    let ty = Type::at_box(ccx, type_of(ccx, body_t));
    let boxptr = PointerCast(bcx, boxptr, ty.ptr_to());
    GEPi(bcx, boxptr, [0u, abi::box_field_body])
}

// malloc_raw_dyn: allocates a box to contain a given type, but with a
// potentially dynamic size.
pub fn malloc_raw_dyn<'a>(
                      bcx: &'a Block<'a>,
                      t: ty::t,
                      heap: heap,
                      size: ValueRef)
                      -> Result<'a> {
    let _icx = push_ctxt("malloc_raw");
    let ccx = bcx.ccx();

    fn require_alloc_fn(bcx: &Block, t: ty::t, it: LangItem) -> ast::DefId {
        let li = &bcx.tcx().lang_items;
        match li.require(it) {
            Ok(id) => id,
            Err(s) => {
                bcx.sess().fatal(format!("allocation of `{}` {}",
                                         bcx.ty_to_str(t), s));
            }
        }
    }

    if heap == heap_exchange {
        let llty_value = type_of::type_of(ccx, t);

        // Allocate space:
        let r = callee::trans_lang_call(
            bcx,
            require_alloc_fn(bcx, t, ExchangeMallocFnLangItem),
            [size],
            None);
        rslt(r.bcx, PointerCast(r.bcx, r.val, llty_value.ptr_to()))
    } else {
        // we treat ~fn as @ here, which isn't ideal
        let langcall = match heap {
            heap_managed => {
                require_alloc_fn(bcx, t, MallocFnLangItem)
            }
            heap_exchange_closure => {
                require_alloc_fn(bcx, t, ClosureExchangeMallocFnLangItem)
            }
            _ => fail!("heap_exchange already handled")
        };

        // Grab the TypeRef type of box_ptr_ty.
        let box_ptr_ty = ty::mk_box(bcx.tcx(), t);
        let llty = type_of(ccx, box_ptr_ty);
        let llalign = C_uint(ccx, llalign_of_min(ccx, llty) as uint);

        // Allocate space:
        let drop_glue = glue::get_drop_glue(ccx, t);
        let r = callee::trans_lang_call(
            bcx,
            langcall,
            [
                PointerCast(bcx, drop_glue, Type::glue_fn(ccx, Type::i8p(ccx)).ptr_to()),
                size,
                llalign
            ],
            None);
        rslt(r.bcx, PointerCast(r.bcx, r.val, llty))
    }
}

// malloc_raw: expects an unboxed type and returns a pointer to
// enough space for a box of that type.  This includes a rust_opaque_box
// header.
pub fn malloc_raw<'a>(bcx: &'a Block<'a>, t: ty::t, heap: heap)
                  -> Result<'a> {
    let ty = type_of(bcx.ccx(), t);
    let size = llsize_of(bcx.ccx(), ty);
    malloc_raw_dyn(bcx, t, heap, size)
}

pub struct MallocResult<'a> {
    pub bcx: &'a Block<'a>,
    pub smart_ptr: ValueRef,
    pub body: ValueRef
}

// malloc_general_dyn: usefully wraps malloc_raw_dyn; allocates a smart
// pointer, and pulls out the body
pub fn malloc_general_dyn<'a>(
                          bcx: &'a Block<'a>,
                          t: ty::t,
                          heap: heap,
                          size: ValueRef)
                          -> MallocResult<'a> {
    assert!(heap != heap_exchange);
    let _icx = push_ctxt("malloc_general");
    let Result {bcx: bcx, val: llbox} = malloc_raw_dyn(bcx, t, heap, size);
    let body = GEPi(bcx, llbox, [0u, abi::box_field_body]);

    MallocResult {
        bcx: bcx,
        smart_ptr: llbox,
        body: body,
    }
}

pub fn malloc_general<'a>(bcx: &'a Block<'a>, t: ty::t, heap: heap)
                      -> MallocResult<'a> {
    let ty = type_of(bcx.ccx(), t);
    assert!(heap != heap_exchange);
    malloc_general_dyn(bcx, t, heap, llsize_of(bcx.ccx(), ty))
}

// Type descriptor and type glue stuff

pub fn get_tydesc(ccx: &CrateContext, t: ty::t) -> @tydesc_info {
    match ccx.tydescs.borrow().find(&t) {
        Some(&inf) => return inf,
        _ => { }
    }

    ccx.stats.n_static_tydescs.set(ccx.stats.n_static_tydescs.get() + 1u);
    let inf = glue::declare_tydesc(ccx, t);

    ccx.tydescs.borrow_mut().insert(t, inf);
    return inf;
}

#[allow(dead_code)] // useful
pub fn set_optimize_for_size(f: ValueRef) {
    lib::llvm::SetFunctionAttribute(f, lib::llvm::OptimizeForSizeAttribute)
}

pub fn set_no_inline(f: ValueRef) {
    lib::llvm::SetFunctionAttribute(f, lib::llvm::NoInlineAttribute)
}

#[allow(dead_code)] // useful
pub fn set_no_unwind(f: ValueRef) {
    lib::llvm::SetFunctionAttribute(f, lib::llvm::NoUnwindAttribute)
}

// Tell LLVM to emit the information necessary to unwind the stack for the
// function f.
pub fn set_uwtable(f: ValueRef) {
    lib::llvm::SetFunctionAttribute(f, lib::llvm::UWTableAttribute)
}

pub fn set_inline_hint(f: ValueRef) {
    lib::llvm::SetFunctionAttribute(f, lib::llvm::InlineHintAttribute)
}

pub fn set_llvm_fn_attrs(attrs: &[ast::Attribute], llfn: ValueRef) {
    use syntax::attr::*;
    // Set the inline hint if there is one
    match find_inline_attr(attrs) {
        InlineHint   => set_inline_hint(llfn),
        InlineAlways => set_always_inline(llfn),
        InlineNever  => set_no_inline(llfn),
        InlineNone   => { /* fallthrough */ }
    }

    // Add the no-split-stack attribute if requested
    if contains_name(attrs, "no_split_stack") {
        set_no_split_stack(llfn);
    }

    if contains_name(attrs, "cold") {
        unsafe { llvm::LLVMAddColdAttribute(llfn) }
    }
}

pub fn set_always_inline(f: ValueRef) {
    lib::llvm::SetFunctionAttribute(f, lib::llvm::AlwaysInlineAttribute)
}

pub fn set_no_split_stack(f: ValueRef) {
    "no-split-stack".with_c_str(|buf| {
        unsafe { llvm::LLVMAddFunctionAttrString(f, buf); }
    })
}

// Double-check that we never ask LLVM to declare the same symbol twice. It
// silently mangles such symbols, breaking our linkage model.
pub fn note_unique_llvm_symbol(ccx: &CrateContext, sym: ~str) {
    if ccx.all_llvm_symbols.borrow().contains(&sym) {
        ccx.sess().bug(~"duplicate LLVM symbol: " + sym);
    }
    ccx.all_llvm_symbols.borrow_mut().insert(sym);
}


pub fn get_res_dtor(ccx: &CrateContext,
                    did: ast::DefId,
                    parent_id: ast::DefId,
                    substs: &[ty::t])
                 -> ValueRef {
    let _icx = push_ctxt("trans_res_dtor");
    let did = if did.krate != ast::LOCAL_CRATE {
        inline::maybe_instantiate_inline(ccx, did)
    } else {
        did
    };
    if !substs.is_empty() {
        assert_eq!(did.krate, ast::LOCAL_CRATE);
        let tsubsts = ty::substs {
            regions: ty::ErasedRegions,
            self_ty: None,
            tps: Vec::from_slice(substs),
        };

        let vtables = typeck::check::vtable::trans_resolve_method(ccx.tcx(), did.node, &tsubsts);
        let (val, _) = monomorphize::monomorphic_fn(ccx, did, &tsubsts, vtables, None, None);

        val
    } else if did.krate == ast::LOCAL_CRATE {
        get_item_val(ccx, did.node)
    } else {
        let tcx = ccx.tcx();
        let name = csearch::get_symbol(&ccx.sess().cstore, did);
        let class_ty = ty::subst_tps(tcx,
                                     substs,
                                     None,
                                     ty::lookup_item_type(tcx, parent_id).ty);
        let llty = type_of_dtor(ccx, class_ty);

        get_extern_fn(&mut *ccx.externs.borrow_mut(), ccx.llmod, name,
                      lib::llvm::CCallConv, llty, ty::mk_nil())
    }
}

// Structural comparison: a rather involved form of glue.
pub fn maybe_name_value(cx: &CrateContext, v: ValueRef, s: &str) {
    if cx.sess().opts.cg.save_temps {
        s.with_c_str(|buf| {
            unsafe {
                llvm::LLVMSetValueName(v, buf)
            }
        })
    }
}


// Used only for creating scalar comparison glue.
pub enum scalar_type { nil_type, signed_int, unsigned_int, floating_point, }

// NB: This produces an i1, not a Rust bool (i8).
pub fn compare_scalar_types<'a>(
                            cx: &'a Block<'a>,
                            lhs: ValueRef,
                            rhs: ValueRef,
                            t: ty::t,
                            op: ast::BinOp)
                            -> Result<'a> {
    let f = |a| rslt(cx, compare_scalar_values(cx, lhs, rhs, a, op));

    match ty::get(t).sty {
        ty::ty_nil => f(nil_type),
        ty::ty_bool | ty::ty_ptr(_) |
        ty::ty_uint(_) | ty::ty_char => f(unsigned_int),
        ty::ty_int(_) => f(signed_int),
        ty::ty_float(_) => f(floating_point),
            // Should never get here, because t is scalar.
        _ => cx.sess().bug("non-scalar type passed to compare_scalar_types")
    }
}


// A helper function to do the actual comparison of scalar values.
pub fn compare_scalar_values<'a>(
                             cx: &'a Block<'a>,
                             lhs: ValueRef,
                             rhs: ValueRef,
                             nt: scalar_type,
                             op: ast::BinOp)
                             -> ValueRef {
    let _icx = push_ctxt("compare_scalar_values");
    fn die(cx: &Block) -> ! {
        cx.sess().bug("compare_scalar_values: must be a comparison operator");
    }
    match nt {
      nil_type => {
        // We don't need to do actual comparisons for nil.
        // () == () holds but () < () does not.
        match op {
          ast::BiEq | ast::BiLe | ast::BiGe => return C_i1(cx.ccx(), true),
          ast::BiNe | ast::BiLt | ast::BiGt => return C_i1(cx.ccx(), false),
          // refinements would be nice
          _ => die(cx)
        }
      }
      floating_point => {
        let cmp = match op {
          ast::BiEq => lib::llvm::RealOEQ,
          ast::BiNe => lib::llvm::RealUNE,
          ast::BiLt => lib::llvm::RealOLT,
          ast::BiLe => lib::llvm::RealOLE,
          ast::BiGt => lib::llvm::RealOGT,
          ast::BiGe => lib::llvm::RealOGE,
          _ => die(cx)
        };
        return FCmp(cx, cmp, lhs, rhs);
      }
      signed_int => {
        let cmp = match op {
          ast::BiEq => lib::llvm::IntEQ,
          ast::BiNe => lib::llvm::IntNE,
          ast::BiLt => lib::llvm::IntSLT,
          ast::BiLe => lib::llvm::IntSLE,
          ast::BiGt => lib::llvm::IntSGT,
          ast::BiGe => lib::llvm::IntSGE,
          _ => die(cx)
        };
        return ICmp(cx, cmp, lhs, rhs);
      }
      unsigned_int => {
        let cmp = match op {
          ast::BiEq => lib::llvm::IntEQ,
          ast::BiNe => lib::llvm::IntNE,
          ast::BiLt => lib::llvm::IntULT,
          ast::BiLe => lib::llvm::IntULE,
          ast::BiGt => lib::llvm::IntUGT,
          ast::BiGe => lib::llvm::IntUGE,
          _ => die(cx)
        };
        return ICmp(cx, cmp, lhs, rhs);
      }
    }
}

pub type val_and_ty_fn<'r,'b> =
    'r |&'b Block<'b>, ValueRef, ty::t| -> &'b Block<'b>;

// Iterates through the elements of a structural type.
pub fn iter_structural_ty<'r,
                          'b>(
                          cx: &'b Block<'b>,
                          av: ValueRef,
                          t: ty::t,
                          f: val_and_ty_fn<'r,'b>)
                          -> &'b Block<'b> {
    let _icx = push_ctxt("iter_structural_ty");

    fn iter_variant<'r,
                    'b>(
                    cx: &'b Block<'b>,
                    repr: &adt::Repr,
                    av: ValueRef,
                    variant: @ty::VariantInfo,
                    tps: &[ty::t],
                    f: val_and_ty_fn<'r,'b>)
                    -> &'b Block<'b> {
        let _icx = push_ctxt("iter_variant");
        let tcx = cx.tcx();
        let mut cx = cx;

        for (i, &arg) in variant.args.iter().enumerate() {
            cx = f(cx,
                   adt::trans_field_ptr(cx, repr, av, variant.disr_val, i),
                   ty::subst_tps(tcx, tps, None, arg));
        }
        return cx;
    }

    let mut cx = cx;
    match ty::get(t).sty {
      ty::ty_struct(..) => {
          let repr = adt::represent_type(cx.ccx(), t);
          expr::with_field_tys(cx.tcx(), t, None, |discr, field_tys| {
              for (i, field_ty) in field_tys.iter().enumerate() {
                  let llfld_a = adt::trans_field_ptr(cx, repr, av, discr, i);
                  cx = f(cx, llfld_a, field_ty.mt.ty);
              }
          })
      }
      ty::ty_str(ty::vstore_fixed(_)) |
      ty::ty_vec(_, ty::vstore_fixed(_)) => {
        let (base, len) = tvec::get_base_and_byte_len(cx, av, t);
        cx = tvec::iter_vec_raw(cx, base, t, len, f);
      }
      ty::ty_tup(ref args) => {
          let repr = adt::represent_type(cx.ccx(), t);
          for (i, arg) in args.iter().enumerate() {
              let llfld_a = adt::trans_field_ptr(cx, repr, av, 0, i);
              cx = f(cx, llfld_a, *arg);
          }
      }
      ty::ty_enum(tid, ref substs) => {
          let fcx = cx.fcx;
          let ccx = fcx.ccx;

          let repr = adt::represent_type(ccx, t);
          let variants = ty::enum_variants(ccx.tcx(), tid);
          let n_variants = (*variants).len();

          // NB: we must hit the discriminant first so that structural
          // comparison know not to proceed when the discriminants differ.

          match adt::trans_switch(cx, repr, av) {
              (_match::single, None) => {
                  cx = iter_variant(cx, repr, av, *variants.get(0),
                                    substs.tps.as_slice(), f);
              }
              (_match::switch, Some(lldiscrim_a)) => {
                  cx = f(cx, lldiscrim_a, ty::mk_int());
                  let unr_cx = fcx.new_temp_block("enum-iter-unr");
                  Unreachable(unr_cx);
                  let llswitch = Switch(cx, lldiscrim_a, unr_cx.llbb,
                                        n_variants);
                  let next_cx = fcx.new_temp_block("enum-iter-next");

                  for variant in (*variants).iter() {
                      let variant_cx =
                          fcx.new_temp_block(~"enum-iter-variant-" +
                                             variant.disr_val.to_str());
                      match adt::trans_case(cx, repr, variant.disr_val) {
                          _match::single_result(r) => {
                              AddCase(llswitch, r.val, variant_cx.llbb)
                          }
                          _ => ccx.sess().unimpl("value from adt::trans_case \
                                                  in iter_structural_ty")
                      }
                      let variant_cx =
                          iter_variant(variant_cx,
                                       repr,
                                       av,
                                       *variant,
                                       substs.tps.as_slice(),
                                       |x,y,z| f(x,y,z));
                      Br(variant_cx, next_cx.llbb);
                  }
                  cx = next_cx;
              }
              _ => ccx.sess().unimpl("value from adt::trans_switch \
                                      in iter_structural_ty")
          }
      }
      _ => cx.sess().unimpl("type in iter_structural_ty")
    }
    return cx;
}

pub fn cast_shift_expr_rhs<'a>(
                           cx: &'a Block<'a>,
                           op: ast::BinOp,
                           lhs: ValueRef,
                           rhs: ValueRef)
                           -> ValueRef {
    cast_shift_rhs(op, lhs, rhs,
                   |a,b| Trunc(cx, a, b),
                   |a,b| ZExt(cx, a, b))
}

pub fn cast_shift_const_rhs(op: ast::BinOp,
                            lhs: ValueRef, rhs: ValueRef) -> ValueRef {
    cast_shift_rhs(op, lhs, rhs,
                   |a, b| unsafe { llvm::LLVMConstTrunc(a, b.to_ref()) },
                   |a, b| unsafe { llvm::LLVMConstZExt(a, b.to_ref()) })
}

pub fn cast_shift_rhs(op: ast::BinOp,
                      lhs: ValueRef,
                      rhs: ValueRef,
                      trunc: |ValueRef, Type| -> ValueRef,
                      zext: |ValueRef, Type| -> ValueRef)
                      -> ValueRef {
    // Shifts may have any size int on the rhs
    unsafe {
        if ast_util::is_shift_binop(op) {
            let mut rhs_llty = val_ty(rhs);
            let mut lhs_llty = val_ty(lhs);
            if rhs_llty.kind() == Vector { rhs_llty = rhs_llty.element_type() }
            if lhs_llty.kind() == Vector { lhs_llty = lhs_llty.element_type() }
            let rhs_sz = llvm::LLVMGetIntTypeWidth(rhs_llty.to_ref());
            let lhs_sz = llvm::LLVMGetIntTypeWidth(lhs_llty.to_ref());
            if lhs_sz < rhs_sz {
                trunc(rhs, lhs_llty)
            } else if lhs_sz > rhs_sz {
                // FIXME (#1877: If shifting by negative
                // values becomes not undefined then this is wrong.
                zext(rhs, lhs_llty)
            } else {
                rhs
            }
        } else {
            rhs
        }
    }
}

pub fn fail_if_zero<'a>(
                    cx: &'a Block<'a>,
                    span: Span,
                    divrem: ast::BinOp,
                    rhs: ValueRef,
                    rhs_t: ty::t)
                    -> &'a Block<'a> {
    let text = if divrem == ast::BiDiv {
        "attempted to divide by zero"
    } else {
        "attempted remainder with a divisor of zero"
    };
    let is_zero = match ty::get(rhs_t).sty {
      ty::ty_int(t) => {
        let zero = C_integral(Type::int_from_ty(cx.ccx(), t), 0u64, false);
        ICmp(cx, lib::llvm::IntEQ, rhs, zero)
      }
      ty::ty_uint(t) => {
        let zero = C_integral(Type::uint_from_ty(cx.ccx(), t), 0u64, false);
        ICmp(cx, lib::llvm::IntEQ, rhs, zero)
      }
      _ => {
        cx.sess().bug(~"fail-if-zero on unexpected type: " +
                      ty_to_str(cx.tcx(), rhs_t));
      }
    };
    with_cond(cx, is_zero, |bcx| {
        controlflow::trans_fail(bcx, span, InternedString::new(text))
    })
}

pub fn trans_external_path(ccx: &CrateContext, did: ast::DefId, t: ty::t) -> ValueRef {
    let name = csearch::get_symbol(&ccx.sess().cstore, did);
    match ty::get(t).sty {
        ty::ty_bare_fn(ref fn_ty) => {
            match fn_ty.abis.for_target(ccx.sess().targ_cfg.os,
                                        ccx.sess().targ_cfg.arch) {
                Some(Rust) | Some(RustIntrinsic) => {
                    get_extern_rust_fn(ccx,
                                       fn_ty.sig.inputs.as_slice(),
                                       fn_ty.sig.output,
                                       name,
                                       did)
                }
                Some(..) | None => {
                    let c = foreign::llvm_calling_convention(ccx, fn_ty.abis);
                    let cconv = c.unwrap_or(lib::llvm::CCallConv);
                    let llty = type_of_fn_from_ty(ccx, t);
                    get_extern_fn(&mut *ccx.externs.borrow_mut(), ccx.llmod,
                                  name, cconv, llty, fn_ty.sig.output)
                }
            }
        }
        ty::ty_closure(ref f) => {
            get_extern_rust_fn(ccx,
                               f.sig.inputs.as_slice(),
                               f.sig.output,
                               name,
                               did)
        }
        _ => {
            let llty = type_of(ccx, t);
            get_extern_const(&mut *ccx.externs.borrow_mut(), ccx.llmod, name,
                             llty)
        }
    }
}

pub fn invoke<'a>(
              bcx: &'a Block<'a>,
              llfn: ValueRef,
              llargs: Vec<ValueRef> ,
              attributes: &[(uint, lib::llvm::Attribute)],
              call_info: Option<NodeInfo>)
              -> (ValueRef, &'a Block<'a>) {
    let _icx = push_ctxt("invoke_");
    if bcx.unreachable.get() {
        return (C_null(Type::i8(bcx.ccx())), bcx);
    }

    match bcx.opt_node_id {
        None => {
            debug!("invoke at ???");
        }
        Some(id) => {
            debug!("invoke at {}", bcx.tcx().map.node_to_str(id));
        }
    }

    if need_invoke(bcx) {
        debug!("invoking {} at {}", llfn, bcx.llbb);
        for &llarg in llargs.iter() {
            debug!("arg: {}", llarg);
        }
        let normal_bcx = bcx.fcx.new_temp_block("normal-return");
        let landing_pad = bcx.fcx.get_landing_pad();

        match call_info {
            Some(info) => debuginfo::set_source_location(bcx.fcx, info.id, info.span),
            None => debuginfo::clear_source_location(bcx.fcx)
        };

        let llresult = Invoke(bcx,
                              llfn,
                              llargs.as_slice(),
                              normal_bcx.llbb,
                              landing_pad,
                              attributes);
        return (llresult, normal_bcx);
    } else {
        debug!("calling {} at {}", llfn, bcx.llbb);
        for &llarg in llargs.iter() {
            debug!("arg: {}", llarg);
        }

        match call_info {
            Some(info) => debuginfo::set_source_location(bcx.fcx, info.id, info.span),
            None => debuginfo::clear_source_location(bcx.fcx)
        };

        let llresult = Call(bcx, llfn, llargs.as_slice(), attributes);
        return (llresult, bcx);
    }
}

pub fn need_invoke(bcx: &Block) -> bool {
    if bcx.sess().no_landing_pads() {
        return false;
    }

    // Avoid using invoke if we are already inside a landing pad.
    if bcx.is_lpad {
        return false;
    }

    bcx.fcx.needs_invoke()
}

pub fn load_if_immediate(cx: &Block, v: ValueRef, t: ty::t) -> ValueRef {
    let _icx = push_ctxt("load_if_immediate");
    if type_is_immediate(cx.ccx(), t) { return Load(cx, v); }
    return v;
}

pub fn ignore_lhs(_bcx: &Block, local: &ast::Local) -> bool {
    match local.pat.node {
        ast::PatWild => true, _ => false
    }
}

pub fn init_local<'a>(bcx: &'a Block<'a>, local: &ast::Local)
                  -> &'a Block<'a> {

    debug!("init_local(bcx={}, local.id={:?})",
           bcx.to_str(), local.id);
    let _indenter = indenter();

    let _icx = push_ctxt("init_local");

    if ignore_lhs(bcx, local) {
        // Handle let _ = e; just like e;
        match local.init {
            Some(init) => {
              return expr::trans_into(bcx, init, expr::Ignore);
            }
            None => { return bcx; }
        }
    }

    _match::store_local(bcx, local)
}

pub fn raw_block<'a>(
                 fcx: &'a FunctionContext<'a>,
                 is_lpad: bool,
                 llbb: BasicBlockRef)
                 -> &'a Block<'a> {
    Block::new(llbb, is_lpad, None, fcx)
}

pub fn with_cond<'a>(
                 bcx: &'a Block<'a>,
                 val: ValueRef,
                 f: |&'a Block<'a>| -> &'a Block<'a>)
                 -> &'a Block<'a> {
    let _icx = push_ctxt("with_cond");
    let fcx = bcx.fcx;
    let next_cx = fcx.new_temp_block("next");
    let cond_cx = fcx.new_temp_block("cond");
    CondBr(bcx, val, cond_cx.llbb, next_cx.llbb);
    let after_cx = f(cond_cx);
    if !after_cx.terminated.get() {
        Br(after_cx, next_cx.llbb);
    }
    next_cx
}

pub fn call_memcpy(cx: &Block, dst: ValueRef, src: ValueRef, n_bytes: ValueRef, align: u32) {
    let _icx = push_ctxt("call_memcpy");
    let ccx = cx.ccx();
    let key = match ccx.sess().targ_cfg.arch {
        X86 | Arm | Mips => "llvm.memcpy.p0i8.p0i8.i32",
        X86_64 => "llvm.memcpy.p0i8.p0i8.i64"
    };
    let memcpy = ccx.intrinsics.get_copy(&key);
    let src_ptr = PointerCast(cx, src, Type::i8p(ccx));
    let dst_ptr = PointerCast(cx, dst, Type::i8p(ccx));
    let size = IntCast(cx, n_bytes, ccx.int_type);
    let align = C_i32(ccx, align as i32);
    let volatile = C_i1(ccx, false);
    Call(cx, memcpy, [dst_ptr, src_ptr, size, align, volatile], []);
}

pub fn memcpy_ty(bcx: &Block, dst: ValueRef, src: ValueRef, t: ty::t) {
    let _icx = push_ctxt("memcpy_ty");
    let ccx = bcx.ccx();
    if ty::type_is_structural(t) {
        let llty = type_of::type_of(ccx, t);
        let llsz = llsize_of(ccx, llty);
        let llalign = llalign_of_min(ccx, llty);
        call_memcpy(bcx, dst, src, llsz, llalign as u32);
    } else {
        Store(bcx, Load(bcx, src), dst);
    }
}

pub fn zero_mem(cx: &Block, llptr: ValueRef, t: ty::t) {
    if cx.unreachable.get() { return; }
    let _icx = push_ctxt("zero_mem");
    let bcx = cx;
    let ccx = cx.ccx();
    let llty = type_of::type_of(ccx, t);
    memzero(&B(bcx), llptr, llty);
}

// Always use this function instead of storing a zero constant to the memory
// in question. If you store a zero constant, LLVM will drown in vreg
// allocation for large data structures, and the generated code will be
// awful. (A telltale sign of this is large quantities of
// `mov [byte ptr foo],0` in the generated code.)
fn memzero(b: &Builder, llptr: ValueRef, ty: Type) {
    let _icx = push_ctxt("memzero");
    let ccx = b.ccx;

    let intrinsic_key = match ccx.sess().targ_cfg.arch {
        X86 | Arm | Mips => "llvm.memset.p0i8.i32",
        X86_64 => "llvm.memset.p0i8.i64"
    };

    let llintrinsicfn = ccx.intrinsics.get_copy(&intrinsic_key);
    let llptr = b.pointercast(llptr, Type::i8(ccx).ptr_to());
    let llzeroval = C_u8(ccx, 0);
    let size = machine::llsize_of(ccx, ty);
    let align = C_i32(ccx, llalign_of_min(ccx, ty) as i32);
    let volatile = C_i1(ccx, false);
    b.call(llintrinsicfn, [llptr, llzeroval, size, align, volatile], []);
}

pub fn alloc_ty(bcx: &Block, t: ty::t, name: &str) -> ValueRef {
    let _icx = push_ctxt("alloc_ty");
    let ccx = bcx.ccx();
    let ty = type_of::type_of(ccx, t);
    assert!(!ty::type_has_params(t));
    let val = alloca(bcx, ty, name);
    return val;
}

pub fn alloca(cx: &Block, ty: Type, name: &str) -> ValueRef {
    alloca_maybe_zeroed(cx, ty, name, false)
}

pub fn alloca_maybe_zeroed(cx: &Block, ty: Type, name: &str, zero: bool) -> ValueRef {
    let _icx = push_ctxt("alloca");
    if cx.unreachable.get() {
        unsafe {
            return llvm::LLVMGetUndef(ty.ptr_to().to_ref());
        }
    }
    debuginfo::clear_source_location(cx.fcx);
    let p = Alloca(cx, ty, name);
    if zero {
        let b = cx.fcx.ccx.builder();
        b.position_before(cx.fcx.alloca_insert_pt.get().unwrap());
        memzero(&b, p, ty);
    }
    p
}

pub fn arrayalloca(cx: &Block, ty: Type, v: ValueRef) -> ValueRef {
    let _icx = push_ctxt("arrayalloca");
    if cx.unreachable.get() {
        unsafe {
            return llvm::LLVMGetUndef(ty.to_ref());
        }
    }
    debuginfo::clear_source_location(cx.fcx);
    return ArrayAlloca(cx, ty, v);
}

// Creates and returns space for, or returns the argument representing, the
// slot where the return value of the function must go.
pub fn make_return_pointer(fcx: &FunctionContext, output_type: ty::t)
                           -> ValueRef {
    unsafe {
        if type_of::return_uses_outptr(fcx.ccx, output_type) {
            llvm::LLVMGetParam(fcx.llfn, 0)
        } else {
            let lloutputtype = type_of::type_of(fcx.ccx, output_type);
            let bcx = fcx.entry_bcx.borrow().clone().unwrap();
            Alloca(bcx, lloutputtype, "__make_return_pointer")
        }
    }
}

// NB: must keep 4 fns in sync:
//
//  - type_of_fn
//  - create_datums_for_fn_args.
//  - new_fn_ctxt
//  - trans_args
//
// Be warned! You must call `init_function` before doing anything with the
// returned function context.
pub fn new_fn_ctxt<'a>(ccx: &'a CrateContext,
                       llfndecl: ValueRef,
                       id: ast::NodeId,
                       has_env: bool,
                       output_type: ty::t,
                       param_substs: Option<@param_substs>,
                       sp: Option<Span>,
                       block_arena: &'a TypedArena<Block<'a>>)
                       -> FunctionContext<'a> {
    for p in param_substs.iter() { p.validate(); }

    debug!("new_fn_ctxt(path={}, id={}, param_substs={})",
           if id == -1 { ~"" } else { ccx.tcx.map.path_to_str(id) },
           id, param_substs.repr(ccx.tcx()));

    let substd_output_type = match param_substs {
        None => output_type,
        Some(substs) => {
            ty::subst_tps(ccx.tcx(),
                          substs.tys.as_slice(),
                          substs.self_ty,
                          output_type)
        }
    };
    let uses_outptr = type_of::return_uses_outptr(ccx, substd_output_type);
    let debug_context = debuginfo::create_function_debug_context(ccx, id, param_substs, llfndecl);

    let mut fcx = FunctionContext {
          llfn: llfndecl,
          llenv: None,
          llretptr: Cell::new(None),
          entry_bcx: RefCell::new(None),
          alloca_insert_pt: Cell::new(None),
          llreturn: Cell::new(None),
          personality: Cell::new(None),
          caller_expects_out_pointer: uses_outptr,
          llargs: RefCell::new(NodeMap::new()),
          lllocals: RefCell::new(NodeMap::new()),
          llupvars: RefCell::new(NodeMap::new()),
          id: id,
          param_substs: param_substs,
          span: sp,
          block_arena: block_arena,
          ccx: ccx,
          debug_context: debug_context,
          scopes: RefCell::new(Vec::new())
    };

    if has_env {
        fcx.llenv = Some(unsafe {
            llvm::LLVMGetParam(fcx.llfn, fcx.env_arg_pos() as c_uint)
        });
    }

    fcx
}

/// Performs setup on a newly created function, creating the entry scope block
/// and allocating space for the return pointer.
pub fn init_function<'a>(
                     fcx: &'a FunctionContext<'a>,
                     skip_retptr: bool,
                     output_type: ty::t,
                     param_substs: Option<@param_substs>) {
    let entry_bcx = fcx.new_temp_block("entry-block");

    *fcx.entry_bcx.borrow_mut() = Some(entry_bcx);

    // Use a dummy instruction as the insertion point for all allocas.
    // This is later removed in FunctionContext::cleanup.
    fcx.alloca_insert_pt.set(Some(unsafe {
        Load(entry_bcx, C_null(Type::i8p(fcx.ccx)));
        llvm::LLVMGetFirstInstruction(entry_bcx.llbb)
    }));

    let substd_output_type = match param_substs {
        None => output_type,
        Some(substs) => {
            ty::subst_tps(fcx.ccx.tcx(),
                          substs.tys.as_slice(),
                          substs.self_ty,
                          output_type)
        }
    };

    if !return_type_is_void(fcx.ccx, substd_output_type) {
        // If the function returns nil/bot, there is no real return
        // value, so do not set `llretptr`.
        if !skip_retptr || fcx.caller_expects_out_pointer {
            // Otherwise, we normally allocate the llretptr, unless we
            // have been instructed to skip it for immediate return
            // values.
            fcx.llretptr.set(Some(make_return_pointer(fcx, substd_output_type)));
        }
    }
}

// NB: must keep 4 fns in sync:
//
//  - type_of_fn
//  - create_datums_for_fn_args.
//  - new_fn_ctxt
//  - trans_args

fn arg_kind(cx: &FunctionContext, t: ty::t) -> datum::Rvalue {
    use middle::trans::datum::{ByRef, ByValue};

    datum::Rvalue {
        mode: if arg_is_indirect(cx.ccx, t) { ByRef } else { ByValue }
    }
}

// work around bizarre resolve errors
pub type RvalueDatum = datum::Datum<datum::Rvalue>;
pub type LvalueDatum = datum::Datum<datum::Lvalue>;

// create_datums_for_fn_args: creates rvalue datums for each of the
// incoming function arguments. These will later be stored into
// appropriate lvalue datums.
pub fn create_datums_for_fn_args(fcx: &FunctionContext,
                                 arg_tys: &[ty::t])
                                 -> Vec<RvalueDatum> {
    let _icx = push_ctxt("create_datums_for_fn_args");

    // Return an array wrapping the ValueRefs that we get from
    // llvm::LLVMGetParam for each argument into datums.
    arg_tys.iter().enumerate().map(|(i, &arg_ty)| {
        let llarg = unsafe {
            llvm::LLVMGetParam(fcx.llfn, fcx.arg_pos(i) as c_uint)
        };
        datum::Datum(llarg, arg_ty, arg_kind(fcx, arg_ty))
    }).collect()
}

fn copy_args_to_allocas<'a>(fcx: &FunctionContext<'a>,
                            arg_scope: cleanup::CustomScopeIndex,
                            bcx: &'a Block<'a>,
                            args: &[ast::Arg],
                            arg_datums: Vec<RvalueDatum> )
                            -> &'a Block<'a> {
    debug!("copy_args_to_allocas");

    let _icx = push_ctxt("copy_args_to_allocas");
    let mut bcx = bcx;

    let arg_scope_id = cleanup::CustomScope(arg_scope);

    for (i, arg_datum) in arg_datums.move_iter().enumerate() {
        // For certain mode/type combinations, the raw llarg values are passed
        // by value.  However, within the fn body itself, we want to always
        // have all locals and arguments be by-ref so that we can cancel the
        // cleanup and for better interaction with LLVM's debug info.  So, if
        // the argument would be passed by value, we store it into an alloca.
        // This alloca should be optimized away by LLVM's mem-to-reg pass in
        // the event it's not truly needed.

        bcx = _match::store_arg(bcx, args[i].pat, arg_datum, arg_scope_id);

        if fcx.ccx.sess().opts.debuginfo == FullDebugInfo {
            debuginfo::create_argument_metadata(bcx, &args[i]);
        }
    }

    bcx
}

// Ties up the llstaticallocas -> llloadenv -> lltop edges,
// and builds the return block.
pub fn finish_fn<'a>(fcx: &'a FunctionContext<'a>,
                     last_bcx: &'a Block<'a>) {
    let _icx = push_ctxt("finish_fn");

    let ret_cx = match fcx.llreturn.get() {
        Some(llreturn) => {
            if !last_bcx.terminated.get() {
                Br(last_bcx, llreturn);
            }
            raw_block(fcx, false, llreturn)
        }
        None => last_bcx
    };
    build_return_block(fcx, ret_cx);
    debuginfo::clear_source_location(fcx);
    fcx.cleanup();
}

// Builds the return block for a function.
pub fn build_return_block(fcx: &FunctionContext, ret_cx: &Block) {
    // Return the value if this function immediate; otherwise, return void.
    if fcx.llretptr.get().is_none() || fcx.caller_expects_out_pointer {
        return RetVoid(ret_cx);
    }

    let retptr = Value(fcx.llretptr.get().unwrap());
    let retval = match retptr.get_dominating_store(ret_cx) {
        // If there's only a single store to the ret slot, we can directly return
        // the value that was stored and omit the store and the alloca
        Some(s) => {
            let retval = s.get_operand(0).unwrap().get();
            s.erase_from_parent();

            if retptr.has_no_uses() {
                retptr.erase_from_parent();
            }

            retval
        }
        // Otherwise, load the return value from the ret slot
        None => Load(ret_cx, fcx.llretptr.get().unwrap())
    };


    Ret(ret_cx, retval);
}

// trans_closure: Builds an LLVM function out of a source function.
// If the function closes over its environment a closure will be
// returned.
pub fn trans_closure(ccx: &CrateContext,
                     decl: &ast::FnDecl,
                     body: &ast::Block,
                     llfndecl: ValueRef,
                     param_substs: Option<@param_substs>,
                     id: ast::NodeId,
                     _attributes: &[ast::Attribute],
                     output_type: ty::t,
                     maybe_load_env: <'a> |&'a Block<'a>| -> &'a Block<'a>) {
    ccx.stats.n_closures.set(ccx.stats.n_closures.get() + 1);

    let _icx = push_ctxt("trans_closure");
    set_uwtable(llfndecl);

    debug!("trans_closure(..., param_substs={})",
           param_substs.repr(ccx.tcx()));

    let has_env = match ty::get(ty::node_id_to_type(ccx.tcx(), id)).sty {
        ty::ty_closure(_) => true,
        _ => false
    };

    let arena = TypedArena::new();
    let fcx = new_fn_ctxt(ccx,
                          llfndecl,
                          id,
                          has_env,
                          output_type,
                          param_substs,
                          Some(body.span),
                          &arena);
    init_function(&fcx, false, output_type, param_substs);

    // cleanup scope for the incoming arguments
    let arg_scope = fcx.push_custom_cleanup_scope();

    // Create the first basic block in the function and keep a handle on it to
    //  pass to finish_fn later.
    let bcx_top = fcx.entry_bcx.borrow().clone().unwrap();
    let mut bcx = bcx_top;
    let block_ty = node_id_type(bcx, body.id);

    // Set up arguments to the function.
    let arg_tys = ty::ty_fn_args(node_id_type(bcx, id));
    let arg_datums = create_datums_for_fn_args(&fcx, arg_tys.as_slice());

    bcx = copy_args_to_allocas(&fcx,
                               arg_scope,
                               bcx,
                               decl.inputs.as_slice(),
                               arg_datums);

    bcx = maybe_load_env(bcx);

    // Up until here, IR instructions for this function have explicitly not been annotated with
    // source code location, so we don't step into call setup code. From here on, source location
    // emitting should be enabled.
    debuginfo::start_emitting_source_locations(&fcx);

    let dest = match fcx.llretptr.get() {
        Some(e) => {expr::SaveIn(e)}
        None => {
            assert!(type_is_zero_size(bcx.ccx(), block_ty))
            expr::Ignore
        }
    };

    // This call to trans_block is the place where we bridge between
    // translation calls that don't have a return value (trans_crate,
    // trans_mod, trans_item, et cetera) and those that do
    // (trans_block, trans_expr, et cetera).
    bcx = controlflow::trans_block(bcx, body, dest);

    match fcx.llreturn.get() {
        Some(_) => {
            Br(bcx, fcx.return_exit_block());
            fcx.pop_custom_cleanup_scope(arg_scope);
        }
        None => {
            // Microoptimization writ large: avoid creating a separate
            // llreturn basic block
            bcx = fcx.pop_and_trans_custom_cleanup_scope(bcx, arg_scope);
        }
    };

    // Put return block after all other blocks.
    // This somewhat improves single-stepping experience in debugger.
    unsafe {
        let llreturn = fcx.llreturn.get();
        for &llreturn in llreturn.iter() {
            llvm::LLVMMoveBasicBlockAfter(llreturn, bcx.llbb);
        }
    }

    // Insert the mandatory first few basic blocks before lltop.
    finish_fn(&fcx, bcx);
}

// trans_fn: creates an LLVM function corresponding to a source language
// function.
pub fn trans_fn(ccx: &CrateContext,
                decl: &ast::FnDecl,
                body: &ast::Block,
                llfndecl: ValueRef,
                param_substs: Option<@param_substs>,
                id: ast::NodeId,
                attrs: &[ast::Attribute]) {
    let _s = StatRecorder::new(ccx, ccx.tcx.map.path_to_str(id));
    debug!("trans_fn(param_substs={})", param_substs.repr(ccx.tcx()));
    let _icx = push_ctxt("trans_fn");
    let output_type = ty::ty_fn_ret(ty::node_id_to_type(ccx.tcx(), id));
    trans_closure(ccx, decl, body, llfndecl,
                  param_substs, id, attrs, output_type, |bcx| bcx);
}

pub fn trans_enum_variant(ccx: &CrateContext,
                          _enum_id: ast::NodeId,
                          variant: &ast::Variant,
                          _args: &[ast::VariantArg],
                          disr: ty::Disr,
                          param_substs: Option<@param_substs>,
                          llfndecl: ValueRef) {
    let _icx = push_ctxt("trans_enum_variant");

    trans_enum_variant_or_tuple_like_struct(
        ccx,
        variant.node.id,
        disr,
        param_substs,
        llfndecl);
}

pub fn trans_tuple_struct(ccx: &CrateContext,
                          _fields: &[ast::StructField],
                          ctor_id: ast::NodeId,
                          param_substs: Option<@param_substs>,
                          llfndecl: ValueRef) {
    let _icx = push_ctxt("trans_tuple_struct");

    trans_enum_variant_or_tuple_like_struct(
        ccx,
        ctor_id,
        0,
        param_substs,
        llfndecl);
}

fn trans_enum_variant_or_tuple_like_struct(ccx: &CrateContext,
                                           ctor_id: ast::NodeId,
                                           disr: ty::Disr,
                                           param_substs: Option<@param_substs>,
                                           llfndecl: ValueRef) {
    let no_substs: &[ty::t] = [];
    let ty_param_substs = match param_substs {
        Some(ref substs) => {
            let v: &[ty::t] = substs.tys.as_slice();
            v
        }
        None => {
            let v: &[ty::t] = no_substs;
            v
        }
    };

    let ctor_ty = ty::subst_tps(ccx.tcx(),
                                ty_param_substs,
                                None,
                                ty::node_id_to_type(ccx.tcx(), ctor_id));

    let result_ty = match ty::get(ctor_ty).sty {
        ty::ty_bare_fn(ref bft) => bft.sig.output,
        _ => ccx.sess().bug(
            format!("trans_enum_variant_or_tuple_like_struct: \
                  unexpected ctor return type {}",
                 ty_to_str(ccx.tcx(), ctor_ty)))
    };

    let arena = TypedArena::new();
    let fcx = new_fn_ctxt(ccx, llfndecl, ctor_id, false, result_ty,
                          param_substs, None, &arena);
    init_function(&fcx, false, result_ty, param_substs);

    let arg_tys = ty::ty_fn_args(ctor_ty);

    let arg_datums = create_datums_for_fn_args(&fcx, arg_tys.as_slice());

    let bcx = fcx.entry_bcx.borrow().clone().unwrap();

    if !type_is_zero_size(fcx.ccx, result_ty) {
        let repr = adt::represent_type(ccx, result_ty);
        adt::trans_start_init(bcx, repr, fcx.llretptr.get().unwrap(), disr);
        for (i, arg_datum) in arg_datums.move_iter().enumerate() {
            let lldestptr = adt::trans_field_ptr(bcx,
                                                 repr,
                                                 fcx.llretptr.get().unwrap(),
                                                 disr,
                                                 i);
            arg_datum.store_to(bcx, lldestptr);
        }
    }

    finish_fn(&fcx, bcx);
}

pub fn trans_enum_def(ccx: &CrateContext, enum_definition: &ast::EnumDef,
                      id: ast::NodeId, vi: @Vec<@ty::VariantInfo>,
                      i: &mut uint) {
    for &variant in enum_definition.variants.iter() {
        let disr_val = vi.get(*i).disr_val;
        *i += 1;

        match variant.node.kind {
            ast::TupleVariantKind(ref args) if args.len() > 0 => {
                let llfn = get_item_val(ccx, variant.node.id);
                trans_enum_variant(ccx, id, variant, args.as_slice(),
                                   disr_val, None, llfn);
            }
            ast::TupleVariantKind(_) => {
                // Nothing to do.
            }
            ast::StructVariantKind(struct_def) => {
                trans_struct_def(ccx, struct_def);
            }
        }
    }
}

pub struct TransItemVisitor<'a> {
    pub ccx: &'a CrateContext,
}

impl<'a> Visitor<()> for TransItemVisitor<'a> {
    fn visit_item(&mut self, i: &ast::Item, _:()) {
        trans_item(self.ccx, i);
    }
}

pub fn trans_item(ccx: &CrateContext, item: &ast::Item) {
    let _icx = push_ctxt("trans_item");
    match item.node {
      ast::ItemFn(decl, purity, _abis, ref generics, body) => {
        if purity == ast::ExternFn  {
            let llfndecl = get_item_val(ccx, item.id);
            foreign::trans_rust_fn_with_foreign_abi(
                ccx, decl, body, item.attrs.as_slice(), llfndecl, item.id);
        } else if !generics.is_type_parameterized() {
            let llfn = get_item_val(ccx, item.id);
            trans_fn(ccx,
                     decl,
                     body,
                     llfn,
                     None,
                     item.id,
                     item.attrs.as_slice());
        } else {
            // Be sure to travel more than just one layer deep to catch nested
            // items in blocks and such.
            let mut v = TransItemVisitor{ ccx: ccx };
            v.visit_block(body, ());
        }
      }
      ast::ItemImpl(ref generics, _, _, ref ms) => {
        meth::trans_impl(ccx, item.ident, ms.as_slice(), generics, item.id);
      }
      ast::ItemMod(ref m) => {
        trans_mod(ccx, m);
      }
      ast::ItemEnum(ref enum_definition, ref generics) => {
        if !generics.is_type_parameterized() {
            let vi = ty::enum_variants(ccx.tcx(), local_def(item.id));
            let mut i = 0;
            trans_enum_def(ccx, enum_definition, item.id, vi, &mut i);
        }
      }
      ast::ItemStatic(_, m, expr) => {
          consts::trans_const(ccx, m, item.id);
          // Do static_assert checking. It can't really be done much earlier
          // because we need to get the value of the bool out of LLVM
          if attr::contains_name(item.attrs.as_slice(), "static_assert") {
              if m == ast::MutMutable {
                  ccx.sess().span_fatal(expr.span,
                                        "cannot have static_assert on a mutable \
                                         static");
              }

              let v = ccx.const_values.borrow().get_copy(&item.id);
              unsafe {
                  if !(llvm::LLVMConstIntGetZExtValue(v) != 0) {
                      ccx.sess().span_fatal(expr.span, "static assertion failed");
                  }
              }
          }
      },
      ast::ItemForeignMod(ref foreign_mod) => {
        foreign::trans_foreign_mod(ccx, foreign_mod);
      }
      ast::ItemStruct(struct_def, ref generics) => {
        if !generics.is_type_parameterized() {
            trans_struct_def(ccx, struct_def);
        }
      }
      ast::ItemTrait(..) => {
        // Inside of this trait definition, we won't be actually translating any
        // functions, but the trait still needs to be walked. Otherwise default
        // methods with items will not get translated and will cause ICE's when
        // metadata time comes around.
        let mut v = TransItemVisitor{ ccx: ccx };
        visit::walk_item(&mut v, item, ());
      }
      _ => {/* fall through */ }
    }
}

pub fn trans_struct_def(ccx: &CrateContext, struct_def: @ast::StructDef) {
    // If this is a tuple-like struct, translate the constructor.
    match struct_def.ctor_id {
        // We only need to translate a constructor if there are fields;
        // otherwise this is a unit-like struct.
        Some(ctor_id) if struct_def.fields.len() > 0 => {
            let llfndecl = get_item_val(ccx, ctor_id);
            trans_tuple_struct(ccx, struct_def.fields.as_slice(),
                               ctor_id, None, llfndecl);
        }
        Some(_) | None => {}
    }
}

// Translate a module. Doing this amounts to translating the items in the
// module; there ends up being no artifact (aside from linkage names) of
// separate modules in the compiled program.  That's because modules exist
// only as a convenience for humans working with the code, to organize names
// and control visibility.
pub fn trans_mod(ccx: &CrateContext, m: &ast::Mod) {
    let _icx = push_ctxt("trans_mod");
    for item in m.items.iter() {
        trans_item(ccx, *item);
    }
}

fn finish_register_fn(ccx: &CrateContext, sp: Span, sym: ~str, node_id: ast::NodeId,
                      llfn: ValueRef) {
    ccx.item_symbols.borrow_mut().insert(node_id, sym);

    if !ccx.reachable.contains(&node_id) {
        lib::llvm::SetLinkage(llfn, lib::llvm::InternalLinkage);
    }

    if is_entry_fn(ccx.sess(), node_id) && !ccx.sess().building_library.get() {
        create_entry_wrapper(ccx, sp, llfn);
    }
}

fn register_fn(ccx: &CrateContext,
               sp: Span,
               sym: ~str,
               node_id: ast::NodeId,
               node_type: ty::t)
               -> ValueRef {
    let f = match ty::get(node_type).sty {
        ty::ty_bare_fn(ref f) => {
            assert!(f.abis.is_rust() || f.abis.is_intrinsic());
            f
        }
        _ => fail!("expected bare rust fn or an intrinsic")
    };

    let llfn = decl_rust_fn(ccx,
                            false,
                            f.sig.inputs.as_slice(),
                            f.sig.output,
                            sym);
    finish_register_fn(ccx, sp, sym, node_id, llfn);
    llfn
}

// only use this for foreign function ABIs and glue, use `register_fn` for Rust functions
pub fn register_fn_llvmty(ccx: &CrateContext,
                          sp: Span,
                          sym: ~str,
                          node_id: ast::NodeId,
                          cc: lib::llvm::CallConv,
                          fn_ty: Type,
                          output: ty::t) -> ValueRef {
    debug!("register_fn_llvmty id={} sym={}", node_id, sym);

    let llfn = decl_fn(ccx.llmod, sym, cc, fn_ty, output);
    finish_register_fn(ccx, sp, sym, node_id, llfn);
    llfn
}

pub fn is_entry_fn(sess: &Session, node_id: ast::NodeId) -> bool {
    match *sess.entry_fn.borrow() {
        Some((entry_id, _)) => node_id == entry_id,
        None => false
    }
}

// Create a _rust_main(args: ~[str]) function which will be called from the
// runtime rust_start function
pub fn create_entry_wrapper(ccx: &CrateContext,
                           _sp: Span,
                           main_llfn: ValueRef) {
    let et = ccx.sess().entry_type.get().unwrap();
    match et {
        session::EntryMain => {
            create_entry_fn(ccx, main_llfn, true);
        }
        session::EntryStart => create_entry_fn(ccx, main_llfn, false),
        session::EntryNone => {}    // Do nothing.
    }

    fn create_entry_fn(ccx: &CrateContext,
                       rust_main: ValueRef,
                       use_start_lang_item: bool) {
        let llfty = Type::func([ccx.int_type, Type::i8p(ccx).ptr_to()],
                               &ccx.int_type);

        let llfn = decl_cdecl_fn(ccx.llmod, "main", llfty, ty::mk_nil());
        let llbb = "top".with_c_str(|buf| {
            unsafe {
                llvm::LLVMAppendBasicBlockInContext(ccx.llcx, llfn, buf)
            }
        });
        let bld = ccx.builder.b;
        unsafe {
            llvm::LLVMPositionBuilderAtEnd(bld, llbb);

            let (start_fn, args) = if use_start_lang_item {
                let start_def_id = match ccx.tcx.lang_items.require(StartFnLangItem) {
                    Ok(id) => id,
                    Err(s) => { ccx.sess().fatal(s); }
                };
                let start_fn = if start_def_id.krate == ast::LOCAL_CRATE {
                    get_item_val(ccx, start_def_id.node)
                } else {
                    let start_fn_type = csearch::get_type(ccx.tcx(),
                                                          start_def_id).ty;
                    trans_external_path(ccx, start_def_id, start_fn_type)
                };

                let args = {
                    let opaque_rust_main = "rust_main".with_c_str(|buf| {
                        llvm::LLVMBuildPointerCast(bld, rust_main, Type::i8p(ccx).to_ref(), buf)
                    });

                    vec!(
                        opaque_rust_main,
                        llvm::LLVMGetParam(llfn, 0),
                        llvm::LLVMGetParam(llfn, 1)
                     )
                };
                (start_fn, args)
            } else {
                debug!("using user-defined start fn");
                let args = vec!(
                    llvm::LLVMGetParam(llfn, 0 as c_uint),
                    llvm::LLVMGetParam(llfn, 1 as c_uint)
                );

                (rust_main, args)
            };

            let result = llvm::LLVMBuildCall(bld,
                                             start_fn,
                                             args.as_ptr(),
                                             args.len() as c_uint,
                                             noname());

            llvm::LLVMBuildRet(bld, result);
        }
    }
}

fn exported_name(ccx: &CrateContext, id: ast::NodeId,
                 ty: ty::t, attrs: &[ast::Attribute]) -> ~str {
    match attr::first_attr_value_str_by_name(attrs, "export_name") {
        // Use provided name
        Some(name) => name.get().to_owned(),

        _ => ccx.tcx.map.with_path(id, |mut path| {
            if attr::contains_name(attrs, "no_mangle") {
                // Don't mangle
                path.last().unwrap().to_str()
            } else {
                // Usual name mangling
                mangle_exported_name(ccx, path, ty, id)
            }
        })
    }
}

pub fn get_item_val(ccx: &CrateContext, id: ast::NodeId) -> ValueRef {
    debug!("get_item_val(id=`{:?}`)", id);

    match ccx.item_vals.borrow().find_copy(&id) {
        Some(v) => return v,
        None => {}
    }

    let mut foreign = false;
    let item = ccx.tcx.map.get(id);
    let val = match item {
        ast_map::NodeItem(i) => {
            let ty = ty::node_id_to_type(ccx.tcx(), i.id);
            let sym = exported_name(ccx, id, ty, i.attrs.as_slice());

            let v = match i.node {
                ast::ItemStatic(_, _, expr) => {
                    // If this static came from an external crate, then
                    // we need to get the symbol from csearch instead of
                    // using the current crate's name/version
                    // information in the hash of the symbol
                    debug!("making {}", sym);
                    let (sym, is_local) = {
                        match ccx.external_srcs.borrow().find(&i.id) {
                            Some(&did) => {
                                debug!("but found in other crate...");
                                (csearch::get_symbol(&ccx.sess().cstore,
                                                     did), false)
                            }
                            None => (sym, true)
                        }
                    };

                    // We need the translated value here, because for enums the
                    // LLVM type is not fully determined by the Rust type.
                    let (v, inlineable) = consts::const_expr(ccx, expr, is_local);
                    ccx.const_values.borrow_mut().insert(id, v);
                    let mut inlineable = inlineable;

                    unsafe {
                        let llty = llvm::LLVMTypeOf(v);
                        let g = sym.with_c_str(|buf| {
                            llvm::LLVMAddGlobal(ccx.llmod, llty, buf)
                        });

                        if !ccx.reachable.contains(&id) {
                            lib::llvm::SetLinkage(g, lib::llvm::InternalLinkage);
                        }

                        // Apply the `unnamed_addr` attribute if
                        // requested
                        if attr::contains_name(i.attrs.as_slice(),
                                               "address_insignificant") {
                            if ccx.reachable.contains(&id) {
                                ccx.sess().span_bug(i.span,
                                    "insignificant static is reachable");
                            }
                            lib::llvm::SetUnnamedAddr(g, true);

                            // This is a curious case where we must make
                            // all of these statics inlineable. If a
                            // global is tagged as
                            // address_insignificant, then LLVM won't
                            // coalesce globals unless they have an
                            // internal linkage type. This means that
                            // external crates cannot use this global.
                            // This is a problem for things like inner
                            // statics in generic functions, because the
                            // function will be inlined into another
                            // crate and then attempt to link to the
                            // static in the original crate, only to
                            // find that it's not there. On the other
                            // side of inlininig, the crates knows to
                            // not declare this static as
                            // available_externally (because it isn't)
                            inlineable = true;
                        }

                        if attr::contains_name(i.attrs.as_slice(),
                                               "thread_local") {
                            lib::llvm::set_thread_local(g, true);
                        }

                        if !inlineable {
                            debug!("{} not inlined", sym);
                            ccx.non_inlineable_statics.borrow_mut()
                                                      .insert(id);
                        }

                        ccx.item_symbols.borrow_mut().insert(i.id, sym);
                        g
                    }
                }

                ast::ItemFn(_, purity, _, _, _) => {
                    let llfn = if purity != ast::ExternFn {
                        register_fn(ccx, i.span, sym, i.id, ty)
                    } else {
                        foreign::register_rust_fn_with_foreign_abi(ccx,
                                                                   i.span,
                                                                   sym,
                                                                   i.id)
                    };
                    set_llvm_fn_attrs(i.attrs.as_slice(), llfn);
                    llfn
                }

                _ => fail!("get_item_val: weird result in table")
            };

            match attr::first_attr_value_str_by_name(i.attrs.as_slice(),
                                                     "link_section") {
                Some(sect) => unsafe {
                    sect.get().with_c_str(|buf| {
                        llvm::LLVMSetSection(v, buf);
                    })
                },
                None => ()
            }

            v
        }

        ast_map::NodeTraitMethod(trait_method) => {
            debug!("get_item_val(): processing a NodeTraitMethod");
            match *trait_method {
                ast::Required(_) => {
                    ccx.sess().bug("unexpected variant: required trait method in \
                                   get_item_val()");
                }
                ast::Provided(m) => {
                    register_method(ccx, id, m)
                }
            }
        }

        ast_map::NodeMethod(m) => {
            register_method(ccx, id, m)
        }

        ast_map::NodeForeignItem(ni) => {
            foreign = true;

            match ni.node {
                ast::ForeignItemFn(..) => {
                    let abis = ccx.tcx.map.get_foreign_abis(id);
                    foreign::register_foreign_item_fn(ccx, abis, ni)
                }
                ast::ForeignItemStatic(..) => {
                    foreign::register_static(ccx, ni)
                }
            }
        }

        ast_map::NodeVariant(ref v) => {
            let llfn;
            let args = match v.node.kind {
                ast::TupleVariantKind(ref args) => args,
                ast::StructVariantKind(_) => {
                    fail!("struct variant kind unexpected in get_item_val")
                }
            };
            assert!(args.len() != 0u);
            let ty = ty::node_id_to_type(ccx.tcx(), id);
            let parent = ccx.tcx.map.get_parent(id);
            let enm = ccx.tcx.map.expect_item(parent);
            let sym = exported_name(ccx,
                                    id,
                                    ty,
                                    enm.attrs.as_slice());

            llfn = match enm.node {
                ast::ItemEnum(_, _) => {
                    register_fn(ccx, (*v).span, sym, id, ty)
                }
                _ => fail!("NodeVariant, shouldn't happen")
            };
            set_inline_hint(llfn);
            llfn
        }

        ast_map::NodeStructCtor(struct_def) => {
            // Only register the constructor if this is a tuple-like struct.
            let ctor_id = match struct_def.ctor_id {
                None => {
                    ccx.sess().bug("attempt to register a constructor of \
                                    a non-tuple-like struct")
                }
                Some(ctor_id) => ctor_id,
            };
            let parent = ccx.tcx.map.get_parent(id);
            let struct_item = ccx.tcx.map.expect_item(parent);
            let ty = ty::node_id_to_type(ccx.tcx(), ctor_id);
            let sym = exported_name(ccx,
                                    id,
                                    ty,
                                    struct_item.attrs
                                               .as_slice());
            let llfn = register_fn(ccx, struct_item.span,
                                   sym, ctor_id, ty);
            set_inline_hint(llfn);
            llfn
        }

        ref variant => {
            ccx.sess().bug(format!("get_item_val(): unexpected variant: {:?}",
                           variant))
        }
    };

    // foreign items (extern fns and extern statics) don't have internal
    // linkage b/c that doesn't quite make sense. Otherwise items can
    // have internal linkage if they're not reachable.
    if !foreign && !ccx.reachable.contains(&id) {
        lib::llvm::SetLinkage(val, lib::llvm::InternalLinkage);
    }

    ccx.item_vals.borrow_mut().insert(id, val);
    val
}

fn register_method(ccx: &CrateContext, id: ast::NodeId,
                   m: &ast::Method) -> ValueRef {
    let mty = ty::node_id_to_type(ccx.tcx(), id);

    let sym = exported_name(ccx, id, mty, m.attrs.as_slice());

    let llfn = register_fn(ccx, m.span, sym, id, mty);
    set_llvm_fn_attrs(m.attrs.as_slice(), llfn);
    llfn
}

pub fn p2i(ccx: &CrateContext, v: ValueRef) -> ValueRef {
    unsafe {
        return llvm::LLVMConstPtrToInt(v, ccx.int_type.to_ref());
    }
}


pub fn declare_intrinsics(ccx: &mut CrateContext) {
    macro_rules! ifn (
        ($name:expr fn() -> $ret:expr) => ({
            let name = $name;
            // HACK(eddyb) dummy output type, shouln't affect anything.
            let f = decl_cdecl_fn(ccx.llmod, name, Type::func([], &$ret), ty::mk_nil());
            ccx.intrinsics.insert(name, f);
        });
        ($name:expr fn($($arg:expr),*) -> $ret:expr) => ({
            let name = $name;
            // HACK(eddyb) dummy output type, shouln't affect anything.
            let f = decl_cdecl_fn(ccx.llmod, name,
                                  Type::func([$($arg),*], &$ret), ty::mk_nil());
            ccx.intrinsics.insert(name, f);
        })
    )
    macro_rules! mk_struct (
        ($($field_ty:expr),*) => (Type::struct_(ccx, [$($field_ty),*], false))
    )

    let i8p = Type::i8p(ccx);
    let void = Type::void(ccx);
    let i1 = Type::i1(ccx);
    let t_i8 = Type::i8(ccx);
    let t_i16 = Type::i16(ccx);
    let t_i32 = Type::i32(ccx);
    let t_i64 = Type::i64(ccx);
    let t_f32 = Type::f32(ccx);
    let t_f64 = Type::f64(ccx);

    ifn!("llvm.memcpy.p0i8.p0i8.i32" fn(i8p, i8p, t_i32, t_i32, i1) -> void);
    ifn!("llvm.memcpy.p0i8.p0i8.i64" fn(i8p, i8p, t_i64, t_i32, i1) -> void);
    ifn!("llvm.memmove.p0i8.p0i8.i32" fn(i8p, i8p, t_i32, t_i32, i1) -> void);
    ifn!("llvm.memmove.p0i8.p0i8.i64" fn(i8p, i8p, t_i64, t_i32, i1) -> void);
    ifn!("llvm.memset.p0i8.i32" fn(i8p, t_i8, t_i32, t_i32, i1) -> void);
    ifn!("llvm.memset.p0i8.i64" fn(i8p, t_i8, t_i64, t_i32, i1) -> void);

    ifn!("llvm.trap" fn() -> void);
    ifn!("llvm.debugtrap" fn() -> void);
    ifn!("llvm.frameaddress" fn(t_i32) -> i8p);

    ifn!("llvm.powi.f32" fn(t_f32, t_i32) -> t_f32);
    ifn!("llvm.powi.f64" fn(t_f64, t_i32) -> t_f64);
    ifn!("llvm.pow.f32" fn(t_f32, t_f32) -> t_f32);
    ifn!("llvm.pow.f64" fn(t_f64, t_f64) -> t_f64);

    ifn!("llvm.sqrt.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.sqrt.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.sin.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.sin.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.cos.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.cos.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.exp.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.exp.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.exp2.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.exp2.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.log.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.log.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.log10.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.log10.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.log2.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.log2.f64" fn(t_f64) -> t_f64);

    ifn!("llvm.fma.f32" fn(t_f32, t_f32, t_f32) -> t_f32);
    ifn!("llvm.fma.f64" fn(t_f64, t_f64, t_f64) -> t_f64);

    ifn!("llvm.fabs.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.fabs.f64" fn(t_f64) -> t_f64);

    ifn!("llvm.floor.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.floor.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.ceil.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.ceil.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.trunc.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.trunc.f64" fn(t_f64) -> t_f64);

    ifn!("llvm.rint.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.rint.f64" fn(t_f64) -> t_f64);
    ifn!("llvm.nearbyint.f32" fn(t_f32) -> t_f32);
    ifn!("llvm.nearbyint.f64" fn(t_f64) -> t_f64);

    ifn!("llvm.ctpop.i8" fn(t_i8) -> t_i8);
    ifn!("llvm.ctpop.i16" fn(t_i16) -> t_i16);
    ifn!("llvm.ctpop.i32" fn(t_i32) -> t_i32);
    ifn!("llvm.ctpop.i64" fn(t_i64) -> t_i64);

    ifn!("llvm.ctlz.i8" fn(t_i8 , i1) -> t_i8);
    ifn!("llvm.ctlz.i16" fn(t_i16, i1) -> t_i16);
    ifn!("llvm.ctlz.i32" fn(t_i32, i1) -> t_i32);
    ifn!("llvm.ctlz.i64" fn(t_i64, i1) -> t_i64);

    ifn!("llvm.cttz.i8" fn(t_i8 , i1) -> t_i8);
    ifn!("llvm.cttz.i16" fn(t_i16, i1) -> t_i16);
    ifn!("llvm.cttz.i32" fn(t_i32, i1) -> t_i32);
    ifn!("llvm.cttz.i64" fn(t_i64, i1) -> t_i64);

    ifn!("llvm.bswap.i16" fn(t_i16) -> t_i16);
    ifn!("llvm.bswap.i32" fn(t_i32) -> t_i32);
    ifn!("llvm.bswap.i64" fn(t_i64) -> t_i64);

    ifn!("llvm.sadd.with.overflow.i8" fn(t_i8, t_i8) -> mk_struct!{t_i8, i1});
    ifn!("llvm.sadd.with.overflow.i16" fn(t_i16, t_i16) -> mk_struct!{t_i16, i1});
    ifn!("llvm.sadd.with.overflow.i32" fn(t_i32, t_i32) -> mk_struct!{t_i32, i1});
    ifn!("llvm.sadd.with.overflow.i64" fn(t_i64, t_i64) -> mk_struct!{t_i64, i1});

    ifn!("llvm.uadd.with.overflow.i8" fn(t_i8, t_i8) -> mk_struct!{t_i8, i1});
    ifn!("llvm.uadd.with.overflow.i16" fn(t_i16, t_i16) -> mk_struct!{t_i16, i1});
    ifn!("llvm.uadd.with.overflow.i32" fn(t_i32, t_i32) -> mk_struct!{t_i32, i1});
    ifn!("llvm.uadd.with.overflow.i64" fn(t_i64, t_i64) -> mk_struct!{t_i64, i1});

    ifn!("llvm.ssub.with.overflow.i8" fn(t_i8, t_i8) -> mk_struct!{t_i8, i1});
    ifn!("llvm.ssub.with.overflow.i16" fn(t_i16, t_i16) -> mk_struct!{t_i16, i1});
    ifn!("llvm.ssub.with.overflow.i32" fn(t_i32, t_i32) -> mk_struct!{t_i32, i1});
    ifn!("llvm.ssub.with.overflow.i64" fn(t_i64, t_i64) -> mk_struct!{t_i64, i1});

    ifn!("llvm.usub.with.overflow.i8" fn(t_i8, t_i8) -> mk_struct!{t_i8, i1});
    ifn!("llvm.usub.with.overflow.i16" fn(t_i16, t_i16) -> mk_struct!{t_i16, i1});
    ifn!("llvm.usub.with.overflow.i32" fn(t_i32, t_i32) -> mk_struct!{t_i32, i1});
    ifn!("llvm.usub.with.overflow.i64" fn(t_i64, t_i64) -> mk_struct!{t_i64, i1});

    ifn!("llvm.smul.with.overflow.i8" fn(t_i8, t_i8) -> mk_struct!{t_i8, i1});
    ifn!("llvm.smul.with.overflow.i16" fn(t_i16, t_i16) -> mk_struct!{t_i16, i1});
    ifn!("llvm.smul.with.overflow.i32" fn(t_i32, t_i32) -> mk_struct!{t_i32, i1});
    ifn!("llvm.smul.with.overflow.i64" fn(t_i64, t_i64) -> mk_struct!{t_i64, i1});

    ifn!("llvm.umul.with.overflow.i8" fn(t_i8, t_i8) -> mk_struct!{t_i8, i1});
    ifn!("llvm.umul.with.overflow.i16" fn(t_i16, t_i16) -> mk_struct!{t_i16, i1});
    ifn!("llvm.umul.with.overflow.i32" fn(t_i32, t_i32) -> mk_struct!{t_i32, i1});
    ifn!("llvm.umul.with.overflow.i64" fn(t_i64, t_i64) -> mk_struct!{t_i64, i1});

    ifn!("llvm.expect.i1" fn(i1, i1) -> i1);

    // Some intrinsics were introduced in later versions of LLVM, but they have
    // fallbacks in libc or libm and such. Currently, all of these intrinsics
    // were introduced in LLVM 3.4, so we case on that.
    macro_rules! compatible_ifn (
        ($name:expr, $cname:ident ($($arg:expr),*) -> $ret:expr) => ({
            let name = $name;
            if unsafe { llvm::LLVMVersionMinor() >= 4 } {
                ifn!(name fn($($arg),*) -> $ret);
            } else {
                let f = decl_cdecl_fn(ccx.llmod, stringify!($cname),
                                      Type::func([$($arg),*], &$ret),
                                      ty::mk_nil());
                ccx.intrinsics.insert(name, f);
            }
        })
    )

    compatible_ifn!("llvm.copysign.f32", copysignf(t_f32, t_f32) -> t_f32);
    compatible_ifn!("llvm.copysign.f64", copysign(t_f64, t_f64) -> t_f64);
    compatible_ifn!("llvm.round.f32", roundf(t_f32) -> t_f32);
    compatible_ifn!("llvm.round.f64", round(t_f64) -> t_f64);


    if ccx.sess().opts.debuginfo != NoDebugInfo {
        ifn!("llvm.dbg.declare" fn(Type::metadata(ccx), Type::metadata(ccx)) -> void);
        ifn!("llvm.dbg.value" fn(Type::metadata(ccx), t_i64, Type::metadata(ccx)) -> void);
    }
}

pub fn crate_ctxt_to_encode_parms<'r>(cx: &'r CrateContext, ie: encoder::EncodeInlinedItem<'r>)
    -> encoder::EncodeParams<'r> {

        let diag = cx.sess().diagnostic();
        let item_symbols = &cx.item_symbols;
        let link_meta = &cx.link_meta;
        encoder::EncodeParams {
            diag: diag,
            tcx: cx.tcx(),
            reexports2: cx.exp_map2,
            item_symbols: item_symbols,
            non_inlineable_statics: &cx.non_inlineable_statics,
            link_meta: link_meta,
            cstore: &cx.sess().cstore,
            encode_inlined_item: ie,
        }
}

pub fn write_metadata(cx: &CrateContext, krate: &ast::Crate) -> Vec<u8> {
    use flate;

    if !cx.sess().building_library.get() {
        return Vec::new()
    }

    let encode_inlined_item: encoder::EncodeInlinedItem =
        |ecx, ebml_w, ii| astencode::encode_inlined_item(ecx, ebml_w, ii, &cx.maps);

    let encode_parms = crate_ctxt_to_encode_parms(cx, encode_inlined_item);
    let metadata = encoder::encode_metadata(encode_parms, krate);
    let compressed = encoder::metadata_encoding_version +
                        flate::deflate_bytes(metadata.as_slice()).as_slice();
    let llmeta = C_bytes(cx, compressed);
    let llconst = C_struct(cx, [llmeta], false);
    let name = format!("rust_metadata_{}_{}_{}", cx.link_meta.crateid.name,
                       cx.link_meta.crateid.version_or_default(), cx.link_meta.crate_hash);
    let llglobal = name.with_c_str(|buf| {
        unsafe {
            llvm::LLVMAddGlobal(cx.metadata_llmod, val_ty(llconst).to_ref(), buf)
        }
    });
    unsafe {
        llvm::LLVMSetInitializer(llglobal, llconst);
        cx.sess().targ_cfg.target_strs.meta_sect_name.with_c_str(|buf| {
            llvm::LLVMSetSection(llglobal, buf)
        });
    }
    return metadata;
}

pub fn trans_crate(krate: ast::Crate,
                   analysis: CrateAnalysis,
                   output: &OutputFilenames) -> (ty::ctxt, CrateTranslation) {
    let CrateAnalysis { ty_cx: tcx, exp_map2, maps, reachable, .. } = analysis;

    // Before we touch LLVM, make sure that multithreading is enabled.
    unsafe {
        use sync::one::{Once, ONCE_INIT};
        static mut INIT: Once = ONCE_INIT;
        static mut POISONED: bool = false;
        INIT.doit(|| {
            if llvm::LLVMStartMultithreaded() != 1 {
                // use an extra bool to make sure that all future usage of LLVM
                // cannot proceed despite the Once not running more than once.
                POISONED = true;
            }
        });

        if POISONED {
            tcx.sess.bug("couldn't enable multi-threaded LLVM");
        }
    }

    let link_meta = link::build_link_meta(&krate, output.out_filestem);

    // Append ".rs" to crate name as LLVM module identifier.
    //
    // LLVM code generator emits a ".file filename" directive
    // for ELF backends. Value of the "filename" is set as the
    // LLVM module identifier.  Due to a LLVM MC bug[1], LLVM
    // crashes if the module identifer is same as other symbols
    // such as a function name in the module.
    // 1. http://llvm.org/bugs/show_bug.cgi?id=11479
    let llmod_id = link_meta.crateid.name + ".rs";

    let ccx = CrateContext::new(llmod_id, tcx, exp_map2, maps,
                                Sha256::new(), link_meta, reachable);
    {
        let _icx = push_ctxt("text");
        trans_mod(&ccx, &krate.module);
    }

    glue::emit_tydescs(&ccx);
    if ccx.sess().opts.debuginfo != NoDebugInfo {
        debuginfo::finalize(&ccx);
    }

    // Translate the metadata.
    let metadata = write_metadata(&ccx, &krate);
    if ccx.sess().trans_stats() {
        println!("--- trans stats ---");
        println!("n_static_tydescs: {}", ccx.stats.n_static_tydescs.get());
        println!("n_glues_created: {}", ccx.stats.n_glues_created.get());
        println!("n_null_glues: {}", ccx.stats.n_null_glues.get());
        println!("n_real_glues: {}", ccx.stats.n_real_glues.get());

        println!("n_fns: {}", ccx.stats.n_fns.get());
        println!("n_monos: {}", ccx.stats.n_monos.get());
        println!("n_inlines: {}", ccx.stats.n_inlines.get());
        println!("n_closures: {}", ccx.stats.n_closures.get());
        println!("fn stats:");
        ccx.stats.fn_stats.borrow_mut().sort_by(|&(_, _, insns_a), &(_, _, insns_b)| {
            insns_b.cmp(&insns_a)
        });
        for tuple in ccx.stats.fn_stats.borrow().iter() {
            match *tuple {
                (ref name, ms, insns) => {
                    println!("{} insns, {} ms, {}", insns, ms, *name);
                }
            }
        }
    }
    if ccx.sess().count_llvm_insns() {
        for (k, v) in ccx.stats.llvm_insns.borrow().iter() {
            println!("{:7u} {}", *v, *k);
        }
    }

    let llcx = ccx.llcx;
    let link_meta = ccx.link_meta.clone();
    let llmod = ccx.llmod;

    let mut reachable: Vec<~str> = ccx.reachable.iter().filter_map(|id| {
        ccx.item_symbols.borrow().find(id).map(|s| s.to_owned())
    }).collect();

    // Make sure that some other crucial symbols are not eliminated from the
    // module. This includes the main function, the crate map (used for debug
    // log settings and I/O), and finally the curious rust_stack_exhausted
    // symbol. This symbol is required for use by the libmorestack library that
    // we link in, so we must ensure that this symbol is not internalized (if
    // defined in the crate).
    reachable.push(~"main");
    reachable.push(~"rust_stack_exhausted");
    reachable.push(~"rust_eh_personality"); // referenced from .eh_frame section on some platforms
    reachable.push(~"rust_eh_personality_catch"); // referenced from rt/rust_try.ll

    let metadata_module = ccx.metadata_llmod;

    (ccx.tcx, CrateTranslation {
        context: llcx,
        module: llmod,
        link: link_meta,
        metadata_module: metadata_module,
        metadata: metadata,
        reachable: reachable,
    })
}
