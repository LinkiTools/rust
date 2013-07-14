// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*!
 * Handles translation of callees as well as other call-related
 * things.  Callees are a superset of normal rust values and sometimes
 * have different representations.  In particular, top-level fn items
 * and methods are represented as just a fn ptr and not a full
 * closure.
 */

use std::vec;

use back::abi;
use driver::session;
use lib::llvm::ValueRef;
use lib::llvm::llvm;
use metadata::csearch;
use middle::trans::base;
use middle::trans::base::*;
use middle::trans::build::*;
use middle::trans::callee;
use middle::trans::closure;
use middle::trans::common;
use middle::trans::common::*;
use middle::trans::datum::*;
use middle::trans::datum::Datum;
use middle::trans::expr;
use middle::trans::glue;
use middle::trans::inline;
use middle::trans::meth;
use middle::trans::monomorphize;
use middle::trans::type_of;
use middle::ty;
use middle::subst::Subst;
use middle::typeck;
use middle::typeck::coherence::make_substs_for_receiver_types;
use util::ppaux::Repr;

use middle::trans::type_::Type;

use syntax::ast;
use syntax::ast_map;
use syntax::visit;

// Represents a (possibly monomorphized) top-level fn item or method
// item.  Note that this is just the fn-ptr and is not a Rust closure
// value (which is a pair).
pub struct FnData {
    llfn: ValueRef,
}

pub struct MethodData {
    llfn: ValueRef,
    llself: ValueRef,
    temp_cleanup: Option<ValueRef>,
    self_ty: ty::t,
    self_mode: ty::SelfMode,
}

pub enum CalleeData {
    Closure(Datum),
    Fn(FnData),
    Method(MethodData)
}

pub struct Callee {
    bcx: block,
    data: CalleeData
}

pub fn trans(bcx: block, expr: @ast::expr) -> Callee {
    let _icx = push_ctxt("trans_callee");
    debug!("callee::trans(expr=%s)", expr.repr(bcx.tcx()));

    // pick out special kinds of expressions that can be called:
    match expr.node {
        ast::expr_path(_) => {
            return trans_def(bcx, bcx.def(expr.id), expr);
        }
        _ => {}
    }

    // any other expressions are closures:
    return datum_callee(bcx, expr);

    fn datum_callee(bcx: block, expr: @ast::expr) -> Callee {
        let DatumBlock {bcx, datum} = expr::trans_to_datum(bcx, expr);
        match ty::get(datum.ty).sty {
            ty::ty_bare_fn(*) => {
                let llval = datum.to_appropriate_llval(bcx);
                return Callee {bcx: bcx, data: Fn(FnData {llfn: llval})};
            }
            ty::ty_closure(*) => {
                return Callee {bcx: bcx, data: Closure(datum)};
            }
            _ => {
                bcx.tcx().sess.span_bug(
                    expr.span,
                    fmt!("Type of callee is neither bare-fn nor closure: %s",
                         bcx.ty_to_str(datum.ty)));
            }
        }
    }

    fn fn_callee(bcx: block, fd: FnData) -> Callee {
        return Callee {bcx: bcx, data: Fn(fd)};
    }

    fn trans_def(bcx: block, def: ast::def, ref_expr: @ast::expr) -> Callee {
        match def {
            ast::def_fn(did, _) | ast::def_static_method(did, None, _) => {
                fn_callee(bcx, trans_fn_ref(bcx, did, ref_expr.id))
            }
            ast::def_static_method(impl_did, Some(trait_did), _) => {
                fn_callee(bcx, meth::trans_static_method_callee(bcx, impl_did,
                                                                trait_did,
                                                                ref_expr.id))
            }
            ast::def_variant(tid, vid) => {
                // nullary variants are not callable
                assert!(ty::enum_variant_with_id(bcx.tcx(),
                                                      tid,
                                                      vid).args.len() > 0u);
                fn_callee(bcx, trans_fn_ref(bcx, vid, ref_expr.id))
            }
            ast::def_struct(def_id) => {
                fn_callee(bcx, trans_fn_ref(bcx, def_id, ref_expr.id))
            }
            ast::def_arg(*) |
            ast::def_local(*) |
            ast::def_binding(*) |
            ast::def_upvar(*) |
            ast::def_self(*) => {
                datum_callee(bcx, ref_expr)
            }
            ast::def_mod(*) | ast::def_foreign_mod(*) | ast::def_trait(*) |
            ast::def_static(*) | ast::def_ty(*) | ast::def_prim_ty(*) |
            ast::def_use(*) | ast::def_typaram_binder(*) |
            ast::def_region(*) | ast::def_label(*) | ast::def_ty_param(*) |
            ast::def_self_ty(*) | ast::def_method(*) => {
                bcx.tcx().sess.span_bug(
                    ref_expr.span,
                    fmt!("Cannot translate def %? \
                          to a callable thing!", def));
            }
        }
    }
}

