//! Minimal PNG encoding for snapshots and the app icon.

use std::io::Write;

/// Minimal RGBA PNG writer using uncompressed deflate blocks (no dependencies).
pub fn write(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in rgba.chunks_exact(w as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(chunks.peek().is_none() as u8);
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"\x89PNG\r\n\x1a\n")?;
    for (kind, data) in [(b"IHDR", ihdr.as_slice()), (b"IDAT", &z), (b"IEND", &[])] {
        f.write_all(&(data.len() as u32).to_be_bytes())?;
        f.write_all(kind)?;
        f.write_all(data)?;
        let mut crc = !0u32;
        for &byte in kind.iter().chain(data) {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
            }
        }
        f.write_all(&(!crc).to_be_bytes())?;
    }
    f.flush()
}
