// Copyright 2018 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use rustc::traits::{
    Clause,
    Clauses,
    DomainGoal,
    FromEnv,
    ProgramClause,
    ProgramClauseCategory,
    Environment,
};
use rustc::ty::{self, TyCtxt, Ty};
use rustc::hir::def_id::DefId;
use rustc_data_structures::fx::FxHashSet;

struct ClauseVisitor<'set, 'a, 'tcx: 'a + 'set> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    round: &'set mut FxHashSet<Clause<'tcx>>,
}

impl ClauseVisitor<'set, 'a, 'tcx> {
    fn new(tcx: TyCtxt<'a, 'tcx, 'tcx>, round: &'set mut FxHashSet<Clause<'tcx>>) -> Self {
        ClauseVisitor {
            tcx,
            round,
        }
    }

    fn visit_ty(&mut self, ty: Ty<'tcx>) {
        match ty.sty {
            ty::Projection(data) => {
                self.round.extend(
                    self.tcx.program_clauses_for(data.item_def_id)
                        .iter()
                        .filter(|c| c.category() == ProgramClauseCategory::ImpliedBound)
                        .cloned()
                );
            }

            // forall<'a, T> { `Outlives(T, 'a) :- FromEnv(&'a T)` }
            ty::Ref(_region, _sub_ty, ..) => {
                // FIXME: we'd need bound tys in order to properly write the above rule
            }

            ty::Dynamic(..) => {
                // FIXME: trait object rules are not yet implemented
            }

            ty::Adt(def, ..) => {
                self.round.extend(
                    self.tcx.program_clauses_for(def.did)
                        .iter()
                        .filter(|c| c.category() == ProgramClauseCategory::ImpliedBound)
                        .cloned()
                );
            }

            ty::Foreign(def_id) |
            ty::FnDef(def_id, ..) |
            ty::Closure(def_id, ..) |
            ty::Generator(def_id, ..) |
            ty::Opaque(def_id, ..) => {
                self.round.extend(
                    self.tcx.program_clauses_for(def_id)
                        .iter()
                        .filter(|c| c.category() == ProgramClauseCategory::ImpliedBound)
                        .cloned()
                );
            }

            ty::Bool |
            ty::Char |
            ty::Int(..) |
            ty::Uint(..) |
            ty::Float(..) |
            ty::Str |
            ty::Array(..) |
            ty::Slice(..) |
            ty::RawPtr(..) |
            ty::FnPtr(..) |
            ty::Tuple(..) |
            ty::Never |
            ty::Param(..) => (),

            ty::GeneratorWitness(..) |
            ty::UnnormalizedProjection(..) |
            ty::Infer(..) |
            ty::Bound(..) |
            ty::Error => {
                bug!("unexpected type {:?}", ty);
            }
        }
    }

    fn visit_from_env(&mut self, from_env: FromEnv<'tcx>) {
        match from_env {
            FromEnv::Trait(predicate) => {
                self.round.extend(
                    self.tcx.program_clauses_for(predicate.def_id())
                        .iter()
                        .filter(|c| c.category() == ProgramClauseCategory::ImpliedBound)
                        .cloned()
                );
            }

            FromEnv::Ty(ty) => self.visit_ty(ty),
        }
    }

    fn visit_domain_goal(&mut self, domain_goal: DomainGoal<'tcx>) {
        // The only domain goals we can find in an environment are:
        // * `DomainGoal::Holds(..)`
        // * `DomainGoal::FromEnv(..)`
        // The former do not lead to any implied bounds. So we only need
        // to visit the latter.
        if let DomainGoal::FromEnv(from_env) = domain_goal {
            self.visit_from_env(from_env);
        }
    }

    fn visit_program_clause(&mut self, clause: ProgramClause<'tcx>) {
        self.visit_domain_goal(clause.goal);
        // No need to visit `clause.hypotheses`: they are always of the form
        // `FromEnv(...)` and were visited at a previous round.
    }

    fn visit_clause(&mut self, clause: Clause<'tcx>) {
        match clause {
            Clause::Implies(clause) => self.visit_program_clause(clause),
            Clause::ForAll(clause) => self.visit_program_clause(*clause.skip_binder()),
        }
    }
}

