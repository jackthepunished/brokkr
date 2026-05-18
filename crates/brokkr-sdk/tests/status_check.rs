//! Tests for ExecuteResponse status handling via `check_status`.
//!
//! These tests verify the pure `check_status` function that underpins
//! `run_command`'s error-path behavior. See issue #61.

#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::panic)]

use brokkr_proto::google::rpc::Status as RpcStatus;
use brokkr_proto::reapi_v2::{ActionResult, ExecuteResponse};

use brokkr_sdk::client::{check_status, ExecuteError};

/// ExecuteResponse with status.code != 0 returns Err((code, message)).
#[test]
fn check_status_nonzero_code_returns_err() {
    let resp = ExecuteResponse {
        status: Some(RpcStatus {
            code: 3, // FAILED_PRECONDITION
            message: "missing input blob".to_string(),
            details: vec![],
        }),
        result: None,
        cached_result: false,
        ..Default::default()
    };

    let result = check_status(&resp);
    assert!(result.is_err());
    let ExecuteError::Status { code, message } = result.unwrap_err() else {
        panic!("expected Status variant");
    };
    assert_eq!(code, 3);
    assert_eq!(message, "missing input blob");
}

/// ExecuteResponse with status.code == 0 and result present returns Ok.
#[test]
fn check_status_zero_code_with_result_returns_ok() {
    let resp = ExecuteResponse {
        status: Some(RpcStatus {
            code: 0,
            message: String::new(),
            details: vec![],
        }),
        result: Some(ActionResult {
            exit_code: 0,
            stdout_raw: b"hello".to_vec(),
            stderr_raw: b"".to_vec(),
            ..Default::default()
        }),
        cached_result: false,
        ..Default::default()
    };

    let result = check_status(&resp);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().exit_code, 0);
}

/// ExecuteResponse with no status field is treated as OK (proceeds).
#[test]
fn check_status_none_treated_as_ok() {
    let resp = ExecuteResponse {
        status: None,
        result: Some(ActionResult {
            exit_code: 42,
            stdout_raw: b"".to_vec(),
            stderr_raw: b"".to_vec(),
            ..Default::default()
        }),
        cached_result: false,
        ..Default::default()
    };

    let result = check_status(&resp);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().exit_code, 42);
}

/// ExecuteResponse with OK status but no result returns Err.
#[test]
fn check_status_result_none_returns_err() {
    let resp = ExecuteResponse {
        status: Some(RpcStatus {
            code: 0,
            message: String::new(),
            details: vec![],
        }),
        result: None,
        cached_result: false,
        ..Default::default()
    };

    let result = check_status(&resp);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, ExecuteError::MissingResult));
}
