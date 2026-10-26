use super::super::super::test_support::TempFolder;
use super::*;

/// A log in memory. Its sequences start at `first` and can jump, as a reopened log's do.
#[derive(Default)]
pub(crate) struct MemoryLog {
    inner: std::sync::Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    next: u64,
    records: Vec<(u64, Vec<u8>)>,
    fail: bool,
    fail_reads: bool,
    records_read: usize,
}

impl MemoryLog {
    pub(crate) fn jump_to(&self, next: u64) {
        self.inner.lock().unwrap().next = next;
    }

    /// The log is lost and made again: it holds no record and starts at sequence 0.
    pub(crate) fn start_again(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.next = 0;
        inner.records.clear();
    }

    pub(crate) fn fail_appends(&self, fail: bool) {
        self.inner.lock().unwrap().fail = fail;
    }

    pub(crate) fn fail_reads(&self, fail: bool) {
        self.inner.lock().unwrap().fail_reads = fail;
    }

    /// How many records its reads returned so far.
    pub(crate) fn records_read(&self) -> usize {
        self.inner.lock().unwrap().records_read
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap().records.len()
    }
}

#[tonic::async_trait]
impl RecordLog for MemoryLog {
    async fn append(&self, value: Vec<u8>) -> Result<u64, LogError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail {
            return Err(LogError("test write failure".into()));
        }
        let sequence = inner.next;
        inner.next += 1;
        inner.records.push((sequence, value));
        Ok(sequence)
    }

    async fn read_after(&self, after: Option<u64>) -> Result<Vec<(u64, Vec<u8>)>, LogError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_reads {
            return Err(LogError("test read failure".into()));
        }
        let read: Vec<_> = inner
            .records
            .iter()
            .filter(|(sequence, _)| after.is_none_or(|after| *sequence > after))
            .cloned()
            .collect();
        inner.records_read += read.len();
        Ok(read)
    }
}

#[tokio::test]
async fn an_opendata_log_keeps_records_across_a_reopen_and_reads_after_a_sequence() {
    let temp = TempFolder::new();
    let folder = temp.0.join("data-log");
    let first;
    {
        let log = OpenDataLog::open(&folder, b"graph/data").await.unwrap();
        first = log.append(b"one".to_vec()).await.unwrap();
        log.append(b"two".to_vec()).await.unwrap();
    }
    let log = OpenDataLog::open(&folder, b"graph/data").await.unwrap();
    let third = log.append(b"three".to_vec()).await.unwrap();
    assert!(third > first + 1 || third == first + 2);
    let all = log.read_after(None).await.unwrap();
    let values: Vec<&[u8]> = all.iter().map(|(_, v)| v.as_slice()).collect();
    assert_eq!(values, [b"one".as_slice(), b"two", b"three"]);
    let after_first = log.read_after(Some(first)).await.unwrap();
    assert_eq!(after_first.len(), 2);
    assert_eq!(end_sequence(&log).await.unwrap(), Some(third));
    let mode = std::fs::metadata(&folder).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.mode() & 0o777, 0o700);
}

#[tokio::test]
async fn the_memory_log_jumps_and_reads_after() {
    let log = MemoryLog::default();
    assert_eq!(log.append(b"a".to_vec()).await.unwrap(), 0);
    log.jump_to(10);
    assert_eq!(log.append(b"b".to_vec()).await.unwrap(), 10);
    assert_eq!(log.read_after(Some(0)).await.unwrap().len(), 1);
    assert_eq!(log.read_after(Some(10)).await.unwrap().len(), 0);
}
