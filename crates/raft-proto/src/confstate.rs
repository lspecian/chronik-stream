// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.
// Vendored from tikv/raft-rs v0.7.0, adapted for prost compatibility.

use crate::eraftpb::ConfState;

fn eq_without_order(lhs: &[u64], rhs: &[u64]) -> bool {
    if lhs.len() != rhs.len() {
        return false;
    }
    for l in lhs {
        if !rhs.contains(l) {
            return false;
        }
    }
    true
}

/// Check equivalence of two ConfState instances.
/// Does field-by-field equivalence using set equivalence for repeated fields.
pub fn conf_state_eq(lhs: &ConfState, rhs: &ConfState) -> bool {
    eq_without_order(&lhs.voters, &rhs.voters)
        && eq_without_order(&lhs.learners, &rhs.learners)
        && eq_without_order(&lhs.voters_outgoing, &rhs.voters_outgoing)
        && eq_without_order(&lhs.learners_next, &rhs.learners_next)
        && lhs.auto_leave == rhs.auto_leave
}
