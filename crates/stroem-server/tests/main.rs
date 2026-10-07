//! The crate's one integration-test binary: every file under `tests/` is a
//! module here (`autotests = false` in Cargo.toml), never its own target.
//! The one exception is `log_peak_alloc_test.rs` (`harness = false`): its
//! counting global allocator would count every other test's allocations.

mod artifact_api_test;
mod artifact_retention_test;
mod artifact_upload_test;
mod cascade_apply_test;
mod common;
mod dependency_conditions_migration_test;
mod dependency_conditions_recalc_pipeline_test;
mod duration_stats_test;
mod git_refs_claim_test;
mod git_refs_creation_test;
mod git_refs_read_paths_test;
mod git_refs_scheduler_test;
mod ha_test;
mod integration_test;
mod json_input_test;
mod mcp_artifacts_test;
mod mcp_test;
mod metrics_test;
mod oauth_flow_test;
mod orchestrator_test;
mod pin_store_test;
mod pinned_api_fields_test;
mod pinned_rerun_restart_test;
mod propagate_to_parent_test;
mod read_path_audit_test;
mod rerun_integration_test;
mod restart_integration_test;
mod s3_integration_test;
mod state_upload_test;
