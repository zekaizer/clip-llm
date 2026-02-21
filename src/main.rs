use tracing::info;

fn main() {
    tracing_subscriber::fmt::init();
    info!("clip-llm starting");
}
