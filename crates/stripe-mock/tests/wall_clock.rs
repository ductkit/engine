mod common;

use std::time::Duration;

use common::mock_with;
use ductkit_stripe_mock::MockError;

/// 1 s real = 1 h simulated; about one new customer per simulated second.
const WALL: &str = "clock = { mode = \"wall\", speed = 3600.0 }\n[actions]\nnew_customer = 3600.0";

#[tokio::test]
async fn advances_and_generates() {
    let m = mock_with(WALL).await;
    let t0 = m.now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let advanced = m.now().0 - t0.0;
    assert!(advanced >= 300 * 3600 / 2, "{advanced}");
    assert!(!m.truth_log().is_empty());
}

#[tokio::test]
async fn pause_freezes_and_resume_does_not_jump() {
    let m = mock_with(WALL).await;
    m.pause().unwrap();
    let frozen = m.now();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(m.now(), frozen);
    m.resume().unwrap();
    assert!(m.now().0 - frozen.0 < 100 * 3600);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(m.now() > frozen);
}

#[tokio::test]
async fn mode_mismatch_errors() {
    let w = mock_with(WALL).await;
    assert!(matches!(
        w.step(Duration::from_secs(1)).await,
        Err(MockError::ClockMode(_))
    ));
    let v = mock_with("").await;
    assert!(matches!(v.pause(), Err(MockError::ClockMode(_))));
    assert!(matches!(v.resume(), Err(MockError::ClockMode(_))));
}
