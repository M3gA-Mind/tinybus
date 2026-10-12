use super::*;

#[test]
fn unsupported_platform_exits_with_failure() {
    assert_eq!(main(), std::process::ExitCode::FAILURE);
}
