//! gaggle — checklist-driven autonomous review/fix loop on the Goose GDK stack.
//!
//! Architecture (extensible; components are modules so more can be added):
//!
//! ```text
//! main.rs      CLI: init | run | status | list | history | requeue | restart | model
//! checklist.rs component checklist (markdown parse/export, like the Go original)
//! state.rs     per-component phase state machine + JSON persistence
//! status.rs    LIVE observability: status.json + activity.log
//! goose.rs     agent driver: goose run --recipe ... --output-format json
//! recipes.rs   embedded Goose recipes; optional .review/workflows/ overrides
//!              + config.toml template / gitignore snippet
//! verify.rs    run verify commands from config/checklist after a fix
//! commit.rs    git commit ownership (the harness commits, never the agent)
//! loop_engine.rs the main loop: review → fix → verify → commit → next
//!              then a full-suite gate that re-enters the fixer if red
//! ```
//!
//! Workflow: each component goes pending → reviewing → fixing → verifying
//! → committing → done (failed = quarantine). A red end-of-run gate
//! classifies the owning component and sends the fixer rather than
//! stopping. The loop is resumable from `.review/state.json` at any point.
//!
//! Agent model: configurable via optional `provider` / `model` keys in
//! `.review/config.toml` (applied as goose env pins by the driver); when
//! unset, goose's configured default is used. No model is hard-coded.

pub mod checklist;
pub mod commit;
pub mod discover;
pub mod goose;
pub mod loop_engine;
pub mod recipes;
pub mod state;
pub mod status;
pub mod verify;

/// The review directory (created by `init`).
pub const REVIEW_DIR: &str = ".review";
