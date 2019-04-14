//! Type inference, i.e. the process of walking through the code and determining
//! the type of each expression and pattern.
//!
//! For type inference, compare the implementations in rustc (the various
//! check_* methods in librustc_typeck/check/mod.rs are a good entry point) and
//! IntelliJ-Rust (org.rust.lang.core.types.infer). Our entry point for
//! inference here is the `infer` function, which infers the types of all
//! expressions in a given function.
//!
//! During inference, types (i.e. the `Ty` struct) can contain type 'variables'
//! which represent currently unknown types; as we walk through the expressions,
//! we might determine that certain variables need to be equal to each other, or
//! to certain types. To record this, we use the union-find implementation from
//! the `ena` crate, which is extracted from rustc.

use std::borrow::Cow;
use std::iter::repeat;
use std::ops::Index;
use std::sync::Arc;
use std::mem;

use ena::unify::{InPlaceUnificationTable, UnifyKey, UnifyValue, NoError};
use rustc_hash::FxHashMap;

use ra_arena::map::ArenaMap;
use test_utils::tested_by;

use crate::{
    Function, StructField, Path, Name,
    FnSignature, AdtDef,ConstSignature,
    HirDatabase,
    DefWithBody,
    ImplItem,
    type_ref::{TypeRef, Mutability},
    expr::{Body, Expr, BindingAnnotation, Literal, ExprId, Pat, PatId, UnaryOp, BinaryOp, Statement, FieldPat,Array, self},
    generics::{GenericParams, HasGenericParams},
    path::{GenericArgs, GenericArg},
    ModuleDef,
    adt::VariantDef,
    resolve::{Resolver, Resolution},
    nameres::Namespace,
    ty::infer::diagnostics::InferenceDiagnostic,
    diagnostics::DiagnosticSink,
};
use super::{
    Ty, TypableDef, Substs, primitive, op, ApplicationTy, TypeCtor, CallableDef, TraitRef,
    traits::{ Solution, Obligation, Guidance},
};

/// The entry point of type inference.
pub fn infer(db: &impl HirDatabase, def: DefWithBody) -> Arc<InferenceResult> {
    db.check_canceled();
    let body = def.body(db);
    let resolver = def.resolver(db);
    let mut ctx = InferenceContext::new(db, body, resolver);

    match def {
        DefWithBody::Const(ref c) => ctx.collect_const_signature(&c.signature(db)),
        DefWithBody::Function(ref f) => ctx.collect_fn_signature(&f.signature(db)),
        DefWithBody::Static(ref s) => ctx.collect_const_signature(&s.signature(db)),
    }

    ctx.infer_body();

    Arc::new(ctx.resolve_all())
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
enum ExprOrPatId {
    ExprId(ExprId),
    PatId(PatId),
}

impl_froms!(ExprOrPatId: ExprId, PatId);

/// Binding modes inferred for patterns.
/// https://doc.rust-lang.org/reference/patterns.html#binding-modes
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BindingMode {
    Move,
    Ref(Mutability),
}

impl BindingMode {
    pub fn convert(annotation: &BindingAnnotation) -> BindingMode {
        match annotation {
            BindingAnnotation::Unannotated | BindingAnnotation::Mutable => BindingMode::Move,
            BindingAnnotation::Ref => BindingMode::Ref(Mutability::Shared),
            BindingAnnotation::RefMut => BindingMode::Ref(Mutability::Mut),
        }
    }
}

impl Default for BindingMode {
    fn default() -> Self {
        BindingMode::Move
    }
}

/// The result of type inference: A mapping from expressions and patterns to types.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InferenceResult {
    /// For each method call expr, records the function it resolves to.
    method_resolutions: FxHashMap<ExprId, Function>,
    /// For each field access expr, records the field it resolves to.
    field_resolutions: FxHashMap<ExprId, StructField>,
    /// For each associated item record what it resolves to
    assoc_resolutions: FxHashMap<ExprOrPatId, ImplItem>,
    diagnostics: Vec<InferenceDiagnostic>,
    pub(super) type_of_expr: ArenaMap<ExprId, Ty>,
    pub(super) type_of_pat: ArenaMap<PatId, Ty>,
}

impl InferenceResult {
    pub fn method_resolution(&self, expr: ExprId) -> Option<Function> {
        self.method_resolutions.get(&expr).map(|it| *it)
    }
    pub fn field_resolution(&self, expr: ExprId) -> Option<StructField> {
        self.field_resolutions.get(&expr).map(|it| *it)
    }
    pub fn assoc_resolutions_for_expr(&self, id: ExprId) -> Option<ImplItem> {
        self.assoc_resolutions.get(&id.into()).map(|it| *it)
    }
    pub fn assoc_resolutions_for_pat(&self, id: PatId) -> Option<ImplItem> {
        self.assoc_resolutions.get(&id.into()).map(|it| *it)
    }
    pub(crate) fn add_diagnostics(
        &self,
        db: &impl HirDatabase,
        owner: Function,
        sink: &mut DiagnosticSink,
    ) {
        self.diagnostics.iter().for_each(|it| it.add_to(db, owner, sink))
    }
}

impl Index<ExprId> for InferenceResult {
    type Output = Ty;

    fn index(&self, expr: ExprId) -> &Ty {
        self.type_of_expr.get(expr).unwrap_or(&Ty::Unknown)
    }
}

impl Index<PatId> for InferenceResult {
    type Output = Ty;

    fn index(&self, pat: PatId) -> &Ty {
        self.type_of_pat.get(pat).unwrap_or(&Ty::Unknown)
    }
}

/// The inference context contains all information needed during type inference.
#[derive(Clone, Debug)]
struct InferenceContext<'a, D: HirDatabase> {
    db: &'a D,
    body: Arc<Body>,
    resolver: Resolver,
    var_unification_table: InPlaceUnificationTable<TypeVarId>,
    obligations: Vec<Obligation>,
    method_resolutions: FxHashMap<ExprId, Function>,
    field_resolutions: FxHashMap<ExprId, StructField>,
    assoc_resolutions: FxHashMap<ExprOrPatId, ImplItem>,
    type_of_expr: ArenaMap<ExprId, Ty>,
    type_of_pat: ArenaMap<PatId, Ty>,
    diagnostics: Vec<InferenceDiagnostic>,
    /// The return type of the function being inferred.
    return_ty: Ty,
}

