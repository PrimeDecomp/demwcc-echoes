//! Strict function comparator (DESIGN.md "Strict comparator"): mwcc-compare's strict level
//! (`mwcc_compare::strict`, its port of this crate's former `compare.rs` and `placeholder.rs`).
//!
//! Exact means the same size and masked instruction words, the same relocations per instruction,
//! equivalent relocation targets (by name for ordinary symbols, by bytes for generated names,
//! a target placeholder `fn_<addr>` only by a proof by bytes through a
//! [`PlaceholderProver`](crate::PlaceholderProver)) and a one-to-one mapping of targets. Literals
//! the target object only references are resolved through an [`ExternIndex`] of the module's
//! other target objects ([`ObjIndex::with_externs`]).
//!
//! `MWDEC_NO_PLACEHOLDER=1` drops the prover ([`ObjIndex::with_prover`] is given `None`), so
//! placeholders are only resolved against functions defined in our own object.
use mwdec_core::*;
pub use mwcc_compare::strict::{is_generated_name, masked_words, reloc_mask, DiffClass, ExternIndex, Index as ObjIndex};

/// Is placeholder resolution through a prover on?
pub fn placeholders_enabled() -> bool {
    std::env::var("MWDEC_NO_PLACEHOLDER").is_err()
}

#[derive(Clone, Debug)]
pub struct Detailed {
    pub result: CompareResult,
    /// Most fundamental difference found (`Exact` iff `result.exact`).
    pub class: DiffClass,
    /// Every difference class seen.
    pub classes: Vec<DiffClass>,
}

/// Strict comparison with difference classification, using prebuilt indexes.
pub fn compare_indexed(t: &ObjIndex, tf: &Function, o: &ObjIndex, of: &Function) -> Detailed {
    let s = mwcc_compare::strict::compare(t, tf, o, of);
    Detailed {
        result: CompareResult { exact: s.exact, score: s.score, target_len: tf.code.len() as u32, ours_len: of.code.len() as u32, notes: s.notes },
        class: s.class,
        classes: s.classes,
    }
}

/// Strict comparison with classification (builds indexes; prefer [`compare_indexed`] in loops,
/// and [`ObjIndex::with_externs`] for target objects whose literals live in other splits).
pub fn compare_detailed(target: &ObjectFile, target_fn: &Function, ours: &ObjectFile, ours_fn: &Function) -> Detailed {
    compare_indexed(&ObjIndex::new(target), target_fn, &ObjIndex::new(ours), ours_fn)
}

/// Strict comparison of one function (ours vs target), resolving literal values via each object's data.
pub fn compare(target: &ObjectFile, target_fn: &Function, ours: &ObjectFile, ours_fn: &Function) -> CompareResult {
    compare_detailed(target, target_fn, ours, ours_fn).result
}
