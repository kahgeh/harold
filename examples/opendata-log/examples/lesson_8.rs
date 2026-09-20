use opendata_agent_progress::{Result, backend, checkpoint_path, config, durable_round_trip};

#[tokio::main]
async fn main() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), run()).await?
}

async fn run() -> Result<()> {
    if backend()? != "express" {
        return Err(
            "Lesson 8 requires OPENDATA_BACKEND=express and an existing directory bucket".into(),
        );
    }
    durable_round_trip(config("lesson-8")?, &checkpoint_path("lesson-8")).await
}
