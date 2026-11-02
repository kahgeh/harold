use std::sync::{Arc, Mutex};

use super::*;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-order-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn order(&self) -> RemoteOrder {
        RemoteOrder::new(&self.0)
    }

    fn write_file(&self, text: &str) {
        std::fs::write(self.0.join(ORDER_FILE), text).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Runs `action` with the log lines captured.
fn with_logs<T>(action: impl FnOnce() -> T) -> (T, Vec<String>) {
    crate::test_logs::keep_second_dispatcher();
    let logs = Logs::default();
    let writer = logs.clone();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    let result = action();
    drop(guard);
    (result, logs.lines())
}

fn unreadable_lines(lines: &[String], reason: &str) -> usize {
    lines
        .iter()
        .filter(|line| {
            line.contains("remote order unreadable")
                && line.contains(&format!("reason=\"{reason}\""))
        })
        .count()
}

#[test]
fn set_then_get_returns_the_same_order() {
    let scratch = Scratch::new();
    let registry = names(&["a", "b", "c"]);
    scratch.order().set(&names(&["c", "a", "b"])).unwrap();
    assert_eq!(scratch.order().get(&registry), names(&["c", "a", "b"]));
}

#[test]
fn a_product_not_in_the_file_joins_at_the_end_in_registry_order() {
    let scratch = Scratch::new();
    scratch.order().set(&names(&["c"])).unwrap();
    let registry = names(&["a", "b", "c", "d"]);
    assert_eq!(scratch.order().get(&registry), names(&["c", "a", "b", "d"]));
}

#[test]
fn a_product_not_in_the_registry_drops_out() {
    let scratch = Scratch::new();
    scratch.order().set(&names(&["gone", "b", "a"])).unwrap();
    assert_eq!(scratch.order().get(&names(&["a", "b"])), names(&["b", "a"]));
}

#[test]
fn set_stores_exactly_the_names_it_is_given() {
    let scratch = Scratch::new();
    scratch.order().set(&names(&["gone", "b"])).unwrap();
    let text = std::fs::read_to_string(scratch.0.join(ORDER_FILE)).unwrap();
    let stored: Vec<String> = serde_json::from_str(&text).unwrap();
    assert_eq!(stored, names(&["gone", "b"]));
}

#[test]
fn a_duplicate_name_in_the_file_counts_once() {
    let scratch = Scratch::new();
    scratch.write_file(r#"["b","a","b"]"#);
    let (order, lines) = with_logs(|| scratch.order().get(&names(&["a", "b", "c"])));
    assert_eq!(order, names(&["b", "a", "c"]));
    assert_eq!(unreadable_lines(&lines, "malformed"), 0);
}

#[test]
fn a_malformed_file_gives_the_registry_order_and_logs_once() {
    let scratch = Scratch::new();
    scratch.write_file("{not json");
    let (order, lines) = with_logs(|| scratch.order().get(&names(&["a", "b"])));
    assert_eq!(order, names(&["a", "b"]));
    assert_eq!(unreadable_lines(&lines, "malformed"), 1);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("remote order unreadable"))
            .count(),
        1
    );
}

#[test]
fn an_empty_file_is_malformed() {
    let scratch = Scratch::new();
    scratch.write_file("");
    let (order, lines) = with_logs(|| scratch.order().get(&names(&["a", "b"])));
    assert_eq!(order, names(&["a", "b"]));
    assert_eq!(unreadable_lines(&lines, "malformed"), 1);
}

#[test]
fn a_file_that_holds_null_is_malformed() {
    let scratch = Scratch::new();
    scratch.write_file("null");
    let (order, lines) = with_logs(|| scratch.order().get(&names(&["a", "b"])));
    assert_eq!(order, names(&["a", "b"]));
    assert_eq!(unreadable_lines(&lines, "malformed"), 1);
}

#[test]
fn a_missing_file_logs_missing_once_for_each_read() {
    let scratch = Scratch::new();
    let (order, lines) = with_logs(|| {
        let first = scratch.order().get(&names(&["a", "b"]));
        let second = scratch.order().get(&names(&["a", "b"]));
        assert_eq!(first, second);
        first
    });
    assert_eq!(order, names(&["a", "b"]));
    assert_eq!(unreadable_lines(&lines, "missing"), 2);
}

#[test]
fn a_file_that_cannot_be_read_logs_unreadable() {
    let scratch = Scratch::new();
    // A folder in the place of the file cannot be read as text.
    std::fs::create_dir(scratch.0.join(ORDER_FILE)).unwrap();
    let (order, lines) = with_logs(|| scratch.order().get(&names(&["a"])));
    assert_eq!(order, names(&["a"]));
    assert_eq!(unreadable_lines(&lines, "unreadable"), 1);
}

#[test]
fn set_logs_the_count_and_writes_with_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let (result, lines) = with_logs(|| scratch.order().set(&names(&["a", "b"])));
    result.unwrap();
    let saved: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("remote order saved"))
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].contains("products=2"), "{}", saved[0]);
    let mode = std::fs::metadata(scratch.0.join(ORDER_FILE))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
