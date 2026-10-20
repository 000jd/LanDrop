//! Minimal ZIP writer for `Stored` (uncompressed) entries.
//!
//! Browser share links bundle a multi-file selection or a folder into a `.zip`
//! without pulling in a compression crate: ZIP's `Stored` method needs only the
//! CRC-32 and the fixed headers below. Entries are written with UTF-8 names and
//! 0600 permissions, and the archive is capped at the classic (non-ZIP64) size
//! limits.

use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// One file inside the archive.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// Path inside the archive, `/` separated and already sanitized.
    pub name: String,
    /// Source file on disk.
    pub path: PathBuf,
}

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL: u32 = 0x0605_4b50;
const VERSION: u16 = 20;
const UTF8_FLAG: u16 = 0x0800;
const METHOD_STORED: u16 = 0;
/// 1980-01-01, the first date the DOS timestamp can represent.
const DOS_DATE: u16 = 0x0021;
const DOS_TIME: u16 = 0;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32(data: &[u8], mut crc: u32) -> u32 {
    for &byte in data {
        crc = CRC_TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

fn write_u16<W: Write>(out: &mut W, value: u16) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_u32<W: Write>(out: &mut W, value: u32) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Reject names that could escape the archive root or confuse readers.
fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(format!("unsafe archive entry name: {name:?}")));
    }
    Ok(())
}

struct CentralEntry {
    name: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
}

