//! Compressing large inputs on several threads (the `parallel` feature).
//!
//! The input is split into fixed-size chunks, which are compressed independently on rayon's
//! thread pool. Each chunk but the last is compressed with
//! `libdeflate_deflate_compress_nonfinal`, which ends its output with an empty stored block
//! instead of a final block, so the chunks' outputs concatenate into one valid DEFLATE stream.
//! Compressing each chunk without the data before it costs a little compression (about half a
//! percent on large PNG images with 1MiB chunks).

use libdeflate_sys::{libdeflate_adler32, libdeflate_crc32, libdeflate_deflate_compress_nonfinal};
use rayon::prelude::*;

use crate::{CompressionError, CompressionResult, Compressor};

/// The size of the chunks that inputs are compressed in. Fixed, rather than derived from the
/// number of threads, so that the output doesn't depend on the machine.
pub const PARALLEL_CHUNK_SIZE: usize = 1 << 20;

/// The bytes the empty stored block ending each chunk but the last adds.
const SYNC_FLUSH_SIZE: usize = 5;
/// zlib's header (2 bytes) and Adler-32 trailer (4 bytes).
const ZLIB_OVERHEAD: usize = 6;
/// gzip's header (10 bytes), and CRC-32 and size trailer (8 bytes).
const GZIP_OVERHEAD: usize = 18;

impl Compressor {
    /// Whether an input of `n_bytes` is compressed in chunks: at least two, and not at level 0,
    /// which stores the data uncompressed in blocks that are always final.
    fn compresses_in_parallel(&self, n_bytes: usize) -> bool {
        self.lvl.0 > 0 && n_bytes >= 2 * PARALLEL_CHUNK_SIZE
    }

    /// Like [`deflate_compress_bound`](Compressor::deflate_compress_bound), for
    /// [`deflate_compress_parallel`](Compressor::deflate_compress_parallel).
    pub fn deflate_compress_bound_parallel(&mut self, n_bytes: usize) -> usize {
        if !self.compresses_in_parallel(n_bytes) {
            return self.deflate_compress_bound(n_bytes);
        }
        let full_chunks = n_bytes / PARALLEL_CHUNK_SIZE;
        let rest = n_bytes % PARALLEL_CHUNK_SIZE;
        let chunks = full_chunks + (rest > 0) as usize;
        full_chunks * self.deflate_compress_bound(PARALLEL_CHUNK_SIZE)
            + if rest > 0 { self.deflate_compress_bound(rest) } else { 0 }
            + (chunks - 1) * SYNC_FLUSH_SIZE
    }

    /// Like [`zlib_compress_bound`](Compressor::zlib_compress_bound), for
    /// [`zlib_compress_parallel`](Compressor::zlib_compress_parallel).
    pub fn zlib_compress_bound_parallel(&mut self, n_bytes: usize) -> usize {
        self.deflate_compress_bound_parallel(n_bytes) + ZLIB_OVERHEAD
    }

    /// Like [`gzip_compress_bound`](Compressor::gzip_compress_bound), for
    /// [`gzip_compress_parallel`](Compressor::gzip_compress_parallel).
    pub fn gzip_compress_bound_parallel(&mut self, n_bytes: usize) -> usize {
        self.deflate_compress_bound_parallel(n_bytes) + GZIP_OVERHEAD
    }

    /// Like [`deflate_compress`](Compressor::deflate_compress), but compresses inputs of at
    /// least two [`PARALLEL_CHUNK_SIZE`] chunks in parallel. Smaller inputs are compressed as
    /// `deflate_compress` does. `out_deflate_data` may need to be as large as
    /// [`deflate_compress_bound_parallel`](Compressor::deflate_compress_bound_parallel).
    pub fn deflate_compress_parallel(&mut self,
                                     in_raw_data: &[u8],
                                     out_deflate_data: &mut [u8]) -> CompressionResult<usize> {
        if !self.compresses_in_parallel(in_raw_data.len()) {
            return self.deflate_compress(in_raw_data, out_deflate_data);
        }

        let chunks: Vec<&[u8]> = in_raw_data.chunks(PARALLEL_CHUNK_SIZE).collect();
        let (last, rest) = chunks.split_last().unwrap();
        let lvl = self.lvl;
        // The last chunk ends the stream, so it's compressed normally, here.
        let (rest, last) = rayon::join(
            || {
                rest.par_iter()
                    .map(|chunk| Compressor::new(lvl).deflate_compress_nonfinal(chunk))
                    .collect::<CompressionResult<Vec<_>>>()
            },
            || {
                let mut out = vec![0; self.deflate_compress_bound(last.len())];
                let len = self.deflate_compress(last, &mut out)?;
                out.truncate(len);
                Ok(out)
            },
        );
        let (rest, last) = (rest?, last?);

        let len = rest.iter().map(Vec::len).sum::<usize>() + last.len();
        if len > out_deflate_data.len() {
            return Err(CompressionError::InsufficientSpace);
        }
        let mut offset = 0;
        for chunk in rest.iter().chain([&last]) {
            out_deflate_data[offset..offset + chunk.len()].copy_from_slice(chunk);
            offset += chunk.len();
        }
        Ok(len)
    }