pub fn trans_fn_ref_to_callee(bcx: block,
                              def_id: ast::def_id,
                              ref_id: ast::node_id) -> Callee {
    Callee {bcx: bcx,
            data: Fn(trans_fn_ref(bcx, def_id, ref_id))}
}

pub fn trans_fn_ref(bcx: block,
                    def_id: ast::def_id,
                    ref_id: ast::node_id) -> FnData {
    /*!
     *
     * Translates a reference (with id `ref_id`) to the fn/method
     * with id `def_id` into a function pointer.  This may require
     * monomorphization or inlining. */

    let _icx = push_ctxt("trans_fn_ref");

    let type_params = node_id_type_params(bcx, ref_id);
    let vtables = node_vtables(bcx, ref_id);
    debug!("trans_fn_ref(def_id=%s, ref_id=%?, type_params=%s, vtables=%s)",
           def_id.repr(bcx.tcx()), ref_id, type_params.repr(bcx.tcx()),
           vtables.repr(bcx.tcx()));
    trans_fn_ref_with_vtables(bcx, def_id, ref_id, type_params, vtables)
}

pub fn trans_fn_ref_with_vtables_to_callee(
        bcx: block,
        def_id: ast::def_id,
        ref_id: ast::node_id,
        type_params: &[ty::t],
        vtables: Option<typeck::vtable_res>)
     -> Callee {
    Callee {bcx: bcx,
            data: Fn(trans_fn_ref_with_vtables(bcx, def_id, ref_id,
                                               type_params, vtables))}
}

fn get_impl_resolutions(bcx: block,
                        impl_id: ast::def_id)
                         -> typeck::vtable_res {
    if impl_id.crate == ast::local_crate {
        bcx.ccx().maps.vtable_map.get_copy(&impl_id.node)
    } else {
        // XXX: This is a temporary hack to work around not properly
        // exporting information about resolutions for impls.
        // This doesn't actually work if the trait has param bounds,
        // but it does allow us to survive the case when it does not.
        let trait_ref = ty::impl_trait_ref(bcx.tcx(), impl_id).get();
        @vec::from_elem(trait_ref.substs.tps.len(), @~[])
    }
}

fn resolve_default_method_vtables(bcx: block,
                                  impl_id: ast::def_id,
                                  method: &ty::Method,
                                  substs: &ty::substs,
                                  impl_vtables: Option<typeck::vtable_res>)
                                 -> typeck::vtable_res {

    // Get the vtables that the impl implements the trait at
    let trait_vtables = get_impl_resolutions(bcx, impl_id);

    // Build up a param_substs that we are going to resolve the
    // trait_vtables under.
    let param_substs = Some(@param_substs {
        tys: copy substs.tps,
        self_ty: substs.self_ty,
        vtables: impl_vtables,
        self_vtable: None
    });

    let trait_vtables_fixed = resolve_vtables_under_param_substs(
        bcx.tcx(), param_substs, trait_vtables);

    // Now we pull any vtables for parameters on the actual method.
    let num_method_vtables = method.generics.type_param_defs.len();
    let method_vtables = match impl_vtables {
        Some(vtables) => {
            let num_impl_type_parameters =
                vtables.len() - num_method_vtables;
            vtables.tailn(num_impl_type_parameters).to_owned()
        },
        None => vec::from_elem(num_method_vtables, @~[])
    };

    @(*trait_vtables_fixed + method_vtables)
}


