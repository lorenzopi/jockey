mod cli;
mod config;
mod critics;
mod events;
mod process;
mod providers;
mod render;
mod run;
mod security;
mod sessions;
mod snapshot;
mod steering;
#[cfg(test)]
mod testutil;
mod tui;
mod workflows;

#[tokio::main]
async fn main() {
    let code = match cli::main().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
