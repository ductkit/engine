//! Sends one prepared attempt over HTTP (spec section 9).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{CONTENT_TYPE, USER_AGENT};

use crate::delivery::queue::{AttemptOutcome, PreparedAttempt, SignatureFault};
use crate::delivery::signing::sign;

const USER_AGENT_VALUE: &str = "Stripe/1.0 (+https://stripe.com/docs/webhooks)";

fn real_unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// POSTs the attempt, signed with real time. Returns the outcome and the real elapsed ms.
pub(crate) async fn send(client: &reqwest::Client, a: &PreparedAttempt) -> (AttemptOutcome, u64) {
    if a.outage {
        return (AttemptOutcome::Outage, 0);
    }
    let now = real_unix_now();
    let secrets: Vec<&str> = a.secrets.iter().map(String::as_str).collect();
    let signature = match &a.signature {
        SignatureFault::None => sign(now, &a.body, &secrets),
        SignatureFault::Skewed { secs } => sign(now + secs, &a.body, &secrets),
        SignatureFault::Invalid => sign(now, &a.body, &["whsec_invalid"]),
    };
    let start = Instant::now();
    let result = client
        .post(&a.url)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header("Stripe-Signature", signature)
        .body(a.body.clone())
        .timeout(Duration::from_millis(a.timeout_ms))
        .send()
        .await;
    let elapsed = start.elapsed().as_millis() as u64;
    let outcome = match result {
        Ok(r) if r.status().is_success() => AttemptOutcome::Success {
            status: r.status().as_u16(),
        },
        Ok(r) => AttemptOutcome::HttpError {
            status: r.status().as_u16(),
        },
        Err(e) if e.is_timeout() => AttemptOutcome::Timeout,
        Err(e) => AttemptOutcome::ConnectError {
            message: e.to_string(),
        },
    };
    (outcome, elapsed)
}