pub fn trans_fn_ref_with_vtables(
        bcx: block,            //
        def_id: ast::def_id,   // def id of fn
        ref_id: ast::node_id,  // node id of use of fn; may be zero if N/A
        type_params: &[ty::t], // values for fn's ty params
        vtables: Option<typeck::vtable_res>) // vtables for the call
     -> FnData {
    //!
    //
    // Translates a reference to a fn/method item, monomorphizing and
    // inlining as it goes.
    //
    // # Parameters
    //
    // - `bcx`: the current block where the reference to the fn occurs
    // - `def_id`: def id of the fn or method item being referenced
    // - `ref_id`: node id of the reference to the fn/method, if applicable.
    //   This parameter may be zero; but, if so, the resulting value may not
    //   have the right type, so it must be cast before being used.
    // - `type_params`: values for each of the fn/method's type parameters
    // - `vtables`: values for each bound on each of the type parameters

    let _icx = push_ctxt("trans_fn_ref_with_vtables");
    let ccx = bcx.ccx();
    let tcx = ccx.tcx;

    debug!("trans_fn_ref_with_vtables(bcx=%s, def_id=%s, ref_id=%?, \
            type_params=%s, vtables=%s)",
           bcx.to_str(),
           def_id.repr(bcx.tcx()),
           ref_id,
           type_params.repr(bcx.tcx()),
           vtables.repr(bcx.tcx()));

    assert!(type_params.iter().all(|t| !ty::type_needs_infer(*t)));

    // Polytype of the function item (may have type params)
    let fn_tpt = ty::lookup_item_type(tcx, def_id);

    // For simplicity, we want to use the Subst trait when composing
    // substitutions for default methods.  The subst trait does
    // substitutions with regions, though, so we put a dummy self
    // region parameter in to keep it from failing. This is a hack.
    let substs = ty::substs { self_r: Some(ty::re_empty),
                              self_ty: None,
                              tps: /*bad*/ type_params.to_owned() };


    // We need to do a bunch of special handling for default methods.
    // We need to modify the def_id and our substs in order to monomorphize
    // the function.
    let (def_id, opt_impl_did, substs, self_vtable, vtables) =
        match tcx.provided_method_sources.find(&def_id) {
        None => (def_id, None, substs, None, vtables),
        Some(source) => {
            // There are two relevant substitutions when compiling
            // default methods. First, there is the substitution for
            // the type parameters of the impl we are using and the
            // method we are calling. This substitution is the substs
            // argument we already have.
            // In order to compile a default method, though, we need
            // to consider another substitution: the substitution for
            // the type parameters on trait; the impl we are using
            // implements the trait at some particular type
            // parameters, and we need to substitute for those first.
            // So, what we need to do is find this substitution and
            // compose it with the one we already have.

            let trait_ref = ty::impl_trait_ref(tcx, source.impl_id)
                .expect("could not find trait_ref for impl with \
                         default methods");
            let method = ty::method(tcx, source.method_id);

            // Get all of the type params for the receiver
            let param_defs = method.generics.type_param_defs;
            let receiver_substs =
                type_params.initn(param_defs.len()).to_owned();
            let receiver_vtables = match vtables {
                None => @~[],
                Some(call_vtables) => {
                    @call_vtables.initn(param_defs.len()).to_owned()
                }
            };

            let self_vtable =
                typeck::vtable_static(source.impl_id, receiver_substs,
                                      receiver_vtables);
            // Compute the first substitution
            let first_subst = make_substs_for_receiver_types(
                tcx, source.impl_id, trait_ref, method);

            // And compose them
            let new_substs = first_subst.subst(tcx, &substs);


            let vtables =
                resolve_default_method_vtables(bcx, source.impl_id,
                                               method, &new_substs, vtables);

            debug!("trans_fn_with_vtables - default method: \
                    substs = %s, trait_subst = %s, \
                    first_subst = %s, new_subst = %s, \
                    self_vtable = %s, vtables = %s",
                   substs.repr(tcx), trait_ref.substs.repr(tcx),
                   first_subst.repr(tcx), new_substs.repr(tcx),
                   self_vtable.repr(tcx), vtables.repr(tcx));

            (source.method_id, Some(source.impl_id),
             new_substs, Some(self_vtable), Some(vtables))
        }
    };

    // Check whether this fn has an inlined copy and, if so, redirect
    // def_id to the local id of the inlined copy.
    let def_id = {
        if def_id.crate != ast::local_crate {
            inline::maybe_instantiate_inline(ccx, def_id)
        } else {
            def_id
        }
    };

    // We must monomorphise if the fn has type parameters, is a rust
    // intrinsic, or is a default method.  In particular, if we see an
    // intrinsic that is inlined from a different crate, we want to reemit the
    // intrinsic instead of trying to call it in the other crate.
    let must_monomorphise;
    if type_params.len() > 0 || opt_impl_did.is_some() {
        must_monomorphise = true;
    } else if def_id.crate == ast::local_crate {
        let map_node = session::expect(
            ccx.sess,
            ccx.tcx.items.find(&def_id.node),
            || fmt!("local item should be in ast map"));

        match *map_node {
            ast_map::node_foreign_item(_, abis, _, _) => {
                must_monomorphise = abis.is_intrinsic()
            }
            _ => {
                must_monomorphise = false;
            }
        }
    } else {
        must_monomorphise = false;
    }

    // Create a monomorphic verison of generic functions
    if must_monomorphise {
        // Should be either intra-crate or inlined.
        assert_eq!(def_id.crate, ast::local_crate);

        let (val, must_cast) =
            monomorphize::monomorphic_fn(ccx, def_id, &substs,
                                         vtables, self_vtable,
                                         opt_impl_did, Some(ref_id));
        let mut val = val;
        if must_cast && ref_id != 0 {
            // Monotype of the REFERENCE to the function (type params
            // are subst'd)
            let ref_ty = common::node_id_type(bcx, ref_id);

            val = PointerCast(
                bcx, val, type_of::type_of_fn_from_ty(ccx, ref_ty).ptr_to());
        }
        return FnData {llfn: val};
    }

    // Find the actual function pointer.
    let val = {
        if def_id.crate == ast::local_crate {
            // Internal reference.
            get_item_val(ccx, def_id.node)
        } else {
            // External reference.
            trans_external_path(ccx, def_id, fn_tpt.ty)
        }
    };

    return FnData {llfn: val};
}

