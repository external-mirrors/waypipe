/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! Damage merging logic */
use log::debug;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/* Note: (the use of u32 x1/x2 instead of i32 x+width ensures sign and overflow checks need
 * be done only once.) */
#[derive(Debug, Copy, Clone)]
pub struct Rect {
    pub x1: u32,
    pub x2: u32,
    pub y1: u32,
    pub y2: u32,
}

/* Note: this _saturates_ on overflow, and assumes a well-formed rectangle with x1<x2,y1<y2 */
fn bounding_interval(r: &Rect, offset: usize, stride: usize, bpp: usize) -> (usize, usize) {
    let start = offset
        .saturating_add((r.y1 as usize).saturating_mul(stride))
        .saturating_add((r.x1 as usize).saturating_mul(bpp));
    let end = offset
        .saturating_add((r.y2 as usize).saturating_mul(stride))
        .saturating_add((r.x2 as usize).saturating_mul(bpp));

    (start, end)
}

fn align_down(x: usize, align_bits: u32) -> usize {
    (x >> align_bits) << align_bits
}

fn align_up(x: usize, align_bits: u32) -> usize {
    x.checked_next_multiple_of(1_usize << align_bits).unwrap()
}
// todo: handling overflow? Cannot align max_int - 1; better to error?

fn aligned_bounding_interval(
    r: &Rect,
    offset: usize,
    stride: usize,
    bpp: usize,
    align_bits: u32,
) -> (usize, usize) {
    let x = bounding_interval(r, offset, stride, bpp);
    (align_down(x.0, align_bits), align_up(x.1, align_bits))
}

/* Requires: All rectangles, after projection, will fit inside the interval given by bound */
fn process_group(
    rects: &[Rect],
    bound: (usize, usize),
    output: &mut Vec<(usize, usize)>,
    align_bits: u32,
    min_gap: usize,
    offset: usize,
    stride: usize,
    bpp: usize,
) {
    // TODO: implement a pre-pass that computes a disjoint collection of rectangles (e.g.: using a sweep
    // line algorithm plus interval tree); this would be far more efficient than the current solution, which
    // tends to compute almost the same union once per horizontal line. The fact that the output space is
    // almost a cylinder is not actually important -- it at most introduces one seam per horizontal line,
    // which is a minor cost. min_gap can be handled by horizontally expanding the rectangles beforehand;
    // it is permissible to erase gaps slightly larger than min_gap.

    /* A heap of indices into the rectangle, using lex sorted key to track start position and rect index  */
    let mut rect_heap: BinaryHeap<(Reverse<usize>, usize)> = BinaryHeap::new();
    rect_heap.reserve_exact(rects.len());
    let mut row_counters: Vec<usize> = vec![0; rects.len()];

    let mut work_estimate: usize = 0; // if this grows too large relative to bound.1-bound.0, fall back to dumb strategy.

    for (i, r) in rects.iter().enumerate() {
        if (r.x2 - r.x1) as usize * bpp > stride.saturating_sub(min_gap) {
            /* All segments from this rectangle will be merged; this is a common case, so optimize it */
            row_counters[i] = usize::MAX;
        }

        // TODO: special case optimization to merge entire rectangle into contiguous segment if gaps are small;
        // not _strictly_ necessary, but good for perf
        let start_pos = align_down(
            offset + (r.y1 as usize) * stride + (r.x1 as usize) * bpp,
            align_bits,
        );
        rect_heap.push((Reverse(start_pos), i));
    }

    let mut cur: Option<(usize, usize)> = None;
    while let Some((Reverse(start), i)) = rect_heap.pop() {
        /* Process next segment */
        let rect = &rects[i];
        let merge_opt = row_counters[i] == usize::MAX;
        let end = if merge_opt {
            align_up(
                offset + (rect.y2 - 1) as usize * stride + (rect.x2 as usize) * bpp,
                align_bits,
            )
        } else {
            align_up(
                offset + (rect.y1 as usize + row_counters[i]) * stride + (rect.x2 as usize) * bpp,
                align_bits,
            )
        };
        if let Some(cv) = cur {
            if start <= cv.1 || (start - cv.1) < min_gap {
                /* either approximate containment or overlap */
                cur = Some((cv.0, std::cmp::max(cv.1, end)));
            } else {
                output.push(cv);
                cur = Some((start, end));
            }
        } else {
            cur = Some((start, end));
        }

        if !merge_opt {
            /* Update row counter and return rectangle if not done */
            row_counters[i] += 1;
            if row_counters[i] < (rect.y2 - rect.y1) as usize {
                let start_pos = align_down(
                    offset
                        + (rect.y1 as usize + row_counters[i]) * stride
                        + (rect.x1 as usize) * bpp,
                    align_bits,
                );
                rect_heap.push((Reverse(start_pos), i));
            }
        }

        /* Do not spend much more time computing damage regions than it would
         * take to just scan the entire interval for differences. This doesn't
         * need to be a _great_ estimate, just within a few orders of magnitude
         * to avoid pathological behavior */
        work_estimate += std::cmp::max(16, rect_heap.len()).ilog2() as usize;
        if work_estimate > (bound.1 - bound.0) / 8 {
            debug!(
                "Stopped processing block after estimated {} work; length {}",
                work_estimate,
                (bound.1 - bound.0)
            );
            output.push((cur.unwrap().0, bound.1));
            return;
        }
    }

    if let Some(cv) = cur {
        output.push(cv);
    }
}

