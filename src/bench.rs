/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! `waypipe bench` implementation */
use crate::compress::*;
use crate::kernel::{apply_diff_one, construct_diff_segment_two};
use crate::util::*;
use crate::Compression;
use crate::Options;
use std::time::Instant;

#[derive(Debug, PartialEq, Eq)]
enum DiffPattern {
    On,
    IdealOn,
    Off,
    MemcmpOff,
    MinimumOff,
    Alternating100, // very short cycle, should be papered over
    Alternating1K,  // 2kb ~~ 512 pixels
    Alternating2K,  // 4kb cycle ~ 1024 pixels,
}

fn fill_alternating(rng: &mut BadRng, data: &mut [u8], span_min: usize, span_max: usize) {
    let mut i = 0;
    let mut change = false;
    while i < data.len() {
        let jump = span_min + rng.next_usize(1 + span_max - span_min);
        let j = std::cmp::min(data.len(), i + jump);
        data[i..j].fill(change as u8);
        i = j;
        change = !change;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn read_replace_write_avx2(src: &[u8], rdwrite: &mut [u8], dst: &mut [u8]) -> u32 {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;
    assert!(src.as_ptr() as usize % 64 == 0);
    assert!(rdwrite.as_ptr() as usize % 64 == 0);
    assert!(dst.as_ptr() as usize % 4 == 0);
    assert!(src.len() == rdwrite.len());
    assert!(dst.len() >= src.len());

    if true {
        let ones = _mm256_set1_epi64x(u64::MAX as i64);
        const UNROLL: usize = 4;
        for k in 0..(src.len() / (32 * UNROLL)) {
            // Widely unrolled
            let mut xs = [_mm256_undefined_si256(); UNROLL];
            let mut ys = [_mm256_undefined_si256(); UNROLL];

            for j in 0..UNROLL {
                let i = UNROLL * k + j;

                xs[j] = _mm256_load_si256(src.as_ptr().add(i * 32) as *const _);
                ys[j] = _mm256_load_si256(rdwrite.as_ptr().add(i * 32) as *const _);
            }

            let diff0 = _mm256_cmpeq_epi32(xs[0], ys[0]);
            let diff1 = _mm256_cmpeq_epi32(xs[1], ys[1]);
            let diff2 = _mm256_cmpeq_epi32(xs[2], ys[2]);
            let diff3 = _mm256_cmpeq_epi32(xs[3], ys[3]);

            let sum0 = _mm256_add_epi32(diff0, diff1);
            let sum2 = _mm256_add_epi32(diff2, diff3);
            let sum = _mm256_add_epi32(sum0, sum2);

            if _mm256_testc_si256(sum, ones) != 0 {
                // Introduce early exit option to consume reads and inhibit memcpy optimization
                panic!();
            }

            for (j, x) in xs.iter().enumerate().take(UNROLL) {
                let i = UNROLL * k + j;
                _mm256_store_si256(rdwrite.as_mut_ptr().add(i * 32) as *mut _, *x);
            }
            for (j, x) in xs.iter().enumerate().take(UNROLL) {
                let i = UNROLL * k + j;
                _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32) as *mut _, *x);
            }
        }
    }

    if false {
        let ones = _mm256_set1_epi64x(u64::MAX as i64);
        for i in 0..(src.len() / 32) {
            let x = _mm256_load_si256(src.as_ptr().add(i * 32) as *const _);
            let y = _mm256_load_si256(rdwrite.as_ptr().add(i * 32) as *const _);
            let s = _mm256_cmpeq_epi32(x, y);

            _mm256_store_si256(rdwrite.as_mut_ptr().add(i * 32) as *mut _, x);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32) as *mut _, x);
            if i % 4 == 0 && _mm256_testc_si256(s, ones) != 0 {
                /* Should never happen; this test should prevent memcpy optimization */
                panic!();
            }
        }
    }

    if false {
        for i in 0..(src.len() / 32) {
            let x = _mm256_load_si256(src.as_ptr().add(i * 32) as *const _);
            let y = _mm256_load_si256(rdwrite.as_ptr().add(i * 32) as *const _);
            let s = _mm256_add_epi32(x, y);
            /* Store "s" instead of the correct "x", because otherwise the compiler will
             * mis-optimize by extracting a memcpy and increase total memory bandwidth used. */
            _mm256_store_si256(rdwrite.as_mut_ptr().add(i * 32) as *mut _, s);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 32) as *mut _, s);
        }
    }

    src.len() as u32
}