// ______________________________________________________________________
// Translating calls

pub fn trans_call(in_cx: block,
                  call_ex: @ast::expr,
                  f: @ast::expr,
                  args: CallArgs,
                  id: ast::node_id,
                  dest: expr::Dest)
                  -> block {
    let _icx = push_ctxt("trans_call");
    trans_call_inner(in_cx,
                     call_ex.info(),
                     expr_ty(in_cx, f),
                     node_id_type(in_cx, id),
                     |cx| trans(cx, f),
                     args,
                     Some(dest),
                     DontAutorefArg).bcx
}

pub fn trans_method_call(in_cx: block,
                         call_ex: @ast::expr,
                         callee_id: ast::node_id,
                         rcvr: @ast::expr,
                         args: CallArgs,
                         dest: expr::Dest)
                         -> block {
    let _icx = push_ctxt("trans_method_call");
    debug!("trans_method_call(call_ex=%s, rcvr=%s)",
           call_ex.repr(in_cx.tcx()),
           rcvr.repr(in_cx.tcx()));
    trans_call_inner(
        in_cx,
        call_ex.info(),
        node_id_type(in_cx, callee_id),
        expr_ty(in_cx, call_ex),
        |cx| {
            match cx.ccx().maps.method_map.find_copy(&call_ex.id) {
                Some(origin) => {
                    debug!("origin for %s: %s",
                           call_ex.repr(in_cx.tcx()),
                           origin.repr(in_cx.tcx()));

                    meth::trans_method_callee(cx,
                                              callee_id,
                                              rcvr,
                                              origin)
                }
                None => {
                    cx.tcx().sess.span_bug(call_ex.span, "method call expr wasn't in method map")
                }
            }
        },
        args,
        Some(dest),
        DontAutorefArg).bcx
}

