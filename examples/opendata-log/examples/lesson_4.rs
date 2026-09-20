use opendata_agent_progress::{Result, backend, checkpoint_path, config, durable_round_trip};

#[tokio::main]
async fn main() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), run()).await?
}

async fn run() -> Result<()> {
    if backend()? != "local" {
        return Err("Lesson 4 requires OPENDATA_BACKEND=local (the default)".into());
    }
    durable_round_trip(config("lesson-4")?, &checkpoint_path("lesson-4")).await
}
