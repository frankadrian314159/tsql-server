//! Rewrites of the syntax tree that do not depend on where the data lives: shared by the engine (which runs them before it plans a
//! query) and the cluster's control node.
pub mod astutil;
pub mod decorrelate;
