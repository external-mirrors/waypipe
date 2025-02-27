/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! Safe LZ4 and ZSTD compression wrappers */
use core::ffi::{c_char, c_void};
#[cfg(feature = "lz4")]
use waypipe_lz4_wrapper::*;
#[cfg(feature = "zstd")]
use waypipe_zstd_wrapper::*;

pub struct LZ4CCtx {
    #[cfg(feature = "lz4")]
    state: *mut u8,
}
pub struct ZstdCCtx {
    #[cfg(feature = "zstd")]
    ctx: *mut ZSTD_CCtx,
}
pub struct ZstdDCtx {
    #[cfg(feature = "zstd")]
    ctx: *mut ZSTD_DCtx,
}

#[cfg(feature = "zstd")]
pub fn zstd_make_cctx() -> Option<ZstdCCtx> {
    unsafe {
        // SAFETY: ZSTD_createCCtx is thread-safe
        let x = ZSTD_createCCtx();
        if x.is_null() {
            return None;
        }
        Some(ZstdCCtx { ctx: x })
    }
}
#[cfg(not(feature = "zstd"))]
pub fn zstd_make_cctx() -> Option<ZstdCCtx> {
    unreachable!();
}

#[cfg(feature = "zstd")]
pub fn zstd_make_dctx() -> Option<ZstdDCtx> {
    unsafe {
        // SAFETY: ZSTD_createDCtx is thread-safe
        let x = ZSTD_createDCtx();
        if x.is_null() {
            return None;
        }
        Some(ZstdDCtx { ctx: x })
    }
}
#[cfg(not(feature = "zstd"))]
pub fn zstd_make_dctx() -> Option<ZstdDCtx> {
    unreachable!();
}

#[cfg(feature = "zstd")]
impl Drop for ZstdCCtx {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: ZSTD_freeCCtx is thread-safe, operates
            // on non-null pointer made by ZSTD_createCCtx
            ZSTD_freeCCtx(self.ctx);
        }
    }
}
#[cfg(feature = "zstd")]
impl Drop for ZstdDCtx {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: ZSTD_freeDCtx is thread-safe, operates
            // on non-null pointer made by ZSTD_createDCtx
            ZSTD_freeDCtx(self.ctx);
        }
    }
}

#[cfg(feature = "lz4")]
pub fn lz4_make_cctx() -> Option<LZ4CCtx> {
    unsafe {
        let sz = std::cmp::max(LZ4_sizeofState(), LZ4_sizeofStateHC()) as usize;
        assert!(sz > 0);

        // LZ4_compress_extState and LZ4_compress_HC_extStateHC
        // require that layout is 8-aligned
        let layout = std::alloc::Layout::from_size_align(sz, 8).unwrap();

        // SAFETY: layout size is verified to be > 0
        let data = std::alloc::alloc(layout);
        if data.is_null() {
            return None;
        }

        Some(LZ4CCtx { state: data })
    }
}
#[cfg(not(feature = "lz4"))]
pub fn lz4_make_cctx() -> Option<LZ4CCtx> {
    unreachable!();
}
#[cfg(feature = "lz4")]
impl Drop for LZ4CCtx {
    fn drop(&mut self) {
        unsafe {
            let sz = std::cmp::max(LZ4_sizeofState(), LZ4_sizeofStateHC()) as usize;
            let layout = std::alloc::Layout::from_size_align(sz, 8).unwrap();
            // SAFETY: self.state only set in lz4_make_cctx, which uses the same layout
            // because LZ4_sizeofState / LZ4_sizeofStateHC always return same result
            std::alloc::dealloc(self.state, layout);
        }
    }
}

/* Create a vector containing the compressed input, preceded by pad_pre zeros, and followed by pad_post zeros */
// typically used with pad_pre = 16, pad_post = 4, with Vec::truncate called after (which does not reallocate)
#[cfg(feature = "zstd")]
pub fn zstd_compress_to_vec(
    ctx: &mut ZstdCCtx,
    input: &[u8],
    level: i8,
    pad_pre: usize,
    pad_post: usize,
) -> Vec<u8> {
    let mut v = Vec::new();

    unsafe {
        let max_space: usize = ZSTD_compressBound(input.len());
        // Compute required space used without overflow
        let req_space = max_space
            .checked_add(pad_pre)
            .unwrap()
            .checked_add(pad_post)
            .unwrap();
        assert!(req_space <= isize::MAX as usize);
        v.reserve_exact(req_space);

        // SAFETY: function checks inputs for validity, ctx.ctx is non-null from ZstdCCtx construction
        let ret = ZSTD_CCtx_setParameter(
            ctx.ctx,
            ZSTD_cParameter_ZSTD_c_compressionLevel,
            level as i32,
        );
        assert!(
            ZSTD_isError(ret) == 0,
            "Failed to set Zstd CCtx compression level"
        );

        let dst: *mut u8 = v.as_mut_ptr();
        // SAFETY: v has reserved reserve_exact > pad_pre bytes, has aligned plain data contents
        std::ptr::write_bytes(dst, 0, pad_pre);

        // SAFETY: ctx.ctx is not null; ZSTD_compress2 should never write outside dst[..max_space],
        // and pad_pre + max_space is <= req_space so all input regions are allocated
        let sz = ZSTD_compress2(
            ctx.ctx,
            dst.add(pad_pre) as *mut c_void,
            max_space,
            input.as_ptr() as *const c_void,
            input.len(),
        );
        assert!(ZSTD_isError(sz) == 0, "Failed to compress with Zstd");
        assert!(sz <= max_space);

        // SAFETY: dst has aligned plain data contents, and written interval is in range
        // // (because pad_pre + sz + pad_post <= pad_pre + max_space + pad_post = req_space)
        std::ptr::write_bytes(dst.add(pad_pre + sz), 0, pad_post);

        // SAFETY: ZSTD_compress2 wrote as many bytes as its return value indicated
        v.set_len(sz + pad_pre + pad_post);
    }

    v
}
#[cfg(not(feature = "zstd"))]
pub fn zstd_compress_to_vec(
    ctx: &mut ZstdCCtx,
    input: &[u8],
    level: i8,
    pad_pre: usize,
    pad_post: usize,
) -> Vec<u8> {
    unreachable!();
}

