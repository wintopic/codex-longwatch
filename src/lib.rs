#![allow(
    clippy::assigning_clones,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::default_trait_access,
    clippy::double_ended_iterator_last,
    clippy::duration_suboptimal_units,
    clippy::filter_map_bool_then,
    clippy::format_collect,
    clippy::ignored_unit_patterns,
    clippy::map_unwrap_or,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::unreadable_literal,
    clippy::unused_async,
    clippy::unused_self
)]

pub mod app_server;
pub mod backoff;
pub mod classifier;
pub mod config;
pub mod diagnostics;
pub mod gui_fallback;
pub mod jsonl;
pub mod queue;
pub mod runtime;
pub mod transport;

#[cfg(feature = "desktop")]
pub mod ui;