impl<'a, D: HirDatabase> InferenceContext<'a, D> {
    fn new(db: &'a D, body: Arc<Body>, resolver: Resolver) -> Self {
        InferenceContext {
            method_resolutions: FxHashMap::default(),
            field_resolutions: FxHashMap::default(),
            assoc_resolutions: FxHashMap::default(),
            type_of_expr: ArenaMap::default(),
            type_of_pat: ArenaMap::default(),
            diagnostics: Vec::default(),
            var_unification_table: InPlaceUnificationTable::new(),
            obligations: Vec::default(),
            return_ty: Ty::Unknown, // set in collect_fn_signature
            db,
            body,
            resolver,
        }
    }

    fn resolve_all(mut self) -> InferenceResult {
        // FIXME resolve obligations as well (use Guidance if necessary)
        let mut tv_stack = Vec::new();
        let mut expr_types = mem::replace(&mut self.type_of_expr, ArenaMap::default());
        for ty in expr_types.values_mut() {
            let resolved = self.resolve_ty_completely(&mut tv_stack, mem::replace(ty, Ty::Unknown));
            *ty = resolved;
        }
        let mut pat_types = mem::replace(&mut self.type_of_pat, ArenaMap::default());
        for ty in pat_types.values_mut() {
            let resolved = self.resolve_ty_completely(&mut tv_stack, mem::replace(ty, Ty::Unknown));
            *ty = resolved;
        }
        InferenceResult {
            method_resolutions: self.method_resolutions,
            field_resolutions: self.field_resolutions,
            assoc_resolutions: self.assoc_resolutions,
            type_of_expr: expr_types,
            type_of_pat: pat_types,
            diagnostics: self.diagnostics,
        }
    }

    fn write_expr_ty(&mut self, expr: ExprId, ty: Ty) {
        self.type_of_expr.insert(expr, ty);
    }

    fn write_method_resolution(&mut self, expr: ExprId, func: Function) {
        self.method_resolutions.insert(expr, func);
    }

    fn write_field_resolution(&mut self, expr: ExprId, field: StructField) {
        self.field_resolutions.insert(expr, field);
    }

    fn write_assoc_resolution(&mut self, id: ExprOrPatId, item: ImplItem) {
        self.assoc_resolutions.insert(id, item);
    }

    fn write_pat_ty(&mut self, pat: PatId, ty: Ty) {
        self.type_of_pat.insert(pat, ty);
    }

    fn make_ty(&mut self, type_ref: &TypeRef) -> Ty {
        let ty = Ty::from_hir(
            self.db,
            // FIXME use right resolver for block
            &self.resolver,
            type_ref,
        );
        let ty = self.insert_type_vars(ty);
        ty
    }

    fn unify_substs(&mut self, substs1: &Substs, substs2: &Substs, depth: usize) -> bool {
        substs1.0.iter().zip(substs2.0.iter()).all(|(t1, t2)| self.unify_inner(t1, t2, depth))
    }

    fn unify(&mut self, ty1: &Ty, ty2: &Ty) -> bool {
        self.unify_inner(ty1, ty2, 0)
    }

    fn unify_inner(&mut self, ty1: &Ty, ty2: &Ty, depth: usize) -> bool {
        if depth > 1000 {
            // prevent stackoverflows
            panic!("infinite recursion in unification");
        }
        if ty1 == ty2 {
            return true;
        }
        // try to resolve type vars first
        let ty1 = self.resolve_ty_shallow(ty1);
        let ty2 = self.resolve_ty_shallow(ty2);
        match (&*ty1, &*ty2) {
            (Ty::Unknown, ..) => true,
            (.., Ty::Unknown) => true,
            (Ty::Apply(a_ty1), Ty::Apply(a_ty2)) if a_ty1.ctor == a_ty2.ctor => {
                self.unify_substs(&a_ty1.parameters, &a_ty2.parameters, depth + 1)
            }
            (Ty::Infer(InferTy::TypeVar(tv1)), Ty::Infer(InferTy::TypeVar(tv2)))
            | (Ty::Infer(InferTy::IntVar(tv1)), Ty::Infer(InferTy::IntVar(tv2)))
            | (Ty::Infer(InferTy::FloatVar(tv1)), Ty::Infer(InferTy::FloatVar(tv2))) => {
                // both type vars are unknown since we tried to resolve them
                self.var_unification_table.union(*tv1, *tv2);
                true
            }
            (Ty::Infer(InferTy::TypeVar(tv)), other)
            | (other, Ty::Infer(InferTy::TypeVar(tv)))
            | (Ty::Infer(InferTy::IntVar(tv)), other)
            | (other, Ty::Infer(InferTy::IntVar(tv)))
            | (Ty::Infer(InferTy::FloatVar(tv)), other)
            | (other, Ty::Infer(InferTy::FloatVar(tv))) => {
                // the type var is unknown since we tried to resolve it
                self.var_unification_table.union_value(*tv, TypeVarValue::Known(other.clone()));
                true
            }
            _ => false,
        }
    }

    fn new_type_var(&mut self) -> Ty {
        Ty::Infer(InferTy::TypeVar(self.var_unification_table.new_key(TypeVarValue::Unknown)))
    }

    fn new_integer_var(&mut self) -> Ty {
        Ty::Infer(InferTy::IntVar(self.var_unification_table.new_key(TypeVarValue::Unknown)))
    }

    fn new_float_var(&mut self) -> Ty {
        Ty::Infer(InferTy::FloatVar(self.var_unification_table.new_key(TypeVarValue::Unknown)))
    }

    /// Replaces Ty::Unknown by a new type var, so we can maybe still infer it.
    fn insert_type_vars_shallow(&mut self, ty: Ty) -> Ty {
        match ty {
            Ty::Unknown => self.new_type_var(),
            Ty::Apply(ApplicationTy {
                ctor: TypeCtor::Int(primitive::UncertainIntTy::Unknown),
                ..
            }) => self.new_integer_var(),
            Ty::Apply(ApplicationTy {
                ctor: TypeCtor::Float(primitive::UncertainFloatTy::Unknown),
                ..
            }) => self.new_float_var(),
            _ => ty,
        }
    }

    fn insert_type_vars(&mut self, ty: Ty) -> Ty {
        ty.fold(&mut |ty| self.insert_type_vars_shallow(ty))
    }

