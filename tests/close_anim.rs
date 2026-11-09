// tests/close_anim.rs — TDD file for the close-animation jank issue.
//
// Symptom: opening the island is buttery smooth, closing runs at very low
// FPS. Three suspects, all pinned here:
//
//  1. The programmed close curve itself. The compositor gives no readback,
//     so the test samples exactly what `animate_spring` programs (same
//     segments, same AddCubic mapping) at display rate and asserts the
//     close is no choppier than the open, starts at `from`, and ends at
//     idle. A transcription bug in the close branch (wrong t0 base,
//     swapped lo/hi, dropped hold segment) shows up as a RED jump here.
//  2. Sampling density vs the display. The curve only changes value at
//     segment boundaries: on a 144 Hz panel, 16 ms programming holds each
//     value ~2-3 frames then jumps. The density test asserts every run is
//     programmed finer than any common display, so each vsync gets fresh
//     motion instead of hold-then-jump.
//  3. Exit-event thrash. The WebView child is inset in the island, so a
//     downward exit crosses parent surface first: page mouseleave, then a
//     parent mousemove, then the true leave. If any leave-path collapses
//     while the cursor is still inside the bar, close restarts mid-flight
//     (close -> open -> close) and the shell visibly stutters. The routing
//     test replays that exact signal order and asserts one collapse, zero
//     re-expands.

use notchbud::render::{sample_shell_animation, SpringSeg};
use notchbud::state::*;

/// Evaluate the value the compositor plays for `axis` at time `t`,
/// mirroring the AddCubic dispatch in `animate_spring` exactly:
/// value = lo + delta*(d + c*s + b*s^2 + a*s^3) with s = t - t0 in
/// seconds, using the last segment whose begin time has passed.
fn eval_axis(segs: &[SpringSeg], axis: usize, t: f64) -> f32 {
    let mut active = &segs[0];
    for s in segs {
        if s.t0 <= t {
            active = s;
        } else {
            break;
        }
    }
    let delta = active.hi[axis] - active.lo[axis];
    let [d, c, b, q] = active.coefs;
    let s = (t - active.t0).max(0.0) as f32;
    active.lo[axis] + delta * (d + c * s + b * s * s + q * s * s * s)
}

/// Largest absolute per-frame step sampling the run at `rate` Hz.
fn max_step_at(segs: &[SpringSeg], settle: f32, rate: f32) -> f32 {
    let n = (settle * rate).ceil() as usize;
    let mut prev = [eval_axis(segs, 0, 0.0), eval_axis(segs, 1, 0.0)];
    let mut m = 0.0f32;
    for k in 1..=n {
        let t = (k as f32 / rate).min(settle) as f64;
        let cur = [eval_axis(segs, 0, t), eval_axis(segs, 1, t)];
        for a in 0..2 {
            m = m.max((cur[a] - prev[a]).abs());
        }
        prev = cur;
    }
    m
}

#[test]
fn close_curve_is_as_smooth_as_open() {
    // Settled open shell: the usual close start.
    let from = [1.0, 1.0];
    let (close_segs, close_settle) = sample_shell_animation(false, from);
    let (open_segs, open_settle) = sample_shell_animation(true, scale_idle());

    // Close starts where the shell is and ends at the idle tab.
    for a in 0..2 {
        assert!(
            (eval_axis(&close_segs, a, 0.0) - from[a]).abs() < 1e-4,
            "close must start at `from`"
        );
        assert!(
            (eval_axis(&close_segs, a, close_settle as f64) - scale_idle()[a]).abs() < 1e-3,
            "close must end at idle"
        );
    }

    // No jumps: the worst per-frame step of the close may not dwarf the
    // open's, at 60 Hz and at high-refresh rates alike. (A broken t0 base
    // or swapped lo/hi collapses the whole range into one frame and fails
    // here by an order of magnitude; coarse programming holds values across
    // whole vsyncs on a 144 Hz panel and fails the fast rate.)
    for &rate in &[60.0f32, 144.0] {
        let c = max_step_at(&close_segs, close_settle, rate);
        let o = max_step_at(&open_segs, open_settle, rate);
        assert!(
            c <= o * 1.5 + 1e-3,
            "rate {rate}: close max step {c} vs open {o}: close plays choppier"
        );
    }
}