pub fn trans_lang_call(bcx: block,
                       did: ast::def_id,
                       args: &[ValueRef],
                       dest: Option<expr::Dest>)
    -> Result {
    let fty = if did.crate == ast::local_crate {
        ty::node_id_to_type(bcx.ccx().tcx, did.node)
    } else {
        csearch::get_type(bcx.ccx().tcx, did).ty
    };
    let rty = ty::ty_fn_ret(fty);
    callee::trans_call_inner(bcx,
                             None,
                             fty,
                             rty,
                             |bcx| {
                                trans_fn_ref_with_vtables_to_callee(bcx,
                                                                    did,
                                                                    0,
                                                                    [],
                                                                    None)
                             },
                             ArgVals(args),
                             dest,
                             DontAutorefArg)
}

pub fn trans_lang_call_with_type_params(bcx: block,
                                        did: ast::def_id,
                                        args: &[ValueRef],
                                        type_params: &[ty::t],
                                        dest: expr::Dest)
    -> block {
    let fty;
    if did.crate == ast::local_crate {
        fty = ty::node_id_to_type(bcx.tcx(), did.node);
    } else {
        fty = csearch::get_type(bcx.tcx(), did).ty;
    }

    let rty = ty::ty_fn_ret(fty);
    return callee::trans_call_inner(
        bcx, None, fty, rty,
        |bcx| {
            let callee =
                trans_fn_ref_with_vtables_to_callee(bcx, did, 0,
                                                    type_params,
                                                    None);

            let new_llval;
            match callee.data {
                Fn(fn_data) => {
                    let substituted = ty::subst_tps(callee.bcx.tcx(),
                                                    type_params,
                                                    None,
                                                    fty);
                    let llfnty = type_of::type_of(callee.bcx.ccx(),
                                                      substituted);
                    new_llval = PointerCast(callee.bcx, fn_data.llfn, llfnty);
                }
                _ => fail!()
            }
            Callee { bcx: callee.bcx, data: Fn(FnData { llfn: new_llval }) }
        },
        ArgVals(args), Some(dest), DontAutorefArg).bcx;
}

pub fn body_contains_ret(body: &ast::blk) -> bool {
    let cx = @mut false;
    visit::visit_block(body, (cx, visit::mk_vt(@visit::Visitor {
        visit_item: |_i, (_cx, _v)| { },
        visit_expr: |e: @ast::expr, (cx, v): (@mut bool, visit::vt<@mut bool>)| {
            if !*cx {
                match e.node {
                  ast::expr_ret(_) => *cx = true,
                  _ => visit::visit_expr(e, (cx, v)),
                }
            }
        },
        ..*visit::default_visitor()
    })));
    *cx
}

