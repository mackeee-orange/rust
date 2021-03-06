// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Lowers the AST to the HIR.
//!
//! Since the AST and HIR are fairly similar, this is mostly a simple procedure,
//! much like a fold. Where lowering involves a bit more work things get more
//! interesting and there are some invariants you should know about. These mostly
//! concern spans and ids.
//!
//! Spans are assigned to AST nodes during parsing and then are modified during
//! expansion to indicate the origin of a node and the process it went through
//! being expanded. Ids are assigned to AST nodes just before lowering.
//!
//! For the simpler lowering steps, ids and spans should be preserved. Unlike
//! expansion we do not preserve the process of lowering in the spans, so spans
//! should not be modified here. When creating a new node (as opposed to
//! 'folding' an existing one), then you create a new id using `next_id()`.
//!
//! You must ensure that ids are unique. That means that you should only use the
//! id from an AST node in a single HIR node (you can assume that AST node ids
//! are unique). Every new node must have a unique id. Avoid cloning HIR nodes.
//! If you do, you must then set the new node's id to a fresh one.
//!
//! Spans are used for error messages and for tools to map semantics back to
//! source code. It is therefore not as important with spans as ids to be strict
//! about use (you can't break the compiler by screwing up a span). Obviously, a
//! HIR node can only have a single span. But multiple nodes can have the same
//! span and spans don't need to be kept in order, etc. Where code is preserved
//! by lowering, it should have the same span as in the AST. Where HIR nodes are
//! new it is probably best to give a span for the whole AST node being lowered.
//! All nodes should have real spans, don't use dummy spans. Tools are likely to
//! get confused if the spans from leaf AST nodes occur in multiple places
//! in the HIR, especially for multiple identifiers.

use dep_graph::DepGraph;
use hir;
use hir::HirVec;
use hir::map::{Definitions, DefKey, DefPathData};
use hir::def_id::{DefIndex, DefId, CRATE_DEF_INDEX, DefIndexAddressSpace};
use hir::def::{Def, PathResolution};
use lint::builtin::PARENTHESIZED_PARAMS_IN_TYPES_AND_MODULES;
use middle::cstore::CrateStore;
use rustc_data_structures::indexed_vec::IndexVec;
use session::Session;
use util::common::FN_OUTPUT_NAME;
use util::nodemap::{DefIdMap, FxHashMap, NodeMap};

use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;
use std::iter;
use std::mem;
use syntax::attr;
use syntax::ast::*;
use syntax::errors;
use syntax::ext::hygiene::{Mark, SyntaxContext};
use syntax::ptr::P;
use syntax::codemap::{self, respan, Spanned, CompilerDesugaringKind};
use syntax::std_inject;
use syntax::symbol::{Symbol, keywords};
use syntax::tokenstream::{TokenStream, TokenTree, Delimited};
use syntax::parse::token::Token;
use syntax::util::small_vector::SmallVector;
use syntax::visit::{self, Visitor};
use syntax_pos::Span;

const HIR_ID_COUNTER_LOCKED: u32 = 0xFFFFFFFF;

pub struct LoweringContext<'a> {
    crate_root: Option<&'static str>,

    // Use to assign ids to hir nodes that do not directly correspond to an ast node
    sess: &'a Session,

    cstore: &'a CrateStore,

    // As we walk the AST we must keep track of the current 'parent' def id (in
    // the form of a DefIndex) so that if we create a new node which introduces
    // a definition, then we can properly create the def id.
    parent_def: Option<DefIndex>,
    resolver: &'a mut Resolver,
    name_map: FxHashMap<Ident, Name>,

    /// The items being lowered are collected here.
    items: BTreeMap<NodeId, hir::Item>,

    trait_items: BTreeMap<hir::TraitItemId, hir::TraitItem>,
    impl_items: BTreeMap<hir::ImplItemId, hir::ImplItem>,
    bodies: BTreeMap<hir::BodyId, hir::Body>,
    exported_macros: Vec<hir::MacroDef>,

    trait_impls: BTreeMap<DefId, Vec<NodeId>>,
    trait_auto_impl: BTreeMap<DefId, NodeId>,

    is_generator: bool,

    catch_scopes: Vec<NodeId>,
    loop_scopes: Vec<NodeId>,
    is_in_loop_condition: bool,
    is_in_trait_impl: bool,

    // Used to create lifetime definitions from in-band lifetime usages.
    // e.g. `fn foo(x: &'x u8) -> &'x u8` to `fn foo<'x>(x: &'x u8) -> &'x u8`
    // When a named lifetime is encountered in a function or impl header and
    // has not been defined
    // (i.e. it doesn't appear in the in_scope_lifetimes list), it is added
    // to this list. The results of this list are then added to the list of
    // lifetime definitions in the corresponding impl or function generics.
    lifetimes_to_define: Vec<(Span, Name)>,
    // Whether or not in-band lifetimes are being collected. This is used to
    // indicate whether or not we're in a place where new lifetimes will result
    // in in-band lifetime definitions, such a function or an impl header.
    // This will always be false unless the `in_band_lifetimes` feature is
    // enabled.
    is_collecting_in_band_lifetimes: bool,
    // Currently in-scope lifetimes defined in impl headers, fn headers, or HRTB.
    // When `is_collectin_in_band_lifetimes` is true, each lifetime is checked
    // against this list to see if it is already in-scope, or if a definition
    // needs to be created for it.
    in_scope_lifetimes: Vec<Name>,

    type_def_lifetime_params: DefIdMap<usize>,

    current_hir_id_owner: Vec<(DefIndex, u32)>,
    item_local_id_counters: NodeMap<u32>,
    node_id_to_hir_id: IndexVec<NodeId, hir::HirId>,
}

pub trait Resolver {
    /// Resolve a hir path generated by the lowerer when expanding `for`, `if let`, etc.
    fn resolve_hir_path(&mut self, path: &mut hir::Path, is_value: bool);

    /// Obtain the resolution for a node id
    fn get_resolution(&mut self, id: NodeId) -> Option<PathResolution>;

    /// We must keep the set of definitions up to date as we add nodes that weren't in the AST.
    /// This should only return `None` during testing.
    fn definitions(&mut self) -> &mut Definitions;
}

#[derive(Clone, Copy, Debug)]
enum ImplTraitContext {
    /// Treat `impl Trait` as shorthand for a new universal generic parameter.
    /// Example: `fn foo(x: impl Debug)`, where `impl Debug` is conceptually
    /// equivalent to a fresh universal parameter like `fn foo<T: Debug>(x: T)`.
    ///
    /// We store a DefId here so we can look up necessary information later
    Universal(DefId),

    /// Treat `impl Trait` as shorthand for a new universal existential parameter.
    /// Example: `fn foo() -> impl Debug`, where `impl Debug` is conceptually
    /// equivalent to a fresh existential parameter like `abstract type T; fn foo() -> T`.
    Existential,

    /// `impl Trait` is not accepted in this position.
    Disallowed,
}

