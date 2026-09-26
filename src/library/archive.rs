// gzip and tar, read by hand: a skill is downloaded from
// codeload.github.com, as github.com's own Download ZIP is, rather than the
// API's /tarball — the API allows 60 requests an hour to an address without
// a token. The crate has no gzip dependency, so DEFLATE (RFC 1951) and the
// gzip and ustar containers are decoded here, small and slow but honest.

use anyhow::{Context, Result, bail};

// gunzip unpacks a gzip stream, checking its CRC32 and size.
pub(crate) fn gunzip(data: &[u8], max_out: usize) -> Result<Vec<u8>> {
    if data.len() < 18 {
        bail!("the gzip stream is too short");
    }
    if data[0] != 0x1f || data[1] != 0x8b {
        bail!("not a gzip stream");
    }
    if data[2] != 8 {
        bail!("the gzip stream doesn't use DEFLATE");
    }
    let flags = data[3];
    let mut pos = 10;
    if flags & 0x04 != 0 {
        if pos + 2 > data.len() {
            bail!("truncated gzip header");
        }
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + xlen;
    }
    for flag in [0x08, 0x10] {
        if flags & flag != 0 {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            pos += 1;
        }
    }
    if flags & 0x02 != 0 {
        pos += 2; // FHCRC
    }
    if pos > data.len() {
        bail!("truncated gzip header");
    }
    let out = inflate(&data[pos..], max_out).context("bad DEFLATE stream")?;
    let end = data.len() - 8;
    let want_crc = u32::from_le_bytes(data[end..end + 4].try_into().unwrap());
    let want_size = u32::from_le_bytes(data[end + 4..].try_into().unwrap());
    if crc32(&out) != want_crc {
        bail!("the gzip stream's checksum doesn't match");
    }
    if out.len() as u32 != want_size {
        bail!("the gzip stream's size doesn't match");
    }
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buffer: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Result<u8> {
        if self.count == 0 {
            let byte = *self
                .data
                .get(self.pos)
                .context("truncated DEFLATE stream")?;
            self.pos += 1;
            self.buffer = u32::from(byte);
            self.count = 8;
        }
        let bit = (self.buffer & 1) as u8;
        self.buffer >>= 1;
        self.count -= 1;
        Ok(bit)
    }

    fn bits(&mut self, need: u32) -> Result<u32> {
        let mut value = 0u32;
        for i in 0..need {
            value |= u32::from(self.bit()?) << i;
        }
        Ok(value)
    }

    // align drops the bits of the current byte, for a stored block.
    fn align(&mut self) {
        self.count = 0;
    }
}

// A Huffman table: code lengths per symbol, and the symbols sorted by
// (length, symbol), so canonical codes decode a bit at a time.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

fn construct(lengths: &[u8]) -> Result<Huffman> {
    let mut counts = [0u16; 16];
    for &len in lengths {
        if usize::from(len) >= counts.len() {
            bail!("a code length over 15");
        }
        counts[usize::from(len)] += 1;
    }
    counts[0] = 0;
    // over-subscribed codes are a corrupt stream
    let mut left = 1i32;
    for len in 1..16 {
        left <<= 1;
        left -= i32::from(counts[len]);
        if left < 0 {
            bail!("over-subscribed Huffman code");
        }
    }
    let mut offsets = [0u16; 16];
    for len in 1..15 {
        offsets[len + 1] = offsets[len] + counts[len];
    }
    let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l != 0).count()];
    for (symbol, &len) in lengths.iter().enumerate() {
        if len != 0 {
            symbols[usize::from(offsets[usize::from(len)])] = symbol as u16;
            offsets[usize::from(len)] += 1;
        }
    }
    Ok(Huffman { counts, symbols })
}