/* Test function: count the number of differences between src1 and rdwrite, and then
 * copy src1 to rdwrite and dst2. The performance of this, if properly optimized, should be
 * a lower bound on diff construction performance in the "everything changed" scenario. */
fn read_replace_write(src: &[u8], rdwrite: &mut [u8], dst: &mut [u8]) -> u32 {
    const CHUNK_SIZE: usize = 256;
    let mut any_nondiff = false;

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    if is_x86_feature_detected!("avx2") {
        return unsafe { read_replace_write_avx2(src, rdwrite, dst) };
    }

    for (bsrc, (brdwr, bdst)) in std::iter::zip(
        src.chunks_exact(CHUNK_SIZE),
        std::iter::zip(
            rdwrite.chunks_exact_mut(CHUNK_SIZE),
            dst.chunks_exact_mut(CHUNK_SIZE),
        ),
    ) {
        any_nondiff |= bsrc.cmp(brdwr).is_eq();
        brdwr.copy_from_slice(bsrc);
        bdst.copy_from_slice(bsrc);
    }

    any_nondiff as u32
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn read_once_avx2(src: &[u8]) -> u32 {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;
    assert!(src.as_ptr() as usize % 64 == 0);

    let mut xs = [_mm256_set1_epi8(0); 4];
    for i in 0..(src.len() / 128) {
        for (j, x) in xs.iter_mut().enumerate() {
            let k = 4 * i + j;
            let y = _mm256_load_si256(src.as_ptr().add(k * 32) as *const _);
            *x = _mm256_max_epi32(*x, y); // max epi32 may be faster than epi8
        }
    }

    let mut nonzero = false;
    for x in xs {
        let ones = _mm256_set1_epi8(0xff_u8 as i8);
        if _mm256_testz_si256(x, ones) != 0 {
            nonzero = true;
        }
    }

    nonzero as u32
}

/* Test function: how long does it take to just _read_ the source, and compute something trivial
 * (like whether it is all-zero); only run on all 0 input */
fn read_once(src: &[u8]) -> u32 {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    if is_x86_feature_detected!("avx2") {
        return unsafe { read_once_avx2(src) };
    }
    src.contains(&1) as u32
}

fn estimate_diff_speed(pattern: DiffPattern) {
    let length: usize = 1 << 20;

    let mut reference_arr = AlignedArray::new(length);
    let mut source_arr = AlignedArray::new(length);
    let mut baseline_arr = AlignedArray::new(length);
    let baseline = baseline_arr.get_mut();
    let reference = reference_arr.get_mut();
    let source = source_arr.get_mut();
    baseline.fill(0);
    reference.fill(0);
    let mut diff = vec![0; length + 8];

    let mut rng = BadRng { state: 0x1 };

    match pattern {
        DiffPattern::IdealOn | DiffPattern::On => {
            /* disagree with baseline everwhere */
            source.fill(1);
        }
        DiffPattern::MemcmpOff | DiffPattern::Off | DiffPattern::MinimumOff => {
            /* match baseline everwhere */
            source.fill(0);
        }
        DiffPattern::Alternating100 => {
            fill_alternating(&mut rng, &mut source[..], 90, 110);
        }
        DiffPattern::Alternating1K => {
            fill_alternating(&mut rng, &mut source[..], 900, 1100);
        }
        DiffPattern::Alternating2K => {
            fill_alternating(&mut rng, &mut source[..], 1800, 2200);
        }
    }

    let ntrials = 1600;
    let mut elapsed_times = Vec::<f64>::new();
    let mut last_diff_len = None;
    for i in 0..ntrials {
        /* Design: construct diffs, periodically diffing the reference against the source and baseline patterns */

        let test_src: &[u8] = std::hint::black_box(if i % 2 == 0 { source } else { baseline });

        let start = Instant::now();

        let diff_len: u32 = if pattern == DiffPattern::MemcmpOff {
            (test_src.cmp(reference) as i32) as u32
        } else if pattern == DiffPattern::MinimumOff {
            read_once(test_src)
        } else if pattern == DiffPattern::IdealOn {
            read_replace_write(test_src, &mut reference[..], &mut diff[..])
        } else {
            construct_diff_segment_two(&mut diff[..], test_src, &mut reference[..], 0, 32)
        };

        let end = Instant::now();

        if let Some(d) = last_diff_len {
            /* Light sanity check: diff length should only depend on the pattern of differences, not on the
             * specific contents of the data */
            assert!(d == diff_len);
        }
        elapsed_times.push(end.duration_since(start).as_secs_f64());
        last_diff_len = Some(diff_len);
    }
    let diff_len = last_diff_len.unwrap();

    /* Skip the first run (may not be in cache) */
    let hot_times = &elapsed_times[1..];

    // minor issue: not stable to outliers or numerical issues, there are better algorithms
    let mean = hot_times.iter().sum::<f64>() / hot_times.len() as f64;
    let sample_var = hot_times
        .iter()
        .map(|x| (*x - mean) * (*x - mean))
        .sum::<f64>()
        / ((hot_times.len() - 1) as f64);
    let min_time = hot_times.iter().fold(f64::INFINITY, |x, y| x.min(*y));
    let max_time = hot_times.iter().fold(-f64::INFINITY, |x, y| x.max(*y));

    let f: f64 = 1e9 / (length as f64);

    println!(
        "{:>8?} (diff len={:>8}): {:.4} +/- {:.4} ns/byte, range [{:.4},{:.3}] ns/byte",
        pattern,
        diff_len,
        mean * f,
        sample_var.sqrt() * f,
        min_time * f,
        max_time * f
    );
}

fn estimate_diff_compress_speed(
    length: usize,
    comp: Compression,
    text_like: bool,
) -> ((f32, f32), (f32, f32), f32) {
    assert!(length % 64 == 0);
    let nshards = std::cmp::max(3, (length / 64) / (1 << 12));

    let mut src_arr = AlignedArray::new(length);
    let mut src_mirror_arr = AlignedArray::new(length);
    let mut diff_arr = AlignedArray::new(length + 8 * nshards);
    let mut dst_arr = AlignedArray::new(length);
    let mut dst_mirror_arr = AlignedArray::new(length);

    let src = src_arr.get_mut();
    let src_mirror = src_mirror_arr.get_mut();
    let diff = diff_arr.get_mut();
    let dst = dst_arr.get_mut();
    let dst_mirror = dst_mirror_arr.get_mut();

    let mut rng = BadRng { state: 1 };
    if text_like {
        let mut i = 0;
        let mut change = false;
        while i < length {
            let jump = 4096 + rng.next_usize(1 + 4096);
            let j = std::cmp::min(length, i + jump);
            if change {
                for s in &mut src[i..j] {
                    *s = (0x7f * rng.next_usize(3)) as u8;
                }
            }
            i = j;
            change = !change;
        }
    } else {
        let mut k: usize = 0;
        for i in 0..(length / 4) {
            if i % 1024 == 0 {
                k = rng.next_usize(1 << 24);
            } else {
                k += 4;
            }

            let noise = rng.next_usize(2);
            src[4 * i] = (k + noise) as u8;
            src[4 * i + 1] = (k >> 8) as u8;
            src[4 * i + 2] = (k >> 16) as u8;
            src[4 * i + 3] = 0xff;
        }
    }

    /* Operate on the large region chunk-by-chunk -- this provides crude statistics
     * to estimate uncertainty for timing and makes it possible to stop early if
     * compression is very slow */
    let mut diff_start = 0;
    let mut data = Vec::new();
    let total_start = Instant::now();
    for i in 0..nshards {
        let istart =
            64 * split_interval(0, (length / 64) as u32, nshards as u32, i as u32) as usize;
        let iend =
            64 * split_interval(0, (length / 64) as u32, nshards as u32, (i + 1) as u32) as usize;

        let start = Instant::now();
        let diff_len = construct_diff_segment_two(
            &mut diff[diff_start..],
            &src[istart..iend],
            &mut src_mirror[istart..iend],
            istart as u32,
            32,
        );

        let shard_diff = &diff[diff_start..diff_start + diff_len as usize];
        let (ndiff, mid, comp_len): (Vec<u8>, Instant, usize) = match comp {
            Compression::None => {
                let x = std::hint::black_box(Vec::from(shard_diff));
                (x, Instant::now(), diff_len as usize)
            }
            Compression::Lz4(lvl) => {
                let mut ctx = lz4_make_cctx().unwrap();
                let comp =
                    std::hint::black_box(lz4_compress_to_vec(&mut ctx, shard_diff, lvl, 0, 0));
                let t = Instant::now();
                (
                    lz4_decompress_to_vec(&comp, diff_len as usize).unwrap(),
                    t,
                    comp.len(),
                )
            }
            Compression::Zstd(lvl) => {
                let mut cctx = zstd_make_cctx().unwrap();
                let mut dctx = zstd_make_dctx().unwrap();
                let comp =
                    std::hint::black_box(zstd_compress_to_vec(&mut cctx, shard_diff, lvl, 0, 0));
                let t = Instant::now();
                (
                    zstd_decompress_to_vec(&mut dctx, &comp, diff_len as usize).unwrap(),
                    t,
                    comp.len(),
                )
            }
        };
        apply_diff_one(&ndiff, 0, 0, dst).unwrap();
        apply_diff_one(&ndiff, 0, 0, dst_mirror).unwrap();

        diff_start += diff_len as usize;

        let end = Instant::now();

        let time_diff = mid.duration_since(start).as_secs_f32();
        let time_apply = end.duration_since(mid).as_secs_f32();
        data.push((time_diff, time_apply, iend - istart, comp_len));

        /* If replicating the buffer takes a long time, then stop; cache
         * effects will be negligible compared to compression/decompression time */
        if end.duration_since(total_start).as_secs_f32() >= 1.0 && data.len() >= 3 {
            break;
        }
    }
    let copy_end =
        64 * split_interval(0, (length / 64) as u32, nshards as u32, data.len() as u32) as usize;

    std::hint::black_box(dst);
    assert!(std::hint::black_box(dst_mirror)[..copy_end] == src[..copy_end]);

    let mut comp_len = 0;
    let mut proc_len = 0;
    let mut speeds_diff: Vec<f32> = Vec::new();
    let mut speeds_apply: Vec<f32> = Vec::new();
    for (time_diff, time_apply, input_len, output_len) in data.iter() {
        proc_len += input_len;
        comp_len += output_len;
        /* The shards are almost equal in length */
        speeds_diff.push((*input_len as f32) / time_diff);
        speeds_apply.push((*input_len as f32) / time_apply);
    }
    let ratio = (comp_len as f32) / (proc_len as f32);
    let n = data.len();
    let diff_speed: f32 = speeds_diff.iter().sum::<f32>() / (n as f32);
    let diff_sstdev2: f32 = speeds_diff
        .iter()
        .map(|x| (x - diff_speed) * (x - diff_speed))
        .sum::<f32>()
        / ((n - 1) as f32);
    let apply_speed: f32 = speeds_apply.iter().sum::<f32>() / (n as f32);
    let apply_sstdev2: f32 = speeds_apply
        .iter()
        .map(|x| (x - diff_speed) * (x - diff_speed))
        .sum::<f32>()
        / ((n - 1) as f32);

    (
        (diff_speed, diff_sstdev2.sqrt()),
        (apply_speed, apply_sstdev2.sqrt()),
        ratio,
    )
}

fn run_diff_speed_benchmark() {
    println!("Diff pattern speed for 2^20 bytes, using sample stdev");
    estimate_diff_speed(DiffPattern::On);
    /* NOTE: these are _not_ perfect benchmarks, and should not be used for comparisons;
     * thermal or other throttling makes results after the first run slower. Thus, run 'On' twice. */
    estimate_diff_speed(DiffPattern::On);
    estimate_diff_speed(DiffPattern::IdealOn);
    estimate_diff_speed(DiffPattern::Off);
    estimate_diff_speed(DiffPattern::MemcmpOff);
    estimate_diff_speed(DiffPattern::MinimumOff);
    estimate_diff_speed(DiffPattern::Alternating100);
    estimate_diff_speed(DiffPattern::Alternating1K);
    estimate_diff_speed(DiffPattern::Alternating2K);
}

pub fn run_benchmark(opts: &Options, fast_mode: bool) -> Result<(), String> {
    if opts.debug {
        run_diff_speed_benchmark();
    }

    let mut cvs = Vec::<Compression>::new();
    cvs.push(Compression::None);
    if cfg!(feature = "lz4") {
        for i in -10..=12 {
            cvs.push(Compression::Lz4(i));
        }
    } else {
        println!("Waypipe was not built with lz4 compression/decompression support, skipping measurements with lz4");
    }
    if cfg!(feature = "zstd") {
        for i in -10..=22 {
            cvs.push(Compression::Zstd(i));
        }
    } else {
        println!("Waypipe was not built with zstd compression/decompression support, skipping measurements with zstd");
    }

    let mut text_results = Vec::new();
    let mut img_results = Vec::new();
    /* A long test length is useful for realism (to avoid having *all data fit into cache), but makes
     * the test slower to run, so no repetitions will be done. */
    let test_size = if fast_mode { 1 << 16 } else { 1 << 22 };
    println!("Measured (diff+compress,decompress+apply) speeds and compression ratios");
    println!("(single-threaded, unpipelined, quite artificial, single measurement)");
    for c in cvs.iter() {
        let (textspeed_diff, textspeed_apply, textratio) =
            estimate_diff_compress_speed(test_size, *c, true);
        let (imgspeed_diff, imgspeed_apply, imgratio) =
            estimate_diff_compress_speed(test_size, *c, false);
        let max_pad = &[' '; 8];
        let padding = max_pad[..8 - c.to_string().len()]
            .iter()
            .collect::<String>();
        println!(
            "{}:{} text-like ({:.2e}±{:.1}%,{:.2e}±{:.1}%) bytes/sec, ratio {:.3} image-like ({:.2e}±{:.1}%,{:.2e}±{:.1}%) bytes/sec, ratio {:.3}",
            c, padding, textspeed_diff.0, textspeed_diff.1 / textspeed_diff.0,
            textspeed_apply.0, textspeed_apply.1 / textspeed_apply.0, textratio,
            imgspeed_diff.0, imgspeed_diff.1 / imgspeed_diff.0, imgspeed_apply.0,
            imgspeed_apply.1 / imgspeed_apply.0, imgratio
        );
        text_results.push((textspeed_diff.0, textspeed_apply.0, textratio));
        img_results.push((imgspeed_diff.0, imgspeed_apply.0, imgratio));
    }

    let nthreads = if opts.threads == 0 {
        std::cmp::max(1, std::thread::available_parallelism().unwrap().get() / 2)
    } else {
        opts.threads as usize
    };
    println!(
        "With {} threads, estimated time for a 32 MB (4k) image transfer, assuming:",
        nthreads
    );
    println!("- perfect utilization and pipelining; no buffering, transfer latency or jitter");
    println!("- equally fast local and remote computers");
    let mbps_opts = [
        1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0,
        20000.0,
    ];

    let test_size = (1 << 25) as f32;
    for (typ, results) in &[("text-like", &text_results), ("image-like", &img_results)] {
        for m in mbps_opts {
            let bandwidth = m * 1e6;
            /* optimal choice: */
            let mut best = Compression::None;
            let mut best_time = f32::INFINITY;
            for (c, (speed_diff, speed_apply, ratio)) in cvs.iter().zip(results.iter()) {
                let full_speed_diff = speed_diff * (nthreads as f32);
                let full_speed_apply = speed_apply * (nthreads as f32);
                let transfer_speed = bandwidth / ratio;
                let processing_speed = transfer_speed.min(full_speed_apply).min(full_speed_diff);
                let test_time = test_size / processing_speed;
                let test_time_unpipelined = test_size / full_speed_diff
                    + test_size / transfer_speed
                    + test_size / full_speed_apply;
                if opts.debug {
                    println!(
                        "comp={}, bandwidth={:e} bytes/sec: estimated {} msec (unpipelined: {} msec)",
                        c,
                        bandwidth,
                        1e3 * test_time,
                        1e3 * test_time_unpipelined
                    );
                }
                if test_time < best_time {
                    best_time = test_time;
                    best = *c;
                }
            }
            println!(
                "bandwidth={:e} bytes/sec, {} suggested comp={} with: estimated {} msec",
                bandwidth,
                typ,
                best,
                1e3 * best_time
            );
        }
    }

    Ok(())
}