// See [Note-arg-mode]
pub fn trans_call_inner(in_cx: block,
                        call_info: Option<NodeInfo>,
                        fn_expr_ty: ty::t,
                        ret_ty: ty::t,
                        get_callee: &fn(block) -> Callee,
                        args: CallArgs,
                        dest: Option<expr::Dest>,
                        autoref_arg: AutorefArg)
                        -> Result {
    do base::with_scope_result(in_cx, call_info, "call") |cx| {
        let ret_in_loop = match args {
          ArgExprs(args) => {
            args.len() > 0u && match args.last().node {
              ast::expr_loop_body(@ast::expr {
                node: ast::expr_fn_block(_, ref body),
                _
              }) =>  body_contains_ret(body),
              _ => false
            }
          }
          _ => false
        };

        let callee = get_callee(cx);
        let mut bcx = callee.bcx;
        let ccx = cx.ccx();
        let ret_flag = if ret_in_loop {
            let flag = alloca(bcx, Type::bool(), "__ret_flag");
            Store(bcx, C_bool(false), flag);
            Some(flag)
        } else {
            None
        };

        let (llfn, llenv) = unsafe {
            match callee.data {
                Fn(d) => {
                    (d.llfn, llvm::LLVMGetUndef(Type::opaque_box(ccx).ptr_to().to_ref()))
                }
                Method(d) => {
                    // Weird but true: we pass self in the *environment* slot!
                    (d.llfn, d.llself)
                }
                Closure(d) => {
                    // Closures are represented as (llfn, llclosure) pair:
                    // load the requisite values out.
                    let pair = d.to_ref_llval(bcx);
                    let llfn = GEPi(bcx, pair, [0u, abi::fn_field_code]);
                    let llfn = Load(bcx, llfn);
                    let llenv = GEPi(bcx, pair, [0u, abi::fn_field_box]);
                    let llenv = Load(bcx, llenv);
                    (llfn, llenv)
                }
            }
        };

        let llretslot = trans_ret_slot(bcx, fn_expr_ty, dest);

        let mut llargs = ~[];

        if !ty::type_is_immediate(bcx.tcx(), ret_ty) {
            llargs.push(llretslot);
        }

        llargs.push(llenv);
        bcx = trans_args(bcx, args, fn_expr_ty,
                         ret_flag, autoref_arg, &mut llargs);


        // Now that the arguments have finished evaluating, we need to revoke
        // the cleanup for the self argument
        match callee.data {
            Method(d) => {
                for d.temp_cleanup.iter().advance |&v| {
                    revoke_clean(bcx, v);
                }
            }
            _ => {}
        }

        // Uncomment this to debug calls.
        /*
        io::println(fmt!("calling: %s", bcx.val_to_str(llfn)));
        for llargs.iter().advance |llarg| {
            io::println(fmt!("arg: %s", bcx.val_to_str(*llarg)));
        }
        io::println("---");
        */

        // If the block is terminated, then one or more of the args
        // has type _|_. Since that means it diverges, the code for
        // the call itself is unreachable.
        let (llresult, new_bcx) = base::invoke(bcx, llfn, llargs);
        bcx = new_bcx;

        match dest {
            None => { assert!(ty::type_is_immediate(bcx.tcx(), ret_ty)) }
            Some(expr::Ignore) => {
                // drop the value if it is not being saved.
                if ty::type_needs_drop(bcx.tcx(), ret_ty) {
                    if ty::type_is_immediate(bcx.tcx(), ret_ty) {
                        let llscratchptr = alloc_ty(bcx, ret_ty, "__ret");
                        Store(bcx, llresult, llscratchptr);
                        bcx = glue::drop_ty(bcx, llscratchptr, ret_ty);
                    } else {
                        bcx = glue::drop_ty(bcx, llretslot, ret_ty);
                    }
                }
            }
            Some(expr::SaveIn(lldest)) => {
                // If this is an immediate, store into the result location.
                // (If this was not an immediate, the result will already be
                // directly written into the output slot.)
                if ty::type_is_immediate(bcx.tcx(), ret_ty) {
                    Store(bcx, llresult, lldest);
                }
            }
        }

        if ty::type_is_bot(ret_ty) {
            Unreachable(bcx);
        } else if ret_in_loop {
            let ret_flag_result = bool_to_i1(bcx, Load(bcx, ret_flag.get()));
            bcx = do with_cond(bcx, ret_flag_result) |bcx| {
                {
                    let r = (copy bcx.fcx.loop_ret);
                    for r.iter().advance |&(flagptr, _)| {
                        Store(bcx, C_bool(true), flagptr);
                        Store(bcx, C_bool(false), bcx.fcx.llretptr.get());
                    }
                }
                base::cleanup_and_leave(bcx, None, Some(bcx.fcx.get_llreturn()));
                Unreachable(bcx);
                bcx
            }
        }
        rslt(bcx, llresult)
    }
}