crate fn program_clauses_for_env<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    environment: Environment<'tcx>,
) -> Clauses<'tcx> {
    debug!("program_clauses_for_env(environment={:?})", environment);

    let mut last_round = FxHashSet::default();
    {
        let mut visitor = ClauseVisitor::new(tcx, &mut last_round);
        for &clause in environment.clauses {
            visitor.visit_clause(clause);
        }
    }

    let mut closure = last_round.clone();
    let mut next_round = FxHashSet::default();
    while !last_round.is_empty() {
        let mut visitor = ClauseVisitor::new(tcx, &mut next_round);
        for clause in last_round.drain() {
            visitor.visit_clause(clause);
        }
        last_round.extend(
            next_round.drain().filter(|&clause| closure.insert(clause))
        );
    }

    debug!("program_clauses_for_env: closure = {:#?}", closure);

    return tcx.mk_clauses(
        closure.into_iter()
    );
}

crate fn environment<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) -> Environment<'tcx> {
    use super::{Lower, IntoFromEnvGoal};
    use rustc::hir::{Node, TraitItemKind, ImplItemKind, ItemKind, ForeignItemKind};

    // The environment of an impl Trait type is its defining function's environment.
    if let Some(parent) = ty::is_impl_trait_defn(tcx, def_id) {
        return environment(tcx, parent);
    }

    // Compute the bounds on `Self` and the type parameters.
    let ty::InstantiatedPredicates { predicates } =
        tcx.predicates_of(def_id).instantiate_identity(tcx);

    let clauses = predicates.into_iter()
        .map(|predicate| predicate.lower())
        .map(|domain_goal| domain_goal.map_bound(|bound| bound.into_from_env_goal()))
        .map(|domain_goal| domain_goal.map_bound(|bound| bound.into_program_clause()))

        // `ForAll` because each `domain_goal` is a `PolyDomainGoal` and
        // could bound lifetimes.
        .map(Clause::ForAll);

    let node_id = tcx.hir.as_local_node_id(def_id).unwrap();
    let node = tcx.hir.get(node_id);

    let mut is_fn = false;
    let mut is_impl = false;
    match node {
        Node::TraitItem(item) => match item.node {
            TraitItemKind::Method(..) => is_fn = true,
            _ => (),
        }

        Node::ImplItem(item) => match item.node {
            ImplItemKind::Method(..) => is_fn = true,
            _ => (),
        }

        Node::Item(item) => match item.node {
            ItemKind::Impl(..) => is_impl = true,
            ItemKind::Fn(..) => is_fn = true,
            _ => (),
        }

        Node::ForeignItem(item) => match item.node {
            ForeignItemKind::Fn(..) => is_fn = true,
            _ => (),
        }

        // FIXME: closures?
        _ => (),
    }

    let mut input_tys = FxHashSet::default();

    // In an impl, we assume that the receiver type and all its constituents
    // are well-formed.
    if is_impl {
        let trait_ref = tcx.impl_trait_ref(def_id).expect("not an impl");
        input_tys.extend(trait_ref.self_ty().walk());
    }

    // In an fn, we assume that the arguments and all their constituents are
    // well-formed.
    if is_fn {
        let fn_sig = tcx.fn_sig(def_id);
        input_tys.extend(
            // FIXME: `skip_binder` seems ok for now? In a real setting,
            // the late bound regions would next be instantiated with things
            // in the inference table.
            fn_sig.skip_binder().inputs().iter().flat_map(|ty| ty.walk())
        );
    }

    let clauses = clauses.chain(
        input_tys.into_iter()
            .map(|ty| DomainGoal::FromEnv(FromEnv::Ty(ty)))
            .map(|domain_goal| domain_goal.into_program_clause())
            .map(Clause::Implies)
    );

    Environment {
        clauses: tcx.mk_clauses(clauses),
    }
}
