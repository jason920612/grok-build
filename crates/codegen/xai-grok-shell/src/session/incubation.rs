//! Shell-side re-export of the incubation engine.
//!
//! The engine (prompt, rate limiter, spawn) lives in
//! `xai_grok_tools::implementations::grok_build::compass::incubation` so
//! both triggers share one implementation and one per-session budget:
//! - the goal wait window (`update_goal(waiting_on: ...)`, hooked in
//!   `acp_session_impl/goal.rs`), and
//! - `map_update` marking a phase `waiting` (tools-side, non-goal too).

pub(crate) use xai_grok_tools::implementations::grok_build::compass::incubation::{
    should_incubate, spawn_incubation,
};
