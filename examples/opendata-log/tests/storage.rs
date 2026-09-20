use bytes::Bytes;
use opendata_agent_progress::{Checkpoints, Result, local_config};
use opendata_log::{LogDb, LogRead, Record};
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn durable_records_and_separate_checkpoints_survive_reopen() -> Result<()> {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root =
        std::env::temp_dir().join(format!("opendata-storage-{}-{unique}", std::process::id()));
    let config = local_config(&root, "log")?;
    let checkpoint_path = root.join("monitor.json");
    let log = LogDb::open(config.clone()).await?;
    let first = log
        .try_append(vec![
            Record {
                key: Bytes::from_static(b"a"),
                value: Bytes::from_static(b"first"),
            },
            Record {
                key: Bytes::from_static(b"b"),
                value: Bytes::from_static(b"other"),
            },
        ])
        .await?;
    log.flush().await?;
    let mut checkpoints = Checkpoints::default();
    checkpoints
        .next
        .insert("a".into(), first.start_sequence + 1);
    checkpoints.save(&checkpoint_path)?;
    log.close().await?;

    let log = LogDb::open(config).await?;
    let checkpoints = Checkpoints::load(&checkpoint_path)?;
    assert_eq!(checkpoints.position("b"), 0);
    assert!(
        log.scan(Bytes::from_static(b"a"), checkpoints.position("a")..)
            .await?
            .next()
            .await?
            .is_none()
    );
    let second = log
        .try_append(vec![Record {
            key: Bytes::from_static(b"a"),
            value: Bytes::from_static(b"second"),
        }])
        .await?;
    log.flush().await?;
    let mut records = log
        .scan(Bytes::from_static(b"a"), checkpoints.position("a")..)
        .await?;
    let record = records
        .next()
        .await?
        .expect("new report must survive cursor gaps");
    assert_eq!(record.value, Bytes::from_static(b"second"));
    assert_eq!(record.sequence, second.start_sequence);
    assert!(record.sequence > first.start_sequence);
    assert!(records.next().await?.is_none());
    drop(records);
    log.close().await?;
    std::fs::write(&checkpoint_path, b"broken json")?;
    assert!(
        Checkpoints::load(&checkpoint_path).is_err(),
        "corruption must not silently reset the cursor"
    );
    std::fs::remove_dir_all(root)?;
    Ok(())
}
