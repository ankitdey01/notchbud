// motion.rs — pure math checks for state.rs springs/knots and render.rs hermite.
// Integration tests live here so the crate root stays window-free.

use notchbud::render::hermite;
use notchbud::state::*;

// knots must be strictly increasing and the table must end held at 0
#[test]
fn close_knots_shape() {
    for w in CLOSE_KNOTS.windows(2) {
        assert!(w[1].0 > w[0].0, "knot times not strictly increasing");
    }
    let last = CLOSE_KNOTS.last().unwrap();
    assert_eq!(last.1, 0.0, "close must end held at idle (v=0)");
}

#[test]
fn close_progress_at_knots() {
    for (t, v) in CLOSE_KNOTS {
        assert!((close_progress(t) - v).abs() < 1e-6, "at t={t}");
    }
    assert_eq!(close_progress(-1.0), 1.0); // clamp before start
    assert_eq!(close_progress(10.0), 0.0); // clamp after end
}

#[test]
fn springs_start_at_rest_end_settled() {
    for (resp, zeta) in [
        (SWELL_RESPONSE, SWELL_ZETA),
        (OPEN_RESPONSE, OPEN_ZETA),
        (0.21, 1.0), // critically damped branch
    ] {
        let p = spring_progress(resp, zeta, 0.0);
        assert!(p.abs() < 1e-9, "progress must start at 0");
        let v = spring_velocity(resp, zeta, 0.0);
        assert!(v.abs() < 1e-6, "starts at rest");
        let s = spring_settle(resp, zeta);
        assert!(s.is_finite() && s > 0.0);
        // envelope bound is c·1e-3 at exactly settle; tolerate 2e-3 for f32
        assert!(
            (spring_progress(resp, zeta, s) - 1.0).abs() < 2e-3,
            "settled ~1"
        );
    }
}

#[test]
fn hermite_pinned_to_values_and_slopes() {
    let [d, c, b, a] = hermite(0.0, 1.0, 0.0, 1.0, 0.0, 1.0);
    // f(x) = a·x³ + b·x² + c·x + d pins value and slope at both ends:
    // f(0)=d=v0, f'(0)=c=m0, f(1)=a+b+c+d=v1, f'(1)=3a+2b+c=m1
    let g = (a + b + c + d - 1.0).abs();
    let g1 = (3.0 * a + 2.0 * b + c - 1.0).abs();
    assert!(d.abs() < 1e-6 && c.abs() < 1e-6 && g < 1e-6 && g1 < 1e-6);
}
