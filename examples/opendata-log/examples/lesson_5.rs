use opendata_agent_progress::{Result, backend, checkpoint_path, config, durable_round_trip};

#[tokio::main]
async fn main() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), run()).await?
}

async fn run() -> Result<()> {
    if backend()? != "s3" {
        return Err(
            "Lesson 5 requires OPENDATA_BACKEND=s3 and an existing general purpose bucket".into(),
        );
    }
    durable_round_trip(config("lesson-5")?, &checkpoint_path("lesson-5")).await
}