/* Returns None if the input does not decompress to exactly uncomp_len, or if input/output is too large */
#[cfg(feature = "zstd")]
pub fn zstd_decompress_to_vec(
    ctx: &mut ZstdDCtx,
    input: &[u8],
    uncomp_len: usize,
) -> Option<Vec<u8>> {
    let mut v = Vec::new();

    unsafe {
        // SAFETY: iff ZSTD_decompressDCtx succeeds, all of dst will be overwritten. v.reserve_exact
        // ensures `uncomp_len` bytes are available in v, and ZSTD_decompressDCtx should not read or
        // write outside these bounds, or read outside the input. There is no alignment requirement
        v.reserve_exact(uncomp_len);
        let ndecomp = ZSTD_decompressDCtx(
            ctx.ctx,
            v.as_mut_ptr() as *mut c_void,
            uncomp_len,
            input.as_ptr() as *const c_void,
            input.len(),
        );
        if ndecomp != uncomp_len {
            return None;
        }
        // SAFETY: all of dst[..uncomp_len] has been written to by ZSTD_decompressDCtx
        v.set_len(uncomp_len);
    }
    Some(v)
}
#[cfg(not(feature = "zstd"))]
pub fn zstd_decompress_to_vec(
    ctx: &mut ZstdDCtx,
    input: &[u8],
    uncomp_len: usize,
) -> Option<Vec<u8>> {
    unreachable!();
}

#[cfg(feature = "zstd")]
pub fn zstd_decompress_to_slice(ctx: &mut ZstdDCtx, input: &[u8], dst: &mut [u8]) -> Option<()> {
    unsafe {
        // SAFETY: iff ZSTD_decompressDCtx succeeds, all of dst will be overwritten; ZSTD_decompressDCtx
        // should never write outside `dst` vector allocated by v, or read outside the input. There is
        // no alignment requirement
        let ndecomp = ZSTD_decompressDCtx(
            ctx.ctx,
            dst.as_mut_ptr() as *mut c_void,
            dst.len(),
            input.as_ptr() as *const c_void,
            input.len(),
        );
        if ndecomp != dst.len() {
            return None;
        }
    }
    Some(())
}
#[cfg(not(feature = "zstd"))]
pub fn zstd_decompress_to_slice(ctx: &mut ZstdDCtx, input: &[u8], dst: &mut [u8]) -> Option<()> {
    unreachable!();
}

