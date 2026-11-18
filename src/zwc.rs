//! `.zwc` wordcode dump support (`zcompile` and `source` lookup).
//!
//! Uses zsh's `FDHead` layout (`Src/parse.c`):
//! - 12 words (`FD_PRELEN * 4 = 48` bytes): magic, flags, 40-byte version string.
//! - One or more `FDHead` records (`start, len, npats, strs, hlen, flags`, 24 bytes)
//!   followed by the NUL-terminated 4-byte-aligned entry name, terminated by a 0 word.
//!   Note `FDHead.npats` sits at byte offset 56 (`48 + 8`).

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::SystemTime;

use crate::exec::strerror;
use crate::parser::{ParseError, Parser};
use crate::shell::{builtin_err, bytes_to_zstr, shell_err};

const FD_PRELEN: usize = 12;
const FD_MAGIC: u32 = 0x0405_0607;
const FD_OMAGIC: u32 = 0x0706_0504;
const VERSION_BYTES: &[u8] = b"meowsh";

fn read_u32(data: &[u8], off: usize, swap: bool) -> Option<u32> {
    let bytes: [u8; 4] = data.get(off..off + 4)?.try_into().ok()?;
    let v = u32::from_ne_bytes(bytes);
    Some(if swap { v.swap_bytes() } else { v })
}

/// Checks for a compiled `<path>.zwc` newer than (or as new as) `<path>` and
/// returns its verified payload bytes, emitting zsh's diagnostic if malformed.
pub fn try_load_for_source(source_path: &str) -> Option<Vec<u8>> {
    let zwc_path = format!("{}.zwc", source_path);
    let zwc_meta = std::fs::metadata(&zwc_path).ok()?;
    if !zwc_meta.is_file() {
        return None;
    }
    if let Ok(src_meta) = std::fs::metadata(source_path) {
        let zwc_mt = zwc_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let src_mt = src_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if zwc_mt < src_mt {
            return None;
        }
    }
    let name = Path::new(source_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(source_path);
    load_entry(&zwc_path, name)
}

fn load_entry(zwc_path: &str, name: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(zwc_path).ok()?;
    if data.len() < FD_PRELEN * 4 + 4 {
        return None;
    }
    let magic = u32::from_ne_bytes(data[0..4].try_into().ok()?);
    let swap = match magic {
        FD_MAGIC => false,
        FD_OMAGIC => true,
        _ => return None,
    };
    let ver = &data[8..FD_PRELEN * 4];
    let ver_end = ver.iter().position(|&b| b == 0).unwrap_or(ver.len());
    if &ver[..ver_end] != VERSION_BYTES {
        return None;
    }

    let mut pos = FD_PRELEN * 4;
    while pos + 4 <= data.len() {
        let hlen_peek = read_u32(&data, pos + 16, swap).unwrap_or(0);
        if pos + 24 > data.len() {
            break;
        }
        let start = read_u32(&data, pos, swap)?;
        let len = read_u32(&data, pos + 4, swap)?;
        let npats = read_u32(&data, pos + 8, swap)?;
        let _strs = read_u32(&data, pos + 12, swap)?;
        let _flags = read_u32(&data, pos + 20, swap)?;
        if hlen_peek == 0 {
            break;
        }
        let step = (hlen_peek as usize).checked_mul(4)?;
        if step < 24 || pos + step > data.len() {
            break;
        }
        let name_area = &data[pos + 24..pos + step];
        let nul = name_area
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_area.len());
        let entry_name = bytes_to_zstr(&name_area[..nul]);
        if entry_name == name {
            if npats > len / 4 {
                shell_err(&format!("{}: invalid description: {}", zwc_path, name));
                return None;
            }
            let start_byte = (start as usize).checked_mul(4)?;
            let end_byte = start_byte.checked_add(len as usize)?;
            if end_byte > data.len() {
                shell_err(&format!("{}: invalid description: {}", zwc_path, name));
                return None;
            }
            return Some(data[start_byte..end_byte].to_vec());
        }
        pos += step;
    }
    None
}