// todo: eventually add support for multiplanar and subsampled formats?
/**
 * Note: all rectangles must be clipped to the wl_buffer bounds, otherwise function may produce nonsense or panic
 *
 * min_gap: minimum allowed gap on output
 * align_bits: e.g. value 6 => output segment positions are multiples of 64
 *
 * The last interval may be rounded up align(buffer end, 1<<align_bits), and need not be
 * entirely contained by the buffer.
 */
pub fn compute_damaged_segments(
    rects: &mut [Rect],
    align_bits: u32,
    min_gap: usize,
    offset: usize,
    stride: usize,
    bpp: usize,
) -> Vec<(usize, usize)> {
    if rects.is_empty() {
        return Vec::new();
    }
    assert!(stride > 0);
    assert!(bpp > 0);

    let mut output = Vec::new();
    for r in rects.iter() {
        assert!(r.x1 < r.x2 && r.y1 < r.y2);
    }

    rects.sort_unstable_by_key(|rect: &Rect| -> usize {
        bounding_interval(rect, offset, stride, bpp).0
    });

    struct Group {
        i_start: usize,
        i_end: usize,
        /* The aligned region covered by the corresponding rectangles */
        region: (usize, usize),
    }

    /* First: group intervals into overlapping clumps separated by >= min_gap, post alignment.
     * */
    let mut spans: Vec<Group> = Vec::new();
    let mut rect_iter = rects.iter().enumerate();
    let mut current = Group {
        i_start: 0,
        i_end: 1,
        region: aligned_bounding_interval(
            rect_iter.next().unwrap().1,
            offset,
            stride,
            bpp,
            align_bits,
        ),
    };
    for (i, rect) in rect_iter {
        let b = aligned_bounding_interval(rect, offset, stride, bpp, align_bits);
        if b.0 <= current.region.1 || (b.0 - current.region.1) < min_gap {
            current = Group {
                i_start: current.i_start,
                i_end: i + 1,
                region: (current.region.0, std::cmp::max(current.region.1, b.1)),
            }
        } else {
            spans.push(current);
            current = Group {
                i_start: i,
                i_end: i + 1,
                region: b,
            }
        }
    }
    spans.push(current);

    /* Next: process each span (or if it isn't worth the time, complete it) */
    for group in spans {
        process_group(
            &rects[group.i_start..group.i_end],
            group.region,
            &mut output,
            align_bits,
            min_gap,
            offset,
            stride,
            bpp,
        );
    }

    output
}