#[test]
fn close_segments_cover_the_run() {
    let from = [1.0, 1.0];
    let (segs, settle) = sample_shell_animation(false, from);

    // The visible shrink starts after the content-fade hold.
    assert!(
        (segs[0].t0 - CLOSE_DELAY as f64).abs() < 1e-9,
        "first close segment must begin at CLOSE_DELAY"
    );
    // Begin times are ordered — the compositor must never jump backwards.
    for w in segs.windows(2) {
        assert!(w[1].t0 >= w[0].t0, "close segments out of order");
    }
    // Settle sits past the last segment, and the hold keeps the end pinned
    // at idle (no polynomial runaway past the fitted range).
    assert!(
        (settle - (CLOSE_DELAY + CLOSE_RECOVER_T + 0.06)).abs() < 1e-6,
        "close settle must sit past the recover"
    );
    let last = segs.last().unwrap();
    assert_eq!(last.coefs, [1.0, 0.0, 0.0, 0.0]);
    for a in 0..2 {
        assert!(
            (eval_axis(&segs, a, settle as f64) - scale_idle()[a]).abs() < 1e-3,
            "hold segment must pin the end at idle"
        );
    }
}

#[test]
fn open_curve_starts_at_from_and_settles_at_open() {
    let from = scale_idle();
    let (segs, settle) = sample_shell_animation(true, from);
    for a in 0..2 {
        assert!((eval_axis(&segs, a, 0.0) - from[a]).abs() < 1e-4);
        assert!((eval_axis(&segs, a, settle as f64) - 1.0).abs() < 2e-3);
    }
}

#[test]
fn shell_sampled_finer_than_any_common_display() {
    // The curve only changes value at segment boundaries: on a 144 Hz
    // panel, 16 ms programming holds each value ~2-3 frames then jumps.
    // Every run must be programmed finer than a 120 Hz vsync so each
    // compositor frame gets fresh motion instead of hold-then-jump.
    for expand in [true, false] {
        let from = if expand { scale_idle() } else { [1.0, 1.0] };
        let (segs, _) = sample_shell_animation(expand, from);
        let mut gap: f64 = 0.0;
        for w in segs.windows(2) {
            gap = gap.max(w[1].t0 - w[0].t0);
        }
        assert!(
            gap <= 1.0 / 120.0 + 1e-6,
            "expand={expand}: max segment begin gap {gap}s holds frames on fast panels"
        );
    }
}

#[test]
fn downward_exit_collapses_exactly_once() {
    // Replays a slow downward exit across the child boundary with the same
    // predicates the wndproc uses: page mouseleave (cursor on the parent
    // strip, inside the bar) -> parent mousemove -> parent leave (outside).
    // Buggy routing collapsed on the page leave, re-expanded on the move,
    // and collapsed again: close -> open -> close mid-gesture.
    let bar = [110.0, 0.0, 530.0, 140.4];
    let mut expanded = true;
    let mut mouse_inside = false; // parent saw a leave entering the child
    let mut collapses = 0;
    let mut expands = 0;

    // 1. page mouseleave, cursor on the parent strip (inside the bar).
    if page_leave_collapses(&bar, Some((120.0, 70.0))) {
        collapses += 1;
        expanded = false;
    } else {
        mouse_inside = false;
    }
    // 2. parent mousemove on the strip.
    if !mouse_inside {
        mouse_inside = true;
        if expand_on_mousemove(expanded) {
            expands += 1;
            expanded = true;
        }
    }
    // 3. parent leave, cursor outside the island.
    if page_leave_collapses(&bar, Some((600.0, 200.0))) {
        collapses += 1;
        expanded = false;
    }

    assert_eq!(collapses, 1, "exactly one collapse per exit");
    assert_eq!(expands, 0, "no re-expand while already expanded");
    assert!(!expanded);
    assert!(mouse_inside);
}

#[test]
fn leave_decisions_agree_on_boundaries() {
    let bar = [110.0, 0.0, 530.0, 140.4];
    // Inside (strip around an inset child) never collapses...
    assert!(!page_leave_collapses(&bar, Some((111.0, 1.0))));
    assert!(!page_leave_collapses(&bar, Some((529.9, 139.0))));
    // ...outside always does, and an unreadable position fails closed.
    assert!(page_leave_collapses(&bar, Some((530.0, 70.0))));
    assert!(page_leave_collapses(&bar, Some((300.0, 140.4))));
    assert!(page_leave_collapses(&bar, None));
    // Choreography only on closed -> open.
    assert!(expand_on_mousemove(false));
    assert!(!expand_on_mousemove(true));
}