    /// Like [`zlib_compress`](Compressor::zlib_compress), but compresses inputs of at least two
    /// [`PARALLEL_CHUNK_SIZE`] chunks in parallel. Smaller inputs are compressed as
    /// `zlib_compress` does. `out_zlib_data` may need to be as large as
    /// [`zlib_compress_bound_parallel`](Compressor::zlib_compress_bound_parallel).
    pub fn zlib_compress_parallel(&mut self,
                                  in_raw_data: &[u8],
                                  out_zlib_data: &mut [u8]) -> CompressionResult<usize> {
        if !self.compresses_in_parallel(in_raw_data.len()) {
            return self.zlib_compress(in_raw_data, out_zlib_data);
        }
        if out_zlib_data.len() <= ZLIB_OVERHEAD {
            return Err(CompressionError::InsufficientSpace);
        }

        // As libdeflate writes it: CMF (deflate, 32KiB window), then FLG with a hint of the
        // compression level, and a check value making the pair a multiple of 31.
        let level = self.lvl.0;
        let level_hint = if level < 2 { 0 } else if level < 6 { 1 } else if level < 8 { 2 } else { 3 };
        let mut header: u16 = (8 << 8) | (7 << 12) | (level_hint << 6);
        header |= 31 - (header % 31);
        out_zlib_data[..2].copy_from_slice(&header.to_be_bytes());

        let end = out_zlib_data.len() - 4;
        let deflate_len = self.deflate_compress_parallel(in_raw_data, &mut out_zlib_data[2..end])?;
        let adler = unsafe {
            libdeflate_adler32(1, in_raw_data.as_ptr().cast(), in_raw_data.len())
        };
        let end = 2 + deflate_len;
        out_zlib_data[end..end + 4].copy_from_slice(&adler.to_be_bytes());
        Ok(end + 4)
    }

