//! Bound ZIP central-directory allocation before library parsing, including ZIP64.

use crate::{Error, ErrorKind, Result};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

pub(super) fn inspect(input: &mut File, maximum_entries: usize) -> Result<u64> {
    let (entries, directory_bytes) = read_end(input).map_err(|error| {
        Error::new(
            ErrorKind::CorruptData,
            format!("Inspect release archive: {error}"),
        )
    })?;
    input
        .seek(SeekFrom::Start(0))
        .map_err(|error| Error::new(ErrorKind::CorruptData, error.to_string()))?;
    if entries > maximum_entries as u64 || directory_bytes > 64 * 1024 * 1024 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Archive central directory exceeds its inspection limits",
        ));
    }
    Ok(entries)
}

fn read_end(input: &mut File) -> io::Result<(u64, u64)> {
    let size = input.metadata()?.len();
    let tail_size = size.min(65557) as usize;
    input.seek(SeekFrom::End(-(tail_size as i64)))?;
    let mut tail = vec![0; tail_size];
    input.read_exact(&mut tail)?;
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|offset| {
            tail.get(*offset..*offset + 4) == Some(&b"PK\x05\x06"[..])
                && *offset
                    + 22
                    + usize::from(u16::from_le_bytes([tail[*offset + 20], tail[*offset + 21]]))
                    == tail.len()
        })
        .ok_or_else(|| invalid("ZIP end record is missing"))?;
    let record = &tail[end..];
    let word = |offset| u16::from_le_bytes([record[offset], record[offset + 1]]);
    let dword = |offset| u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap());
    if word(4) != 0 || word(6) != 0 || word(8) != word(10) {
        return Err(invalid("Multi-disk release archives are unsupported"));
    }
    if word(10) != u16::MAX && dword(12) != u32::MAX && dword(16) != u32::MAX {
        return Ok((u64::from(word(10)), u64::from(dword(12))));
    }
    let end_position = size - tail_size as u64 + end as u64;
    if end_position < 20 {
        return Err(invalid("ZIP64 locator is missing"));
    }
    input.seek(SeekFrom::Start(end_position - 20))?;
    let mut locator = [0; 20];
    input.read_exact(&mut locator)?;
    if &locator[..4] != b"PK\x06\x07"
        || u32::from_le_bytes(locator[4..8].try_into().unwrap()) != 0
        || u32::from_le_bytes(locator[16..20].try_into().unwrap()) != 1
    {
        return Err(invalid("Invalid ZIP64 locator"));
    }
    let position = u64::from_le_bytes(locator[8..16].try_into().unwrap());
    if position
        .checked_add(56)
        .is_none_or(|end| end > end_position - 20)
    {
        return Err(invalid("Invalid ZIP64 offset"));
    }
    input.seek(SeekFrom::Start(position))?;
    let mut record = [0; 56];
    input.read_exact(&mut record)?;
    if &record[..4] != b"PK\x06\x06" || record[16..24] != [0; 8] {
        return Err(invalid("Invalid ZIP64 end record"));
    }
    let disk_entries = u64::from_le_bytes(record[24..32].try_into().unwrap());
    let entries = u64::from_le_bytes(record[32..40].try_into().unwrap());
    if disk_entries != entries {
        return Err(invalid("ZIP64 disk entries do not match"));
    }
    Ok((
        entries,
        u64::from_le_bytes(record[40..48].try_into().unwrap()),
    ))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