/// Write `entries` to `out` as an uncompressed ZIP archive.
///
/// The caller owns `out`: it must be a unique path that does not exist yet.
pub fn write(out: &Path, entries: &[ZipEntry]) -> io::Result<()> {
    if entries.len() > u16::MAX as usize {
        return Err(invalid("too many files for a single archive"));
    }

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut writer = BufWriter::new(options.open(out)?);

    let mut offset: u64 = 0;
    let mut central: Vec<CentralEntry> = Vec::with_capacity(entries.len());
    let mut buffer = vec![0u8; 64 * 1024];

    for entry in entries {
        validate_name(&entry.name)?;
        let name = entry.name.as_bytes();
        if name.len() > u16::MAX as usize {
            return Err(invalid(format!(
                "archive entry name is too long: {}",
                entry.name
            )));
        }
        let metadata = fs::metadata(&entry.path)?;
        if !metadata.is_file() {
            return Err(invalid(format!(
                "not a regular file: {}",
                entry.path.display()
            )));
        }
        if metadata.len() > u32::MAX as u64 {
            return Err(invalid(format!(
                "file is too large for a non-ZIP64 archive: {}",
                entry.path.display()
            )));
        }

        // First pass: CRC-32 (the local header needs it before the data).
        let mut source = fs::File::open(&entry.path)?;
        let mut crc = 0xFFFF_FFFFu32;
        let mut size: u64 = 0;
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            crc = crc32(&buffer[..n], crc);
        }
        let crc = crc ^ 0xFFFF_FFFF;
        if size != metadata.len() {
            return Err(invalid(format!(
                "file changed while reading it: {}",
                entry.path.display()
            )));
        }
        if offset > u32::MAX as u64 {
            return Err(invalid("archive is too large for a non-ZIP64 file"));
        }

        // Local file header, then the payload.
        write_u32(&mut writer, LOCAL_HEADER)?;
        write_u16(&mut writer, VERSION)?;
        write_u16(&mut writer, UTF8_FLAG)?;
        write_u16(&mut writer, METHOD_STORED)?;
        write_u16(&mut writer, DOS_TIME)?;
        write_u16(&mut writer, DOS_DATE)?;
        write_u32(&mut writer, crc)?;
        write_u32(&mut writer, size as u32)?;
        write_u32(&mut writer, size as u32)?;
        write_u16(&mut writer, name.len() as u16)?;
        write_u16(&mut writer, 0)?;
        writer.write_all(name)?;

        // Second pass: copy the bytes, verifying the content did not change.
        let mut source = fs::File::open(&entry.path)?;
        let mut copied: u64 = 0;
        let mut check = 0xFFFF_FFFFu32;
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            copied += n as u64;
            if copied > size {
                return Err(invalid(format!(
                    "file grew while archiving it: {}",
                    entry.path.display()
                )));
            }
            check = crc32(&buffer[..n], check);
            writer.write_all(&buffer[..n])?;
        }
        if copied != size || (check ^ 0xFFFF_FFFF) != crc {
            return Err(invalid(format!(
                "file changed while archiving it: {}",
                entry.path.display()
            )));
        }

        central.push(CentralEntry {
            name: name.to_vec(),
            crc,
            size: size as u32,
            offset: offset as u32,
        });
        offset += 30 + name.len() as u64 + size;
    }

    let central_offset = offset;
    for entry in &central {
        write_u32(&mut writer, CENTRAL_HEADER)?;
        write_u16(&mut writer, VERSION)?;
        write_u16(&mut writer, VERSION)?;
        write_u16(&mut writer, UTF8_FLAG)?;
        write_u16(&mut writer, METHOD_STORED)?;
        write_u16(&mut writer, DOS_TIME)?;
        write_u16(&mut writer, DOS_DATE)?;
        write_u32(&mut writer, entry.crc)?;
        write_u32(&mut writer, entry.size)?;
        write_u32(&mut writer, entry.size)?;
        write_u16(&mut writer, entry.name.len() as u16)?;
        write_u16(&mut writer, 0)?; // extra
        write_u16(&mut writer, 0)?; // comment
        write_u16(&mut writer, 0)?; // disk
        write_u16(&mut writer, 0)?; // internal attributes
        write_u32(&mut writer, 0)?; // external attributes
        write_u32(&mut writer, entry.offset)?;
        writer.write_all(&entry.name)?;
        offset += 46 + entry.name.len() as u64;
    }
    if central_offset > u32::MAX as u64 || offset - central_offset > u32::MAX as u64 {
        return Err(invalid("archive is too large for a non-ZIP64 file"));
    }

    write_u32(&mut writer, END_OF_CENTRAL)?;
    write_u16(&mut writer, 0)?;
    write_u16(&mut writer, 0)?;
    write_u16(&mut writer, central.len() as u16)?;
    write_u16(&mut writer, central.len() as u16)?;
    write_u32(&mut writer, (offset - central_offset) as u32)?;
    write_u32(&mut writer, central_offset as u32)?;
    write_u16(&mut writer, 0)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn u16_at(data: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([data[at], data[at + 1]])
    }

    fn u32_at(data: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    }

    /// Read back a `Stored` archive: name -> (crc, bytes).
    fn read_stored(data: &[u8]) -> HashMap<String, (u32, Vec<u8>)> {
        let eocd = (0..data.len().saturating_sub(21))
            .rev()
            .find(|&at| u32_at(data, at) == END_OF_CENTRAL)
            .expect("end of central directory");
        assert_eq!(u16_at(data, eocd + 4), 0);
        assert_eq!(u16_at(data, eocd + 6), 0);
        let count = u16_at(data, eocd + 10) as usize;
        assert_eq!(count, u16_at(data, eocd + 8) as usize);
        let mut at = u32_at(data, eocd + 16) as usize;
        let mut out = HashMap::new();
        for _ in 0..count {
            assert_eq!(u32_at(data, at), CENTRAL_HEADER);
            assert_eq!(u16_at(data, at + 10), METHOD_STORED);
            let crc = u32_at(data, at + 16);
            let size = u32_at(data, at + 20) as usize;
            let name_len = u16_at(data, at + 28) as usize;
            let extra = u16_at(data, at + 30) as usize;
            let comment = u16_at(data, at + 32) as usize;
            let local = u32_at(data, at + 42) as usize;
            let name = String::from_utf8(data[at + 46..at + 46 + name_len].to_vec()).unwrap();
            assert_eq!(u32_at(data, local), LOCAL_HEADER);
            let local_name = u16_at(data, local + 26) as usize;
            let local_extra = u16_at(data, local + 28) as usize;
            let start = local + 30 + local_name + local_extra;
            let bytes = data[start..start + size].to_vec();
            assert_eq!(
                crc32(&bytes, 0xFFFF_FFFF) ^ 0xFFFF_FFFF,
                crc,
                "CRC for {name}"
            );
            out.insert(name, (crc, bytes));
            at += 46 + name_len + extra + comment;
        }
        out
    }

    #[test]
    fn stored_zip_round_trips_names_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.bin");
        std::fs::write(&a, b"hello zip").unwrap();
        std::fs::write(&b, [0u8, 1, 2, 3, 255]).unwrap();
        let out = dir.path().join("bundle.zip");
        write(
            &out,
            &[
                ZipEntry {
                    name: "folder/a.txt".into(),
                    path: a,
                },
                ZipEntry {
                    name: "b.bin".into(),
                    path: b,
                },
            ],
        )
        .unwrap();

        let archive = std::fs::read(&out).unwrap();
        assert_eq!(&archive[..4], &LOCAL_HEADER.to_le_bytes());
        let entries = read_stored(&archive);
        assert_eq!(entries["folder/a.txt"].1, b"hello zip");
        assert_eq!(entries["b.bin"].1, [0u8, 1, 2, 3, 255]);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn unsafe_entry_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x");
        std::fs::write(&src, b"x").unwrap();
        for name in ["/abs", "../escape", "a/../b", "a//b", "a\\b", ""] {
            let out = dir.path().join(format!("bad-{}.zip", name.len()));
            let _ = std::fs::remove_file(&out);
            let result = write(
                &out,
                &[ZipEntry {
                    name: name.into(),
                    path: src.clone(),
                }],
            );
            assert!(result.is_err(), "{name:?} must be refused");
            let _ = std::fs::remove_file(&out);
        }
    }
}
