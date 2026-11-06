#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Rule matching, ephemeral/stale classification, and scoring for bclean.
//!
//! * [`RuleSet`] compiles the rules of a [`bclean_model::Config`] into a fast matcher (glob
//!   sets for names, a parent-directory index for path globs, anchors answered from
//!   caller-supplied sibling names). See [`matcher`] for the exact semantics and priority
//!   resolution.
//! * [`RuleSet::ephemeral_children`] is the scanner's prune-oracle entry point: given a
//!   directory and its children it reports which children to summarize.
//! * [`Classifier`] walks a [`bclean_tree::Tree`], producing a
//!   [`bclean_model::Classification`] for every matched entry and every durable-stale
//!   suggestion, with staleness from [`bclean_staleness`] and a score (see [`score`]).
//! * [`Classifier::explain`] classifies one arbitrary path and explains every decision.

mod classify;
mod explain;
mod ignore;
pub mod matcher;
pub mod score;

pub use classify::{ClassifiedEntry, Classifier, REASON_RULE_MATCH};
pub use explain::{Enclosing, Explanation, NearMiss};
pub use ignore::GitIgnoreCache;
pub use matcher::{
    CACHEDIR_TAG, CACHEDIR_TAG_RULE, ChildEntry, CompileWarning, CompiledRule, Contents,
    GITIGNORED_RULE, MatchResult, Miss, Probe, RuleError, RuleSet, SiblingNames,
};