    fn resolve_obligations_as_possible(&mut self) {
        let obligations = mem::replace(&mut self.obligations, Vec::new());
        for obligation in obligations {
            // FIXME resolve types in the obligation first
            let (solution, var_mapping) = match &obligation {
                Obligation::Trait(tr) => {
                    let (tr, var_mapping) = super::traits::canonicalize(tr.clone());
                    (self.db.implements(tr), var_mapping)
                }
            };
            match solution {
                Some(Solution::Unique(substs)) => {
                    for (i, subst) in substs.0.iter().enumerate() {
                        let uncanonical = var_mapping[i];
                        // FIXME the subst may contain type variables, which would need to be mapped back as well
                        self.unify(&Ty::Infer(InferTy::TypeVar(uncanonical)), subst);
                    }
                }
                Some(Solution::Ambig(Guidance::Definite(substs))) => {
                    for (i, subst) in substs.0.iter().enumerate() {
                        let uncanonical = var_mapping[i];
                        // FIXME the subst may contain type variables, which would need to be mapped back as well
                        self.unify(&Ty::Infer(InferTy::TypeVar(uncanonical)), subst);
                    }
                    self.obligations.push(obligation);
                }
                Some(_) => {
                    self.obligations.push(obligation);
                }
                None => {
                    // FIXME obligation cannot be fulfilled => diagnostic
                }
            }
        }
    }

    /// Resolves the type as far as currently possible, replacing type variables
    /// by their known types. All types returned by the infer_* functions should
    /// be resolved as far as possible, i.e. contain no type variables with
    /// known type.
    fn resolve_ty_as_possible(&mut self, tv_stack: &mut Vec<TypeVarId>, ty: Ty) -> Ty {
        self.resolve_obligations_as_possible();

        ty.fold(&mut |ty| match ty {
            Ty::Infer(tv) => {
                let inner = tv.to_inner();
                if tv_stack.contains(&inner) {
                    tested_by!(type_var_cycles_resolve_as_possible);
                    // recursive type
                    return tv.fallback_value();
                }
                if let Some(known_ty) = self.var_unification_table.probe_value(inner).known() {
                    // known_ty may contain other variables that are known by now
                    tv_stack.push(inner);
                    let result = self.resolve_ty_as_possible(tv_stack, known_ty.clone());
                    tv_stack.pop();
                    result
                } else {
                    ty
                }
            }
            _ => ty,
        })
    }

