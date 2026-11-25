mod common;

use std::time::{Duration, Instant};

use common::mock_with;
use ductkit_stripe_mock::{ChangeKind, ObjectType};

/// The spec example's action and churn rates (spec section 4).
pub const SPEC_RATES: &str = "[actions]\nnew_customer = 10.0\nstart_subscription = 4.0\ncancel_subscription = 0.5\ncatalog_change = 0.2\none_off_invoice = 2.0\nstep_delay = { distribution = \"lognormal\", median_ms = 200, p99_ms = 2000 }\npayment_failure_probability = 0.1\n[churn.customer]\nupdate = 3.0\ndelete = 0.2\n[churn.product]\nupdate = 0.5\narchive = 0.1\n[churn.price]\nupdate = 0.5\narchive = 0.1\n[churn.subscription]\nupdate = 0.5\n[churn.invoice]\nupdate = 0.5\n";

/// Run with `cargo test -p ductkit-stripe-mock --release --test perf -- --ignored`.
#[tokio::test]
#[ignore]
async fn spec_rates_90_days_is_fast() {
    let m = mock_with(SPEC_RATES).await;
    let started = Instant::now();
    m.step(Duration::from_secs(90 * 86_400)).await.unwrap();
    let took = started.elapsed();
    let customers = m
        .truth_log()
        .iter()
        .filter(|u| u.object_type == ObjectType::Customer && u.kind == ChangeKind::Created)
        .count();
    println!("90 days at spec rates: {took:?}, {customers} customers");
    assert!(customers >= 20_000, "{customers}");
    assert!(took < Duration::from_secs(60), "{took:?}");
}

/// Run with `cargo test -p ductkit-stripe-mock --release --test perf -- --ignored`.
#[tokio::test]
#[ignore]
async fn history_90d_spec_rates_is_fast() {
    let started = Instant::now();
    let m = mock_with(&format!("{SPEC_RATES}[history]\nduration = \"90d\"\n")).await;
    let took = started.elapsed();
    println!(
        "90 days of history at spec rates: {took:?}, {} truth-log entries",
        m.truth_log().len()
    );
    assert!(took < Duration::from_secs(60), "{took:?}");
}