/* Given two sorted lists of disjoint intervals, compute their union, automatically closing any
 * gaps of size <min_gap in the process. Panics if validation fails, as this should be an invariant.
 * This preserves endpoint alignment.
 */
pub fn union_damage(
    a: &[(usize, usize)],
    b: &[(usize, usize)],
    min_gap: usize,
) -> Vec<(usize, usize)> {
    assert!(validate_output(a, 0, min_gap).is_ok());
    assert!(validate_output(b, 0, min_gap).is_ok());

    let mut output = Vec::new();

    let mut iter_a = a.iter().peekable();
    let mut iter_b = b.iter().peekable();

    let mut last: Option<(usize, usize)> = None;
    loop {
        // Extract element with next earliest endpoint from either of A or B
        let pa = iter_a.peek();
        let pb = iter_b.peek();

        let nxt = *match (pa, pb) {
            (Some(ea), Some(eb)) => {
                if ea.0 <= eb.0 {
                    iter_a.next().unwrap()
                } else {
                    iter_b.next().unwrap()
                }
            }
            (Some(_), None) => iter_a.next().unwrap(),
            (None, Some(_)) => iter_b.next().unwrap(),
            (None, None) => {
                break;
            }
        };

        let Some(mut y) = last else {
            last = Some(nxt);
            continue;
        };

        // Merge intervals or push old
        if nxt.0 <= y.1 || (nxt.0 - y.1) < min_gap {
            y.1 = std::cmp::max(y.1, nxt.1);
            last = Some(y);
        } else {
            output.push(y);
            last = Some(nxt);
        }
    }
    if let Some(e) = last {
        output.push(e);
    }

    output
}

fn validate_output(a: &[(usize, usize)], align_bits: u32, min_gap: usize) -> Result<(), String> {
    for (x, y) in a {
        if x >= y {
            return Err(format!("negative or empty interval {} {}", x, y));
        }
        let mask = (1_usize << align_bits) - 1;
        if x & mask != 0 || y & mask != 0 {
            return Err(format!("misaligned {} {}", x, y));
        }
    }

    for i in 1..a.len() {
        if a[i].0 < a[i - 1].1 {
            return Err(format!("overlapping {:?} {:?}", a[i - 1], a[i]));
        }
        if a[i].0 < a[i - 1].1 + min_gap {
            return Err(format!(
                "min gap too small {}-{}={} < {}",
                a[i].0,
                a[i - 1].1,
                a[i].0 - a[i - 1].1,
                min_gap
            ));
        }
    }

    Ok(())
}

#[test]
fn test_union_damage() {
    let x: &[(usize, usize)] = &[(0, 6)];
    let y: &[(usize, usize)] = &[(8, 10), (14, 20)];
    let align_bits = 1;
    let max_gap = 4;
    assert!(validate_output(x, align_bits, max_gap).is_ok());
    assert!(validate_output(y, align_bits, max_gap).is_ok());

    let bad1: &[(usize, usize)] = &[(8, 10), (15, 20)];
    let bad2: &[(usize, usize)] = &[(8, 10), (12, 20)];
    let bad3: &[(usize, usize)] = &[(8, 10), (6, 20)];
    assert!(validate_output(bad1, align_bits, max_gap).is_err());
    assert!(validate_output(bad2, align_bits, max_gap).is_err());
    assert!(validate_output(bad3, align_bits, max_gap).is_err());

    let output = union_damage(x, y, max_gap);
    println!("output: {:?}", output);
    assert_eq!(&output, &[(0, 10), (14, 20)]);

    // TODO: test overflow conditions
}