pub fn lower_crate(sess: &Session,
                   cstore: &CrateStore,
                   dep_graph: &DepGraph,
                   krate: &Crate,
                   resolver: &mut Resolver)
                   -> hir::Crate {
    // We're constructing the HIR here; we don't care what we will
    // read, since we haven't even constructed the *input* to
    // incr. comp. yet.
    let _ignore = dep_graph.in_ignore();

    LoweringContext {
        crate_root: std_inject::injected_crate_name(),
        sess,
        cstore,
        parent_def: None,
        resolver,
        name_map: FxHashMap(),
        items: BTreeMap::new(),
        trait_items: BTreeMap::new(),
        impl_items: BTreeMap::new(),
        bodies: BTreeMap::new(),
        trait_impls: BTreeMap::new(),
        trait_auto_impl: BTreeMap::new(),
        exported_macros: Vec::new(),
        catch_scopes: Vec::new(),
        loop_scopes: Vec::new(),
        is_in_loop_condition: false,
        type_def_lifetime_params: DefIdMap(),
        current_hir_id_owner: vec![(CRATE_DEF_INDEX, 0)],
        item_local_id_counters: NodeMap(),
        node_id_to_hir_id: IndexVec::new(),
        is_generator: false,
        is_in_trait_impl: false,
        lifetimes_to_define: Vec::new(),
        is_collecting_in_band_lifetimes: false,
        in_scope_lifetimes: Vec::new(),
    }.lower_crate(krate)
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ParamMode {
    /// Any path in a type context.
    Explicit,
    /// The `module::Type` in `module::Type::method` in an expression.
    Optional
}

struct LoweredNodeId {
    node_id: NodeId,
    hir_id: hir::HirId,
}

enum ParenthesizedGenericArgs {
    Ok,
    Warn,
    Err,
}

impl<'a> LoweringContext<'a> {
    fn lower_crate(mut self, c: &Crate) -> hir::Crate {
        /// Full-crate AST visitor that inserts into a fresh
        /// `LoweringContext` any information that may be
        /// needed from arbitrary locations in the crate.
        /// E.g. The number of lifetime generic parameters
        /// declared for every type and trait definition.
        struct MiscCollector<'lcx, 'interner: 'lcx> {
            lctx: &'lcx mut LoweringContext<'interner>,
        }

        impl<'lcx, 'interner> Visitor<'lcx> for MiscCollector<'lcx, 'interner> {
            fn visit_item(&mut self, item: &'lcx Item) {
                self.lctx.allocate_hir_id_counter(item.id, item);

                match item.node {
                    ItemKind::Struct(_, ref generics) |
                    ItemKind::Union(_, ref generics) |
                    ItemKind::Enum(_, ref generics) |
                    ItemKind::Ty(_, ref generics) |
                    ItemKind::Trait(_, _, ref generics, ..) => {
                        let def_id = self.lctx.resolver.definitions().local_def_id(item.id);
                        let count = generics.lifetimes.len();
                        self.lctx.type_def_lifetime_params.insert(def_id, count);
                    }
                    _ => {}
                }
                visit::walk_item(self, item);
            }

            fn visit_trait_item(&mut self, item: &'lcx TraitItem) {
                self.lctx.allocate_hir_id_counter(item.id, item);
                visit::walk_trait_item(self, item);
            }

            fn visit_impl_item(&mut self, item: &'lcx ImplItem) {
                self.lctx.allocate_hir_id_counter(item.id, item);
                visit::walk_impl_item(self, item);
            }
        }

        struct ItemLowerer<'lcx, 'interner: 'lcx> {
            lctx: &'lcx mut LoweringContext<'interner>,
        }

        impl<'lcx, 'interner> ItemLowerer<'lcx, 'interner> {
            fn with_trait_impl_ref<F>(&mut self, trait_impl_ref: &Option<TraitRef>, f: F)
                where F: FnOnce(&mut Self)
            {
                let old = self.lctx.is_in_trait_impl;
                self.lctx.is_in_trait_impl = if let &None = trait_impl_ref {
                    false
                } else {
                    true
                };
                f(self);
                self.lctx.is_in_trait_impl = old;
            }
        }

        impl<'lcx, 'interner> Visitor<'lcx> for ItemLowerer<'lcx, 'interner> {
            fn visit_item(&mut self, item: &'lcx Item) {
                let mut item_lowered = true;
                self.lctx.with_hir_id_owner(item.id, |lctx| {
                    if let Some(hir_item) = lctx.lower_item(item) {
                        lctx.items.insert(item.id, hir_item);
                    } else {
                        item_lowered = false;
                    }
                });

                if item_lowered {
                    let item_lifetimes = match self.lctx.items.get(&item.id).unwrap().node {
                        hir::Item_::ItemImpl(_,_,_,ref generics,..) |
                        hir::Item_::ItemTrait(_,_,ref generics,..) =>
                            generics.lifetimes.clone(),
                        _ => Vec::new().into(),
                    };

                    self.lctx.with_parent_impl_lifetime_defs(&item_lifetimes, |this| {
                        let this = &mut ItemLowerer { lctx: this };
                        if let ItemKind::Impl(_,_,_,_,ref opt_trait_ref,_,_) = item.node {
                            this.with_trait_impl_ref(opt_trait_ref, |this| {
                                visit::walk_item(this, item)
                            });
                        } else {
                            visit::walk_item(this, item);
                        }
                    });
                }
            }

            fn visit_trait_item(&mut self, item: &'lcx TraitItem) {
                self.lctx.with_hir_id_owner(item.id, |lctx| {
                    let id = hir::TraitItemId { node_id: item.id };
                    let hir_item = lctx.lower_trait_item(item);
                    lctx.trait_items.insert(id, hir_item);
                });

                visit::walk_trait_item(self, item);
            }

            fn visit_impl_item(&mut self, item: &'lcx ImplItem) {
                self.lctx.with_hir_id_owner(item.id, |lctx| {
                    let id = hir::ImplItemId { node_id: item.id };
                    let hir_item = lctx.lower_impl_item(item);
                    lctx.impl_items.insert(id, hir_item);
                });
                visit::walk_impl_item(self, item);
            }
        }

        self.lower_node_id(CRATE_NODE_ID);
        debug_assert!(self.node_id_to_hir_id[CRATE_NODE_ID] == hir::CRATE_HIR_ID);

        visit::walk_crate(&mut MiscCollector { lctx: &mut self }, c);
        visit::walk_crate(&mut ItemLowerer { lctx: &mut self }, c);

        let module = self.lower_mod(&c.module);
        let attrs = self.lower_attrs(&c.attrs);
        let body_ids = body_ids(&self.bodies);

        self.resolver
            .definitions()
            .init_node_id_to_hir_id_mapping(self.node_id_to_hir_id);

        hir::Crate {
            module,
            attrs,
            span: c.span,
            exported_macros: hir::HirVec::from(self.exported_macros),
            items: self.items,
            trait_items: self.trait_items,
            impl_items: self.impl_items,
            bodies: self.bodies,
            body_ids,
            trait_impls: self.trait_impls,
            trait_auto_impl: self.trait_auto_impl,
        }
    }

    fn allocate_hir_id_counter<T: Debug>(&mut self,
                                         owner: NodeId,
                                         debug: &T) {
        if self.item_local_id_counters.insert(owner, 0).is_some() {
            bug!("Tried to allocate item_local_id_counter for {:?} twice", debug);
        }
        // Always allocate the first HirId for the owner itself
        self.lower_node_id_with_owner(owner, owner);
    }

    fn lower_node_id_generic<F>(&mut self,
                                ast_node_id: NodeId,
                                alloc_hir_id: F)
                                -> LoweredNodeId
        where F: FnOnce(&mut Self) -> hir::HirId
    {
        if ast_node_id == DUMMY_NODE_ID {
            return LoweredNodeId {
                node_id: DUMMY_NODE_ID,
                hir_id: hir::DUMMY_HIR_ID,
            }
        }

        let min_size = ast_node_id.as_usize() + 1;

        if min_size > self.node_id_to_hir_id.len() {
            self.node_id_to_hir_id.resize(min_size, hir::DUMMY_HIR_ID);
        }

        let existing_hir_id = self.node_id_to_hir_id[ast_node_id];

        if existing_hir_id == hir::DUMMY_HIR_ID {
            // Generate a new HirId
            let hir_id = alloc_hir_id(self);
            self.node_id_to_hir_id[ast_node_id] = hir_id;
            LoweredNodeId {
                node_id: ast_node_id,
                hir_id,
            }
        } else {
            LoweredNodeId {
                node_id: ast_node_id,
                hir_id: existing_hir_id,
            }
        }
    }

    fn with_hir_id_owner<F>(&mut self, owner: NodeId, f: F)
        where F: FnOnce(&mut Self)
    {
        let counter = self.item_local_id_counters
                          .insert(owner, HIR_ID_COUNTER_LOCKED)
                          .unwrap();
        let def_index = self.resolver.definitions().opt_def_index(owner).unwrap();
        self.current_hir_id_owner.push((def_index, counter));
        f(self);
        let (new_def_index, new_counter) = self.current_hir_id_owner.pop().unwrap();

        debug_assert!(def_index == new_def_index);
        debug_assert!(new_counter >= counter);

        let prev = self.item_local_id_counters.insert(owner, new_counter).unwrap();
        debug_assert!(prev == HIR_ID_COUNTER_LOCKED);
    }

    /// This method allocates a new HirId for the given NodeId and stores it in
    /// the LoweringContext's NodeId => HirId map.
    /// Take care not to call this method if the resulting HirId is then not
    /// actually used in the HIR, as that would trigger an assertion in the
    /// HirIdValidator later on, which makes sure that all NodeIds got mapped
    /// properly. Calling the method twice with the same NodeId is fine though.
    fn lower_node_id(&mut self, ast_node_id: NodeId) -> LoweredNodeId {
        self.lower_node_id_generic(ast_node_id, |this| {
            let &mut (def_index, ref mut local_id_counter) = this.current_hir_id_owner
                                                                 .last_mut()
                                                                 .unwrap();
            let local_id = *local_id_counter;
            *local_id_counter += 1;
            hir::HirId {
                owner: def_index,
                local_id: hir::ItemLocalId(local_id),
            }
        })
    }

    fn lower_node_id_with_owner(&mut self,
                                ast_node_id: NodeId,
                                owner: NodeId)
                                -> LoweredNodeId {
        self.lower_node_id_generic(ast_node_id, |this| {
            let local_id_counter = this.item_local_id_counters
                                       .get_mut(&owner)
                                       .unwrap();
            let local_id = *local_id_counter;

            // We want to be sure not to modify the counter in the map while it
            // is also on the stack. Otherwise we'll get lost updates when writing
            // back from the stack to the map.
            debug_assert!(local_id != HIR_ID_COUNTER_LOCKED);

            *local_id_counter += 1;
            let def_index = this.resolver.definitions().opt_def_index(owner).unwrap();

            hir::HirId {
                owner: def_index,
                local_id: hir::ItemLocalId(local_id),
            }
        })
    }

    fn record_body(&mut self, value: hir::Expr, decl: Option<&FnDecl>)
                   -> hir::BodyId {
        let body = hir::Body {
            arguments: decl.map_or(hir_vec![], |decl| {
                decl.inputs.iter().map(|x| self.lower_arg(x)).collect()
            }),
            is_generator: self.is_generator,
            value,
        };
        let id = body.id();
        self.bodies.insert(id, body);
        id
    }

    fn next_id(&mut self) -> LoweredNodeId {
        self.lower_node_id(self.sess.next_node_id())
    }

    fn expect_full_def(&mut self, id: NodeId) -> Def {
        self.resolver.get_resolution(id).map_or(Def::Err, |pr| {
            if pr.unresolved_segments() != 0 {
                bug!("path not fully resolved: {:?}", pr);
            }
            pr.base_def()
        })
    }

    fn diagnostic(&self) -> &errors::Handler {
        self.sess.diagnostic()
    }

    fn str_to_ident(&self, s: &'static str) -> Name {
        Symbol::gensym(s)
    }

    fn allow_internal_unstable(&self, reason: CompilerDesugaringKind, span: Span) -> Span
    {
        let mark = Mark::fresh(Mark::root());
        mark.set_expn_info(codemap::ExpnInfo {
            call_site: span,
            callee: codemap::NameAndSpan {
                format: codemap::CompilerDesugaring(reason),
                span: Some(span),
                allow_internal_unstable: true,
                allow_internal_unsafe: false,
            },
        });
        span.with_ctxt(SyntaxContext::empty().apply_mark(mark))
    }

    // Creates a new hir::LifetimeDef for every new lifetime encountered
    // while evaluating `f`. Definitions are created with the parent provided.
    // If no `parent_id` is provided, no definitions will be returned.
    fn collect_in_band_lifetime_defs<T, F>(
        &mut self,
        parent_id: Option<DefId>,
        f: F
    ) -> (Vec<hir::LifetimeDef>, T) where F: FnOnce(&mut LoweringContext) -> T
    {
        assert!(!self.is_collecting_in_band_lifetimes);
        assert!(self.lifetimes_to_define.is_empty());
        self.is_collecting_in_band_lifetimes = self.sess.features.borrow().in_band_lifetimes;

        let res = f(self);

        self.is_collecting_in_band_lifetimes = false;

        let lifetimes_to_define = self.lifetimes_to_define.split_off(0);

        let lifetime_defs = match parent_id {
            Some(parent_id) => lifetimes_to_define.into_iter().map(|(span, name)| {
                    let def_node_id = self.next_id().node_id;

                    // Add a definition for the in-band lifetime def
                    self.resolver.definitions().create_def_with_parent(
                        parent_id.index,
                        def_node_id,
                        DefPathData::LifetimeDef(name.as_str()),
                        DefIndexAddressSpace::High,
                        Mark::root()
                    );

                    hir::LifetimeDef {
                        lifetime: hir::Lifetime {
                            id: def_node_id,
                            span,
                            name: hir::LifetimeName::Name(name),
                        },
                        bounds: Vec::new().into(),
                        pure_wrt_drop: false,
                        in_band: true,
                    }
                }).collect(),
            None => Vec::new(),
        };

        (lifetime_defs, res)
    }

    // Evaluates `f` with the lifetimes in `lt_defs` in-scope.
    // This is used to track which lifetimes have already been defined, and
    // which are new in-band lifetimes that need to have a definition created
    // for them.
    fn with_in_scope_lifetime_defs<T, F>(
        &mut self,
        lt_defs: &[LifetimeDef],
        f: F
    ) -> T where F: FnOnce(&mut LoweringContext) -> T
    {
        let old_len = self.in_scope_lifetimes.len();
        let lt_def_names = lt_defs.iter().map(|lt_def| lt_def.lifetime.ident.name);
        self.in_scope_lifetimes.extend(lt_def_names);

        let res = f(self);

        self.in_scope_lifetimes.truncate(old_len);
        res
    }

    // Same as the method above, but accepts `hir::LifetimeDef`s
    // instead of `ast::LifetimeDef`s.
    // This should only be used with generics that have already had their
    // in-band lifetimes added. In practice, this means that this function is
    // only used when lowering a child item of a trait or impl.
    fn with_parent_impl_lifetime_defs<T, F>(
        &mut self,
        lt_defs: &[hir::LifetimeDef],
        f: F
    ) -> T where F: FnOnce(&mut LoweringContext) -> T
    {
        let old_len = self.in_scope_lifetimes.len();
        let lt_def_names = lt_defs.iter().map(|lt_def| lt_def.lifetime.name.name());
        self.in_scope_lifetimes.extend(lt_def_names);

        let res = f(self);

        self.in_scope_lifetimes.truncate(old_len);
        res
    }

    // Appends in-band lifetime defs to the existing set of out-of-band lifetime defs.
    // Evaluates all within the context of the out-of-band defs.
    // If provided, `impl_item_id` is used to find the parent impls of impl items so
    // that their generics are not duplicated.
    fn add_in_band_lifetime_defs<F, T>(
        &mut self,
        generics: &Generics,
        parent_id: Option<DefId>,
        f: F
    ) -> (hir::Generics, T)
        where F: FnOnce(&mut LoweringContext) -> T
    {
        let (in_band_defs, (mut lowered_generics, res)) =
            self.with_in_scope_lifetime_defs(&generics.lifetimes, |this| {
                this.collect_in_band_lifetime_defs(parent_id, |this| {
                    (this.lower_generics(generics), f(this))
                })
            });

        lowered_generics.lifetimes =
            lowered_generics.lifetimes
                .iter().cloned()
               .chain(in_band_defs.into_iter())
               .collect();

        (lowered_generics, res)
    }

    fn with_catch_scope<T, F>(&mut self, catch_id: NodeId, f: F) -> T
        where F: FnOnce(&mut LoweringContext) -> T
    {
        let len = self.catch_scopes.len();
        self.catch_scopes.push(catch_id);

        let result = f(self);
        assert_eq!(len + 1, self.catch_scopes.len(),
            "catch scopes should be added and removed in stack order");

        self.catch_scopes.pop().unwrap();

        result
    }

    fn lower_body<F>(&mut self, decl: Option<&FnDecl>, f: F) -> hir::BodyId
        where F: FnOnce(&mut LoweringContext) -> hir::Expr
    {
        let prev = mem::replace(&mut self.is_generator, false);
        let result = f(self);
        let r = self.record_body(result, decl);
        self.is_generator = prev;
        return r
    }

    fn with_loop_scope<T, F>(&mut self, loop_id: NodeId, f: F) -> T
        where F: FnOnce(&mut LoweringContext) -> T
    {
        // We're no longer in the base loop's condition; we're in another loop.
        let was_in_loop_condition = self.is_in_loop_condition;
        self.is_in_loop_condition = false;

        let len = self.loop_scopes.len();
        self.loop_scopes.push(loop_id);

        let result = f(self);
        assert_eq!(len + 1, self.loop_scopes.len(),
            "Loop scopes should be added and removed in stack order");

        self.loop_scopes.pop().unwrap();

        self.is_in_loop_condition = was_in_loop_condition;

        result
    }

    fn with_loop_condition_scope<T, F>(&mut self, f: F) -> T
        where F: FnOnce(&mut LoweringContext) -> T
    {
        let was_in_loop_condition = self.is_in_loop_condition;
        self.is_in_loop_condition = true;

        let result = f(self);

        self.is_in_loop_condition = was_in_loop_condition;

        result
    }

    fn with_new_scopes<T, F>(&mut self, f: F) -> T
        where F: FnOnce(&mut LoweringContext) -> T
    {
        let was_in_loop_condition = self.is_in_loop_condition;
        self.is_in_loop_condition = false;

        let catch_scopes = mem::replace(&mut self.catch_scopes, Vec::new());
        let loop_scopes = mem::replace(&mut self.loop_scopes, Vec::new());
        let result = f(self);
        self.catch_scopes = catch_scopes;
        self.loop_scopes = loop_scopes;

        self.is_in_loop_condition = was_in_loop_condition;

        result
    }

    fn with_parent_def<T, F>(&mut self, parent_id: NodeId, f: F) -> T
        where F: FnOnce(&mut LoweringContext) -> T
    {
        let old_def = self.parent_def;
        self.parent_def = {
            let defs = self.resolver.definitions();
            Some(defs.opt_def_index(parent_id).unwrap())
        };

        let result = f(self);

        self.parent_def = old_def;
        result
    }

    fn def_key(&mut self, id: DefId) -> DefKey {
        if id.is_local() {
            self.resolver.definitions().def_key(id.index)
        } else {
            self.cstore.def_key(id)
        }
    }

    fn lower_ident(&mut self, ident: Ident) -> Name {
        let ident = ident.modern();
        if ident.ctxt == SyntaxContext::empty() {
            return ident.name;
        }
        *self.name_map.entry(ident).or_insert_with(|| Symbol::from_ident(ident))
    }

    fn lower_opt_sp_ident(&mut self, o_id: Option<Spanned<Ident>>) -> Option<Spanned<Name>> {
        o_id.map(|sp_ident| respan(sp_ident.span, sp_ident.node.name))
    }

    fn lower_loop_destination(&mut self, destination: Option<(NodeId, Spanned<Ident>)>)
        -> hir::Destination
    {
        match destination {
            Some((id, label_ident)) => {
                let target = if let Def::Label(loop_id) = self.expect_full_def(id) {
                    hir::LoopIdResult::Ok(self.lower_node_id(loop_id).node_id)
                } else {
                    hir::LoopIdResult::Err(hir::LoopIdError::UnresolvedLabel)
                };
                hir::Destination {
                    ident: Some(label_ident),
                    target_id: hir::ScopeTarget::Loop(target),
                }
            },
            None => {
                let loop_id = self.loop_scopes
                                  .last()
                                  .map(|innermost_loop_id| *innermost_loop_id);

                hir::Destination {
                    ident: None,
                    target_id: hir::ScopeTarget::Loop(
                        loop_id.map(|id| Ok(self.lower_node_id(id).node_id))
                               .unwrap_or(Err(hir::LoopIdError::OutsideLoopScope))
                               .into())
                }
            }
        }
    }

    fn lower_attrs(&mut self, attrs: &Vec<Attribute>) -> hir::HirVec<Attribute> {
        attrs.iter().map(|a| self.lower_attr(a)).collect::<Vec<_>>().into()
    }

    fn lower_attr(&mut self, attr: &Attribute) -> Attribute {
        Attribute {
            id: attr.id,
            style: attr.style,
            path: attr.path.clone(),
            tokens: self.lower_token_stream(attr.tokens.clone()),
            is_sugared_doc: attr.is_sugared_doc,
            span: attr.span,
        }
    }

    fn lower_token_stream(&mut self, tokens: TokenStream) -> TokenStream {
        tokens.into_trees()
            .flat_map(|tree| self.lower_token_tree(tree).into_trees())
            .collect()
    }

    fn lower_token_tree(&mut self, tree: TokenTree) -> TokenStream {
        match tree {
            TokenTree::Token(span, token) => {
                self.lower_token(token, span)
            }
            TokenTree::Delimited(span, delimited) => {
                TokenTree::Delimited(span, Delimited {
                    delim: delimited.delim,
                    tts: self.lower_token_stream(delimited.tts.into()).into(),
                }).into()
            }
        }
    }

    fn lower_token(&mut self, token: Token, span: Span) -> TokenStream {
        match token {
            Token::Interpolated(_) => {}
            other => return TokenTree::Token(span, other).into(),
        }

        let tts = token.interpolated_to_tokenstream(&self.sess.parse_sess, span);
        self.lower_token_stream(tts)
    }

    fn lower_arm(&mut self, arm: &Arm) -> hir::Arm {
        hir::Arm {
            attrs: self.lower_attrs(&arm.attrs),
            pats: arm.pats.iter().map(|x| self.lower_pat(x)).collect(),
            guard: arm.guard.as_ref().map(|ref x| P(self.lower_expr(x))),
            body: P(self.lower_expr(&arm.body)),
        }
    }

    fn lower_ty_binding(&mut self, b: &TypeBinding, itctx: ImplTraitContext) -> hir::TypeBinding {
        hir::TypeBinding {
            id: self.lower_node_id(b.id).node_id,
            name: self.lower_ident(b.ident),
            ty: self.lower_ty(&b.ty, itctx),
            span: b.span,
        }
    }

    fn lower_ty(&mut self, t: &Ty, itctx: ImplTraitContext) -> P<hir::Ty> {
        let kind = match t.node {
            TyKind::Infer => hir::TyInfer,
            TyKind::Err => hir::TyErr,
            TyKind::Slice(ref ty) => hir::TySlice(self.lower_ty(ty, itctx)),
            TyKind::Ptr(ref mt) => hir::TyPtr(self.lower_mt(mt, itctx)),
            TyKind::Rptr(ref region, ref mt) => {
                let span = t.span.with_hi(t.span.lo());
                let lifetime = match *region {
                    Some(ref lt) => self.lower_lifetime(lt),
                    None => self.elided_lifetime(span)
                };
                hir::TyRptr(lifetime, self.lower_mt(mt, itctx))
            }
            TyKind::BareFn(ref f) => {
                self.with_in_scope_lifetime_defs(&f.lifetimes, |this|
                    hir::TyBareFn(P(hir::BareFnTy {
                        lifetimes: this.lower_lifetime_defs(&f.lifetimes),
                        unsafety: this.lower_unsafety(f.unsafety),
                        abi: f.abi,
                        decl: this.lower_fn_decl(&f.decl, None, false),
                        arg_names: this.lower_fn_args_to_names(&f.decl),
                    })))
            }
            TyKind::Never => hir::TyNever,
            TyKind::Tup(ref tys) => {
                hir::TyTup(tys.iter().map(|ty| self.lower_ty(ty, itctx)).collect())
            }
            TyKind::Paren(ref ty) => {
                return self.lower_ty(ty, itctx);
            }
            TyKind::Path(ref qself, ref path) => {
                let id = self.lower_node_id(t.id);
                let qpath = self.lower_qpath(t.id, qself, path, ParamMode::Explicit, itctx);
                return self.ty_path(id, t.span, qpath);
            }
            TyKind::ImplicitSelf => {
                hir::TyPath(hir::QPath::Resolved(None, P(hir::Path {
                    def: self.expect_full_def(t.id),
                    segments: hir_vec![
                        hir::PathSegment::from_name(keywords::SelfType.name())
                    ],
                    span: t.span,
                })))
            }
            TyKind::Array(ref ty, ref length) => {
                let length = self.lower_body(None, |this| this.lower_expr(length));
                hir::TyArray(self.lower_ty(ty, itctx), length)
            }
            TyKind::Typeof(ref expr) => {
                let expr = self.lower_body(None, |this| this.lower_expr(expr));
                hir::TyTypeof(expr)
            }
            TyKind::TraitObject(ref bounds, ..) => {
                let mut lifetime_bound = None;
                let bounds = bounds.iter().filter_map(|bound| {
                    match *bound {
                        TraitTyParamBound(ref ty, TraitBoundModifier::None) => {
                            Some(self.lower_poly_trait_ref(ty, itctx))
                        }
                        TraitTyParamBound(_, TraitBoundModifier::Maybe) => None,
                        RegionTyParamBound(ref lifetime) => {
                            if lifetime_bound.is_none() {
                                lifetime_bound = Some(self.lower_lifetime(lifetime));
                            }
                            None
                        }
                    }
                }).collect();
                let lifetime_bound = lifetime_bound.unwrap_or_else(|| {
                    self.elided_lifetime(t.span)
                });
                hir::TyTraitObject(bounds, lifetime_bound)
            }
            TyKind::ImplTrait(ref bounds) => {
                use syntax::feature_gate::{emit_feature_err, GateIssue};
                match itctx {
                    ImplTraitContext::Existential => {
                        let has_feature = self.sess.features.borrow().conservative_impl_trait;
                        if !t.span.allows_unstable() && !has_feature {
                            emit_feature_err(&self.sess.parse_sess, "conservative_impl_trait",
                                             t.span, GateIssue::Language,
                                             "`impl Trait` in return position is experimental");
                        }
                        let def_index = self.resolver.definitions().opt_def_index(t.id).unwrap();
                        let hir_bounds = self.lower_bounds(bounds, itctx);
                        let (lifetimes, lifetime_defs) =
                            self.lifetimes_from_impl_trait_bounds(def_index, &hir_bounds);

                        hir::TyImplTraitExistential(hir::ExistTy {
                            generics: hir::Generics {
                                lifetimes: lifetime_defs,
                                // Type parameters are taken from environment:
                                ty_params: Vec::new().into(),
                                where_clause: hir::WhereClause {
                                    id: self.next_id().node_id,
                                    predicates: Vec::new().into(),
                                },
                                span: t.span,
                            },
                            bounds: hir_bounds,
                        }, lifetimes)
                    },
                    ImplTraitContext::Universal(def_id) => {
                        let has_feature = self.sess.features.borrow().universal_impl_trait;
                        if !t.span.allows_unstable() && !has_feature {
                            emit_feature_err(&self.sess.parse_sess, "universal_impl_trait",
                                             t.span, GateIssue::Language,
                                             "`impl Trait` in argument position is experimental");
                        }
                        hir::TyImplTraitUniversal(def_id, self.lower_bounds(bounds, itctx))
                    },
                    ImplTraitContext::Disallowed => {
                        span_err!(self.sess, t.span, E0562,
                                  "`impl Trait` not allowed outside of function \
                                  and inherent method return types");
                        hir::TyErr
                    }
                }
            }
            TyKind::Mac(_) => panic!("TyMac should have been expanded by now."),
        };

        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(t.id);
        P(hir::Ty {
            id: node_id,
            node: kind,
            span: t.span,
            hir_id,
        })
    }

    fn lifetimes_from_impl_trait_bounds(
        &mut self,
        parent_index: DefIndex,
        bounds: &hir::TyParamBounds
    ) -> (HirVec<hir::Lifetime>, HirVec<hir::LifetimeDef>) {

        // This visitor walks over impl trait bounds and creates defs for all lifetimes which
        // appear in the bounds, excluding lifetimes that are created within the bounds.
        // e.g. 'a, 'b, but not 'c in `impl for<'c> SomeTrait<'a, 'b, 'c>`
        struct ImplTraitLifetimeCollector<'r, 'a: 'r> {
            context: &'r mut LoweringContext<'a>,
            parent: DefIndex,
            collect_elided_lifetimes: bool,
            currently_bound_lifetimes: Vec<hir::LifetimeName>,
            already_defined_lifetimes: HashSet<hir::LifetimeName>,
            output_lifetimes: Vec<hir::Lifetime>,
            output_lifetime_defs: Vec<hir::LifetimeDef>,
        }

        impl<'r, 'a: 'r, 'v> hir::intravisit::Visitor<'v> for ImplTraitLifetimeCollector<'r, 'a> {
            fn nested_visit_map<'this>(&'this mut self)
                -> hir::intravisit::NestedVisitorMap<'this, 'v> {
                hir::intravisit::NestedVisitorMap::None
            }

            fn visit_path_parameters(&mut self, span: Span, parameters: &'v hir::PathParameters) {
                // Don't collect elided lifetimes used inside of `Fn()` syntax.
                if parameters.parenthesized {
                    let old_collect_elided_lifetimes = self.collect_elided_lifetimes;
                    self.collect_elided_lifetimes = false;
                    hir::intravisit::walk_path_parameters(self, span, parameters);
                    self.collect_elided_lifetimes = old_collect_elided_lifetimes;
                } else {
                    hir::intravisit::walk_path_parameters(self, span, parameters);
                }
            }

            fn visit_ty(&mut self, t: &'v hir::Ty) {
                // Don't collect elided lifetimes used inside of `fn()` syntax
                if let &hir::Ty_::TyBareFn(_) = &t.node {
                    let old_collect_elided_lifetimes = self.collect_elided_lifetimes;
                    self.collect_elided_lifetimes = false;
                    hir::intravisit::walk_ty(self, t);
                    self.collect_elided_lifetimes = old_collect_elided_lifetimes;
                } else {
                    hir::intravisit::walk_ty(self, t);
                }
            }

            fn visit_poly_trait_ref(&mut self,
                                    polytr: &'v hir::PolyTraitRef,
                                    _: hir::TraitBoundModifier) {
                let old_len = self.currently_bound_lifetimes.len();

                // Record the introduction of 'a in `for<'a> ...`
                for lt_def in &polytr.bound_lifetimes {
                    // Introduce lifetimes one at a time so that we can handle
                    // cases like `fn foo<'d>() -> impl for<'a, 'b: 'a, 'c: 'b + 'd>`
                    self.currently_bound_lifetimes.push(lt_def.lifetime.name);

                    // Visit the lifetime bounds
                    for lt_bound in &lt_def.bounds {
                        self.visit_lifetime(&lt_bound);
                    }
                }

                hir::intravisit::walk_trait_ref(self, &polytr.trait_ref);

                self.currently_bound_lifetimes.truncate(old_len);
            }

            fn visit_lifetime(&mut self, lifetime: &'v hir::Lifetime) {
                let name = match lifetime.name {
                    hir::LifetimeName::Implicit |
                    hir::LifetimeName::Underscore =>
                        if self.collect_elided_lifetimes {
                            // Use `'_` for both implicit and underscore lifetimes in
                            // `abstract type Foo<'_>: SomeTrait<'_>;`
                            hir::LifetimeName::Underscore
                        } else {
                            return
                        }
                    name @ hir::LifetimeName::Name(_) => name,
                    hir::LifetimeName::Static => return,
                };

                if !self.currently_bound_lifetimes.contains(&name) &&
                   !self.already_defined_lifetimes.contains(&name)
                {
                    self.already_defined_lifetimes.insert(name);

                    self.output_lifetimes.push(hir::Lifetime {
                        id: self.context.next_id().node_id,
                        span: lifetime.span,
                        name,
                    });

                    let def_node_id = self.context.next_id().node_id;
                    self.context.resolver.definitions().create_def_with_parent(
                        self.parent,
                        def_node_id,
                        DefPathData::LifetimeDef(name.name().as_str()),
                        DefIndexAddressSpace::High,
                        Mark::root()
                    );
                    let def_lifetime = hir::Lifetime {
                        id: def_node_id,
                        span: lifetime.span,
                        name: name,
                    };
                    self.output_lifetime_defs.push(hir::LifetimeDef {
                        lifetime: def_lifetime,
                        bounds: Vec::new().into(),
                        pure_wrt_drop: false,
                        in_band: false,
                    });
                }
            }
        }

        let mut lifetime_collector = ImplTraitLifetimeCollector {
            context: self,
            parent: parent_index,
            collect_elided_lifetimes: true,
            currently_bound_lifetimes: Vec::new(),
            already_defined_lifetimes: HashSet::new(),
            output_lifetimes: Vec::new(),
            output_lifetime_defs: Vec::new(),
        };

        for bound in bounds {
            hir::intravisit::walk_ty_param_bound(&mut lifetime_collector, &bound);
        }

        (
            lifetime_collector.output_lifetimes.into(),
            lifetime_collector.output_lifetime_defs.into()
        )
    }

    fn lower_foreign_mod(&mut self, fm: &ForeignMod) -> hir::ForeignMod {
        hir::ForeignMod {
            abi: fm.abi,
            items: fm.items.iter().map(|x| self.lower_foreign_item(x)).collect(),
        }
    }

    fn lower_global_asm(&mut self, ga: &GlobalAsm) -> P<hir::GlobalAsm> {
        P(hir::GlobalAsm {
            asm: ga.asm,
            ctxt: ga.ctxt,
        })
    }

    fn lower_variant(&mut self, v: &Variant) -> hir::Variant {
        Spanned {
            node: hir::Variant_ {
                name: v.node.name.name,
                attrs: self.lower_attrs(&v.node.attrs),
                data: self.lower_variant_data(&v.node.data),
                disr_expr: v.node.disr_expr.as_ref().map(|e| {
                    self.lower_body(None, |this| this.lower_expr(e))
                }),
            },
            span: v.span,
        }
    }

    fn lower_qpath(&mut self,
                   id: NodeId,
                   qself: &Option<QSelf>,
                   p: &Path,
                   param_mode: ParamMode,
                   itctx: ImplTraitContext)
                   -> hir::QPath {
        let qself_position = qself.as_ref().map(|q| q.position);
        let qself = qself.as_ref().map(|q| self.lower_ty(&q.ty, itctx));

        let resolution = self.resolver.get_resolution(id)
                                      .unwrap_or(PathResolution::new(Def::Err));

        let proj_start = p.segments.len() - resolution.unresolved_segments();
        let path = P(hir::Path {
            def: resolution.base_def(),
            segments: p.segments[..proj_start].iter().enumerate().map(|(i, segment)| {
                let param_mode = match (qself_position, param_mode) {
                    (Some(j), ParamMode::Optional) if i < j => {
                        // This segment is part of the trait path in a
                        // qualified path - one of `a`, `b` or `Trait`
                        // in `<X as a::b::Trait>::T::U::method`.
                        ParamMode::Explicit
                    }
                    _ => param_mode
                };

                // Figure out if this is a type/trait segment,
                // which may need lifetime elision performed.
                let parent_def_id = |this: &mut Self, def_id: DefId| {
                    DefId {
                        krate: def_id.krate,
                        index: this.def_key(def_id).parent.expect("missing parent")
                    }
                };
                let type_def_id = match resolution.base_def() {
                    Def::AssociatedTy(def_id) if i + 2 == proj_start => {
                        Some(parent_def_id(self, def_id))
                    }
                    Def::Variant(def_id) if i + 1 == proj_start => {
                        Some(parent_def_id(self, def_id))
                    }
                    Def::Struct(def_id) |
                    Def::Union(def_id) |
                    Def::Enum(def_id) |
                    Def::TyAlias(def_id) |
                    Def::Trait(def_id) if i + 1 == proj_start => Some(def_id),
                    _ => None
                };
                let parenthesized_generic_args = match resolution.base_def() {
                    // `a::b::Trait(Args)`
                    Def::Trait(..) if i + 1 == proj_start => ParenthesizedGenericArgs::Ok,
                    // `a::b::Trait(Args)::TraitItem`
                    Def::Method(..) |
                    Def::AssociatedConst(..) |
                    Def::AssociatedTy(..) if i + 2 == proj_start => ParenthesizedGenericArgs::Ok,
                    // Avoid duplicated errors
                    Def::Err => ParenthesizedGenericArgs::Ok,
                    // An error
                    Def::Struct(..) | Def::Enum(..) | Def::Union(..) | Def::TyAlias(..) |
                    Def::Variant(..) if i + 1 == proj_start => ParenthesizedGenericArgs::Err,
                    // A warning for now, for compatibility reasons
                    _ => ParenthesizedGenericArgs::Warn,
                };

                let num_lifetimes = type_def_id.map_or(0, |def_id| {
                    if let Some(&n) = self.type_def_lifetime_params.get(&def_id) {
                        return n;
                    }
                    assert!(!def_id.is_local());
                    let n = self.cstore
                                .item_generics_cloned_untracked(def_id, self.sess)
                                .regions
                                .len();
                    self.type_def_lifetime_params.insert(def_id, n);
                    n
                });
                self.lower_path_segment(p.span, segment, param_mode, num_lifetimes,
                                        parenthesized_generic_args, itctx)
            }).collect(),
            span: p.span,
        });

        // Simple case, either no projections, or only fully-qualified.
        // E.g. `std::mem::size_of` or `<I as Iterator>::Item`.
        if resolution.unresolved_segments() == 0 {
            return hir::QPath::Resolved(qself, path);
        }

        // Create the innermost type that we're projecting from.
        let mut ty = if path.segments.is_empty() {
            // If the base path is empty that means there exists a
            // syntactical `Self`, e.g. `&i32` in `<&i32>::clone`.
            qself.expect("missing QSelf for <T>::...")
        } else {
            // Otherwise, the base path is an implicit `Self` type path,
            // e.g. `Vec` in `Vec::new` or `<I as Iterator>::Item` in
            // `<I as Iterator>::Item::default`.
            let new_id = self.next_id();
            self.ty_path(new_id, p.span, hir::QPath::Resolved(qself, path))
        };

        // Anything after the base path are associated "extensions",
        // out of which all but the last one are associated types,
        // e.g. for `std::vec::Vec::<T>::IntoIter::Item::clone`:
        // * base path is `std::vec::Vec<T>`
        // * "extensions" are `IntoIter`, `Item` and `clone`
        // * type nodes are:
        //   1. `std::vec::Vec<T>` (created above)
        //   2. `<std::vec::Vec<T>>::IntoIter`
        //   3. `<<std::vec::Vec<T>>::IntoIter>::Item`
        // * final path is `<<<std::vec::Vec<T>>::IntoIter>::Item>::clone`
        for (i, segment) in p.segments.iter().enumerate().skip(proj_start) {
            let segment = P(self.lower_path_segment(p.span, segment, param_mode, 0,
                                                    ParenthesizedGenericArgs::Warn,
                                                    itctx));
            let qpath = hir::QPath::TypeRelative(ty, segment);

            // It's finished, return the extension of the right node type.
            if i == p.segments.len() - 1 {
                return qpath;
            }

            // Wrap the associated extension in another type node.
            let new_id = self.next_id();
            ty = self.ty_path(new_id, p.span, qpath);
        }

        // Should've returned in the for loop above.
        span_bug!(p.span, "lower_qpath: no final extension segment in {}..{}",
                  proj_start, p.segments.len())
    }

    fn lower_path_extra(&mut self,
                        id: NodeId,
                        p: &Path,
                        name: Option<Name>,
                        param_mode: ParamMode,
                        defaults_to_global: bool)
                        -> hir::Path {
        let mut segments = p.segments.iter();
        if defaults_to_global && p.is_global() {
            segments.next();
        }

        hir::Path {
            def: self.expect_full_def(id),
            segments: segments.map(|segment| {
                self.lower_path_segment(p.span, segment, param_mode, 0,
                                        ParenthesizedGenericArgs::Err,
                                        ImplTraitContext::Disallowed)
            }).chain(name.map(|name| hir::PathSegment::from_name(name)))
              .collect(),
            span: p.span,
        }
    }

    fn lower_path(&mut self,
                  id: NodeId,
                  p: &Path,
                  param_mode: ParamMode,
                  defaults_to_global: bool)
                  -> hir::Path {
        self.lower_path_extra(id, p, None, param_mode, defaults_to_global)
    }

    fn lower_path_segment(&mut self,
                          path_span: Span,
                          segment: &PathSegment,
                          param_mode: ParamMode,
                          expected_lifetimes: usize,
                          parenthesized_generic_args: ParenthesizedGenericArgs,
                          itctx: ImplTraitContext)
                          -> hir::PathSegment {
        let (mut parameters, infer_types) = if let Some(ref parameters) = segment.parameters {
            let msg = "parenthesized parameters may only be used with a trait";
            match **parameters {
                PathParameters::AngleBracketed(ref data) => {
                    self.lower_angle_bracketed_parameter_data(data, param_mode, itctx)
                }
                PathParameters::Parenthesized(ref data) => match parenthesized_generic_args {
                    ParenthesizedGenericArgs::Ok =>
                        self.lower_parenthesized_parameter_data(data),
                    ParenthesizedGenericArgs::Warn => {
                        self.sess.buffer_lint(PARENTHESIZED_PARAMS_IN_TYPES_AND_MODULES,
                                              CRATE_NODE_ID, data.span, msg.into());
                        (hir::PathParameters::none(), true)
                    }
                    ParenthesizedGenericArgs::Err => {
                        struct_span_err!(self.sess, data.span, E0214, "{}", msg)
                            .span_label(data.span, "only traits may use parentheses").emit();
                        (hir::PathParameters::none(), true)
                    }
                }
            }
        } else {
            self.lower_angle_bracketed_parameter_data(&Default::default(), param_mode, itctx)
        };

        if !parameters.parenthesized && parameters.lifetimes.is_empty() {
            parameters.lifetimes = (0..expected_lifetimes).map(|_| {
                self.elided_lifetime(path_span)
            }).collect();
        }

        hir::PathSegment::new(
            self.lower_ident(segment.identifier),
            parameters,
            infer_types
        )
    }

    fn lower_angle_bracketed_parameter_data(&mut self,
                                            data: &AngleBracketedParameterData,
                                            param_mode: ParamMode,
                                            itctx: ImplTraitContext)
                                            -> (hir::PathParameters, bool) {
        let &AngleBracketedParameterData { ref lifetimes, ref types, ref bindings, .. } = data;
        (hir::PathParameters {
            lifetimes: self.lower_lifetimes(lifetimes),
            types: types.iter().map(|ty| self.lower_ty(ty, itctx)).collect(),
            bindings: bindings.iter().map(|b| self.lower_ty_binding(b, itctx)).collect(),
            parenthesized: false,
        }, types.is_empty() && param_mode == ParamMode::Optional)
    }

    fn lower_parenthesized_parameter_data(&mut self,
                                          data: &ParenthesizedParameterData)
                                          -> (hir::PathParameters, bool) {
        const DISALLOWED: ImplTraitContext = ImplTraitContext::Disallowed;
        let &ParenthesizedParameterData { ref inputs, ref output, span } = data;
        let inputs = inputs.iter().map(|ty| self.lower_ty(ty, DISALLOWED)).collect();
        let mk_tup = |this: &mut Self, tys, span| {
            let LoweredNodeId { node_id, hir_id } = this.next_id();
            P(hir::Ty { node: hir::TyTup(tys), id: node_id, hir_id, span })
        };

        (hir::PathParameters {
            lifetimes: hir::HirVec::new(),
            types: hir_vec![mk_tup(self, inputs, span)],
            bindings: hir_vec![hir::TypeBinding {
                id: self.next_id().node_id,
                name: Symbol::intern(FN_OUTPUT_NAME),
                ty: output.as_ref().map(|ty| self.lower_ty(&ty, DISALLOWED))
                                   .unwrap_or_else(|| mk_tup(self, hir::HirVec::new(), span)),
                span: output.as_ref().map_or(span, |ty| ty.span),
            }],
            parenthesized: true,
        }, false)
    }

    fn lower_local(&mut self, l: &Local) -> P<hir::Local> {
        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(l.id);
        P(hir::Local {
            id: node_id,
            hir_id,
            ty: l.ty.as_ref().map(|t| self.lower_ty(t, ImplTraitContext::Disallowed)),
            pat: self.lower_pat(&l.pat),
            init: l.init.as_ref().map(|e| P(self.lower_expr(e))),
            span: l.span,
            attrs: l.attrs.clone(),
            source: hir::LocalSource::Normal,
        })
    }

    fn lower_mutability(&mut self, m: Mutability) -> hir::Mutability {
        match m {
            Mutability::Mutable => hir::MutMutable,
            Mutability::Immutable => hir::MutImmutable,
        }
    }

    fn lower_arg(&mut self, arg: &Arg) -> hir::Arg {
        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(arg.id);
        hir::Arg {
            id: node_id,
            hir_id,
            pat: self.lower_pat(&arg.pat),
        }
    }

    fn lower_fn_args_to_names(&mut self, decl: &FnDecl)
                              -> hir::HirVec<Spanned<Name>> {
        decl.inputs.iter().map(|arg| {
            match arg.pat.node {
                PatKind::Ident(_, ident, None) => {
                    respan(ident.span, ident.node.name)
                }
                _ => respan(arg.pat.span, keywords::Invalid.name()),
            }
        }).collect()
    }


    fn lower_fn_decl(&mut self,
                     decl: &FnDecl,
                     fn_def_id: Option<DefId>,
                     impl_trait_return_allow: bool)
                     -> P<hir::FnDecl> {
        // NOTE: The two last paramters here have to do with impl Trait. If fn_def_id is Some,
        //       then impl Trait arguments are lowered into generic paramters on the given
        //       fn_def_id, otherwise impl Trait is disallowed. (for now)
        //
        //       Furthermore, if impl_trait_return_allow is true, then impl Trait may be used in
        //       return positions as well. This guards against trait declarations and their impls
        //       where impl Trait is disallowed. (again for now)
        P(hir::FnDecl {
            inputs: decl.inputs.iter()
                .map(|arg| if let Some(def_id) = fn_def_id {
                    self.lower_ty(&arg.ty, ImplTraitContext::Universal(def_id))
                } else {
                    self.lower_ty(&arg.ty, ImplTraitContext::Disallowed)
                }).collect(),
            output: match decl.output {
                FunctionRetTy::Ty(ref ty) => match fn_def_id {
                    Some(_) if impl_trait_return_allow =>
                        hir::Return(self.lower_ty(ty, ImplTraitContext::Existential)),
                    _ => hir::Return(self.lower_ty(ty, ImplTraitContext::Disallowed)),
                },
                FunctionRetTy::Default(span) => hir::DefaultReturn(span),
            },
            variadic: decl.variadic,
            has_implicit_self: decl.inputs.get(0).map_or(false, |arg| {
                match arg.ty.node {
                    TyKind::ImplicitSelf => true,
                    TyKind::Rptr(_, ref mt) => mt.ty.node == TyKind::ImplicitSelf,
                    _ => false
                }
            })
        })
    }

    fn lower_ty_param_bound(&mut self, tpb: &TyParamBound, itctx: ImplTraitContext)
                            -> hir::TyParamBound {
        match *tpb {
            TraitTyParamBound(ref ty, modifier) => {
                hir::TraitTyParamBound(self.lower_poly_trait_ref(ty, itctx),
                                       self.lower_trait_bound_modifier(modifier))
            }
            RegionTyParamBound(ref lifetime) => {
                hir::RegionTyParamBound(self.lower_lifetime(lifetime))
            }
        }
    }

    fn lower_ty_param(&mut self, tp: &TyParam, add_bounds: &[TyParamBound]) -> hir::TyParam {
        let mut name = self.lower_ident(tp.ident);

        // Don't expose `Self` (recovered "keyword used as ident" parse error).
        // `rustc::ty` expects `Self` to be only used for a trait's `Self`.
        // Instead, use gensym("Self") to create a distinct name that looks the same.
        if name == keywords::SelfType.name() {
            name = Symbol::gensym("Self");
        }

        let itctx = ImplTraitContext::Universal(self.resolver.definitions().local_def_id(tp.id));
        let mut bounds = self.lower_bounds(&tp.bounds, itctx);
        if !add_bounds.is_empty() {
            bounds = bounds.into_iter().chain(
                self.lower_bounds(add_bounds, itctx).into_iter()
            ).collect();
        }

        hir::TyParam {
            id: self.lower_node_id(tp.id).node_id,
            name,
            bounds,
            default: tp.default.as_ref().map(|x| self.lower_ty(x, ImplTraitContext::Disallowed)),
            span: tp.span,
            pure_wrt_drop: tp.attrs.iter().any(|attr| attr.check_name("may_dangle")),
            synthetic: tp.attrs.iter()
                               .filter(|attr| attr.check_name("rustc_synthetic"))
                               .map(|_| hir::SyntheticTyParamKind::ImplTrait)
                               .nth(0),
        }
    }

    fn lower_ty_params(&mut self, tps: &Vec<TyParam>, add_bounds: &NodeMap<Vec<TyParamBound>>)
                       -> hir::HirVec<hir::TyParam> {
        tps.iter().map(|tp| {
            self.lower_ty_param(tp, add_bounds.get(&tp.id).map_or(&[][..], |x| &x))
        }).collect()
    }

    fn lower_lifetime(&mut self, l: &Lifetime) -> hir::Lifetime {
        let name = match self.lower_ident(l.ident) {
            x if x == "'_" => hir::LifetimeName::Underscore,
            x if x == "'static" => hir::LifetimeName::Static,
            name => {
                if  self.is_collecting_in_band_lifetimes &&
                    !self.in_scope_lifetimes.contains(&name) &&
                    self.lifetimes_to_define.iter()
                        .find(|&&(_, lt_name)| lt_name == name)
                        .is_none()
                {
                    self.lifetimes_to_define.push((l.span, name));
                }

                hir::LifetimeName::Name(name)
            }
        };

        hir::Lifetime {
            id: self.lower_node_id(l.id).node_id,
            name,
            span: l.span,
        }
    }

    fn lower_lifetime_def(&mut self, l: &LifetimeDef) -> hir::LifetimeDef {
        let was_collecting_in_band = self.is_collecting_in_band_lifetimes;
        self.is_collecting_in_band_lifetimes = false;

        let def = hir::LifetimeDef {
            lifetime: self.lower_lifetime(&l.lifetime),
            bounds: self.lower_lifetimes(&l.bounds),
            pure_wrt_drop: l.attrs.iter().any(|attr| attr.check_name("may_dangle")),
            in_band: false,
        };

        self.is_collecting_in_band_lifetimes = was_collecting_in_band;

        def
    }

    fn lower_lifetimes(&mut self, lts: &Vec<Lifetime>) -> hir::HirVec<hir::Lifetime> {
        lts.iter().map(|l| self.lower_lifetime(l)).collect()
    }

    fn lower_lifetime_defs(&mut self, lts: &Vec<LifetimeDef>) -> hir::HirVec<hir::LifetimeDef> {
        lts.iter().map(|l| self.lower_lifetime_def(l)).collect()
    }

    fn lower_generics(&mut self, g: &Generics) -> hir::Generics {
        // Collect `?Trait` bounds in where clause and move them to parameter definitions.
        let mut add_bounds = NodeMap();
        for pred in &g.where_clause.predicates {
            if let WherePredicate::BoundPredicate(ref bound_pred) = *pred {
                'next_bound: for bound in &bound_pred.bounds {
                    if let TraitTyParamBound(_, TraitBoundModifier::Maybe) = *bound {
                        let report_error = |this: &mut Self| {
                            this.diagnostic().span_err(bound_pred.bounded_ty.span,
                                                       "`?Trait` bounds are only permitted at the \
                                                        point where a type parameter is declared");
                        };
                        // Check if the where clause type is a plain type parameter.
                        match bound_pred.bounded_ty.node {
                            TyKind::Path(None, ref path)
                                    if path.segments.len() == 1 &&
                                       bound_pred.bound_lifetimes.is_empty() => {
                                if let Some(Def::TyParam(def_id)) =
                                        self.resolver.get_resolution(bound_pred.bounded_ty.id)
                                                     .map(|d| d.base_def()) {
                                    if let Some(node_id) =
                                            self.resolver.definitions().as_local_node_id(def_id) {
                                        for ty_param in &g.ty_params {
                                            if node_id == ty_param.id {
                                                add_bounds.entry(ty_param.id).or_insert(Vec::new())
                                                                            .push(bound.clone());
                                                continue 'next_bound;
                                            }
                                        }
                                    }
                                }
                                report_error(self)
                            }
                            _ => report_error(self)
                        }
                    }
                }
            }
        }

        hir::Generics {
            ty_params: self.lower_ty_params(&g.ty_params, &add_bounds),
            lifetimes: self.lower_lifetime_defs(&g.lifetimes),
            where_clause: self.lower_where_clause(&g.where_clause),
            span: g.span,
        }
    }

    fn lower_where_clause(&mut self, wc: &WhereClause) -> hir::WhereClause {
        hir::WhereClause {
            id: self.lower_node_id(wc.id).node_id,
            predicates: wc.predicates
                          .iter()
                          .map(|predicate| self.lower_where_predicate(predicate))
                          .collect(),
        }
    }

    fn lower_where_predicate(&mut self, pred: &WherePredicate) -> hir::WherePredicate {
        match *pred {
            WherePredicate::BoundPredicate(WhereBoundPredicate{ ref bound_lifetimes,
                                                                ref bounded_ty,
                                                                ref bounds,
                                                                span}) => {
                self.with_in_scope_lifetime_defs(bound_lifetimes, |this| {
                    hir::WherePredicate::BoundPredicate(hir::WhereBoundPredicate {
                        bound_lifetimes: this.lower_lifetime_defs(bound_lifetimes),
                        bounded_ty: this.lower_ty(bounded_ty, ImplTraitContext::Disallowed),
                        bounds: bounds.iter().filter_map(|bound| match *bound {
                            // Ignore `?Trait` bounds.
                            // Tthey were copied into type parameters already.
                            TraitTyParamBound(_, TraitBoundModifier::Maybe) => None,
                            _ => Some(this.lower_ty_param_bound(
                                    bound, ImplTraitContext::Disallowed))
                        }).collect(),
                        span,
                    })
                })
            }
            WherePredicate::RegionPredicate(WhereRegionPredicate{ ref lifetime,
                                                                  ref bounds,
                                                                  span}) => {
                hir::WherePredicate::RegionPredicate(hir::WhereRegionPredicate {
                    span,
                    lifetime: self.lower_lifetime(lifetime),
                    bounds: bounds.iter().map(|bound| self.lower_lifetime(bound)).collect(),
                })
            }
            WherePredicate::EqPredicate(WhereEqPredicate{ id,
                                                          ref lhs_ty,
                                                          ref rhs_ty,
                                                          span}) => {
                hir::WherePredicate::EqPredicate(hir::WhereEqPredicate {
                    id: self.lower_node_id(id).node_id,
                    lhs_ty: self.lower_ty(lhs_ty, ImplTraitContext::Disallowed),
                    rhs_ty: self.lower_ty(rhs_ty, ImplTraitContext::Disallowed),
                    span,
                })
            }
        }
    }

    fn lower_variant_data(&mut self, vdata: &VariantData) -> hir::VariantData {
        match *vdata {
            VariantData::Struct(ref fields, id) => {
                hir::VariantData::Struct(fields.iter()
                                               .enumerate()
                                               .map(|f| self.lower_struct_field(f))
                                               .collect(),
                                         self.lower_node_id(id).node_id)
            }
            VariantData::Tuple(ref fields, id) => {
                hir::VariantData::Tuple(fields.iter()
                                              .enumerate()
                                              .map(|f| self.lower_struct_field(f))
                                              .collect(),
                                        self.lower_node_id(id).node_id)
            }
            VariantData::Unit(id) => hir::VariantData::Unit(self.lower_node_id(id).node_id),
        }
    }

    fn lower_trait_ref(&mut self, p: &TraitRef, itctx: ImplTraitContext) -> hir::TraitRef {
        let path = match self.lower_qpath(p.ref_id, &None, &p.path, ParamMode::Explicit, itctx) {
            hir::QPath::Resolved(None, path) => path.and_then(|path| path),
            qpath => bug!("lower_trait_ref: unexpected QPath `{:?}`", qpath)
        };
        hir::TraitRef {
            path,
            ref_id: self.lower_node_id(p.ref_id).node_id,
        }
    }

    fn lower_poly_trait_ref(&mut self,
                            p: &PolyTraitRef,
                            itctx: ImplTraitContext)
                            -> hir::PolyTraitRef {
        let bound_lifetimes = self.lower_lifetime_defs(&p.bound_lifetimes);
        let trait_ref = self.with_parent_impl_lifetime_defs(&bound_lifetimes,
                                |this| this.lower_trait_ref(&p.trait_ref, itctx));

        hir::PolyTraitRef {
            bound_lifetimes,
            trait_ref,
            span: p.span,
        }
    }

    fn lower_struct_field(&mut self, (index, f): (usize, &StructField)) -> hir::StructField {
        hir::StructField {
            span: f.span,
            id: self.lower_node_id(f.id).node_id,
            name: self.lower_ident(match f.ident {
                Some(ident) => ident,
                // FIXME(jseyfried) positional field hygiene
                None => Ident { name: Symbol::intern(&index.to_string()), ctxt: f.span.ctxt() },
            }),
            vis: self.lower_visibility(&f.vis, None),
            ty: self.lower_ty(&f.ty, ImplTraitContext::Disallowed),
            attrs: self.lower_attrs(&f.attrs),
        }
    }

    fn lower_field(&mut self, f: &Field) -> hir::Field {
        hir::Field {
            name: respan(f.ident.span, self.lower_ident(f.ident.node)),
            expr: P(self.lower_expr(&f.expr)),
            span: f.span,
            is_shorthand: f.is_shorthand,
        }
    }

    fn lower_mt(&mut self, mt: &MutTy, itctx: ImplTraitContext) -> hir::MutTy {
        hir::MutTy {
            ty: self.lower_ty(&mt.ty, itctx),
            mutbl: self.lower_mutability(mt.mutbl),
        }
    }

    fn lower_bounds(&mut self, bounds: &[TyParamBound], itctx: ImplTraitContext)
                    -> hir::TyParamBounds {
        bounds.iter().map(|bound| self.lower_ty_param_bound(bound, itctx)).collect()
    }

    fn lower_block(&mut self, b: &Block, targeted_by_break: bool) -> P<hir::Block> {
        let mut expr = None;

        let mut stmts = vec![];

        for (index, stmt) in b.stmts.iter().enumerate() {
            if index == b.stmts.len() - 1 {
                if let StmtKind::Expr(ref e) = stmt.node {
                    expr = Some(P(self.lower_expr(e)));
                } else {
                    stmts.extend(self.lower_stmt(stmt));
                }
            } else {
                stmts.extend(self.lower_stmt(stmt));
            }
        }

        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(b.id);

        P(hir::Block {
            id: node_id,
            hir_id,
            stmts: stmts.into(),
            expr,
            rules: self.lower_block_check_mode(&b.rules),
            span: b.span,
            targeted_by_break,
        })
    }

    fn lower_item_kind(&mut self,
                       id: NodeId,
                       name: &mut Name,
                       attrs: &hir::HirVec<Attribute>,
                       vis: &mut hir::Visibility,
                       i: &ItemKind)
                       -> hir::Item_ {
        match *i {
            ItemKind::ExternCrate(string) => hir::ItemExternCrate(string),
            ItemKind::Use(ref use_tree) => {
                // Start with an empty prefix
                let prefix = Path {
                    segments: vec![],
                    span: use_tree.span,
                };

                self.lower_use_tree(use_tree, &prefix, id, vis, name, attrs)
            }
            ItemKind::Static(ref t, m, ref e) => {
                let value = self.lower_body(None, |this| this.lower_expr(e));
                hir::ItemStatic(self.lower_ty(t, ImplTraitContext::Disallowed),
                                self.lower_mutability(m),
                                value)
            }
            ItemKind::Const(ref t, ref e) => {
                let value = self.lower_body(None, |this| this.lower_expr(e));
                hir::ItemConst(self.lower_ty(t, ImplTraitContext::Disallowed), value)
            }
            ItemKind::Fn(ref decl, unsafety, constness, abi, ref generics, ref body) => {
                let fn_def_id = self.resolver.definitions().opt_local_def_id(id);
                self.with_new_scopes(|this| {
                    let body_id = this.lower_body(Some(decl), |this| {
                        let body = this.lower_block(body, false);
                        this.expr_block(body, ThinVec::new())
                    });
                    let (generics, fn_decl) =
                        this.add_in_band_lifetime_defs(generics, fn_def_id, |this|
                            this.lower_fn_decl(decl, fn_def_id, true));

                    hir::ItemFn(fn_decl,
                                this.lower_unsafety(unsafety),
                                this.lower_constness(constness),
                                abi,
                                generics,
                                body_id)
                })
            }
            ItemKind::Mod(ref m) => hir::ItemMod(self.lower_mod(m)),
            ItemKind::ForeignMod(ref nm) => hir::ItemForeignMod(self.lower_foreign_mod(nm)),
            ItemKind::GlobalAsm(ref ga) => hir::ItemGlobalAsm(self.lower_global_asm(ga)),
            ItemKind::Ty(ref t, ref generics) => {
                hir::ItemTy(self.lower_ty(t, ImplTraitContext::Disallowed),
                            self.lower_generics(generics))
            }
            ItemKind::Enum(ref enum_definition, ref generics) => {
                hir::ItemEnum(hir::EnumDef {
                                  variants: enum_definition.variants
                                                           .iter()
                                                           .map(|x| self.lower_variant(x))
                                                           .collect(),
                              },
                              self.lower_generics(generics))
            }
            ItemKind::Struct(ref struct_def, ref generics) => {
                let struct_def = self.lower_variant_data(struct_def);
                hir::ItemStruct(struct_def, self.lower_generics(generics))
            }
            ItemKind::Union(ref vdata, ref generics) => {
                let vdata = self.lower_variant_data(vdata);
                hir::ItemUnion(vdata, self.lower_generics(generics))
            }
            ItemKind::AutoImpl(unsafety, ref trait_ref) => {
                let trait_ref = self.lower_trait_ref(trait_ref, ImplTraitContext::Disallowed);

                if let Def::Trait(def_id) = trait_ref.path.def {
                    self.trait_auto_impl.insert(def_id, id);
                }

                hir::ItemAutoImpl(self.lower_unsafety(unsafety),
                                     trait_ref)
            }
            ItemKind::Impl(unsafety,
                           polarity,
                           defaultness,
                           ref ast_generics,
                           ref ifce,
                           ref ty,
                           ref impl_items) => {
                let def_id = self.resolver.definitions().opt_local_def_id(id);
                let (generics, (ifce, lowered_ty)) =
                    self.add_in_band_lifetime_defs(ast_generics, def_id, |this| {
                        let ifce = ifce.as_ref().map(|trait_ref| {
                            this.lower_trait_ref(trait_ref, ImplTraitContext::Disallowed)
                        });

                        if let Some(ref trait_ref) = ifce {
                            if let Def::Trait(def_id) = trait_ref.path.def {
                                this.trait_impls.entry(def_id).or_insert(vec![]).push(id);
                            }
                        }

                        let lowered_ty = this.lower_ty(ty, ImplTraitContext::Disallowed);

                        (ifce, lowered_ty)
                    });

                let new_impl_items = self.with_in_scope_lifetime_defs(
                                            &ast_generics.lifetimes, |this| {
                    impl_items.iter()
                              .map(|item| this.lower_impl_item_ref(item))
                              .collect()
                });


                hir::ItemImpl(self.lower_unsafety(unsafety),
                              self.lower_impl_polarity(polarity),
                              self.lower_defaultness(defaultness, true /* [1] */),
                              generics,
                              ifce,
                              lowered_ty,
                              new_impl_items)
            }
            ItemKind::Trait(is_auto, unsafety, ref generics, ref bounds, ref items) => {
                let bounds = self.lower_bounds(bounds, ImplTraitContext::Disallowed);
                let items = items.iter().map(|item| self.lower_trait_item_ref(item)).collect();
                hir::ItemTrait(self.lower_is_auto(is_auto),
                               self.lower_unsafety(unsafety),
                               self.lower_generics(generics),
                               bounds,
                               items)
            }
            ItemKind::TraitAlias(ref generics, ref bounds) => {
                hir::ItemTraitAlias(self.lower_generics(generics),
                                    self.lower_bounds(bounds, ImplTraitContext::Disallowed))
            }
            ItemKind::MacroDef(..) | ItemKind::Mac(..) => panic!("Shouldn't still be around"),
        }

        // [1] `defaultness.has_value()` is never called for an `impl`, always `true` in order to
        //     not cause an assertion failure inside the `lower_defaultness` function
    }

    fn lower_use_tree(&mut self,
                       tree: &UseTree,
                       prefix: &Path,
                       id: NodeId,
                       vis: &mut hir::Visibility,
                       name: &mut Name,
                       attrs: &hir::HirVec<Attribute>)
                       -> hir::Item_ {
        let path = &tree.prefix;

        match tree.kind {
            UseTreeKind::Simple(ident) => {
                *name = ident.name;

                // First apply the prefix to the path
                let mut path = Path {
                    segments: prefix.segments
                        .iter()
                        .chain(path.segments.iter())
                        .cloned()
                        .collect(),
                    span: path.span.to(prefix.span),
                };

                // Correctly resolve `self` imports
                if path.segments.last().unwrap().identifier.name == keywords::SelfValue.name() {
                    let _ = path.segments.pop();
                    if ident.name == keywords::SelfValue.name() {
                        *name = path.segments.last().unwrap().identifier.name;
                    }
                }

                let path = P(self.lower_path(id, &path, ParamMode::Explicit, true));
                hir::ItemUse(path, hir::UseKind::Single)
            }
            UseTreeKind::Glob => {
                let path = P(self.lower_path(id, &Path {
                    segments: prefix.segments
                        .iter()
                        .chain(path.segments.iter())
                        .cloned()
                        .collect(),
                    span: path.span,
                }, ParamMode::Explicit, true));
                hir::ItemUse(path, hir::UseKind::Glob)
            }
            UseTreeKind::Nested(ref trees) => {
                let prefix = Path {
                    segments: prefix.segments
                        .iter()
                        .chain(path.segments.iter())
                        .cloned()
                        .collect(),
                    span: prefix.span.to(path.span),
                };

                // Add all the nested PathListItems in the HIR
                for &(ref use_tree, id) in trees {
                    self.allocate_hir_id_counter(id, &use_tree);
                    let LoweredNodeId {
                        node_id: new_id,
                        hir_id: new_hir_id,
                    } = self.lower_node_id(id);

                    let mut vis = vis.clone();
                    let mut name = name.clone();
                    let item = self.lower_use_tree(
                        use_tree, &prefix, new_id, &mut vis, &mut name, &attrs,
                    );

                    self.with_hir_id_owner(new_id, |this| {
                        let vis = match vis {
                            hir::Visibility::Public => hir::Visibility::Public,
                            hir::Visibility::Crate => hir::Visibility::Crate,
                            hir::Visibility::Inherited => hir::Visibility::Inherited,
                            hir::Visibility::Restricted { ref path, id: _  } => {
                                hir::Visibility::Restricted {
                                    path: path.clone(),
                                    // We are allocating a new NodeId here
                                    id: this.next_id().node_id,
                                }
                            }
                        };

                        this.items.insert(new_id, hir::Item {
                            id: new_id,
                            hir_id: new_hir_id,
                            name: name,
                            attrs: attrs.clone(),
                            node: item,
                            vis,
                            span: use_tree.span,
                        });
                    });
                }

                // Privatize the degenerate import base, used only to check
                // the stability of `use a::{};`, to avoid it showing up as
                // a reexport by accident when `pub`, e.g. in documentation.
                let path = P(self.lower_path(id, &prefix, ParamMode::Explicit, true));
                *vis = hir::Inherited;
                hir::ItemUse(path, hir::UseKind::ListStem)
            }
        }
    }

    fn lower_trait_item(&mut self, i: &TraitItem) -> hir::TraitItem {
        self.with_parent_def(i.id, |this| {
            let LoweredNodeId { node_id, hir_id } = this.lower_node_id(i.id);
            let fn_def_id = this.resolver.definitions().opt_local_def_id(node_id);

            let (generics, node) = match i.node {
                TraitItemKind::Const(ref ty, ref default) => {
                    (
                        this.lower_generics(&i.generics),
                        hir::TraitItemKind::Const(
                            this.lower_ty(ty, ImplTraitContext::Disallowed),
                            default.as_ref().map(|x| {
                                this.lower_body(None, |this| this.lower_expr(x))
                            }))
                    )
                }
                TraitItemKind::Method(ref sig, None) => {
                    let names = this.lower_fn_args_to_names(&sig.decl);
                    this.add_in_band_lifetime_defs(&i.generics, fn_def_id, |this|
                        hir::TraitItemKind::Method(
                            this.lower_method_sig(sig, fn_def_id, false),
                            hir::TraitMethod::Required(names)))
                }
                TraitItemKind::Method(ref sig, Some(ref body)) => {
                    let body_id = this.lower_body(Some(&sig.decl), |this| {
                        let body = this.lower_block(body, false);
                        this.expr_block(body, ThinVec::new())
                    });

                    this.add_in_band_lifetime_defs(&i.generics, fn_def_id, |this|
                        hir::TraitItemKind::Method(
                            this.lower_method_sig(sig, fn_def_id, false),
                           hir::TraitMethod::Provided(body_id)))
                }
                TraitItemKind::Type(ref bounds, ref default) => {
                    (
                        this.lower_generics(&i.generics),
                        hir::TraitItemKind::Type(
                            this.lower_bounds(bounds, ImplTraitContext::Disallowed),
                            default.as_ref().map(|x| {
                                this.lower_ty(x, ImplTraitContext::Disallowed)
                            }))
                    )
                }
                TraitItemKind::Macro(..) => panic!("Shouldn't exist any more"),
            };

            hir::TraitItem {
                id: node_id,
                hir_id,
                name: this.lower_ident(i.ident),
                attrs: this.lower_attrs(&i.attrs),
                generics,
                node,
                span: i.span,
            }
        })
    }

    fn lower_trait_item_ref(&mut self, i: &TraitItem) -> hir::TraitItemRef {
        let (kind, has_default) = match i.node {
            TraitItemKind::Const(_, ref default) => {
                (hir::AssociatedItemKind::Const, default.is_some())
            }
            TraitItemKind::Type(_, ref default) => {
                (hir::AssociatedItemKind::Type, default.is_some())
            }
            TraitItemKind::Method(ref sig, ref default) => {
                (hir::AssociatedItemKind::Method {
                    has_self: sig.decl.has_self(),
                 }, default.is_some())
            }
            TraitItemKind::Macro(..) => unimplemented!(),
        };
        hir::TraitItemRef {
            id: hir::TraitItemId { node_id: i.id },
            name: self.lower_ident(i.ident),
            span: i.span,
            defaultness: self.lower_defaultness(Defaultness::Default, has_default),
            kind,
        }
    }

    fn lower_impl_item(&mut self, i: &ImplItem) -> hir::ImplItem {
        self.with_parent_def(i.id, |this| {
            let LoweredNodeId { node_id, hir_id } = this.lower_node_id(i.id);
            let fn_def_id = this.resolver.definitions().opt_local_def_id(node_id);

            let (generics, node) = match i.node {
                ImplItemKind::Const(ref ty, ref expr) => {
                    let body_id = this.lower_body(None, |this| this.lower_expr(expr));
                    (
                        this.lower_generics(&i.generics),
                        hir::ImplItemKind::Const(
                            this.lower_ty(ty, ImplTraitContext::Disallowed),
                            body_id
                        )
                    )
                }
                ImplItemKind::Method(ref sig, ref body) => {
                    let body_id = this.lower_body(Some(&sig.decl), |this| {
                        let body = this.lower_block(body, false);
                        this.expr_block(body, ThinVec::new())
                    });
                    let impl_trait_return_allow = !this.is_in_trait_impl;

                    this.add_in_band_lifetime_defs(&i.generics, fn_def_id, |this|
                        hir::ImplItemKind::Method(
                            this.lower_method_sig(sig, fn_def_id, impl_trait_return_allow),
                            body_id))
                }
                ImplItemKind::Type(ref ty) => (
                    this.lower_generics(&i.generics),
                    hir::ImplItemKind::Type(
                        this.lower_ty(ty, ImplTraitContext::Disallowed)),
                ),
                ImplItemKind::Macro(..) => panic!("Shouldn't exist any more"),
            };

            hir::ImplItem {
                id: node_id,
                hir_id,
                name: this.lower_ident(i.ident),
                attrs: this.lower_attrs(&i.attrs),
                generics,
                vis: this.lower_visibility(&i.vis, None),
                defaultness: this.lower_defaultness(i.defaultness, true /* [1] */),
                node,
                span: i.span,
            }
        })

        // [1] since `default impl` is not yet implemented, this is always true in impls
    }

    fn lower_impl_item_ref(&mut self, i: &ImplItem) -> hir::ImplItemRef {
        hir::ImplItemRef {
            id: hir::ImplItemId { node_id: i.id },
            name: self.lower_ident(i.ident),
            span: i.span,
            vis: self.lower_visibility(&i.vis, Some(i.id)),
            defaultness: self.lower_defaultness(i.defaultness, true /* [1] */),
            kind: match i.node {
                ImplItemKind::Const(..) => hir::AssociatedItemKind::Const,
                ImplItemKind::Type(..) => hir::AssociatedItemKind::Type,
                ImplItemKind::Method(ref sig, _) => {
                    hir::AssociatedItemKind::Method {
                        has_self: sig.decl.has_self(),
                    }
                },
                ImplItemKind::Macro(..) => unimplemented!(),
            },
        }

        // [1] since `default impl` is not yet implemented, this is always true in impls
    }

    fn lower_mod(&mut self, m: &Mod) -> hir::Mod {
        hir::Mod {
            inner: m.inner,
            item_ids: m.items.iter().flat_map(|x| self.lower_item_id(x)).collect(),
        }
    }

    fn lower_item_id(&mut self, i: &Item) -> SmallVector<hir::ItemId> {
        match i.node {
            ItemKind::Use(ref use_tree) => {
                let mut vec = SmallVector::one(hir::ItemId { id: i.id });
                self.lower_item_id_use_tree(use_tree, &mut vec);
                return vec;
            }
            ItemKind::MacroDef(..) => return SmallVector::new(),
            _ => {}
        }
        SmallVector::one(hir::ItemId { id: i.id })
    }

    fn lower_item_id_use_tree(&self, tree: &UseTree, vec: &mut SmallVector<hir::ItemId>) {
        match tree.kind {
            UseTreeKind::Nested(ref nested_vec) => {
                for &(ref nested, id) in nested_vec {
                    vec.push(hir::ItemId { id, });
                    self.lower_item_id_use_tree(nested, vec);
                }
            }
            UseTreeKind::Glob => {}
            UseTreeKind::Simple(..) => {}
        }
    }

    pub fn lower_item(&mut self, i: &Item) -> Option<hir::Item> {
        let mut name = i.ident.name;
        let mut vis = self.lower_visibility(&i.vis, None);
        let attrs = self.lower_attrs(&i.attrs);
        if let ItemKind::MacroDef(ref def) = i.node {
            if !def.legacy || i.attrs.iter().any(|attr| attr.path == "macro_export") {
                let body = self.lower_token_stream(def.stream());
                self.exported_macros.push(hir::MacroDef {
                    name,
                    vis,
                    attrs,
                    id: i.id,
                    span: i.span,
                    body,
                    legacy: def.legacy,
                });
            }
            return None;
        }

        let node = self.with_parent_def(i.id, |this| {
            this.lower_item_kind(i.id, &mut name, &attrs, &mut vis, &i.node)
        });

        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(i.id);

        Some(hir::Item {
            id: node_id,
            hir_id,
            name,
            attrs,
            node,
            vis,
            span: i.span,
        })
    }

    fn lower_foreign_item(&mut self, i: &ForeignItem) -> hir::ForeignItem {
        self.with_parent_def(i.id, |this| {
            let node_id = this.lower_node_id(i.id).node_id;
            let def_id = this.resolver.definitions().local_def_id(node_id);
            hir::ForeignItem {
                id: node_id,
                name: i.ident.name,
                attrs: this.lower_attrs(&i.attrs),
                node: match i.node {
                    ForeignItemKind::Fn(ref fdec, ref generics) => {
                        // Disallow impl Trait in foreign items
                        let (generics, (fn_dec, fn_args)) =
                            this.add_in_band_lifetime_defs(
                                generics,
                                Some(def_id),
                                |this| (
                                    this.lower_fn_decl(fdec, None, false),
                                    this.lower_fn_args_to_names(fdec)
                                )
                            );

                        hir::ForeignItemFn(fn_dec, fn_args, generics)
                    }
                    ForeignItemKind::Static(ref t, m) => {
                        hir::ForeignItemStatic(this.lower_ty(t, ImplTraitContext::Disallowed), m)
                    }
                    ForeignItemKind::Ty => {
                        hir::ForeignItemType
                    }
                },
                vis: this.lower_visibility(&i.vis, None),
                span: i.span,
            }
        })
    }

    fn lower_method_sig(&mut self,
                        sig: &MethodSig,
                        fn_def_id: Option<DefId>,
                        impl_trait_return_allow: bool)
                        -> hir::MethodSig {
        hir::MethodSig {
            abi: sig.abi,
            unsafety: self.lower_unsafety(sig.unsafety),
            constness: self.lower_constness(sig.constness),
            decl: self.lower_fn_decl(&sig.decl, fn_def_id, impl_trait_return_allow),
        }
    }

    fn lower_is_auto(&mut self, a: IsAuto) -> hir::IsAuto {
        match a {
            IsAuto::Yes => hir::IsAuto::Yes,
            IsAuto::No => hir::IsAuto::No,
        }
    }

    fn lower_unsafety(&mut self, u: Unsafety) -> hir::Unsafety {
        match u {
            Unsafety::Unsafe => hir::Unsafety::Unsafe,
            Unsafety::Normal => hir::Unsafety::Normal,
        }
    }

    fn lower_constness(&mut self, c: Spanned<Constness>) -> hir::Constness {
        match c.node {
            Constness::Const => hir::Constness::Const,
            Constness::NotConst => hir::Constness::NotConst,
        }
    }

    fn lower_unop(&mut self, u: UnOp) -> hir::UnOp {
        match u {
            UnOp::Deref => hir::UnDeref,
            UnOp::Not => hir::UnNot,
            UnOp::Neg => hir::UnNeg,
        }
    }

    fn lower_binop(&mut self, b: BinOp) -> hir::BinOp {
        Spanned {
            node: match b.node {
                BinOpKind::Add => hir::BiAdd,
                BinOpKind::Sub => hir::BiSub,
                BinOpKind::Mul => hir::BiMul,
                BinOpKind::Div => hir::BiDiv,
                BinOpKind::Rem => hir::BiRem,
                BinOpKind::And => hir::BiAnd,
                BinOpKind::Or => hir::BiOr,
                BinOpKind::BitXor => hir::BiBitXor,
                BinOpKind::BitAnd => hir::BiBitAnd,
                BinOpKind::BitOr => hir::BiBitOr,
                BinOpKind::Shl => hir::BiShl,
                BinOpKind::Shr => hir::BiShr,
                BinOpKind::Eq => hir::BiEq,
                BinOpKind::Lt => hir::BiLt,
                BinOpKind::Le => hir::BiLe,
                BinOpKind::Ne => hir::BiNe,
                BinOpKind::Ge => hir::BiGe,
                BinOpKind::Gt => hir::BiGt,
            },
            span: b.span,
        }
    }

    fn lower_pat(&mut self, p: &Pat) -> P<hir::Pat> {
        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(p.id);

        P(hir::Pat {
            id: node_id,
            hir_id,
            node: match p.node {
                PatKind::Wild => hir::PatKind::Wild,
                PatKind::Ident(ref binding_mode, pth1, ref sub) => {
                    match self.resolver.get_resolution(p.id).map(|d| d.base_def()) {
                        // `None` can occur in body-less function signatures
                        def @ None | def @ Some(Def::Local(_)) => {
                            let canonical_id = match def {
                                Some(Def::Local(id)) => id,
                                _ => p.id
                            };
                            hir::PatKind::Binding(self.lower_binding_mode(binding_mode),
                                                  canonical_id,
                                                  respan(pth1.span, pth1.node.name),
                                                  sub.as_ref().map(|x| self.lower_pat(x)))
                        }
                        Some(def) => {
                            hir::PatKind::Path(hir::QPath::Resolved(None, P(hir::Path {
                                span: pth1.span,
                                def,
                                segments: hir_vec![
                                    hir::PathSegment::from_name(pth1.node.name)
                                ],
                            })))
                        }
                    }
                }
                PatKind::Lit(ref e) => hir::PatKind::Lit(P(self.lower_expr(e))),
                PatKind::TupleStruct(ref path, ref pats, ddpos) => {
                    let qpath = self.lower_qpath(p.id, &None, path, ParamMode::Optional,
                                                 ImplTraitContext::Disallowed);
                    hir::PatKind::TupleStruct(qpath,
                                              pats.iter().map(|x| self.lower_pat(x)).collect(),
                                              ddpos)
                }
                PatKind::Path(ref qself, ref path) => {
                    hir::PatKind::Path(self.lower_qpath(p.id, qself, path, ParamMode::Optional,
                                                        ImplTraitContext::Disallowed))
                }
                PatKind::Struct(ref path, ref fields, etc) => {
                    let qpath = self.lower_qpath(p.id, &None, path, ParamMode::Optional,
                                                 ImplTraitContext::Disallowed);

                    let fs = fields.iter()
                                   .map(|f| {
                                       Spanned {
                                           span: f.span,
                                           node: hir::FieldPat {
                                               name: self.lower_ident(f.node.ident),
                                               pat: self.lower_pat(&f.node.pat),
                                               is_shorthand: f.node.is_shorthand,
                                           },
                                       }
                                   })
                                   .collect();
                    hir::PatKind::Struct(qpath, fs, etc)
                }
                PatKind::Tuple(ref elts, ddpos) => {
                    hir::PatKind::Tuple(elts.iter().map(|x| self.lower_pat(x)).collect(), ddpos)
                }
                PatKind::Box(ref inner) => hir::PatKind::Box(self.lower_pat(inner)),
                PatKind::Ref(ref inner, mutbl) => {
                    hir::PatKind::Ref(self.lower_pat(inner), self.lower_mutability(mutbl))
                }
                PatKind::Range(ref e1, ref e2, ref end) => {
                    hir::PatKind::Range(P(self.lower_expr(e1)),
                                        P(self.lower_expr(e2)),
                                        self.lower_range_end(end))
                }
                PatKind::Slice(ref before, ref slice, ref after) => {
                    hir::PatKind::Slice(before.iter().map(|x| self.lower_pat(x)).collect(),
                                slice.as_ref().map(|x| self.lower_pat(x)),
                                after.iter().map(|x| self.lower_pat(x)).collect())
                }
                PatKind::Mac(_) => panic!("Shouldn't exist here"),
            },
            span: p.span,
        })
    }

    fn lower_range_end(&mut self, e: &RangeEnd) -> hir::RangeEnd {
        match *e {
            RangeEnd::Included(_) => hir::RangeEnd::Included,
            RangeEnd::Excluded => hir::RangeEnd::Excluded,
        }
    }

    fn lower_expr(&mut self, e: &Expr) -> hir::Expr {
        let kind = match e.node {
            // Issue #22181:
            // Eventually a desugaring for `box EXPR`
            // (similar to the desugaring above for `in PLACE BLOCK`)
            // should go here, desugaring
            //
            // to:
            //
            // let mut place = BoxPlace::make_place();
            // let raw_place = Place::pointer(&mut place);
            // let value = $value;
            // unsafe {
            //     ::std::ptr::write(raw_place, value);
            //     Boxed::finalize(place)
            // }
            //
            // But for now there are type-inference issues doing that.
            ExprKind::Box(ref inner) => {
                hir::ExprBox(P(self.lower_expr(inner)))
            }

            // Desugar ExprBox: `in (PLACE) EXPR`
            ExprKind::InPlace(ref placer, ref value_expr) => {
                // to:
                //
                // let p = PLACE;
                // let mut place = Placer::make_place(p);
                // let raw_place = Place::pointer(&mut place);
                // push_unsafe!({
                //     std::intrinsics::move_val_init(raw_place, pop_unsafe!( EXPR ));
                //     InPlace::finalize(place)
                // })
                let placer_expr = P(self.lower_expr(placer));
                let value_expr = P(self.lower_expr(value_expr));

                let placer_ident = self.str_to_ident("placer");
                let place_ident = self.str_to_ident("place");
                let p_ptr_ident = self.str_to_ident("p_ptr");

                let make_place = ["ops", "Placer", "make_place"];
                let place_pointer = ["ops", "Place", "pointer"];
                let move_val_init = ["intrinsics", "move_val_init"];
                let inplace_finalize = ["ops", "InPlace", "finalize"];

                let unstable_span =
                    self.allow_internal_unstable(CompilerDesugaringKind::BackArrow, e.span);
                let make_call = |this: &mut LoweringContext, p, args| {
                    let path = P(this.expr_std_path(unstable_span, p, ThinVec::new()));
                    P(this.expr_call(e.span, path, args))
                };

                let mk_stmt_let = |this: &mut LoweringContext, bind, expr| {
                    this.stmt_let(e.span, false, bind, expr)
                };

                let mk_stmt_let_mut = |this: &mut LoweringContext, bind, expr| {
                    this.stmt_let(e.span, true, bind, expr)
                };

                // let placer = <placer_expr> ;
                let (s1, placer_binding) = {
                    mk_stmt_let(self, placer_ident, placer_expr)
                };

                // let mut place = Placer::make_place(placer);
                let (s2, place_binding) = {
                    let placer = self.expr_ident(e.span, placer_ident, placer_binding);
                    let call = make_call(self, &make_place, hir_vec![placer]);
                    mk_stmt_let_mut(self, place_ident, call)
                };

                // let p_ptr = Place::pointer(&mut place);
                let (s3, p_ptr_binding) = {
                    let agent = P(self.expr_ident(e.span, place_ident, place_binding));
                    let args = hir_vec![self.expr_mut_addr_of(e.span, agent)];
                    let call = make_call(self, &place_pointer, args);
                    mk_stmt_let(self, p_ptr_ident, call)
                };

                // pop_unsafe!(EXPR));
                let pop_unsafe_expr = {
                    self.signal_block_expr(hir_vec![],
                                           value_expr,
                                           e.span,
                                           hir::PopUnsafeBlock(hir::CompilerGenerated),
                                           ThinVec::new())
                };

                // push_unsafe!({
                //     std::intrinsics::move_val_init(raw_place, pop_unsafe!( EXPR ));
                //     InPlace::finalize(place)
                // })
                let expr = {
                    let ptr = self.expr_ident(e.span, p_ptr_ident, p_ptr_binding);
                    let call_move_val_init =
                        hir::StmtSemi(
                            make_call(self, &move_val_init, hir_vec![ptr, pop_unsafe_expr]),
                            self.next_id().node_id);
                    let call_move_val_init = respan(e.span, call_move_val_init);

                    let place = self.expr_ident(e.span, place_ident, place_binding);
                    let call = make_call(self, &inplace_finalize, hir_vec![place]);
                    P(self.signal_block_expr(hir_vec![call_move_val_init],
                                             call,
                                             e.span,
                                             hir::PushUnsafeBlock(hir::CompilerGenerated),
                                             ThinVec::new()))
                };

                let block = self.block_all(e.span, hir_vec![s1, s2, s3], Some(expr));
                hir::ExprBlock(P(block))
            }

            ExprKind::Array(ref exprs) => {
                hir::ExprArray(exprs.iter().map(|x| self.lower_expr(x)).collect())
            }
            ExprKind::Repeat(ref expr, ref count) => {
                let expr = P(self.lower_expr(expr));
                let count = self.lower_body(None, |this| this.lower_expr(count));
                hir::ExprRepeat(expr, count)
            }
            ExprKind::Tup(ref elts) => {
                hir::ExprTup(elts.iter().map(|x| self.lower_expr(x)).collect())
            }
            ExprKind::Call(ref f, ref args) => {
                let f = P(self.lower_expr(f));
                hir::ExprCall(f, args.iter().map(|x| self.lower_expr(x)).collect())
            }
            ExprKind::MethodCall(ref seg, ref args) => {
                let hir_seg = self.lower_path_segment(e.span, seg, ParamMode::Optional, 0,
                                                      ParenthesizedGenericArgs::Err,
                                                      ImplTraitContext::Disallowed);
                let args = args.iter().map(|x| self.lower_expr(x)).collect();
                hir::ExprMethodCall(hir_seg, seg.span, args)
            }
            ExprKind::Binary(binop, ref lhs, ref rhs) => {
                let binop = self.lower_binop(binop);
                let lhs = P(self.lower_expr(lhs));
                let rhs = P(self.lower_expr(rhs));
                hir::ExprBinary(binop, lhs, rhs)
            }
            ExprKind::Unary(op, ref ohs) => {
                let op = self.lower_unop(op);
                let ohs = P(self.lower_expr(ohs));
                hir::ExprUnary(op, ohs)
            }
            ExprKind::Lit(ref l) => hir::ExprLit(P((**l).clone())),
            ExprKind::Cast(ref expr, ref ty) => {
                let expr = P(self.lower_expr(expr));
                hir::ExprCast(expr, self.lower_ty(ty, ImplTraitContext::Disallowed))
            }
            ExprKind::Type(ref expr, ref ty) => {
                let expr = P(self.lower_expr(expr));
                hir::ExprType(expr, self.lower_ty(ty, ImplTraitContext::Disallowed))
            }
            ExprKind::AddrOf(m, ref ohs) => {
                let m = self.lower_mutability(m);
                let ohs = P(self.lower_expr(ohs));
                hir::ExprAddrOf(m, ohs)
            }
            // More complicated than you might expect because the else branch
            // might be `if let`.
            ExprKind::If(ref cond, ref blk, ref else_opt) => {
                let else_opt = else_opt.as_ref().map(|els| {
                    match els.node {
                        ExprKind::IfLet(..) => {
                            // wrap the if-let expr in a block
                            let span = els.span;
                            let els = P(self.lower_expr(els));
                            let LoweredNodeId {
                                node_id,
                                hir_id,
                            } = self.next_id();
                            let blk = P(hir::Block {
                                stmts: hir_vec![],
                                expr: Some(els),
                                id: node_id,
                                hir_id,
                                rules: hir::DefaultBlock,
                                span,
                                targeted_by_break: false,
                            });
                            P(self.expr_block(blk, ThinVec::new()))
                        }
                        _ => P(self.lower_expr(els)),
                    }
                });

                let then_blk = self.lower_block(blk, false);
                let then_expr = self.expr_block(then_blk, ThinVec::new());

                hir::ExprIf(P(self.lower_expr(cond)), P(then_expr), else_opt)
            }
            ExprKind::While(ref cond, ref body, opt_ident) => {
                self.with_loop_scope(e.id, |this|
                    hir::ExprWhile(
                        this.with_loop_condition_scope(|this| P(this.lower_expr(cond))),
                        this.lower_block(body, false),
                        this.lower_opt_sp_ident(opt_ident)))
            }
            ExprKind::Loop(ref body, opt_ident) => {
                self.with_loop_scope(e.id, |this|
                    hir::ExprLoop(this.lower_block(body, false),
                                  this.lower_opt_sp_ident(opt_ident),
                                  hir::LoopSource::Loop))
            }
            ExprKind::Catch(ref body) => {
                self.with_catch_scope(body.id, |this|
                    hir::ExprBlock(this.lower_block(body, true)))
            }
            ExprKind::Match(ref expr, ref arms) => {
                hir::ExprMatch(P(self.lower_expr(expr)),
                               arms.iter().map(|x| self.lower_arm(x)).collect(),
                               hir::MatchSource::Normal)
            }
            ExprKind::Closure(capture_clause, ref decl, ref body, fn_decl_span) => {
                self.with_new_scopes(|this| {
                    this.with_parent_def(e.id, |this| {
                        let mut is_generator = false;
                        let body_id = this.lower_body(Some(decl), |this| {
                            let e = this.lower_expr(body);
                            is_generator = this.is_generator;
                            e
                        });
                        if is_generator && !decl.inputs.is_empty() {
                            span_err!(this.sess, fn_decl_span, E0628,
                                      "generators cannot have explicit arguments");
                            this.sess.abort_if_errors();
                        }
                        hir::ExprClosure(this.lower_capture_clause(capture_clause),
                                         this.lower_fn_decl(decl, None, false),
                                         body_id,
                                         fn_decl_span,
                                         is_generator)
                    })
                })
            }
            ExprKind::Block(ref blk) => hir::ExprBlock(self.lower_block(blk, false)),
            ExprKind::Assign(ref el, ref er) => {
                hir::ExprAssign(P(self.lower_expr(el)), P(self.lower_expr(er)))
            }
            ExprKind::AssignOp(op, ref el, ref er) => {
                hir::ExprAssignOp(self.lower_binop(op),
                                  P(self.lower_expr(el)),
                                  P(self.lower_expr(er)))
            }
            ExprKind::Field(ref el, ident) => {
                hir::ExprField(P(self.lower_expr(el)),
                               respan(ident.span, self.lower_ident(ident.node)))
            }
            ExprKind::TupField(ref el, ident) => {
                hir::ExprTupField(P(self.lower_expr(el)), ident)
            }
            ExprKind::Index(ref el, ref er) => {
                hir::ExprIndex(P(self.lower_expr(el)), P(self.lower_expr(er)))
            }
            ExprKind::Range(ref e1, ref e2, lims) => {
                use syntax::ast::RangeLimits::*;

                let path = match (e1, e2, lims) {
                    (&None, &None, HalfOpen) => "RangeFull",
                    (&Some(..), &None, HalfOpen) => "RangeFrom",
                    (&None, &Some(..), HalfOpen) => "RangeTo",
                    (&Some(..), &Some(..), HalfOpen) => "Range",
                    (&None, &Some(..), Closed) => "RangeToInclusive",
                    (&Some(..), &Some(..), Closed) => "RangeInclusive",
                    (_, &None, Closed) =>
                        panic!(self.diagnostic().span_fatal(
                            e.span, "inclusive range with no end")),
                };

                let fields =
                    e1.iter().map(|e| ("start", e)).chain(e2.iter().map(|e| ("end", e)))
                    .map(|(s, e)| {
                        let expr = P(self.lower_expr(&e));
                        let unstable_span =
                            self.allow_internal_unstable(CompilerDesugaringKind::DotFill, e.span);
                        self.field(Symbol::intern(s), expr, unstable_span)
                    }).collect::<P<[hir::Field]>>();

                let is_unit = fields.is_empty();
                let unstable_span =
                    self.allow_internal_unstable(CompilerDesugaringKind::DotFill, e.span);
                let struct_path =
                    iter::once("ops").chain(iter::once(path))
                    .collect::<Vec<_>>();
                let struct_path = self.std_path(unstable_span, &struct_path, is_unit);
                let struct_path = hir::QPath::Resolved(None, P(struct_path));

                let LoweredNodeId { node_id, hir_id } = self.lower_node_id(e.id);

                return hir::Expr {
                    id: node_id,
                    hir_id,
                    node: if is_unit {
                        hir::ExprPath(struct_path)
                    } else {
                        hir::ExprStruct(struct_path, fields, None)
                    },
                    span: unstable_span,
                    attrs: e.attrs.clone(),
                };
            }
            ExprKind::Path(ref qself, ref path) => {
                hir::ExprPath(self.lower_qpath(e.id, qself, path, ParamMode::Optional,
                                               ImplTraitContext::Disallowed))
            }
            ExprKind::Break(opt_ident, ref opt_expr) => {
                let label_result = if self.is_in_loop_condition && opt_ident.is_none() {
                    hir::Destination {
                        ident: opt_ident,
                        target_id: hir::ScopeTarget::Loop(
                                Err(hir::LoopIdError::UnlabeledCfInWhileCondition).into()),
                    }
                } else {
                    self.lower_loop_destination(opt_ident.map(|ident| (e.id, ident)))
                };
                hir::ExprBreak(
                        label_result,
                        opt_expr.as_ref().map(|x| P(self.lower_expr(x))))
            }
            ExprKind::Continue(opt_ident) =>
                hir::ExprAgain(
                    if self.is_in_loop_condition && opt_ident.is_none() {
                        hir::Destination {
                            ident: opt_ident,
                            target_id: hir::ScopeTarget::Loop(Err(
                                hir::LoopIdError::UnlabeledCfInWhileCondition).into()),
                        }
                    } else {
                        self.lower_loop_destination(opt_ident.map( |ident| (e.id, ident)))
                    }),
            ExprKind::Ret(ref e) => hir::ExprRet(e.as_ref().map(|x| P(self.lower_expr(x)))),
            ExprKind::InlineAsm(ref asm) => {
                let hir_asm = hir::InlineAsm {
                    inputs: asm.inputs.iter().map(|&(ref c, _)| c.clone()).collect(),
                    outputs: asm.outputs.iter().map(|out| {
                        hir::InlineAsmOutput {
                            constraint: out.constraint.clone(),
                            is_rw: out.is_rw,
                            is_indirect: out.is_indirect,
                        }
                    }).collect(),
                    asm: asm.asm.clone(),
                    asm_str_style: asm.asm_str_style,
                    clobbers: asm.clobbers.clone().into(),
                    volatile: asm.volatile,
                    alignstack: asm.alignstack,
                    dialect: asm.dialect,
                    ctxt: asm.ctxt,
                };
                let outputs =
                    asm.outputs.iter().map(|out| self.lower_expr(&out.expr)).collect();
                let inputs =
                    asm.inputs.iter().map(|&(_, ref input)| self.lower_expr(input)).collect();
                hir::ExprInlineAsm(P(hir_asm), outputs, inputs)
            }
            ExprKind::Struct(ref path, ref fields, ref maybe_expr) => {
                hir::ExprStruct(self.lower_qpath(e.id, &None, path, ParamMode::Optional,
                                                 ImplTraitContext::Disallowed),
                                fields.iter().map(|x| self.lower_field(x)).collect(),
                                maybe_expr.as_ref().map(|x| P(self.lower_expr(x))))
            }
            ExprKind::Paren(ref ex) => {
                let mut ex = self.lower_expr(ex);
                // include parens in span, but only if it is a super-span.
                if e.span.contains(ex.span) {
                    ex.span = e.span;
                }
                // merge attributes into the inner expression.
                let mut attrs = e.attrs.clone();
                attrs.extend::<Vec<_>>(ex.attrs.into());
                ex.attrs = attrs;
                return ex;
            }

            ExprKind::Yield(ref opt_expr) => {
                self.is_generator = true;
                let expr = opt_expr.as_ref().map(|x| self.lower_expr(x)).unwrap_or_else(|| {
                    self.expr(e.span, hir::ExprTup(hir_vec![]), ThinVec::new())
                });
                hir::ExprYield(P(expr))
            }

            // Desugar ExprIfLet
            // From: `if let <pat> = <sub_expr> <body> [<else_opt>]`
            ExprKind::IfLet(ref pat, ref sub_expr, ref body, ref else_opt) => {
                // to:
                //
                //   match <sub_expr> {
                //     <pat> => <body>,
                //     _ => [<else_opt> | ()]
                //   }

                let mut arms = vec![];

                // `<pat> => <body>`
                {
                    let body = self.lower_block(body, false);
                    let body_expr = P(self.expr_block(body, ThinVec::new()));
                    let pat = self.lower_pat(pat);
                    arms.push(self.arm(hir_vec![pat], body_expr));
                }

                // _ => [<else_opt>|()]
                {
                    let wildcard_arm: Option<&Expr> = else_opt.as_ref().map(|p| &**p);
                    let wildcard_pattern = self.pat_wild(e.span);
                    let body = if let Some(else_expr) = wildcard_arm {
                        P(self.lower_expr(else_expr))
                    } else {
                        self.expr_tuple(e.span, hir_vec![])
                    };
                    arms.push(self.arm(hir_vec![wildcard_pattern], body));
                }

                let contains_else_clause = else_opt.is_some();

                let sub_expr = P(self.lower_expr(sub_expr));

                hir::ExprMatch(
                    sub_expr,
                    arms.into(),
                    hir::MatchSource::IfLetDesugar {
                        contains_else_clause,
                    })
            }

            // Desugar ExprWhileLet
            // From: `[opt_ident]: while let <pat> = <sub_expr> <body>`
            ExprKind::WhileLet(ref pat, ref sub_expr, ref body, opt_ident) => {
                // to:
                //
                //   [opt_ident]: loop {
                //     match <sub_expr> {
                //       <pat> => <body>,
                //       _ => break
                //     }
                //   }

                // Note that the block AND the condition are evaluated in the loop scope.
                // This is done to allow `break` from inside the condition of the loop.
                let (body, break_expr, sub_expr) = self.with_loop_scope(e.id, |this| (
                    this.lower_block(body, false),
                    this.expr_break(e.span, ThinVec::new()),
                    this.with_loop_condition_scope(|this| P(this.lower_expr(sub_expr))),
                ));

                // `<pat> => <body>`
                let pat_arm = {
                    let body_expr = P(self.expr_block(body, ThinVec::new()));
                    let pat = self.lower_pat(pat);
                    self.arm(hir_vec![pat], body_expr)
                };

                // `_ => break`
                let break_arm = {
                    let pat_under = self.pat_wild(e.span);
                    self.arm(hir_vec![pat_under], break_expr)
                };

                // `match <sub_expr> { ... }`
                let arms = hir_vec![pat_arm, break_arm];
                let match_expr = self.expr(e.span,
                                           hir::ExprMatch(sub_expr,
                                                          arms,
                                                          hir::MatchSource::WhileLetDesugar),
                                           ThinVec::new());

                // `[opt_ident]: loop { ... }`
                let loop_block = P(self.block_expr(P(match_expr)));
                let loop_expr = hir::ExprLoop(loop_block, self.lower_opt_sp_ident(opt_ident),
                                              hir::LoopSource::WhileLet);
                // add attributes to the outer returned expr node
                loop_expr
            }

            // Desugar ExprForLoop
            // From: `[opt_ident]: for <pat> in <head> <body>`
            ExprKind::ForLoop(ref pat, ref head, ref body, opt_ident) => {
                // to:
                //
                //   {
                //     let result = match ::std::iter::IntoIterator::into_iter(<head>) {
                //       mut iter => {
                //         [opt_ident]: loop {
                //           let mut __next;
                //           match ::std::iter::Iterator::next(&mut iter) {
                //             ::std::option::Option::Some(val) => __next = val,
                //             ::std::option::Option::None => break
                //           };
                //           let <pat> = __next;
                //           StmtExpr(<body>);
                //         }
                //       }
                //     };
                //     result
                //   }

                // expand <head>
                let head = self.lower_expr(head);

                let iter = self.str_to_ident("iter");

                let next_ident = self.str_to_ident("__next");
                let next_pat = self.pat_ident_binding_mode(e.span,
                                                           next_ident,
                                                           hir::BindingAnnotation::Mutable);

                // `::std::option::Option::Some(val) => next = val`
                let pat_arm = {
                    let val_ident = self.str_to_ident("val");
                    let val_pat = self.pat_ident(e.span, val_ident);
                    let val_expr = P(self.expr_ident(e.span, val_ident, val_pat.id));
                    let next_expr = P(self.expr_ident(e.span, next_ident, next_pat.id));
                    let assign = P(self.expr(e.span,
                                             hir::ExprAssign(next_expr, val_expr),
                                             ThinVec::new()));
                    let some_pat = self.pat_some(e.span, val_pat);
                    self.arm(hir_vec![some_pat], assign)
                };

                // `::std::option::Option::None => break`
                let break_arm = {
                    let break_expr = self.with_loop_scope(e.id, |this|
                        this.expr_break(e.span, ThinVec::new()));
                    let pat = self.pat_none(e.span);
                    self.arm(hir_vec![pat], break_expr)
                };

                // `mut iter`
                let iter_pat = self.pat_ident_binding_mode(e.span,
                                                           iter,
                                                           hir::BindingAnnotation::Mutable);

                // `match ::std::iter::Iterator::next(&mut iter) { ... }`
                let match_expr = {
                    let iter = P(self.expr_ident(e.span, iter, iter_pat.id));
                    let ref_mut_iter = self.expr_mut_addr_of(e.span, iter);
                    let next_path = &["iter", "Iterator", "next"];
                    let next_path = P(self.expr_std_path(e.span, next_path, ThinVec::new()));
                    let next_expr = P(self.expr_call(e.span, next_path,
                                      hir_vec![ref_mut_iter]));
                    let arms = hir_vec![pat_arm, break_arm];

                    P(self.expr(e.span,
                                hir::ExprMatch(next_expr, arms,
                                               hir::MatchSource::ForLoopDesugar),
                                ThinVec::new()))
                };
                let match_stmt = respan(e.span, hir::StmtExpr(match_expr, self.next_id().node_id));

                let next_expr = P(self.expr_ident(e.span, next_ident, next_pat.id));

                // `let mut __next`
                let next_let = self.stmt_let_pat(e.span,
                    None,
                    next_pat,
                    hir::LocalSource::ForLoopDesugar);

                // `let <pat> = __next`
                let pat = self.lower_pat(pat);
                let pat_let = self.stmt_let_pat(e.span,
                    Some(next_expr),
                    pat,
                    hir::LocalSource::ForLoopDesugar);

                let body_block = self.with_loop_scope(e.id,
                                                        |this| this.lower_block(body, false));
                let body_expr = P(self.expr_block(body_block, ThinVec::new()));
                let body_stmt = respan(e.span, hir::StmtExpr(body_expr, self.next_id().node_id));

                let loop_block = P(self.block_all(e.span,
                                                  hir_vec![next_let,
                                                           match_stmt,
                                                           pat_let,
                                                           body_stmt],
                                                  None));

                // `[opt_ident]: loop { ... }`
                let loop_expr = hir::ExprLoop(loop_block, self.lower_opt_sp_ident(opt_ident),
                                              hir::LoopSource::ForLoop);
                let LoweredNodeId { node_id, hir_id } = self.lower_node_id(e.id);
                let loop_expr = P(hir::Expr {
                    id: node_id,
                    hir_id,
                    node: loop_expr,
                    span: e.span,
                    attrs: ThinVec::new(),
                });

                // `mut iter => { ... }`
                let iter_arm = self.arm(hir_vec![iter_pat], loop_expr);

                // `match ::std::iter::IntoIterator::into_iter(<head>) { ... }`
                let into_iter_expr = {
                    let into_iter_path = &["iter", "IntoIterator", "into_iter"];
                    let into_iter = P(self.expr_std_path(e.span, into_iter_path,
                                                         ThinVec::new()));
                    P(self.expr_call(e.span, into_iter, hir_vec![head]))
                };

                let match_expr = P(self.expr_match(e.span,
                                                   into_iter_expr,
                                                   hir_vec![iter_arm],
                                                   hir::MatchSource::ForLoopDesugar));

                // `{ let _result = ...; _result }`
                // underscore prevents an unused_variables lint if the head diverges
                let result_ident = self.str_to_ident("_result");
                let (let_stmt, let_stmt_binding) =
                    self.stmt_let(e.span, false, result_ident, match_expr);

                let result = P(self.expr_ident(e.span, result_ident, let_stmt_binding));
                let block = P(self.block_all(e.span, hir_vec![let_stmt], Some(result)));
                // add the attributes to the outer returned expr node
                return self.expr_block(block, e.attrs.clone());
            }

            // Desugar ExprKind::Try
            // From: `<expr>?`
            ExprKind::Try(ref sub_expr) => {
                // to:
                //
                // match Try::into_result(<expr>) {
                //     Ok(val) => #[allow(unreachable_code)] val,
                //     Err(err) => #[allow(unreachable_code)]
                //                 // If there is an enclosing `catch {...}`
                //                 break 'catch_target Try::from_error(From::from(err)),
                //                 // Otherwise
                //                 return Try::from_error(From::from(err)),
                // }

                let unstable_span =
                    self.allow_internal_unstable(CompilerDesugaringKind::QuestionMark, e.span);

                // Try::into_result(<expr>)
                let discr = {
                    // expand <expr>
                    let sub_expr = self.lower_expr(sub_expr);

                    let path = &["ops", "Try", "into_result"];
                    let path = P(self.expr_std_path(unstable_span, path, ThinVec::new()));
                    P(self.expr_call(e.span, path, hir_vec![sub_expr]))
                };

                // #[allow(unreachable_code)]
                let attr = {
                    // allow(unreachable_code)
                    let allow = {
                        let allow_ident = self.str_to_ident("allow");
                        let uc_ident = self.str_to_ident("unreachable_code");
                        let uc_meta_item = attr::mk_spanned_word_item(e.span, uc_ident);
                        let uc_nested = NestedMetaItemKind::MetaItem(uc_meta_item);
                        let uc_spanned = respan(e.span, uc_nested);
                        attr::mk_spanned_list_item(e.span, allow_ident, vec![uc_spanned])
                    };
                    attr::mk_spanned_attr_outer(e.span, attr::mk_attr_id(), allow)
                };
                let attrs = vec![attr];

                // Ok(val) => #[allow(unreachable_code)] val,
                let ok_arm = {
                    let val_ident = self.str_to_ident("val");
                    let val_pat = self.pat_ident(e.span, val_ident);
                    let val_expr = P(self.expr_ident_with_attrs(e.span,
                                                                val_ident,
                                                                val_pat.id,
                                                                ThinVec::from(attrs.clone())));
                    let ok_pat = self.pat_ok(e.span, val_pat);

                    self.arm(hir_vec![ok_pat], val_expr)
                };

                // Err(err) => #[allow(unreachable_code)]
                //             return Try::from_error(From::from(err)),
                let err_arm = {
                    let err_ident = self.str_to_ident("err");
                    let err_local = self.pat_ident(e.span, err_ident);
                    let from_expr = {
                        let path = &["convert", "From", "from"];
                        let from = P(self.expr_std_path(e.span, path, ThinVec::new()));
                        let err_expr = self.expr_ident(e.span, err_ident, err_local.id);

                        self.expr_call(e.span, from, hir_vec![err_expr])
                    };
                    let from_err_expr = {
                        let path = &["ops", "Try", "from_error"];
                        let from_err = P(self.expr_std_path(unstable_span, path,
                                                            ThinVec::new()));
                        P(self.expr_call(e.span, from_err, hir_vec![from_expr]))
                    };

                    let thin_attrs = ThinVec::from(attrs);
                    let catch_scope = self.catch_scopes.last().map(|x| *x);
                    let ret_expr = if let Some(catch_node) = catch_scope {
                        P(self.expr(
                            e.span,
                            hir::ExprBreak(
                                hir::Destination {
                                    ident: None,
                                    target_id: hir::ScopeTarget::Block(catch_node),
                                },
                                Some(from_err_expr)
                            ),
                            thin_attrs))
                    } else {
                        P(self.expr(e.span,
                                    hir::Expr_::ExprRet(Some(from_err_expr)),
                                    thin_attrs))
                    };


                    let err_pat = self.pat_err(e.span, err_local);
                    self.arm(hir_vec![err_pat], ret_expr)
                };

                hir::ExprMatch(discr,
                               hir_vec![err_arm, ok_arm],
                               hir::MatchSource::TryDesugar)
            }

            ExprKind::Mac(_) => panic!("Shouldn't exist here"),
        };

        let LoweredNodeId { node_id, hir_id } = self.lower_node_id(e.id);

        hir::Expr {
            id: node_id,
            hir_id,
            node: kind,
            span: e.span,
            attrs: e.attrs.clone(),
        }
    }

    fn lower_stmt(&mut self, s: &Stmt) -> SmallVector<hir::Stmt> {
        SmallVector::one(match s.node {
            StmtKind::Local(ref l) => Spanned {
                node: hir::StmtDecl(P(Spanned {
                    node: hir::DeclLocal(self.lower_local(l)),
                    span: s.span,
                }), self.lower_node_id(s.id).node_id),
                span: s.span,
            },
            StmtKind::Item(ref it) => {
                // Can only use the ID once.
                let mut id = Some(s.id);
                return self.lower_item_id(it).into_iter().map(|item_id| Spanned {
                    node: hir::StmtDecl(P(Spanned {
                        node: hir::DeclItem(item_id),
                        span: s.span,
                    }), id.take()
                          .map(|id| self.lower_node_id(id).node_id)
                          .unwrap_or_else(|| self.next_id().node_id)),
                    span: s.span,
                }).collect();
            }
            StmtKind::Expr(ref e) => {
                Spanned {
                    node: hir::StmtExpr(P(self.lower_expr(e)),
                                          self.lower_node_id(s.id).node_id),
                    span: s.span,
                }
            }
            StmtKind::Semi(ref e) => {
                Spanned {
                    node: hir::StmtSemi(P(self.lower_expr(e)),
                                          self.lower_node_id(s.id).node_id),
                    span: s.span,
                }
            }
            StmtKind::Mac(..) => panic!("Shouldn't exist here"),
        })
    }

    fn lower_capture_clause(&mut self, c: CaptureBy) -> hir::CaptureClause {
        match c {
            CaptureBy::Value => hir::CaptureByValue,
            CaptureBy::Ref => hir::CaptureByRef,
        }
    }

    /// If an `explicit_owner` is given, this method allocates the `HirId` in
    /// the address space of that item instead of the item currently being
    /// lowered. This can happen during `lower_impl_item_ref()` where we need to
    /// lower a `Visibility` value although we haven't lowered the owning
    /// `ImplItem` in question yet.
    fn lower_visibility(&mut self,
                        v: &Visibility,
                        explicit_owner: Option<NodeId>)
                        -> hir::Visibility {
        match *v {
            Visibility::Public => hir::Public,
            Visibility::Crate(..) => hir::Visibility::Crate,
            Visibility::Restricted { ref path, id } => {
                hir::Visibility::Restricted {
                    path: P(self.lower_path(id, path, ParamMode::Explicit, true)),
                    id: if let Some(owner) = explicit_owner {
                        self.lower_node_id_with_owner(id, owner).node_id
                    } else {
                        self.lower_node_id(id).node_id
                    }
                }
            }
            Visibility::Inherited => hir::Inherited,
        }
    }

    fn lower_defaultness(&mut self, d: Defaultness, has_value: bool) -> hir::Defaultness {
        match d {
            Defaultness::Default => hir::Defaultness::Default { has_value: has_value },
            Defaultness::Final => {
                assert!(has_value);
                hir::Defaultness::Final
            }
        }
    }

    fn lower_block_check_mode(&mut self, b: &BlockCheckMode) -> hir::BlockCheckMode {
        match *b {
            BlockCheckMode::Default => hir::DefaultBlock,
            BlockCheckMode::Unsafe(u) => hir::UnsafeBlock(self.lower_unsafe_source(u)),
        }
    }

    fn lower_binding_mode(&mut self, b: &BindingMode) -> hir::BindingAnnotation {
        match *b {
            BindingMode::ByValue(Mutability::Immutable) =>
                hir::BindingAnnotation::Unannotated,
            BindingMode::ByRef(Mutability::Immutable) => hir::BindingAnnotation::Ref,
            BindingMode::ByValue(Mutability::Mutable) => hir::BindingAnnotation::Mutable,
            BindingMode::ByRef(Mutability::Mutable) => hir::BindingAnnotation::RefMut,
        }
    }

    fn lower_unsafe_source(&mut self, u: UnsafeSource) -> hir::UnsafeSource {
        match u {
            CompilerGenerated => hir::CompilerGenerated,
            UserProvided => hir::UserProvided,
        }
    }

    fn lower_impl_polarity(&mut self, i: ImplPolarity) -> hir::ImplPolarity {
        match i {
            ImplPolarity::Positive => hir::ImplPolarity::Positive,
            ImplPolarity::Negative => hir::ImplPolarity::Negative,
        }
    }

    fn lower_trait_bound_modifier(&mut self, f: TraitBoundModifier) -> hir::TraitBoundModifier {
        match f {
            TraitBoundModifier::None => hir::TraitBoundModifier::None,
            TraitBoundModifier::Maybe => hir::TraitBoundModifier::Maybe,
        }
    }

    // Helper methods for building HIR.

    fn arm(&mut self, pats: hir::HirVec<P<hir::Pat>>, expr: P<hir::Expr>) -> hir::Arm {
        hir::Arm {
            attrs: hir_vec![],
            pats,
            guard: None,
            body: expr,
        }
    }

    fn field(&mut self, name: Name, expr: P<hir::Expr>, span: Span) -> hir::Field {
        hir::Field {
            name: Spanned {
                node: name,
                span,
            },
            span,
            expr,
            is_shorthand: false,
        }
    }

    fn expr_break(&mut self, span: Span, attrs: ThinVec<Attribute>) -> P<hir::Expr> {
        let expr_break = hir::ExprBreak(self.lower_loop_destination(None), None);
        P(self.expr(span, expr_break, attrs))
    }

    fn expr_call(&mut self, span: Span, e: P<hir::Expr>, args: hir::HirVec<hir::Expr>)
                 -> hir::Expr {
        self.expr(span, hir::ExprCall(e, args), ThinVec::new())
    }

    fn expr_ident(&mut self, span: Span, id: Name, binding: NodeId) -> hir::Expr {
        self.expr_ident_with_attrs(span, id, binding, ThinVec::new())
    }

    fn expr_ident_with_attrs(&mut self, span: Span,
                                        id: Name,
                                        binding: NodeId,
                                        attrs: ThinVec<Attribute>) -> hir::Expr {
        let expr_path = hir::ExprPath(hir::QPath::Resolved(None, P(hir::Path {
            span,
            def: Def::Local(binding),
            segments: hir_vec![hir::PathSegment::from_name(id)],
        })));

        self.expr(span, expr_path, attrs)
    }

    fn expr_mut_addr_of(&mut self, span: Span, e: P<hir::Expr>) -> hir::Expr {
        self.expr(span, hir::ExprAddrOf(hir::MutMutable, e), ThinVec::new())
    }

    fn expr_std_path(&mut self,
                     span: Span,
                     components: &[&str],
                     attrs: ThinVec<Attribute>)
                     -> hir::Expr {
        let path = self.std_path(span, components, true);
        self.expr(span, hir::ExprPath(hir::QPath::Resolved(None, P(path))), attrs)
    }

    fn expr_match(&mut self,
                  span: Span,
                  arg: P<hir::Expr>,
                  arms: hir::HirVec<hir::Arm>,
                  source: hir::MatchSource)
                  -> hir::Expr {
        self.expr(span, hir::ExprMatch(arg, arms, source), ThinVec::new())
    }

    fn expr_block(&mut self, b: P<hir::Block>, attrs: ThinVec<Attribute>) -> hir::Expr {
        self.expr(b.span, hir::ExprBlock(b), attrs)
    }

    fn expr_tuple(&mut self, sp: Span, exprs: hir::HirVec<hir::Expr>) -> P<hir::Expr> {
        P(self.expr(sp, hir::ExprTup(exprs), ThinVec::new()))
    }

    fn expr(&mut self, span: Span, node: hir::Expr_, attrs: ThinVec<Attribute>) -> hir::Expr {
        let LoweredNodeId { node_id, hir_id } = self.next_id();
        hir::Expr {
            id: node_id,
            hir_id,
            node,
            span,
            attrs,
        }
    }

    fn stmt_let_pat(&mut self,
                    sp: Span,
                    ex: Option<P<hir::Expr>>,
                    pat: P<hir::Pat>,
                    source: hir::LocalSource)
                    -> hir::Stmt {
        let LoweredNodeId { node_id, hir_id } = self.next_id();

        let local = P(hir::Local {
            pat,
            ty: None,
            init: ex,
            id: node_id,
            hir_id,
            span: sp,
            attrs: ThinVec::new(),
            source,
        });
        let decl = respan(sp, hir::DeclLocal(local));
        respan(sp, hir::StmtDecl(P(decl), self.next_id().node_id))
    }

    fn stmt_let(&mut self, sp: Span, mutbl: bool, ident: Name, ex: P<hir::Expr>)
                -> (hir::Stmt, NodeId) {
        let pat = if mutbl {
            self.pat_ident_binding_mode(sp, ident, hir::BindingAnnotation::Mutable)
        } else {
            self.pat_ident(sp, ident)
        };
        let pat_id = pat.id;
        (self.stmt_let_pat(sp, Some(ex), pat, hir::LocalSource::Normal), pat_id)
    }

    fn block_expr(&mut self, expr: P<hir::Expr>) -> hir::Block {
        self.block_all(expr.span, hir::HirVec::new(), Some(expr))
    }

    fn block_all(&mut self, span: Span, stmts: hir::HirVec<hir::Stmt>, expr: Option<P<hir::Expr>>)
                 -> hir::Block {
        let LoweredNodeId { node_id, hir_id } = self.next_id();

        hir::Block {
            stmts,
            expr,
            id: node_id,
            hir_id,
            rules: hir::DefaultBlock,
            span,
            targeted_by_break: false,
        }
    }

    fn pat_ok(&mut self, span: Span, pat: P<hir::Pat>) -> P<hir::Pat> {
        self.pat_std_enum(span, &["result", "Result", "Ok"], hir_vec![pat])
    }

    fn pat_err(&mut self, span: Span, pat: P<hir::Pat>) -> P<hir::Pat> {
        self.pat_std_enum(span, &["result", "Result", "Err"], hir_vec![pat])
    }

    fn pat_some(&mut self, span: Span, pat: P<hir::Pat>) -> P<hir::Pat> {
        self.pat_std_enum(span, &["option", "Option", "Some"], hir_vec![pat])
    }

    fn pat_none(&mut self, span: Span) -> P<hir::Pat> {
        self.pat_std_enum(span, &["option", "Option", "None"], hir_vec![])
    }

    fn pat_std_enum(&mut self,
                    span: Span,
                    components: &[&str],
                    subpats: hir::HirVec<P<hir::Pat>>)
                    -> P<hir::Pat> {
        let path = self.std_path(span, components, true);
        let qpath = hir::QPath::Resolved(None, P(path));
        let pt = if subpats.is_empty() {
            hir::PatKind::Path(qpath)
        } else {
            hir::PatKind::TupleStruct(qpath, subpats, None)
        };
        self.pat(span, pt)
    }

    fn pat_ident(&mut self, span: Span, name: Name) -> P<hir::Pat> {
        self.pat_ident_binding_mode(span, name, hir::BindingAnnotation::Unannotated)
    }

    fn pat_ident_binding_mode(&mut self, span: Span, name: Name, bm: hir::BindingAnnotation)
                              -> P<hir::Pat> {
        let LoweredNodeId { node_id, hir_id } = self.next_id();

        P(hir::Pat {
            id: node_id,
            hir_id,
            node: hir::PatKind::Binding(bm,
                                        node_id,
                                        Spanned {
                                            span,
                                            node: name,
                                        },
                                        None),
            span,
        })
    }

    fn pat_wild(&mut self, span: Span) -> P<hir::Pat> {
        self.pat(span, hir::PatKind::Wild)
    }

    fn pat(&mut self, span: Span, pat: hir::PatKind) -> P<hir::Pat> {
        let LoweredNodeId { node_id, hir_id } = self.next_id();
        P(hir::Pat {
            id: node_id,
            hir_id,
            node: pat,
            span,
        })
    }

    /// Given suffix ["b","c","d"], returns path `::std::b::c::d` when
    /// `fld.cx.use_std`, and `::core::b::c::d` otherwise.
    /// The path is also resolved according to `is_value`.
    fn std_path(&mut self, span: Span, components: &[&str], is_value: bool) -> hir::Path {
        let mut path = hir::Path {
            span,
            def: Def::Err,
            segments: iter::once(keywords::CrateRoot.name()).chain({
                self.crate_root.into_iter().chain(components.iter().cloned()).map(Symbol::intern)
            }).map(hir::PathSegment::from_name).collect(),
        };

        self.resolver.resolve_hir_path(&mut path, is_value);
        path
    }

    fn signal_block_expr(&mut self,
                         stmts: hir::HirVec<hir::Stmt>,
                         expr: P<hir::Expr>,
                         span: Span,
                         rule: hir::BlockCheckMode,
                         attrs: ThinVec<Attribute>)
                         -> hir::Expr {
        let LoweredNodeId { node_id, hir_id } = self.next_id();

        let block = P(hir::Block {
            rules: rule,
            span,
            id: node_id,
            hir_id,
            stmts,
            expr: Some(expr),
            targeted_by_break: false,
        });
        self.expr_block(block, attrs)
    }

    fn ty_path(&mut self, id: LoweredNodeId, span: Span, qpath: hir::QPath) -> P<hir::Ty> {
        let mut id = id;
        let node = match qpath {
            hir::QPath::Resolved(None, path) => {
                // Turn trait object paths into `TyTraitObject` instead.
                if let Def::Trait(_) = path.def {
                    let principal = hir::PolyTraitRef {
                        bound_lifetimes: hir_vec![],
                        trait_ref: hir::TraitRef {
                            path: path.and_then(|path| path),
                            ref_id: id.node_id,
                        },
                        span,
                    };

                    // The original ID is taken by the `PolyTraitRef`,
                    // so the `Ty` itself needs a different one.
                    id = self.next_id();

                    hir::TyTraitObject(hir_vec![principal], self.elided_lifetime(span))
                } else {
                    hir::TyPath(hir::QPath::Resolved(None, path))
                }
            }
            _ => hir::TyPath(qpath)
        };
        P(hir::Ty { id: id.node_id, hir_id: id.hir_id, node, span })
    }

    fn elided_lifetime(&mut self, span: Span) -> hir::Lifetime {
        hir::Lifetime {
            id: self.next_id().node_id,
            span,
            name: hir::LifetimeName::Implicit,
        }
    }
}

fn body_ids(bodies: &BTreeMap<hir::BodyId, hir::Body>) -> Vec<hir::BodyId> {
    // Sorting by span ensures that we get things in order within a
    // file, and also puts the files in a sensible order.
    let mut body_ids: Vec<_> = bodies.keys().cloned().collect();
    body_ids.sort_by_key(|b| bodies[b].value.span);
    body_ids
}
