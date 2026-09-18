//! Source-role classification and retrieval filter request validation.
//!
//! Public items are re-exported from private child modules so existing paths
//! such as `crate::filter::FilterRequest` remain stable.

mod request;
mod source_role;

pub use request::{
    EffectiveFilter, EffectiveFilterReport, FilterError, FilterRequest, GlobPattern,
};
pub use source_role::{SourceRole, SourceRoleError, classify_source_role};

#[cfg(test)]
mod tests;