#[test]
fn test_damage_computation() {
    {
        let w = 100;
        let h = 50;
        let bpp = 1;
        let stride = bpp * w;

        let example_pattern = [
            Rect {
                x1: 0,
                x2: 10,
                y1: 0,
                y2: 10,
            },
            Rect {
                x1: 90,
                x2: 100,
                y1: 40,
                y2: 50,
            },
        ];

        let align_bits = 0;
        let offset = 0;
        let mut tmp = example_pattern;
        let slices = compute_damaged_segments(&mut tmp, align_bits, 0, offset, stride, bpp);
        assert!(slices.len() == 20);
        println!("slices: {:?}", slices);

        /* Check that, when min_gap is huge, only one big interval is reported. */
        let mut tmp = example_pattern;
        let min_gap = usize::MAX;
        let slices = compute_damaged_segments(&mut tmp, align_bits, min_gap, offset, stride, bpp);
        assert_eq!(slices, &[(0, w * h * bpp)]);
    }

    fn fill_mask(mask: &mut [bool], w: usize, h: usize, stride: usize, bpp: usize, rects: &[Rect]) {
        mask.fill(false);
        for r in rects {
            assert!(r.x1 < r.x2 && r.x2 <= w as u32, "{:?}", r);
            assert!(r.y1 < r.y2 && r.y2 <= h as u32, "{:?}", r);
            for y in r.y1..r.y2 {
                mask[((y as usize) * stride + (r.x1 as usize) * bpp)
                    ..((y as usize) * stride + (r.x2 as usize) * bpp)]
                    .fill(true);
            }
        }
    }
    fn test_segments(mask: &mut [bool], segments: &[(usize, usize)]) {
        for (a, b) in segments {
            let b = std::cmp::min(*b, mask.len());
            mask[*a..b].fill(false);
        }
        assert!(mask.iter().all(|x| !*x));
    }

    let w = 100;
    let h = 100;
    let bpp = 1;
    let stride = 200;
    assert!(stride >= w * bpp);
    let mut mask = vec![false; h * stride];
    for i in 0..100_usize {
        /* A variety of test patterns */
        let mut rects: Vec<Rect> = Vec::new();
        if i == 0 {
            for x in 0..((w / 2) as u32) {
                for y in 0..((h / 2) as u32) {
                    rects.push(Rect {
                        x1: 2 * x,
                        x2: 2 * x + 1,
                        y1: 2 * y,
                        y2: 2 * y + 1,
                    });
                }
            }
        } else if i % 4 == 0 {
            for j in 0..(i as u32) {
                rects.push(Rect {
                    x1: j,
                    x2: j + 1,
                    y1: 0,
                    y2: (h as u32) - j,
                });
                rects.push(Rect {
                    x1: 0,
                    x2: (w as u32) - j,
                    y1: j,
                    y2: j + 1,
                });
            }
        } else if i % 2 == 0 {
            for j in 0..(i as u32) {
                rects.push(Rect {
                    x1: j,
                    x2: j + 2,
                    y1: j,
                    y2: j + 2,
                });
            }
        } else {
            let (dw, dh, di) = ((w / 2) as u32, (h / 2) as u32, (i / 2) as u32);
            for j in 1..di {
                rects.push(Rect {
                    x1: dw - j,
                    x2: dw + j,
                    y1: dh - (di - j),
                    y2: dh + (di - j),
                });
            }
        }

        let align_bits = 2;
        let min_gap = 1;

        fill_mask(&mut mask, w, h, stride, bpp, &rects);
        let nset = mask.iter().map(|x| *x as usize).sum::<usize>();

        let mut tmp = rects;
        let slices = compute_damaged_segments(&mut tmp, align_bits, min_gap, 0, stride, bpp);

        let ncover = slices.iter().map(|(x, y)| y - x).sum::<usize>();
        println!(
            "test {}, {} rects, {} slices, {} filled, {} covered",
            i,
            tmp.len(),
            slices.len(),
            nset,
            ncover
        );

        validate_output(&slices, align_bits, min_gap).unwrap();
        test_segments(&mut mask, &slices);
    }
}