    /// Like [`gzip_compress`](Compressor::gzip_compress), but compresses inputs of at least two
    /// [`PARALLEL_CHUNK_SIZE`] chunks in parallel. Smaller inputs are compressed as
    /// `gzip_compress` does. `out_gzip_data` may need to be as large as
    /// [`gzip_compress_bound_parallel`](Compressor::gzip_compress_bound_parallel).
    pub fn gzip_compress_parallel(&mut self,
                                  in_raw_data: &[u8],
                                  out_gzip_data: &mut [u8]) -> CompressionResult<usize> {
        if !self.compresses_in_parallel(in_raw_data.len()) {
            return self.gzip_compress(in_raw_data, out_gzip_data);
        }
        if out_gzip_data.len() <= GZIP_OVERHEAD {
            return Err(CompressionError::InsufficientSpace);
        }

        // As libdeflate writes it: ID1, ID2, CM (deflate), FLG, MTIME (unavailable), XFL (a hint
        // of the compression level), and OS (unknown).
        let level = self.lvl.0;
        let xfl = if level < 2 { 4 } else if level >= 8 { 2 } else { 0 };
        out_gzip_data[..10].copy_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 0xff]);

        let end = out_gzip_data.len() - 8;
        let deflate_len = self.deflate_compress_parallel(in_raw_data, &mut out_gzip_data[10..end])?;
        let crc = unsafe { libdeflate_crc32(0, in_raw_data.as_ptr().cast(), in_raw_data.len()) };
        let end = 10 + deflate_len;
        out_gzip_data[end..end + 4].copy_from_slice(&crc.to_le_bytes());
        out_gzip_data[end + 4..end + 8].copy_from_slice(&(in_raw_data.len() as u32).to_le_bytes());
        Ok(end + 8)
    }

    /// Compresses `in_raw_data` as DEFLATE blocks that are followed by more, ending with a sync
    /// flush rather than a final block.
    fn deflate_compress_nonfinal(&mut self, in_raw_data: &[u8]) -> CompressionResult<Vec<u8>> {
        let mut out = vec![0; self.deflate_compress_bound(in_raw_data.len()) + SYNC_FLUSH_SIZE];
        let len = unsafe {
            libdeflate_deflate_compress_nonfinal(self.p.as_ptr(),
                                                 in_raw_data.as_ptr().cast(),
                                                 in_raw_data.len(),
                                                 out.as_mut_ptr().cast(),
                                                 out.len())
        };
        if len == 0 {
            return Err(CompressionError::InsufficientSpace);
        }
        out.truncate(len);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompressionLvl, Decompressor};

    /// Data with stretches that compress well and some that don't, so chunks end in both
    /// compressed and stored blocks.
    fn data(len: usize) -> Vec<u8> {
        let mut seed = 1u32;
        (0..len)
            .map(|i| {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                if (i / 100_000) % 5 == 0 { (seed >> 24) as u8 } else { (i % 251) as u8 }
            })
            .collect()
    }

    fn levels() -> impl Iterator<Item = CompressionLvl> {
        [1, 6, 9, 12].iter().map(|&l| CompressionLvl::new(l).unwrap())
    }

    #[test]
    fn round_trips() {
        let input = data(PARALLEL_CHUNK_SIZE * 3 + 12345);
        for lvl in levels() {
            let mut c = Compressor::new(lvl);
            let mut d = Decompressor::new();
            let mut decompressed = vec![0; input.len()];

            let mut out = vec![0; c.deflate_compress_bound_parallel(input.len())];
            let len = c.deflate_compress_parallel(&input, &mut out).unwrap();
            let n = d.deflate_decompress(&out[..len], &mut decompressed).unwrap();
            assert_eq!(&decompressed[..n], &input[..]);

            let mut out = vec![0; c.zlib_compress_bound_parallel(input.len())];
            let len = c.zlib_compress_parallel(&input, &mut out).unwrap();
            let n = d.zlib_decompress(&out[..len], &mut decompressed).unwrap();
            assert_eq!(&decompressed[..n], &input[..]);

            let mut out = vec![0; c.gzip_compress_bound_parallel(input.len())];
            let len = c.gzip_compress_parallel(&input, &mut out).unwrap();
            let n = d.gzip_decompress(&out[..len], &mut decompressed).unwrap();
            assert_eq!(&decompressed[..n], &input[..]);
        }
    }

    #[test]
    fn writes_the_same_headers_and_trailers() {
        let input = data(PARALLEL_CHUNK_SIZE * 2 + 1);
        for lvl in levels() {
            let mut c = Compressor::new(lvl);
            let mut serial = vec![0; c.zlib_compress_bound(input.len())];
            let serial_len = c.zlib_compress(&input, &mut serial).unwrap();
            let mut parallel = vec![0; c.zlib_compress_bound_parallel(input.len())];
            let parallel_len = c.zlib_compress_parallel(&input, &mut parallel).unwrap();
            assert_eq!(serial[..2], parallel[..2]);
            assert_eq!(serial[serial_len - 4..serial_len], parallel[parallel_len - 4..parallel_len]);

            let mut serial = vec![0; c.gzip_compress_bound(input.len())];
            let serial_len = c.gzip_compress(&input, &mut serial).unwrap();
            let mut parallel = vec![0; c.gzip_compress_bound_parallel(input.len())];
            let parallel_len = c.gzip_compress_parallel(&input, &mut parallel).unwrap();
            assert_eq!(serial[..10], parallel[..10]);
            assert_eq!(serial[serial_len - 8..serial_len], parallel[parallel_len - 8..parallel_len]);
        }
    }

    #[test]
    fn small_inputs_are_compressed_as_usual() {
        let input = data(PARALLEL_CHUNK_SIZE * 2 - 1);
        let mut c = Compressor::new(CompressionLvl::new(11).unwrap());
        let mut serial = vec![0; c.zlib_compress_bound(input.len())];
        let serial_len = c.zlib_compress(&input, &mut serial).unwrap();
        let mut parallel = vec![0; c.zlib_compress_bound_parallel(input.len())];
        let parallel_len = c.zlib_compress_parallel(&input, &mut parallel).unwrap();
        assert_eq!(serial[..serial_len], parallel[..parallel_len]);
    }

    #[test]
    fn bound_fits_incompressible_data() {
        let mut seed = 7u32;
        let input: Vec<u8> = (0..PARALLEL_CHUNK_SIZE * 2 + 999)
            .map(|_| {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                (seed >> 24) as u8
            })
            .collect();
        for lvl in levels() {
            let mut c = Compressor::new(lvl);
            let mut out = vec![0; c.zlib_compress_bound_parallel(input.len())];
            assert!(c.zlib_compress_parallel(&input, &mut out).is_ok());
        }
    }

    #[test]
    fn reports_insufficient_space() {
        let input = data(PARALLEL_CHUNK_SIZE * 2);
        let mut c = Compressor::new(CompressionLvl::new(6).unwrap());
        let mut out = vec![0; 100];
        assert_eq!(c.zlib_compress_parallel(&input, &mut out), Err(CompressionError::InsufficientSpace));
    }
}
