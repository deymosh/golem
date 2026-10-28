//! SHA-256, hand-written: the hash a release's `.sha256` file carries, and
//! what `golem update` checks its download against.
//!
//! The dependency set is deliberate and has no crypto in it, and the whole
//! of SHA-256 that golem needs is one streaming hash - `new`, `update`,
//! `finish` - over files that arrive in chunks. No constant-time anything:
//! the comparison is of a hash we computed against one GitHub published,
//! not of secrets.

/// One SHA-256 computation, fed with `update` and finished with [`finish`].
pub struct Sha256 {
    state: [u32; 8],
    /// The bytes carried since the last block: a stream rarely ends on a
    /// 64-byte boundary.
    buf: [u8; 64],
    filled: usize,
    /// Total bytes fed, for the length in the padding.
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            filled: 0,
            len: 0,
        }
    }

    /// Feed the next bytes of the stream.
    pub fn update(&mut self, data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        let mut rest = data;
        if self.filled > 0 {
            let take = (64 - self.filled).min(rest.len());
            self.buf[self.filled..self.filled + take].copy_from_slice(&rest[..take]);
            self.filled += take;
            rest = &rest[take..];
            if self.filled == 64 {
                let block = self.buf;
                self.compress(&block);
                self.filled = 0;
            }
        }
        while rest.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&rest[..64]);
            self.compress(&block);
            rest = &rest[64..];
        }
        if !rest.is_empty() {
            self.buf[..rest.len()].copy_from_slice(rest);
            self.filled = rest.len();
        }
    }

    /// The digest, with the padding and the length appended.
    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.len.wrapping_mul(8);
        // The 0x80 terminator, then zeros up to the length: when less than
        // nine bytes are left in the block, the length goes into the next
        // one.
        self.buf[self.filled] = 0x80;
        self.filled += 1;
        if self.filled > 56 {
            for byte in self.buf[self.filled..].iter_mut() {
                *byte = 0;
            }
            let block = self.buf;
            self.compress(&block);
            self.filled = 0;
        }
        for byte in self.buf[self.filled..56].iter_mut() {
            *byte = 0;
        }
        self.buf[56..64].copy_from_slice(&bits.to_be_bytes());
        let block = self.buf;
        self.compress(&block);
        let mut out = [0u8; 32];
        for (word, chunk) in self.state.iter().zip(out.chunks_mut(4)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// One 64-byte block through the compression function.
    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes(chunk.try_into().expect("four bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (word, part) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(part);
        }
    }
}

/// The digest as lowercase hex, the form a `.sha256` file carries.
pub fn hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The SHA-256 of a file, streamed: a release binary or a 20 GB model is far
/// larger than a read into memory should be. `read` hears the running byte
/// count after every chunk, for a caller that shows how far it has got.
pub fn file(path: &std::path::Path, read: &mut dyn FnMut(u64)) -> std::io::Result<[u8; 32]> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finish());
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        read(done);
    }
}

/// The line a `.sha256` file holds for `name`: `sha256sum`'s own shape,
/// `<hash>  <name>`, so `sha256sum -c` checks it from the file's folder.
pub fn line(hash: &[u8; 32], name: &str) -> String {
    format!("{}  {name}\n", hex(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_hashes_like_its_bytes_and_reports_its_progress() {
        let path = std::env::temp_dir().join(format!("golem-core-sha256-{}", std::process::id()));
        std::fs::write(&path, b"abc").expect("write");
        let mut seen = Vec::new();
        let hash = file(&path, &mut |done| seen.push(done)).expect("hash");
        std::fs::remove_file(&path).ok();
        assert_eq!(hex(&hash), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(seen, vec![3]);
        assert_eq!(
            line(&hash, "x.gguf"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  x.gguf\n"
        );
    }

    /// The one-shot digest of `data`, in hex.
    fn digest(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex(&hasher.finish())
    }

    /// The same digest, fed a few bytes at a time in uneven chunks - the
    /// shape a streamed file has.
    fn digest_in_chunks(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        let mut at = 0;
        let mut step = 1;
        while at < data.len() {
            let end = (at + step).min(data.len());
            hasher.update(&data[at..end]);
            at = end;
            step = step * 3 % 97 + 1;
        }
        hex(&hasher.finish())
    }

    #[test]
    fn the_nist_vectors_hold() {
        // FIPS 180-4's vectors, the empty message through the one the
        // standard is usually quoted with.
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_million_a_hashes_like_the_standard_says() {
        let data = vec![b'a'; 1_000_000];
        assert_eq!(
            digest(&data),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn chunked_updates_are_one_hash() {
        // A download arrives in whatever chunks the reader asks for; the
        // padding has to come out the same for every split of the stream.
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(digest_in_chunks(&data), digest(&data));
        // Exactly one block, and a block plus one byte: the boundaries the
        // padding code is most likely to get wrong.
        for len in [64usize, 65, 63, 128, 119, 56, 57] {
            assert_eq!(digest_in_chunks(&data[..len]), digest(&data[..len]), "{len} bytes");
        }
    }
}