#[cfg(feature = "lz4")]
pub fn lz4_compress_to_vec(
    ctx: &mut LZ4CCtx,
    input: &[u8],
    level: i8,
    pad_pre: usize,
    pad_post: usize,
) -> Vec<u8> {
    let mut v = Vec::new();

    unsafe {
        let max_space: i32 = LZ4_compressBound(input.len().try_into().unwrap());
        let req_space = TryInto::<usize>::try_into(max_space)
            .unwrap()
            .checked_add(pad_pre)
            .unwrap()
            .checked_add(pad_post)
            .unwrap();
        assert!(req_space < isize::MAX as usize);
        v.reserve_exact(req_space as usize);

        let dst: *mut u8 = v.as_mut_ptr();
        // SAFETY: the req_space >= pad_pre bytes written here been allocated;
        // dst is aligned and contents valid as data is u8
        std::ptr::write_bytes(dst, 0, pad_pre);

        // SAFETY: Same in both cases. ctx.state is not null, is 8-aligned, and was made at
        // least as large as required for both LZ4/LZ4HC. The LZ4_compress functions do
        // not write outside their provided intervals, and dst[pad_pre..pad_pre+max_space]
        // is contained in dst[..req_space] so all output regions are allocated and pointer
        // calculations are in bounds
        let sz: i32 = if level <= 0 {
            // todo: currently input values <= 1 are replaced with 1, so level=0,level=1 are equivalent
            // waypipe-c has the same behavior
            LZ4_compress_fast_extState(
                ctx.state as *mut c_void,
                input.as_ptr() as *const c_char,
                dst.add(pad_pre) as *mut c_char,
                input.len().try_into().unwrap(),
                max_space,
                -(level as i32),
            )
        } else {
            LZ4_compress_HC_extStateHC(
                ctx.state as *mut c_void,
                input.as_ptr() as *const c_char,
                dst.add(pad_pre) as *mut c_char,
                input.len().try_into().unwrap(),
                max_space,
                level as i32,
            )
        };
        assert!(sz >= 0 && sz <= max_space, "Failed to compress with LZ4");
        let usz = sz as usize;
        // SAFETY: the region up to pad_pre + usz + pad_post has been allocated
        // dst is aligned and contents valid as data is u8
        std::ptr::write_bytes(dst.add(pad_pre + usz), 0, pad_post);
        // SAFETY: usz + pad_pre + pad_post < req_space, and the write_bytes/LZ4_compress
        // functions have written to every byte
        v.set_len(usz + pad_pre + pad_post);
    }

    v
}
#[cfg(not(feature = "lz4"))]
pub fn lz4_compress_to_vec(
    ctx: &mut LZ4CCtx,
    input: &[u8],
    level: i8,
    pad_pre: usize,
    pad_post: usize,
) -> Vec<u8> {
    unreachable!();
}

#[cfg(feature = "lz4")]
pub fn lz4_decompress_to_vec(input: &[u8], uncomp_len: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();

    let ilen: i32 = input.len().try_into().ok()?;
    let olen: i32 = uncomp_len.try_into().ok()?;

    unsafe {
        // SAFETY: iff LZ4_decompress_safe succeeds, all of dst will be overwritten; LZ4_decompress_safe
        // should never write outside `dst` vector allocated by v, or read outside the input. There is
        // no alignment requirement. The reserved space `uncomp_len` equals `olen` as overflow was checked.
        v.reserve_exact(uncomp_len);
        let ndecomp = LZ4_decompress_safe(
            input.as_ptr() as *const c_char,
            v.as_mut_ptr() as *mut c_char,
            ilen,
            olen,
        );
        if ndecomp != olen {
            return None;
        }

        // SAFETY: all of dst[..uncomp_len] has been written to by LZ4_decompress_safe
        v.set_len(uncomp_len);
    }
    Some(v)
}

#[cfg(not(feature = "lz4"))]
pub fn lz4_decompress_to_vec(input: &[u8], uncomp_len: usize) -> Option<Vec<u8>> {
    unreachable!();
}

#[cfg(feature = "lz4")]
pub fn lz4_decompress_to_slice(input: &[u8], dst: &mut [u8]) -> Option<()> {
    let ilen: i32 = input.len().try_into().ok()?;
    let olen: i32 = dst.len().try_into().ok()?;

    unsafe {
        // SAFETY: iff LZ4_decompress_safe succeeds, all of dst will be overwritten; LZ4_decompress_safe
        // should never operate outside its input/output regions. There is no alignment requirement.
        // The reserved space `uncomp_len` equals `olen` as overflow was checked.
        let ndecomp = LZ4_decompress_safe(
            input.as_ptr() as *const c_char,
            dst.as_mut_ptr() as *mut c_char,
            ilen,
            olen,
        );
        if ndecomp != olen {
            return None;
        }
    }
    Some(())
}

#[cfg(not(feature = "lz4"))]
pub fn lz4_decompress_to_slice(input: &[u8], dst: &mut [u8]) -> Option<()> {
    unreachable!();
}

#[cfg(feature = "zstd")]
#[test]
fn test_zstd_compression() {
    let mut x: Vec<u8> = vec![0; 1000];
    for (i, v) in x.iter_mut().enumerate() {
        *v = ((11 * i) % 256) as u8;
    }
    let mut c = zstd_make_cctx().unwrap();
    let w = zstd_compress_to_vec(&mut c, &x[..], 0, 16, 4);
    let mut d = zstd_make_dctx().unwrap();
    let y = zstd_decompress_to_vec(&mut d, &w[16..w.len() - 4], x.len()).unwrap();
    assert_eq!(x, y);
}

#[cfg(feature = "lz4")]
#[test]
fn test_lz4_compression() {
    let mut x: Vec<u8> = vec![0; 1000];
    for (i, v) in x.iter_mut().enumerate() {
        *v = ((11 * i) % 256) as u8;
    }
    let mut c = lz4_make_cctx().unwrap();
    let w = lz4_compress_to_vec(&mut c, &x[..], 0, 16, 4);
    let y = lz4_decompress_to_vec(&w[16..w.len() - 4], x.len()).unwrap();
    assert_eq!(x, y);
}
