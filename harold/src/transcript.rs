//! Reads the new lines of a transcript file from a kept byte offset.
//!
//! The restart reminder and the recap reader both use this reader.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// The most a first read takes from the end of a file, for a reader that needs a limit.
pub(crate) const FIRST_READ_LIMIT: u64 = 16 * 1024 * 1024;

/// How far a reader has read in one file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) offset: u64,
}

/// Reads the whole lines after `tail.offset`, calls `on_line` for each whole line (newline
/// kept), and moves `tail.offset` past them. A file that shrank is read again from its start.
/// A last line with no newline yet is left for the next read.
///
/// `first_read_limit`: `Some(n)` limits a read from offset 0. The read starts at `len - n`
/// and after the first newline in that window.
///
/// Returns the number of bytes of whole lines read, including a skipped partial line.
pub(crate) fn read_new_lines(
    path: &Path,
    tail: &mut Tail,
    first_read_limit: Option<u64>,
    mut on_line: impl FnMut(&[u8]),
) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length < tail.offset {
        tail.offset = 0;
    }
    let mut start = tail.offset;
    let mut skip_partial_line = false;
    if let Some(limit) = first_read_limit
        && start == 0
        && length > limit
    {
        start = length - limit;
        skip_partial_line = true;
    }
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut position = start;
    if skip_partial_line {
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            return Ok(0);
        }
        position += read as u64;
        tail.offset = position;
    }
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        position += read as u64;
        tail.offset = position;
        on_line(&buffer);
    }
    Ok(position - start)
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