fn decode(bits: &mut Bits, h: &Huffman) -> Result<u16> {
    let mut code = 0i32;
    let mut first = 0i32;
    let mut index = 0i32;
    for len in 1..16 {
        code |= i32::from(bits.bit()?);
        let count = i32::from(h.counts[len]);
        if code - first < count {
            return Ok(h.symbols[usize::try_from(index + (code - first)).unwrap_or(0)]);
        }
        index += count;
        first = (first + count) << 1;
        code <<= 1;
    }
    bail!("invalid Huffman code");
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn inflate(data: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut bits = Bits {
        data,
        pos: 0,
        buffer: 0,
        count: 0,
    };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = bits.bit()?;
        match bits.bits(2)? {
            0 => {
                bits.align();
                let at = bits.pos;
                if at + 4 > data.len() {
                    bail!("truncated stored block");
                }
                let len = u16::from_le_bytes(data[at..at + 2].try_into().unwrap());
                let nlen = u16::from_le_bytes(data[at + 2..at + 4].try_into().unwrap());
                if len != !nlen {
                    bail!("stored block lengths don't match");
                }
                let len = usize::from(len);
                let start = at + 4;
                if start + len > data.len() {
                    bail!("truncated stored block");
                }
                if out.len() + len > max_out {
                    bail!("the archive is too big to unpack");
                }
                out.extend_from_slice(&data[start..start + len]);
                bits.pos = start + len;
            }
            1 => {
                let mut lengths = [0u8; 288];
                for (i, len) in lengths.iter_mut().enumerate() {
                    *len = match i {
                        0..=143 => 8,
                        144..=255 => 9,
                        256..=279 => 7,
                        _ => 8,
                    };
                }
                let dists = [5u8; 30];
                codes(
                    &mut bits,
                    &mut out,
                    &construct(&lengths)?,
                    &construct(&dists)?,
                    max_out,
                )?;
            }
            2 => {
                let hlit = bits.bits(5)? + 257;
                let hdist = bits.bits(5)? + 1;
                let hclen = bits.bits(4)? + 4;
                if hlit > 286 || hdist > 30 {
                    bail!("too many codes");
                }
                let mut lengths = [0u8; 19];
                for i in 0..hclen {
                    lengths[CODE_LENGTH_ORDER[i as usize]] = bits.bits(3)? as u8;
                }
                let code_lengths = construct(&lengths)?;
                let mut lengths = vec![0u8; hlit as usize + hdist as usize];
                let mut i = 0;
                while i < lengths.len() {
                    match decode(&mut bits, &code_lengths)? {
                        symbol @ 0..=15 => {
                            lengths[i] = symbol as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                bail!("a repeat with nothing before it");
                            }
                            let previous = lengths[i - 1];
                            let repeats = 3 + bits.bits(2)? as usize;
                            for _ in 0..repeats {
                                if i >= lengths.len() {
                                    bail!("a repeat past the end");
                                }
                                lengths[i] = previous;
                                i += 1;
                            }
                        }
                        17 => i += 3 + bits.bits(3)? as usize,
                        18 => i += 11 + bits.bits(7)? as usize,
                        _ => bail!("bad code length symbol"),
                    }
                }
                if i > lengths.len() {
                    bail!("a repeat past the end");
                }
                if lengths[256] == 0 {
                    bail!("no end-of-block code");
                }
                let lit = construct(&lengths[..hlit as usize])?;
                let dist = construct(&lengths[hlit as usize..])?;
                codes(&mut bits, &mut out, &lit, &dist, max_out)?;
            }
            _ => bail!("bad block type"),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

fn codes(
    bits: &mut Bits,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    max_out: usize,
) -> Result<()> {
    loop {
        if out.len() > max_out {
            bail!("the archive is too big to unpack (over {max_out} bytes)");
        }
        match decode(bits, lit)? {
            symbol @ 0..=255 => out.push(symbol as u8),
            256 => return Ok(()),
            symbol => {
                let symbol = symbol as usize;
                if symbol > LENGTH_BASE.len() + 256 {
                    bail!("bad length symbol");
                }
                let length = usize::from(LENGTH_BASE[symbol - 257])
                    + bits.bits(u32::from(LENGTH_EXTRA[symbol - 257]))? as usize;
                let dsym = decode(bits, dist)? as usize;
                if dsym >= DIST_BASE.len() {
                    bail!("bad distance symbol");
                }
                let distance =
                    usize::from(DIST_BASE[dsym]) + bits.bits(u32::from(DIST_EXTRA[dsym]))? as usize;
                if distance > out.len() {
                    bail!("a distance before the start of the output");
                }
                let start = out.len() - distance;
                for i in 0..length {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
        }
    }
}

// untar unpacks a tar archive, whose one top folder is dropped. Regular
// files and folders only; nothing outside dst.
pub(crate) fn untar(data: &[u8], dst: &std::path::Path, max_total: u64) -> Result<()> {
    let mut pos = 0usize;
    let mut long_name: Option<String> = None;
    let mut total = 0u64;
    while pos + 512 <= data.len() {
        let header = &data[pos..pos + 512];
        if header.iter().all(|&b| b == 0) {
            return Ok(());
        }
        let mut name = cstr(&header[..100]);
        let size = usize::try_from(octal(&header[124..136])?)
            .context("a tar entry too large for this machine")?;
        let typeflag = header[156];
        let mode = octal(&header[100..108]).unwrap_or(0o644);
        let data_start = pos + 512;
        let data_end = data_start
            .checked_add(size)
            .filter(|&end| end <= data.len())
            .context("truncated tar archive")?;
        if typeflag == b'L' {
            long_name = Some(cstr(&data[data_start..data_end]));
            pos = align(data_end);
            continue;
        }
        let prefix = cstr(&header[345..500]);
        if !prefix.is_empty() && matches!(typeflag, 0 | b'0' | b'5') {
            name = format!("{prefix}/{name}");
        }
        if let Some(long) = long_name.take() {
            name = long;
        }
        let rel = name.split_once('/').map(|(_, rest)| rest).unwrap_or("");
        if !rel.is_empty() {
            let p = dst.join(rel.replace('\\', "/"));
            if !p.starts_with(dst) {
                pos = align(data_end);
                continue; // never outside
            }
            match typeflag {
                0 | b'0' => {
                    total += size;
                    if total > max_total {
                        bail!("the repository is too big to install skills from (over 200 MB)");
                    }
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("create {}", parent.display()))?;
                    }
                    std::fs::write(&p, &data[data_start..data_end])
                        .with_context(|| format!("write {}", p.display()))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let executable = mode & 0o111 != 0;
                        let _ = std::fs::set_permissions(
                            &p,
                            std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
                        );
                    }
                }
                b'5' => {
                    std::fs::create_dir_all(&p)
                        .with_context(|| format!("create {}", p.display()))?;
                }
                _ => {}
            }
        }
        pos = align(data_end);
    }
    Ok(())
}

fn align(pos: usize) -> usize {
    (pos + 511) / 512 * 512
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// octal reads a tar's octal field, or a base-256 one (GNU, for big sizes).
fn octal(bytes: &[u8]) -> Result<u64> {
    if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        let mut value = u64::from(bytes[0] & 0x7f);
        for &b in &bytes[1..] {
            value = (value << 8) | u64::from(b);
        }
        return Ok(value);
    }
    let text = cstr(bytes);
    let trimmed = text.trim_matches(|c: char| c == ' ' || c == '\0');
    if trimmed.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(trimmed, 8).context("bad octal field in tar header")
}

// gzip_stored_for_tests builds a gzip stream with stored (uncompressed)
// DEFLATE blocks, for tests that need a server handing out a tarball.
#[cfg(test)]
pub(crate) fn gzip_stored_for_tests(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let last = data.len().saturating_sub(1) / 65535;
    for (i, chunk) in data.chunks(65535).enumerate() {
        out.push(u8::from(i == last));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

// tar_gz_for_tests builds a tar.gz of a few files, as codeload would hand
// one out, for tests that need a server.
#[cfg(test)]
pub(crate) fn tar_gz_for_tests(files: &[(&str, &str)]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (name, body) in files {
        tar_entry_for_tests(&mut tar, name, body.as_bytes());
    }
    tar.extend_from_slice(&[0u8; 1024]);
    gzip_stored_for_tests(&tar)
}

#[cfg(test)]
fn tar_entry_for_tests(out: &mut Vec<u8>, name: &str, body: &[u8]) {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let size = format!("{:011o}\0", body.len());
    header[124..124 + size.len()].copy_from_slice(size.as_bytes());
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    let check = format!("{:06o}\0 ", sum);
    header[148..156].copy_from_slice(check.as_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    let pad = (512 - body.len() % 512) % 512;
    out.extend(std::iter::repeat(0u8).take(pad));
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn unbase64(text: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .unwrap()
    }

    #[test]
    fn gunzip_reads_a_dynamic_huffman_block() {
        let data = unbase64(
            "H4sIAAAAAAAC/yvOzszJKVZILEpVSMvPSUktKlYozyzJUEhUCPb29PHRy01RyMwrzkxJ5SoeVUmESgC1NhX1UAEAAA==",
        );
        let out = gunzip(&data, 1 << 20).unwrap();
        assert_eq!(
            out,
            b"skills are folders with a SKILL.md inside\n".repeat(8)
        );
    }

    #[test]
    fn gunzip_reads_a_stored_block() {
        let data = unbase64("H4sIAAAAAAAE/wEMAPP/aGVsbG8gc3RvcmVkjqNzJAwAAAA=");
        assert_eq!(gunzip(&data, 1 << 20).unwrap(), b"hello stored");
    }

    #[test]
    fn gunzip_reads_a_fixed_huffman_block() {
        let data = unbase64("H4sIAAAAAAAE/0tMSgYAwkEkNQMAAAA=");
        assert_eq!(gunzip(&data, 1 << 20).unwrap(), b"abc");
    }

    #[test]
    fn gunzip_refuses_a_corrupt_stream() {
        let mut data = unbase64("H4sIAAAAAAAE/0tMSgYAwkEkNQMAAAA=");
        let last = data.len() - 1;
        data[last] ^= 0xff;
        assert!(gunzip(&data, 1 << 20).is_err());
    }

    #[test]
    fn untar_drops_the_top_folder_and_never_goes_outside() {
        let data = unbase64(
            "H4sIAJzXt2oC/+3TMQrCMBSA4cw9RS8QG9vq4CaoIFYQPUG1EYu2KYni9W11kYKDKB30/5YXQiDD4zfXUltpdWVkut0F7pifTi6osn2wWcyTpFdk4mOqNozj+6y1p1KDp3Nz349VFApfiQ5c3Dm19ffiP0kpvTIt9Mivd+5l2u1sXp1zU4781WTmfFNqr3kj8JPMy/73xhaum/7VoN1/pOi/E/ctkwH9P/pfT8eT5fQr1b/Tf9jqP1RRTP9dOOQ0AAAAAAAAAAAAAAC/4AaXg+vDACgAAA==",
        );
        let dir = std::env::temp_dir().join(format!("magpie-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        untar(&gunzip(&data, 1 << 20).unwrap(), &dir, 200 << 20).unwrap();
        let skill = std::fs::read_to_string(dir.join("skills/pdf/SKILL.md")).unwrap();
        assert!(skill.contains("name: pdf"));
        assert_eq!(
            std::fs::read_to_string(dir.join("skills/pdf/forms.md")).unwrap(),
            "forms"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            "hi"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untar_rejects_a_size_over_the_cap() {
        let mut data = vec![0u8; 1024];
        data[..8].copy_from_slice(b"top/file");
        data[156] = b'0';
        data[124..130].copy_from_slice(b"000777\0");
        let dir = std::env::temp_dir().join(format!("magpie-archive-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(untar(&data, &dir, 100).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