    /// If `ty` is a type variable with known type, returns that type;
    /// otherwise, return ty.
    fn resolve_ty_shallow<'b>(&mut self, ty: &'b Ty) -> Cow<'b, Ty> {
        let mut ty = Cow::Borrowed(ty);
        // The type variable could resolve to a int/float variable. Hence try
        // resolving up to three times; each type of variable shouldn't occur
        // more than once
        for i in 0..3 {
            if i > 0 {
                tested_by!(type_var_resolves_to_int_var);
            }
            match &*ty {
                Ty::Infer(tv) => {
                    let inner = tv.to_inner();
                    match self.var_unification_table.probe_value(inner).known() {
                        Some(known_ty) => {
                            // The known_ty can't be a type var itself
                            ty = Cow::Owned(known_ty.clone());
                        }
                        _ => return ty,
                    }
                }
                _ => return ty,
            }
        }
        log::error!("Inference variable still not resolved: {:?}", ty);
        ty
    }

    /// Resolves the type completely; type variables without known type are
    /// replaced by Ty::Unknown.
    fn resolve_ty_completely(&mut self, tv_stack: &mut Vec<TypeVarId>, ty: Ty) -> Ty {
        ty.fold(&mut |ty| match ty {
            Ty::Infer(tv) => {
                let inner = tv.to_inner();
                if tv_stack.contains(&inner) {
                    tested_by!(type_var_cycles_resolve_completely);
                    // recursive type
                    return tv.fallback_value();
                }
                if let Some(known_ty) = self.var_unification_table.probe_value(inner).known() {
                    // known_ty may contain other variables that are known by now
                    tv_stack.push(inner);
                    let result = self.resolve_ty_completely(tv_stack, known_ty.clone());
                    tv_stack.pop();
                    result
                } else {
                    tv.fallback_value()
                }
            }
            _ => ty,
        })
    }

    fn infer_path_expr(&mut self, resolver: &Resolver, path: &Path, id: ExprOrPatId) -> Option<Ty> {
        let resolved = resolver.resolve_path_segments(self.db, &path);

        let (def, remaining_index) = resolved.into_inner();

        log::debug!(
            "path {:?} resolved to {:?} with remaining index {:?}",
            path,
            def,
            remaining_index
        );

        // if the remaining_index is None, we expect the path
        // to be fully resolved, in this case we continue with
        // the default by attempting to `take_values´ from the resolution.
        // Otherwise the path was partially resolved, which means
        // we might have resolved into a type for which
        // we may find some associated item starting at the
        // path.segment pointed to by `remaining_index´
        let mut resolved =
            if remaining_index.is_none() { def.take_values()? } else { def.take_types()? };

        let remaining_index = remaining_index.unwrap_or(path.segments.len());
        let mut actual_def_ty: Option<Ty> = None;

        let krate = resolver.module().map(|t| t.0.krate());
        // resolve intermediate segments
        for (i, segment) in path.segments[remaining_index..].iter().enumerate() {
            let ty = match resolved {
                Resolution::Def(def) => {
                    // FIXME resolve associated items from traits as well
                    let typable: Option<TypableDef> = def.into();
                    let typable = typable?;

                    let ty = self.db.type_for_def(typable, Namespace::Types);

                    // For example, this substs will take `Gen::*<u32>*::make`
                    assert!(remaining_index > 0);
                    let substs = Ty::substs_from_path_segment(
                        self.db,
                        &self.resolver,
                        &path.segments[remaining_index + i - 1],
                        typable,
                    );

                    ty.subst(&substs)
                }
                Resolution::LocalBinding(_) => {
                    // can't have a local binding in an associated item path
                    return None;
                }
                Resolution::GenericParam(..) => {
                    // FIXME associated item of generic param
                    return None;
                }
                Resolution::SelfType(_) => {
                    // FIXME associated item of self type
                    return None;
                }
            };

            // Attempt to find an impl_item for the type which has a name matching
            // the current segment
            log::debug!("looking for path segment: {:?}", segment);

            actual_def_ty = Some(ty.clone());

            let item: crate::ModuleDef = krate.and_then(|k| {
                ty.iterate_impl_items(self.db, k, |item| {
                    let matching_def: Option<crate::ModuleDef> = match item {
                        crate::ImplItem::Method(func) => {
                            let sig = func.signature(self.db);
                            if segment.name == *sig.name() {
                                Some(func.into())
                            } else {
                                None
                            }
                        }

                        crate::ImplItem::Const(konst) => {
                            let sig = konst.signature(self.db);
                            if segment.name == *sig.name() {
                                Some(konst.into())
                            } else {
                                None
                            }
                        }

                        // FIXME: Resolve associated types
                        crate::ImplItem::TypeAlias(_) => None,
                    };
                    match matching_def {
                        Some(_) => {
                            self.write_assoc_resolution(id, item);
                            return matching_def;
                        }
                        None => None,
                    }
                })
            })?;

            resolved = Resolution::Def(item.into());
        }

        match resolved {
            Resolution::Def(def) => {
                let typable: Option<TypableDef> = def.into();
                let typable = typable?;
                let mut ty = self.db.type_for_def(typable, Namespace::Values);
                if let Some(sts) = self.find_self_types(&def, actual_def_ty) {
                    ty = ty.subst(&sts);
                }

                let substs = Ty::substs_from_path(self.db, &self.resolver, path, typable);
                let ty = ty.subst(&substs);
                let ty = self.insert_type_vars(ty);
                Some(ty)
            }
            Resolution::LocalBinding(pat) => {
                let ty = self.type_of_pat.get(pat)?.clone();
                let ty = self.resolve_ty_as_possible(&mut vec![], ty);
                Some(ty)
            }
            Resolution::GenericParam(..) => {
                // generic params can't refer to values... yet
                None
            }
            Resolution::SelfType(_) => {
                log::error!("path expr {:?} resolved to Self type in values ns", path);
                None
            }
        }
    }

    fn find_self_types(&self, def: &ModuleDef, actual_def_ty: Option<Ty>) -> Option<Substs> {
        let actual_def_ty = actual_def_ty?;

        if let crate::ModuleDef::Function(func) = def {
            // We only do the infer if parent has generic params
            let gen = func.generic_params(self.db);
            if gen.count_parent_params() == 0 {
                return None;
            }

            let impl_block = func.impl_block(self.db)?.target_ty(self.db);
            let impl_block_substs = impl_block.substs()?;
            let actual_substs = actual_def_ty.substs()?;

            let mut new_substs = vec![Ty::Unknown; gen.count_parent_params()];

            // The following code *link up* the function actual parma type
            // and impl_block type param index
            impl_block_substs.iter().zip(actual_substs.iter()).for_each(|(param, pty)| {
                if let Ty::Param { idx, .. } = param {
                    if let Some(s) = new_substs.get_mut(*idx as usize) {
                        *s = pty.clone();
                    }
                }
            });

            Some(Substs(new_substs.into()))
        } else {
            None
        }
    }

    fn resolve_variant(&mut self, path: Option<&Path>) -> (Ty, Option<VariantDef>) {
        let path = match path {
            Some(path) => path,
            None => return (Ty::Unknown, None),
        };
        let resolver = &self.resolver;
        let typable: Option<TypableDef> = match resolver.resolve_path(self.db, &path).take_types() {
            Some(Resolution::Def(def)) => def.into(),
            Some(Resolution::LocalBinding(..)) => {
                // this cannot happen
                log::error!("path resolved to local binding in type ns");
                return (Ty::Unknown, None);
            }
            Some(Resolution::GenericParam(..)) => {
                // generic params can't be used in struct literals
                return (Ty::Unknown, None);
            }
            Some(Resolution::SelfType(..)) => {
                // FIXME this is allowed in an impl for a struct, handle this
                return (Ty::Unknown, None);
            }
            None => return (Ty::Unknown, None),
        };
        let def = match typable {
            None => return (Ty::Unknown, None),
            Some(it) => it,
        };
        // FIXME remove the duplication between here and `Ty::from_path`?
        let substs = Ty::substs_from_path(self.db, resolver, path, def);
        match def {
            TypableDef::Struct(s) => {
                let ty = s.ty(self.db);
                let ty = self.insert_type_vars(ty.apply_substs(substs));
                (ty, Some(s.into()))
            }
            TypableDef::EnumVariant(var) => {
                let ty = var.parent_enum(self.db).ty(self.db);
                let ty = self.insert_type_vars(ty.apply_substs(substs));
                (ty, Some(var.into()))
            }
            TypableDef::TypeAlias(_)
            | TypableDef::Function(_)
            | TypableDef::Enum(_)
            | TypableDef::Const(_)
            | TypableDef::Static(_) => (Ty::Unknown, None),
        }
    }

    fn infer_tuple_struct_pat(
        &mut self,
        path: Option<&Path>,
        subpats: &[PatId],
        expected: &Ty,
        default_bm: BindingMode,
    ) -> Ty {
        let (ty, def) = self.resolve_variant(path);

        self.unify(&ty, expected);

        let substs = ty.substs().unwrap_or_else(Substs::empty);

        for (i, &subpat) in subpats.iter().enumerate() {
            let expected_ty = def
                .and_then(|d| d.field(self.db, &Name::tuple_field_name(i)))
                .map_or(Ty::Unknown, |field| field.ty(self.db))
                .subst(&substs);
            self.infer_pat(subpat, &expected_ty, default_bm);
        }

        ty
    }

    fn infer_struct_pat(
        &mut self,
        path: Option<&Path>,
        subpats: &[FieldPat],
        expected: &Ty,
        default_bm: BindingMode,
    ) -> Ty {
        let (ty, def) = self.resolve_variant(path);

        self.unify(&ty, expected);

        let substs = ty.substs().unwrap_or_else(Substs::empty);

        for subpat in subpats {
            let matching_field = def.and_then(|it| it.field(self.db, &subpat.name));
            let expected_ty =
                matching_field.map_or(Ty::Unknown, |field| field.ty(self.db)).subst(&substs);
            self.infer_pat(subpat.pat, &expected_ty, default_bm);
        }

        ty
    }

    fn infer_pat(&mut self, pat: PatId, mut expected: &Ty, mut default_bm: BindingMode) -> Ty {
        let body = Arc::clone(&self.body); // avoid borrow checker problem

        let is_non_ref_pat = match &body[pat] {
            Pat::Tuple(..)
            | Pat::TupleStruct { .. }
            | Pat::Struct { .. }
            | Pat::Range { .. }
            | Pat::Slice { .. } => true,
            // FIXME: Path/Lit might actually evaluate to ref, but inference is unimplemented.
            Pat::Path(..) | Pat::Lit(..) => true,
            Pat::Wild | Pat::Bind { .. } | Pat::Ref { .. } | Pat::Missing => false,
        };
        if is_non_ref_pat {
            while let Some((inner, mutability)) = expected.as_reference() {
                expected = inner;
                default_bm = match default_bm {
                    BindingMode::Move => BindingMode::Ref(mutability),
                    BindingMode::Ref(Mutability::Shared) => BindingMode::Ref(Mutability::Shared),
                    BindingMode::Ref(Mutability::Mut) => BindingMode::Ref(mutability),
                }
            }
        } else if let Pat::Ref { .. } = &body[pat] {
            tested_by!(match_ergonomics_ref);
            // When you encounter a `&pat` pattern, reset to Move.
            // This is so that `w` is by value: `let (_, &w) = &(1, &2);`
            default_bm = BindingMode::Move;
        }

        // Lose mutability.
        let default_bm = default_bm;
        let expected = expected;

        let ty = match &body[pat] {
            Pat::Tuple(ref args) => {
                let expectations = match expected.as_tuple() {
                    Some(parameters) => &*parameters.0,
                    _ => &[],
                };
                let expectations_iter = expectations.iter().chain(repeat(&Ty::Unknown));

                let inner_tys = args
                    .iter()
                    .zip(expectations_iter)
                    .map(|(&pat, ty)| self.infer_pat(pat, ty, default_bm))
                    .collect::<Vec<_>>()
                    .into();

                Ty::apply(TypeCtor::Tuple, Substs(inner_tys))
            }
            Pat::Ref { pat, mutability } => {
                let expectation = match expected.as_reference() {
                    Some((inner_ty, exp_mut)) => {
                        if *mutability != exp_mut {
                            // FIXME: emit type error?
                        }
                        inner_ty
                    }
                    _ => &Ty::Unknown,
                };
                let subty = self.infer_pat(*pat, expectation, default_bm);
                Ty::apply_one(TypeCtor::Ref(*mutability), subty.into())
            }
            Pat::TupleStruct { path: ref p, args: ref subpats } => {
                self.infer_tuple_struct_pat(p.as_ref(), subpats, expected, default_bm)
            }
            Pat::Struct { path: ref p, args: ref fields } => {
                self.infer_struct_pat(p.as_ref(), fields, expected, default_bm)
            }
            Pat::Path(path) => {
                // FIXME use correct resolver for the surrounding expression
                let resolver = self.resolver.clone();
                self.infer_path_expr(&resolver, &path, pat.into()).unwrap_or(Ty::Unknown)
            }
            Pat::Bind { mode, name: _name, subpat } => {
                let mode = if mode == &BindingAnnotation::Unannotated {
                    default_bm
                } else {
                    BindingMode::convert(mode)
                };
                let inner_ty = if let Some(subpat) = subpat {
                    self.infer_pat(*subpat, expected, default_bm)
                } else {
                    expected.clone()
                };
                let inner_ty = self.insert_type_vars_shallow(inner_ty);

                let bound_ty = match mode {
                    BindingMode::Ref(mutability) => {
                        Ty::apply_one(TypeCtor::Ref(mutability), inner_ty.clone().into())
                    }
                    BindingMode::Move => inner_ty.clone(),
                };
                let bound_ty = self.resolve_ty_as_possible(&mut vec![], bound_ty);
                self.write_pat_ty(pat, bound_ty);
                return inner_ty;
            }
            _ => Ty::Unknown,
        };
        // use a new type variable if we got Ty::Unknown here
        let ty = self.insert_type_vars_shallow(ty);
        self.unify(&ty, expected);
        let ty = self.resolve_ty_as_possible(&mut vec![], ty);
        self.write_pat_ty(pat, ty.clone());
        ty
    }

    fn substs_for_method_call(
        &mut self,
        def_generics: Option<Arc<GenericParams>>,
        generic_args: Option<&GenericArgs>,
        receiver_ty: &Ty,
    ) -> Substs {
        let (parent_param_count, param_count) =
            def_generics.as_ref().map_or((0, 0), |g| (g.count_parent_params(), g.params.len()));
        let mut substs = Vec::with_capacity(parent_param_count + param_count);
        // Parent arguments are unknown, except for the receiver type
        if let Some(parent_generics) = def_generics.and_then(|p| p.parent_params.clone()) {
            for param in &parent_generics.params {
                if param.name.as_known_name() == Some(crate::KnownName::SelfType) {
                    substs.push(receiver_ty.clone());
                } else {
                    substs.push(Ty::Unknown);
                }
            }
        }
        // handle provided type arguments
        if let Some(generic_args) = generic_args {
            // if args are provided, it should be all of them, but we can't rely on that
            for arg in generic_args.args.iter().take(param_count) {
                match arg {
                    GenericArg::Type(type_ref) => {
                        let ty = self.make_ty(type_ref);
                        substs.push(ty);
                    }
                }
            }
        };
        let supplied_params = substs.len();
        for _ in supplied_params..parent_param_count + param_count {
            substs.push(Ty::Unknown);
        }
        assert_eq!(substs.len(), parent_param_count + param_count);
        Substs(substs.into())
    }

    fn register_obligations_for_call(&mut self, callable_ty: &Ty) {
        match callable_ty {
            Ty::Apply(a_ty) => match a_ty.ctor {
                TypeCtor::FnDef(def) => {
                    // add obligation for trait implementation, if this is a trait method
                    // FIXME also register obligations from where clauses from the trait or impl and method
                    match def {
                        CallableDef::Function(f) => {
                            if let Some(trait_) = f.parent_trait(self.db) {
                                // construct a TraitDef
                                let substs = a_ty.parameters.prefix(
                                    trait_.generic_params(self.db).count_params_including_parent(),
                                );
                                self.obligations
                                    .push(Obligation::Trait(TraitRef { trait_, substs }));
                            }
                        }
                        CallableDef::Struct(_) | CallableDef::EnumVariant(_) => {}
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn infer_method_call(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        args: &[ExprId],
        method_name: &Name,
        generic_args: Option<&GenericArgs>,
    ) -> Ty {
        let receiver_ty = self.infer_expr(receiver, &Expectation::none());
        let resolved = receiver_ty.clone().lookup_method(self.db, method_name, &self.resolver);
        let (derefed_receiver_ty, method_ty, def_generics) = match resolved {
            Some((ty, func)) => {
                self.write_method_resolution(tgt_expr, func);
                (
                    ty,
                    self.db.type_for_def(func.into(), Namespace::Values),
                    Some(func.generic_params(self.db)),
                )
            }
            None => (receiver_ty, Ty::Unknown, None),
        };
        let substs =
            self.substs_for_method_call(def_generics.clone(), generic_args, &derefed_receiver_ty);
        let method_ty = method_ty.apply_substs(substs);
        let method_ty = self.insert_type_vars(method_ty);
        self.register_obligations_for_call(&method_ty);
        let (expected_receiver_ty, param_tys, ret_ty) = match method_ty.callable_sig(self.db) {
            Some(sig) => {
                if !sig.params().is_empty() {
                    (sig.params()[0].clone(), sig.params()[1..].to_vec(), sig.ret().clone())
                } else {
                    (Ty::Unknown, Vec::new(), sig.ret().clone())
                }
            }
            None => (Ty::Unknown, Vec::new(), Ty::Unknown),
        };
        // Apply autoref so the below unification works correctly
        // FIXME: return correct autorefs from lookup_method
        let actual_receiver_ty = match expected_receiver_ty.as_reference() {
            Some((_, mutability)) => Ty::apply_one(TypeCtor::Ref(mutability), derefed_receiver_ty),
            _ => derefed_receiver_ty,
        };
        self.unify(&expected_receiver_ty, &actual_receiver_ty);

        let param_iter = param_tys.into_iter().chain(repeat(Ty::Unknown));
        for (arg, param) in args.iter().zip(param_iter) {
            self.infer_expr(*arg, &Expectation::has_type(param));
        }
        ret_ty
    }

    fn infer_expr(&mut self, tgt_expr: ExprId, expected: &Expectation) -> Ty {
        let body = Arc::clone(&self.body); // avoid borrow checker problem
        let ty = match &body[tgt_expr] {
            Expr::Missing => Ty::Unknown,
            Expr::If { condition, then_branch, else_branch } => {
                // if let is desugared to match, so this is always simple if
                self.infer_expr(*condition, &Expectation::has_type(Ty::simple(TypeCtor::Bool)));
                let then_ty = self.infer_expr(*then_branch, expected);
                match else_branch {
                    Some(else_branch) => {
                        self.infer_expr(*else_branch, expected);
                    }
                    None => {
                        // no else branch -> unit
                        self.unify(&then_ty, &Ty::unit()); // actually coerce
                    }
                };
                then_ty
            }
            Expr::Block { statements, tail } => self.infer_block(statements, *tail, expected),
            Expr::Loop { body } => {
                self.infer_expr(*body, &Expectation::has_type(Ty::unit()));
                // FIXME handle break with value
                Ty::simple(TypeCtor::Never)
            }
            Expr::While { condition, body } => {
                // while let is desugared to a match loop, so this is always simple while
                self.infer_expr(*condition, &Expectation::has_type(Ty::simple(TypeCtor::Bool)));
                self.infer_expr(*body, &Expectation::has_type(Ty::unit()));
                Ty::unit()
            }
            Expr::For { iterable, body, pat } => {
                let _iterable_ty = self.infer_expr(*iterable, &Expectation::none());
                self.infer_pat(*pat, &Ty::Unknown, BindingMode::default());
                self.infer_expr(*body, &Expectation::has_type(Ty::unit()));
                Ty::unit()
            }
            Expr::Lambda { body, args, arg_types } => {
                assert_eq!(args.len(), arg_types.len());

                for (arg_pat, arg_type) in args.iter().zip(arg_types.iter()) {
                    let expected = if let Some(type_ref) = arg_type {
                        let ty = self.make_ty(type_ref);
                        ty
                    } else {
                        Ty::Unknown
                    };
                    self.infer_pat(*arg_pat, &expected, BindingMode::default());
                }

                // FIXME: infer lambda type etc.
                let _body_ty = self.infer_expr(*body, &Expectation::none());
                Ty::Unknown
            }
            Expr::Call { callee, args } => {
                let callee_ty = self.infer_expr(*callee, &Expectation::none());
                let (param_tys, ret_ty) = match callee_ty.callable_sig(self.db) {
                    Some(sig) => (sig.params().to_vec(), sig.ret().clone()),
                    None => {
                        // Not callable
                        // FIXME: report an error
                        (Vec::new(), Ty::Unknown)
                    }
                };
                // FIXME register obligations from where clauses from the function
                let param_iter = param_tys.into_iter().chain(repeat(Ty::Unknown));
                for (arg, param) in args.iter().zip(param_iter) {
                    self.infer_expr(*arg, &Expectation::has_type(param));
                }
                ret_ty
            }
            Expr::MethodCall { receiver, args, method_name, generic_args } => self
                .infer_method_call(tgt_expr, *receiver, &args, &method_name, generic_args.as_ref()),
            Expr::Match { expr, arms } => {
                let expected = if expected.ty == Ty::Unknown {
                    Expectation::has_type(self.new_type_var())
                } else {
                    expected.clone()
                };
                let input_ty = self.infer_expr(*expr, &Expectation::none());

                for arm in arms {
                    for &pat in &arm.pats {
                        let _pat_ty = self.infer_pat(pat, &input_ty, BindingMode::default());
                    }
                    if let Some(guard_expr) = arm.guard {
                        self.infer_expr(
                            guard_expr,
                            &Expectation::has_type(Ty::simple(TypeCtor::Bool)),
                        );
                    }
                    self.infer_expr(arm.expr, &expected);
                }

                expected.ty
            }
            Expr::Path(p) => {
                // FIXME this could be more efficient...
                let resolver = expr::resolver_for_expr(self.body.clone(), self.db, tgt_expr);
                self.infer_path_expr(&resolver, p, tgt_expr.into()).unwrap_or(Ty::Unknown)
            }
            Expr::Continue => Ty::simple(TypeCtor::Never),
            Expr::Break { expr } => {
                if let Some(expr) = expr {
                    // FIXME handle break with value
                    self.infer_expr(*expr, &Expectation::none());
                }
                Ty::simple(TypeCtor::Never)
            }
            Expr::Return { expr } => {
                if let Some(expr) = expr {
                    self.infer_expr(*expr, &Expectation::has_type(self.return_ty.clone()));
                }
                Ty::simple(TypeCtor::Never)
            }
            Expr::StructLit { path, fields, spread } => {
                let (ty, def_id) = self.resolve_variant(path.as_ref());
                let substs = ty.substs().unwrap_or_else(Substs::empty);
                for (field_idx, field) in fields.into_iter().enumerate() {
                    let field_ty = def_id
                        .and_then(|it| match it.field(self.db, &field.name) {
                            Some(field) => Some(field),
                            None => {
                                self.diagnostics.push(InferenceDiagnostic::NoSuchField {
                                    expr: tgt_expr,
                                    field: field_idx,
                                });
                                None
                            }
                        })
                        .map_or(Ty::Unknown, |field| field.ty(self.db))
                        .subst(&substs);
                    self.infer_expr(field.expr, &Expectation::has_type(field_ty));
                }
                if let Some(expr) = spread {
                    self.infer_expr(*expr, &Expectation::has_type(ty.clone()));
                }
                ty
            }
            Expr::Field { expr, name } => {
                let receiver_ty = self.infer_expr(*expr, &Expectation::none());
                let ty = receiver_ty
                    .autoderef(self.db)
                    .find_map(|derefed_ty| match derefed_ty {
                        Ty::Apply(a_ty) => match a_ty.ctor {
                            TypeCtor::Tuple => {
                                let i = name.to_string().parse::<usize>().ok();
                                i.and_then(|i| a_ty.parameters.0.get(i).cloned())
                            }
                            TypeCtor::Adt(AdtDef::Struct(s)) => {
                                s.field(self.db, name).map(|field| {
                                    self.write_field_resolution(tgt_expr, field);
                                    field.ty(self.db).subst(&a_ty.parameters)
                                })
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .unwrap_or(Ty::Unknown);
                self.insert_type_vars(ty)
            }
            Expr::Try { expr } => {
                let _inner_ty = self.infer_expr(*expr, &Expectation::none());
                Ty::Unknown
            }
            Expr::Cast { expr, type_ref } => {
                let _inner_ty = self.infer_expr(*expr, &Expectation::none());
                let cast_ty = self.make_ty(type_ref);
                // FIXME check the cast...
                cast_ty
            }
            Expr::Ref { expr, mutability } => {
                let expectation =
                    if let Some((exp_inner, exp_mutability)) = &expected.ty.as_reference() {
                        if *exp_mutability == Mutability::Mut && *mutability == Mutability::Shared {
                            // FIXME: throw type error - expected mut reference but found shared ref,
                            // which cannot be coerced
                        }
                        Expectation::has_type(Ty::clone(exp_inner))
                    } else {
                        Expectation::none()
                    };
                // FIXME reference coercions etc.
                let inner_ty = self.infer_expr(*expr, &expectation);
                Ty::apply_one(TypeCtor::Ref(*mutability), inner_ty)
            }
            Expr::UnaryOp { expr, op } => {
                let inner_ty = self.infer_expr(*expr, &Expectation::none());
                match op {
                    UnaryOp::Deref => {
                        if let Some(derefed_ty) = inner_ty.builtin_deref() {
                            derefed_ty
                        } else {
                            // FIXME Deref::deref
                            Ty::Unknown
                        }
                    }
                    UnaryOp::Neg => {
                        match &inner_ty {
                            Ty::Apply(a_ty) => match a_ty.ctor {
                                TypeCtor::Int(primitive::UncertainIntTy::Unknown)
                                | TypeCtor::Int(primitive::UncertainIntTy::Known(
                                    primitive::IntTy {
                                        signedness: primitive::Signedness::Signed,
                                        ..
                                    },
                                ))
                                | TypeCtor::Float(..) => inner_ty,
                                _ => Ty::Unknown,
                            },
                            Ty::Infer(InferTy::IntVar(..)) | Ty::Infer(InferTy::FloatVar(..)) => {
                                inner_ty
                            }
                            // FIXME: resolve ops::Neg trait
                            _ => Ty::Unknown,
                        }
                    }
                    UnaryOp::Not => {
                        match &inner_ty {
                            Ty::Apply(a_ty) => match a_ty.ctor {
                                TypeCtor::Bool | TypeCtor::Int(_) => inner_ty,
                                _ => Ty::Unknown,
                            },
                            Ty::Infer(InferTy::IntVar(..)) => inner_ty,
                            // FIXME: resolve ops::Not trait for inner_ty
                            _ => Ty::Unknown,
                        }
                    }
                }
            }
            Expr::BinaryOp { lhs, rhs, op } => match op {
                Some(op) => {
                    let lhs_expectation = match op {
                        BinaryOp::BooleanAnd | BinaryOp::BooleanOr => {
                            Expectation::has_type(Ty::simple(TypeCtor::Bool))
                        }
                        _ => Expectation::none(),
                    };
                    let lhs_ty = self.infer_expr(*lhs, &lhs_expectation);
                    // FIXME: find implementation of trait corresponding to operation
                    // symbol and resolve associated `Output` type
                    let rhs_expectation = op::binary_op_rhs_expectation(*op, lhs_ty);
                    let rhs_ty = self.infer_expr(*rhs, &Expectation::has_type(rhs_expectation));

                    // FIXME: similar as above, return ty is often associated trait type
                    op::binary_op_return_ty(*op, rhs_ty)
                }
                _ => Ty::Unknown,
            },
            Expr::Tuple { exprs } => {
                let mut ty_vec = Vec::with_capacity(exprs.len());
                for arg in exprs.iter() {
                    ty_vec.push(self.infer_expr(*arg, &Expectation::none()));
                }

                Ty::apply(TypeCtor::Tuple, Substs(ty_vec.into()))
            }
            Expr::Array(array) => {
                let elem_ty = match &expected.ty {
                    Ty::Apply(a_ty) => match a_ty.ctor {
                        TypeCtor::Slice | TypeCtor::Array => {
                            Ty::clone(&a_ty.parameters.as_single())
                        }
                        _ => self.new_type_var(),
                    },
                    _ => self.new_type_var(),
                };

                match array {
                    Array::ElementList(items) => {
                        for expr in items.iter() {
                            self.infer_expr(*expr, &Expectation::has_type(elem_ty.clone()));
                        }
                    }
                    Array::Repeat { initializer, repeat } => {
                        self.infer_expr(*initializer, &Expectation::has_type(elem_ty.clone()));
                        self.infer_expr(
                            *repeat,
                            &Expectation::has_type(Ty::simple(TypeCtor::Int(
                                primitive::UncertainIntTy::Known(primitive::IntTy::usize()),
                            ))),
                        );
                    }
                }

                Ty::apply_one(TypeCtor::Array, elem_ty)
            }
            Expr::Literal(lit) => match lit {
                Literal::Bool(..) => Ty::simple(TypeCtor::Bool),
                Literal::String(..) => {
                    Ty::apply_one(TypeCtor::Ref(Mutability::Shared), Ty::simple(TypeCtor::Str))
                }
                Literal::ByteString(..) => {
                    let byte_type = Ty::simple(TypeCtor::Int(primitive::UncertainIntTy::Known(
                        primitive::IntTy::u8(),
                    )));
                    let slice_type = Ty::apply_one(TypeCtor::Slice, byte_type);
                    Ty::apply_one(TypeCtor::Ref(Mutability::Shared), slice_type)
                }
                Literal::Char(..) => Ty::simple(TypeCtor::Char),
                Literal::Int(_v, ty) => Ty::simple(TypeCtor::Int(*ty)),
                Literal::Float(_v, ty) => Ty::simple(TypeCtor::Float(*ty)),
            },
        };
        // use a new type variable if we got Ty::Unknown here
        let ty = self.insert_type_vars_shallow(ty);
        self.unify(&ty, &expected.ty);
        let ty = self.resolve_ty_as_possible(&mut vec![], ty);
        self.write_expr_ty(tgt_expr, ty.clone());
        ty
    }

    fn infer_block(
        &mut self,
        statements: &[Statement],
        tail: Option<ExprId>,
        expected: &Expectation,
    ) -> Ty {
        for stmt in statements {
            match stmt {
                Statement::Let { pat, type_ref, initializer } => {
                    let decl_ty =
                        type_ref.as_ref().map(|tr| self.make_ty(tr)).unwrap_or(Ty::Unknown);
                    let decl_ty = self.insert_type_vars(decl_ty);
                    let ty = if let Some(expr) = initializer {
                        let expr_ty = self.infer_expr(*expr, &Expectation::has_type(decl_ty));
                        expr_ty
                    } else {
                        decl_ty
                    };

                    self.infer_pat(*pat, &ty, BindingMode::default());
                }
                Statement::Expr(expr) => {
                    self.infer_expr(*expr, &Expectation::none());
                }
            }
        }
        let ty = if let Some(expr) = tail { self.infer_expr(expr, expected) } else { Ty::unit() };
        ty
    }

    fn collect_const_signature(&mut self, signature: &ConstSignature) {
        self.return_ty = self.make_ty(signature.type_ref());
    }

    fn collect_fn_signature(&mut self, signature: &FnSignature) {
        let body = Arc::clone(&self.body); // avoid borrow checker problem
        for (type_ref, pat) in signature.params().iter().zip(body.params()) {
            let ty = self.make_ty(type_ref);

            self.infer_pat(*pat, &ty, BindingMode::default());
        }
        self.return_ty = self.make_ty(signature.ret_type());
    }

    fn infer_body(&mut self) {
        self.infer_expr(self.body.body_expr(), &Expectation::has_type(self.return_ty.clone()));
    }
}

/// The ID of a type variable.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TypeVarId(pub(super) u32);

impl UnifyKey for TypeVarId {
    type Value = TypeVarValue;

    fn index(&self) -> u32 {
        self.0
    }

    fn from_index(i: u32) -> Self {
        TypeVarId(i)
    }

    fn tag() -> &'static str {
        "TypeVarId"
    }
}

/// The value of a type variable: either we already know the type, or we don't
/// know it yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeVarValue {
    Known(Ty),
    Unknown,
}

impl TypeVarValue {
    fn known(&self) -> Option<&Ty> {
        match self {
            TypeVarValue::Known(ty) => Some(ty),
            TypeVarValue::Unknown => None,
        }
    }
}

impl UnifyValue for TypeVarValue {
    type Error = NoError;

    fn unify_values(value1: &Self, value2: &Self) -> Result<Self, NoError> {
        match (value1, value2) {
            // We should never equate two type variables, both of which have
            // known types. Instead, we recursively equate those types.
            (TypeVarValue::Known(t1), TypeVarValue::Known(t2)) => panic!(
                "equating two type variables, both of which have known types: {:?} and {:?}",
                t1, t2
            ),

            // If one side is known, prefer that one.
            (TypeVarValue::Known(..), TypeVarValue::Unknown) => Ok(value1.clone()),
            (TypeVarValue::Unknown, TypeVarValue::Known(..)) => Ok(value2.clone()),

            (TypeVarValue::Unknown, TypeVarValue::Unknown) => Ok(TypeVarValue::Unknown),
        }
    }
}

/// The kinds of placeholders we need during type inference. There's separate
/// values for general types, and for integer and float variables. The latter
/// two are used for inference of literal values (e.g. `100` could be one of
/// several integer types).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InferTy {
    TypeVar(TypeVarId),
    IntVar(TypeVarId),
    FloatVar(TypeVarId),
}

impl InferTy {
    fn to_inner(self) -> TypeVarId {
        match self {
            InferTy::TypeVar(ty) | InferTy::IntVar(ty) | InferTy::FloatVar(ty) => ty,
        }
    }

    fn fallback_value(self) -> Ty {
        match self {
            InferTy::TypeVar(..) => Ty::Unknown,
            InferTy::IntVar(..) => {
                Ty::simple(TypeCtor::Int(primitive::UncertainIntTy::Known(primitive::IntTy::i32())))
            }
            InferTy::FloatVar(..) => Ty::simple(TypeCtor::Float(
                primitive::UncertainFloatTy::Known(primitive::FloatTy::f64()),
            )),
        }
    }
}

/// When inferring an expression, we propagate downward whatever type hint we
/// are able in the form of an `Expectation`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Expectation {
    ty: Ty,
    // FIXME: In some cases, we need to be aware whether the expectation is that
    // the type match exactly what we passed, or whether it just needs to be
    // coercible to the expected type. See Expectation::rvalue_hint in rustc.
}

impl Expectation {
    /// The expectation that the type of the expression needs to equal the given
    /// type.
    fn has_type(ty: Ty) -> Self {
        Expectation { ty }
    }

    /// This expresses no expectation on the type.
    fn none() -> Self {
        Expectation { ty: Ty::Unknown }
    }
}

mod diagnostics {
    use crate::{expr::ExprId, diagnostics::{DiagnosticSink, NoSuchField}, HirDatabase, Function};

    #[derive(Debug, PartialEq, Eq, Clone)]
    pub(super) enum InferenceDiagnostic {
        NoSuchField { expr: ExprId, field: usize },
    }

    impl InferenceDiagnostic {
        pub(super) fn add_to(
            &self,
            db: &impl HirDatabase,
            owner: Function,
            sink: &mut DiagnosticSink,
        ) {
            match self {
                InferenceDiagnostic::NoSuchField { expr, field } => {
                    let (file, _) = owner.source(db);
                    let field = owner.body_source_map(db).field_syntax(*expr, *field);
                    sink.push(NoSuchField { file, field })
                }
            }
        }
    }
}
