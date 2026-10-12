//! Regression coverage for the unsupported-platform entry point.

use super::*;
use std::process::Termination;

#[test]
fn unsupported_platform_exits_with_failure() {
    assert_eq!(main().report(), std::process::ExitCode::FAILURE);
}