fn build_zwc(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut header_bytes = FD_PRELEN * 4;
    let mut aligned_names = Vec::with_capacity(entries.len());
    for (name, _) in entries {
        let raw_len = name.len() + 1;
        let padded = (raw_len + 3) & !3;
        aligned_names.push(padded);
        header_bytes += 24 + padded;
    }
    header_bytes += 4; // sentinel zero start/hlen word

    let mut out = Vec::new();
    out.extend_from_slice(&FD_MAGIC.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    let mut ver = [0u8; (FD_PRELEN - 2) * 4];
    ver[..VERSION_BYTES.len()].copy_from_slice(VERSION_BYTES);
    out.extend_from_slice(&ver);

    let mut cur_offset = header_bytes;
    for ((name, body), &padded_name) in entries.iter().zip(&aligned_names) {
        let start_words = (cur_offset / 4) as u32;
        let len_bytes = body.len() as u32;
        let hlen_words = ((24 + padded_name) / 4) as u32;
        out.extend_from_slice(&start_words.to_ne_bytes());
        out.extend_from_slice(&len_bytes.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // npats at byte offset 56
        out.extend_from_slice(&len_bytes.to_ne_bytes());
        out.extend_from_slice(&hlen_words.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(name.as_bytes());
        out.resize(out.len() + (padded_name - name.len()), 0);
        let aligned_body = (body.len() + 3) & !3;
        cur_offset += aligned_body;
    }
    out.extend_from_slice(&0u32.to_ne_bytes());

    for (_, body) in entries {
        out.extend_from_slice(body);
        let rem = body.len() % 4;
        if rem != 0 {
            out.extend(std::iter::repeat_n(0u8, 4 - rem));
        }
    }
    out
}

/// Implements `zcompile [-U] [-z|-k] [-R|-M] [-c] [-t] file [name...]`.
pub fn builtin_zcompile(words: &[String]) -> i32 {
    let mut test_only = false;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        let Some(opts) = a.strip_prefix('-').filter(|o| !o.is_empty()) else {
            break;
        };
        for c in opts.chars() {
            match c {
                'U' | 'z' | 'k' | 'R' | 'M' | 'c' | 'a' | 'm' => {}
                't' => test_only = true,
                other => {
                    builtin_err("zcompile", &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }
    let files = &words[i..];
    if files.is_empty() {
        builtin_err("zcompile", "too few arguments");
        return 1;
    }
    if test_only {
        let data = match std::fs::read(&files[0]) {
            Ok(d) => d,
            Err(_) => return 1,
        };
        if data.len() < FD_PRELEN * 4 {
            return 1;
        }
        let magic = u32::from_ne_bytes(data[0..4].try_into().unwrap_or([0; 4]));
        return if magic == FD_MAGIC || magic == FD_OMAGIC {
            0
        } else {
            1
        };
    }

    let target = &files[0];
    let src_files: Vec<String> = if files.len() > 1 {
        files[1..].to_vec()
    } else {
        vec![target.clone()]
    };
    let out_path = if target.ends_with(".zwc") {
        target.clone()
    } else {
        format!("{}.zwc", target)
    };

    let mut entries = Vec::with_capacity(src_files.len());
    for sf in &src_files {
        let bytes = match std::fs::read(sf) {
            Ok(b) => b,
            Err(e) => {
                builtin_err(
                    "zcompile",
                    &format!(
                        "can't open file: {}: {}",
                        sf,
                        strerror(e.raw_os_error().unwrap_or(0))
                    ),
                );
                return 1;
            }
        };
        let text = bytes_to_zstr(&bytes);
        if let Err(e) = Parser::new(&text, true).parse_program() {
            let msg = match e {
                ParseError::Syntax(m) => m,
                ParseError::Incomplete => "unexpected end of file".to_string(),
            };
            builtin_err("zcompile", &format!("{}: {}", sf, msg));
            return 1;
        }
        let name = Path::new(sf)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(sf)
            .to_string();
        entries.push((name, bytes));
    }

    let zwc = build_zwc(&entries);
    let _ = std::fs::remove_file(&out_path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o444)
        .open(&out_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            if let Err(e) = f.write_all(&zwc) {
                builtin_err("zcompile", &format!("can't write {}: {}", out_path, e));
                return 1;
            }
        }
        Err(e) => {
            builtin_err(
                "zcompile",
                &format!(
                    "can't open zwc file: {}: {}",
                    out_path,
                    strerror(e.raw_os_error().unwrap_or(0))
                ),
            );
            return 1;
        }
    }
    0
}
