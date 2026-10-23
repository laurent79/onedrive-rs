//! QuickXorHash, the content hash Microsoft OneDrive reports for every file.
//!
//! Port of `src/qxor.d` from the D client, itself based on
//! <https://github.com/OneDrive/onedrive-api-docs/blob/live/docs/code-snippets/quickxorhash.md>.

use base64::Engine as _;

const WIDTH_IN_BITS: usize = 160;
const LENGTH_IN_BYTES: usize = (WIDTH_IN_BITS - 1) / 8 + 1; // 20
const LENGTH_IN_QWORDS: usize = (WIDTH_IN_BITS - 1) / 64 + 1; // 3
const BITS_IN_LAST_CELL: usize = WIDTH_IN_BITS % 64; // 32
const SHIFT: usize = 11;

#[derive(Clone, Default)]
pub struct QuickXor {
    data: [u64; LENGTH_IN_QWORDS],
    length_so_far: u64,
    shift_so_far: usize,
}

impl QuickXor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, array: &[u8]) {
        let mut vector_array_index = self.shift_so_far / 64;
        let mut vector_offset = self.shift_so_far % 64;
        let iterations = array.len().min(WIDTH_IN_BITS);

        for i in 0..iterations {
            let is_last_cell = vector_array_index == LENGTH_IN_QWORDS - 1;
            let bits_in_vector_cell = if is_last_cell { BITS_IN_LAST_CELL } else { 64 };

            if vector_offset + 8 <= bits_in_vector_cell {
                let mut j = i;
                while j < array.len() {
                    self.data[vector_array_index] ^= (array[j] as u64) << vector_offset;
                    j += WIDTH_IN_BITS;
                }
            } else {
                let index1 = vector_array_index;
                let index2 = if is_last_cell { 0 } else { vector_array_index + 1 };
                let low = bits_in_vector_cell - vector_offset;

                let mut xored_byte = 0u8;
                let mut j = i;
                while j < array.len() {
                    xored_byte ^= array[j];
                    j += WIDTH_IN_BITS;
                }

                self.data[index1] ^= (xored_byte as u64) << vector_offset;
                self.data[index2] ^= (xored_byte as u64) >> low;
            }

            vector_offset += SHIFT;
            if vector_offset >= bits_in_vector_cell {
                vector_array_index = if is_last_cell { 0 } else { vector_array_index + 1 };
                vector_offset -= bits_in_vector_cell;
            }
        }

        self.shift_so_far = (self.shift_so_far + SHIFT * (array.len() % WIDTH_IN_BITS)) % WIDTH_IN_BITS;
        self.length_so_far += array.len() as u64;
    }

    pub fn finish(&self) -> [u8; LENGTH_IN_BYTES] {
        let mut raw = [0u8; LENGTH_IN_QWORDS * 8];
        for (chunk, word) in raw.chunks_exact_mut(8).zip(self.data.iter()) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        let mut out = [0u8; LENGTH_IN_BYTES];
        out.copy_from_slice(&raw[..LENGTH_IN_BYTES]);
        for (i, b) in self.length_so_far.to_le_bytes().iter().enumerate() {
            out[LENGTH_IN_BYTES - 8 + i] ^= b;
        }
        out
    }

    /// Base64 form, as returned by Microsoft Graph in `file.hashes.quickXorHash`.
    pub fn finish_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.finish())
    }
}

/// Hash a whole file from disk without loading it into memory.
pub fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    Ok(hash_file_state(path)?.0.finish_base64())
}

/// Hasher state after reading `path`, and the number of bytes read: lets an
/// interrupted download resume while still hashing the whole content.
pub fn hash_file_state(path: &std::path::Path) -> std::io::Result<(QuickXor, u64)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = QuickXor::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((hasher, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        assert_eq!(QuickXor::new().finish_base64(), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn chunking_is_irrelevant() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut whole = QuickXor::new();
        whole.update(&data);
        for chunk_size in [1, 7, 160, 161, 4096] {
            let mut parts = QuickXor::new();
            for chunk in data.chunks(chunk_size) {
                parts.update(chunk);
            }
            assert_eq!(parts.finish(), whole.finish(), "chunk size {chunk_size}");
        }
    }
}
