#[tokio::main]
async fn main() -> std::process::ExitCode {
    magpie::cli::entry().await
}