pub enum CallArgs<'self> {
    ArgExprs(&'self [@ast::expr]),
    ArgVals(&'self [ValueRef])
}

pub fn trans_ret_slot(bcx: block, fn_ty: ty::t, dest: Option<expr::Dest>)
                      -> ValueRef {
    let retty = ty::ty_fn_ret(fn_ty);

    match dest {
        Some(expr::SaveIn(dst)) => dst,
        _ => {
            if ty::type_is_immediate(bcx.tcx(), retty) {
                unsafe {
                    llvm::LLVMGetUndef(Type::nil().ptr_to().to_ref())
                }
            } else {
                alloc_ty(bcx, retty, "__trans_ret_slot")
            }
        }
    }
}

pub fn trans_args(cx: block,
                  args: CallArgs,
                  fn_ty: ty::t,
                  ret_flag: Option<ValueRef>,
                  autoref_arg: AutorefArg,
                  llargs: &mut ~[ValueRef]) -> block
{
    let _icx = push_ctxt("trans_args");
    let mut temp_cleanups = ~[];
    let arg_tys = ty::ty_fn_args(fn_ty);

    let mut bcx = cx;

    // First we figure out the caller's view of the types of the arguments.
    // This will be needed if this is a generic call, because the callee has
    // to cast her view of the arguments to the caller's view.
    match args {
      ArgExprs(arg_exprs) => {
        let last = arg_exprs.len() - 1u;
        for arg_exprs.iter().enumerate().advance |(i, arg_expr)| {
            let arg_val = unpack_result!(bcx, {
                trans_arg_expr(bcx,
                               arg_tys[i],
                               ty::ByCopy,
                               *arg_expr,
                               &mut temp_cleanups,
                               if i == last { ret_flag } else { None },
                               autoref_arg)
            });
            llargs.push(arg_val);
        }
      }
      ArgVals(vs) => {
        llargs.push_all(vs);
      }
    }

    // now that all arguments have been successfully built, we can revoke any
    // temporary cleanups, as they are only needed if argument construction
    // should fail (for example, cleanup of copy mode args).
    for temp_cleanups.iter().advance |c| {
        revoke_clean(bcx, *c)
    }

    bcx
}

pub enum AutorefArg {
    DontAutorefArg,
    DoAutorefArg
}

// temp_cleanups: cleanups that should run only if failure occurs before the
// call takes place:
pub fn trans_arg_expr(bcx: block,
                      formal_arg_ty: ty::t,
                      self_mode: ty::SelfMode,
                      arg_expr: @ast::expr,
                      temp_cleanups: &mut ~[ValueRef],
                      ret_flag: Option<ValueRef>,
                      autoref_arg: AutorefArg) -> Result {
    let _icx = push_ctxt("trans_arg_expr");
    let ccx = bcx.ccx();

    debug!("trans_arg_expr(formal_arg_ty=(%s), self_mode=%?, arg_expr=%s, \
            ret_flag=%?)",
           formal_arg_ty.repr(bcx.tcx()),
           self_mode,
           arg_expr.repr(bcx.tcx()),
           ret_flag.map(|v| bcx.val_to_str(*v)));

    // translate the arg expr to a datum
    let arg_datumblock = match ret_flag {
        None => expr::trans_to_datum(bcx, arg_expr),

        // If there is a ret_flag, this *must* be a loop body
        Some(_) => {
            match arg_expr.node {
                ast::expr_loop_body(
                    blk @ @ast::expr {
                        node: ast::expr_fn_block(ref decl, ref body),
                        _
                    }) => {
                    let scratch_ty = expr_ty(bcx, arg_expr);
                    let scratch = alloc_ty(bcx, scratch_ty, "__ret_flag");
                    let arg_ty = expr_ty(bcx, arg_expr);
                    let sigil = ty::ty_closure_sigil(arg_ty);
                    let bcx = closure::trans_expr_fn(
                        bcx, sigil, decl, body, arg_expr.id,
                        blk.id, Some(ret_flag), expr::SaveIn(scratch));
                    DatumBlock {bcx: bcx,
                                datum: Datum {val: scratch,
                                              ty: scratch_ty,
                                              mode: ByRef(RevokeClean)}}
                }
                _ => {
                    bcx.sess().impossible_case(
                        arg_expr.span, "ret_flag with non-loop-body expr");
                }
            }
        }
    };
    let arg_datum = arg_datumblock.datum;
    let bcx = arg_datumblock.bcx;

    debug!("   arg datum: %s", arg_datum.to_str(bcx.ccx()));

    let mut val;
    if ty::type_is_bot(arg_datum.ty) {
        // For values of type _|_, we generate an
        // "undef" value, as such a value should never
        // be inspected. It's important for the value
        // to have type lldestty (the callee's expected type).
        let llformal_arg_ty = type_of::type_of(ccx, formal_arg_ty);
        unsafe {
            val = llvm::LLVMGetUndef(llformal_arg_ty.to_ref());
        }
    } else {
        // FIXME(#3548) use the adjustments table
        match autoref_arg {
            DoAutorefArg => {
                val = arg_datum.to_ref_llval(bcx);
            }
            DontAutorefArg => {
                match self_mode {
                    ty::ByRef => {
                        // This assertion should really be valid, but because
                        // the explicit self code currently passes by-ref, it
                        // does not hold.
                        //
                        //assert !bcx.ccx().maps.moves_map.contains_key(
                        //    &arg_expr.id);
                        debug!("by ref arg with type %s, storing to scratch",
                               bcx.ty_to_str(arg_datum.ty));
                        let scratch = scratch_datum(bcx, arg_datum.ty,
                                                    "__self", false);

                        arg_datum.store_to_datum(bcx,
                                                 INIT,
                                                 scratch);

                        // Technically, ownership of val passes to the callee.
                        // However, we must cleanup should we fail before the
                        // callee is actually invoked.
                        scratch.add_clean(bcx);
                        temp_cleanups.push(scratch.val);

                        val = scratch.to_ref_llval(bcx);
                    }
                    ty::ByCopy => {
                        if ty::type_needs_drop(bcx.tcx(), arg_datum.ty) ||
                                arg_datum.appropriate_mode(bcx.tcx()).is_by_ref() {
                            debug!("by copy arg with type %s, storing to scratch",
                                   bcx.ty_to_str(arg_datum.ty));
                            let scratch = scratch_datum(bcx, arg_datum.ty,
                                                        "__arg", false);

                            arg_datum.store_to_datum(bcx,
                                                     INIT,
                                                     scratch);

                            // Technically, ownership of val passes to the callee.
                            // However, we must cleanup should we fail before the
                            // callee is actually invoked.
                            scratch.add_clean(bcx);
                            temp_cleanups.push(scratch.val);

                            match scratch.appropriate_mode(bcx.tcx()) {
                                ByValue => val = Load(bcx, scratch.val),
                                ByRef(_) => val = scratch.val,
                            }
                        } else {
                            debug!("by copy arg with type %s", bcx.ty_to_str(arg_datum.ty));
                            match arg_datum.mode {
                                ByRef(_) => val = Load(bcx, arg_datum.val),
                                ByValue => val = arg_datum.val,
                            }
                        }
                    }
                }
            }
        }

        if formal_arg_ty != arg_datum.ty {
            // this could happen due to e.g. subtyping
            let llformal_arg_ty = type_of::type_of_explicit_arg(ccx, &formal_arg_ty);
            debug!("casting actual type (%s) to match formal (%s)",
                   bcx.val_to_str(val), bcx.llty_str(llformal_arg_ty));
            val = PointerCast(bcx, val, llformal_arg_ty);
        }
    }

    debug!("--- trans_arg_expr passing %s", bcx.val_to_str(val));
    return rslt(bcx, val);
}
