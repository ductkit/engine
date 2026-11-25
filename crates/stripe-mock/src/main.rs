use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(ductkit_stripe_mock::cli::run(
        std::env::args().skip(1).collect(),
    ))
}
