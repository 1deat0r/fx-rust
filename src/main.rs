use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match fxrs::cli::run_main(args).await {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("fxrs: {e:#}");
            ExitCode::from(1)
        }
    }
}
