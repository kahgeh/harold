use std::io::Write as _;
use std::path::PathBuf;

use super::*;

/// A scratch folder holding one transcript, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-transcript-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("t.jsonl")
    }

    fn write(&self, text: &str) {
        std::fs::write(self.file(), text).unwrap();
    }

    fn append(&self, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.file())
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(scratch: &Scratch, tail: &mut Tail, limit: Option<u64>) -> (Vec<String>, u64) {
    let mut lines = Vec::new();
    let bytes = read_new_lines(&scratch.file(), tail, limit, |line| {
        lines.push(String::from_utf8(line.to_vec()).unwrap());
    })
    .unwrap();
    (lines, bytes)
}

#[test]
fn first_read_takes_the_whole_file() {
    let scratch = Scratch::new();
    scratch.write("one\ntwo\nthree\n");
    let mut tail = Tail::default();
    let (lines, bytes) = read(&scratch, &mut tail, None);
    assert_eq!(lines, ["one\n", "two\n", "three\n"]);
    assert_eq!(bytes, 14);
    assert_eq!(tail.offset, 14);
}

#[test]
fn first_read_with_a_limit_starts_after_the_first_newline_in_the_window() {
    let scratch = Scratch::new();
    scratch.write("aaaaaaaa\nbbbb\ncccc\n");
    let mut tail = Tail::default();
    // The last 8 bytes are "bb\ncccc\n": the window opens inside "bbbb".
    let (lines, _) = read(&scratch, &mut tail, Some(8));
    assert_eq!(lines, ["cccc\n"]);
    assert_eq!(tail.offset, 19);
}

#[test]
fn a_limit_larger_than_the_file_reads_all_of_it() {
    let scratch = Scratch::new();
    scratch.write("one\ntwo\n");
    let mut tail = Tail::default();
    let (lines, _) = read(&scratch, &mut tail, Some(1000));
    assert_eq!(lines, ["one\n", "two\n"]);
}

#[test]
fn a_later_read_takes_only_new_lines() {
    let scratch = Scratch::new();
    scratch.write("one\n");
    let mut tail = Tail::default();
    read(&scratch, &mut tail, Some(2));
    scratch.append("two\nthree\n");
    // The limit applies to a read from offset 0 only.
    let (lines, bytes) = read(&scratch, &mut tail, Some(2));
    assert_eq!(lines, ["two\n", "three\n"]);
    assert_eq!(bytes, 10);
    assert_eq!(tail.offset, 14);
}

#[test]
fn a_shrunk_file_is_read_again_from_its_start() {
    let scratch = Scratch::new();
    scratch.write("one\ntwo\nthree\n");
    let mut tail = Tail::default();
    read(&scratch, &mut tail, None);
    scratch.write("new\n");
    let (lines, _) = read(&scratch, &mut tail, None);
    assert_eq!(lines, ["new\n"]);
    assert_eq!(tail.offset, 4);
}

#[test]
fn a_half_line_waits_for_its_newline() {
    let scratch = Scratch::new();
    scratch.write("one\nhal");
    let mut tail = Tail::default();
    let (lines, bytes) = read(&scratch, &mut tail, None);
    assert_eq!(lines, ["one\n"]);
    assert_eq!(bytes, 4);
    assert_eq!(tail.offset, 4);
    scratch.append("f\n");
    let (lines, _) = read(&scratch, &mut tail, None);
    assert_eq!(lines, ["half\n"]);
    assert_eq!(tail.offset, 9);
}

#[test]
fn a_missing_file_is_an_error_with_kind_not_found() {
    let scratch = Scratch::new();
    let mut tail = Tail::default();
    let error = read_new_lines(&scratch.file(), &mut tail, None, |_| {}).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
