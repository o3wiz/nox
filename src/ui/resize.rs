//! Pure filter-pane resize geometry, full-collapse bounds, and smooth frame-paced motion.
use nox::config::{
    DEFAULT_FILTER_RESIZE_SENSITIVITY, MAX_FILTER_RESIZE_SENSITIVITY, MIN_FILTER_RESIZE_SENSITIVITY,
};
use std::time::{Duration, Instant};

/// Height of the editable file path strip pinned above the source log pane.
pub const FILE_PATH_BAR_HEIGHT: f32 = 28.0;

/// Height of the horizontal split sash between the source log and search/filter dock.
pub const RESIZE_SASH_HEIGHT: f32 = 8.0;

/// Base step (in logical pixels) applied on a single tap or button click.
pub const BASE_RESIZE_STEP: f32 = 16.0;

/// Continuous hold duration before smooth long-press velocity glide auto-engages.
pub const LONG_PRESS_DELAY: Duration = Duration::from_millis(220);

/// Initial continuous glide velocity (px/sec) when long-press mode engages.
pub const MIN_HOLD_VELOCITY: f32 = 220.0;

/// Smooth exponential acceleration gradient (1/sec) applied while holding:
/// `velocity(s) = (MIN_HOLD_VELOCITY * (HOLD_ACCEL_RATE * s).exp()).min(MAX_HOLD_VELOCITY)`.
pub const HOLD_ACCEL_RATE: f32 = 1.32;

/// Maximum continuous glide velocity (px/sec) during sustained holds.
pub const MAX_HOLD_VELOCITY: f32 = 1100.0;

/// Frame-rate-independent exponential ease-out rate (1/sec) for smoothing height transitions.
pub const SMOOTH_FOLLOW_RATE: f32 = 22.0;

/// Snap threshold (px) below which rendered height locks onto target and stops animating.
pub const SETTLE_EPSILON: f32 = 0.35;

/// Visibility threshold (px) for hiding the filter pane at the bottom or the main log at the top.
pub const HIDE_THRESHOLD: f32 = 0.5;

/// Safety timeouts so a missed key-up event can never leave a hold active indefinitely.
pub const INITIAL_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(900);
pub const REPEAT_HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(300);

const MAX_FRAME_DT_SECS: f32 = 0.05;

const HEADER_HEIGHT: f32 = 46.0;
const SEARCH_BAR_HEIGHT: f32 = 44.0;
const RESULTS_HEADER_HEIGHT: f32 = 30.0;
const STATUS_BAR_HEIGHT: f32 = 26.0;
const ERROR_BANNER_HEIGHT: f32 = 40.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeDirection {
    Grow,
    Shrink,
}

impl ResizeDirection {
    pub fn sign(self) -> f32 {
        match self {
            Self::Grow => 1.0,
            Self::Shrink => -1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResultsResizeDrag {
    pub start_y: f32,
    pub start_height: f32,
}

/// Computes `(0.0, max_height)` for the filter results body inside the current viewport.
/// - `0.0` completely hides the filter results body (giving all space to the main log pane).
/// - `max_height` completely hides the main log pane while keeping the file path bar visible
///   above the split sash (giving all remaining space to the filter results body).
pub fn results_height_bounds(
    viewport_height: f32,
    show_status_bar: bool,
    has_error: bool,
) -> (f32, f32) {
    let mut fixed_chrome = HEADER_HEIGHT
        + FILE_PATH_BAR_HEIGHT
        + RESIZE_SASH_HEIGHT
        + SEARCH_BAR_HEIGHT
        + RESULTS_HEADER_HEIGHT;
    if show_status_bar {
        fixed_chrome += STATUS_BAR_HEIGHT;
    }
    if has_error {
        fixed_chrome += ERROR_BANNER_HEIGHT;
    }
    let max_h = (viewport_height - fixed_chrome).max(0.0);
    (0.0, max_h)
}

pub fn is_results_hidden(effective_height: f32) -> bool {
    effective_height <= HIDE_THRESHOLD
}

pub fn is_main_hidden(effective_height: f32, max_height: f32) -> bool {
    max_height > HIDE_THRESHOLD && (max_height - effective_height) <= HIDE_THRESHOLD
}

/// Maps a pointer Y coordinate during an active sash drag into a clamped results-body height.
/// Moving the pointer upward (`pointer_y < drag.start_y`) grows the filter window.
pub fn height_for_drag(drag: ResultsResizeDrag, pointer_y: f32, bounds: (f32, f32)) -> f32 {
    let delta_y = drag.start_y - pointer_y;
    (drag.start_height + delta_y)
        .round()
        .clamp(bounds.0, bounds.1)
}

fn sanitize_sensitivity(sensitivity: f32) -> f32 {
    if sensitivity.is_finite() {
        sensitivity.clamp(MIN_FILTER_RESIZE_SENSITIVITY, MAX_FILTER_RESIZE_SENSITIVITY)
    } else {
        DEFAULT_FILTER_RESIZE_SENSITIVITY
    }
}

#[derive(Clone, Copy, Debug)]
struct HoldState {
    direction: ResizeDirection,
    sensitivity: f32,
    started_at: Instant,
    glide_started_at: Option<Instant>,
    last_heartbeat: Instant,
    saw_repeat: bool,
}

impl HoldState {
    fn velocity(&self, now: Instant) -> f32 {
        let Some(glide_start) = self.glide_started_at else {
            return 0.0;
        };
        let secs = now.saturating_duration_since(glide_start).as_secs_f32();
        (MIN_HOLD_VELOCITY * (HOLD_ACCEL_RATE * secs).exp()).min(MAX_HOLD_VELOCITY)
            * self.sensitivity
    }

    fn heartbeat_expired(&self, now: Instant) -> bool {
        let timeout = if self.saw_repeat {
            REPEAT_HEARTBEAT_TIMEOUT
        } else {
            INITIAL_HEARTBEAT_TIMEOUT
        };
        now.saturating_duration_since(self.last_heartbeat) > timeout
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStep {
    pub changed: bool,
    pub keep_animating: bool,
}

/// Coordinates smooth frame-paced height transitions and automatic continuous
/// long-press velocity-gradient acceleration.
#[derive(Clone, Debug, Default)]
pub struct ResizeMotion {
    hold: Option<HoldState>,
    last_frame: Option<Instant>,
    animating: bool,
}

impl ResizeMotion {
    /// Handles a keyboard resize command trigger.
    ///
    /// - Initial press (or direction reversal): bumps `target` by `BASE_RESIZE_STEP * sensitivity`
    ///   and arms hold detection with the same sensitivity multiplier for continuous glide speed.
    /// - Subsequent OS repeats while still held: refreshes the hold heartbeat and engages continuous
    ///   smooth glide immediately (without injecting discrete stepped jumps).
    pub fn on_key_trigger(
        &mut self,
        direction: ResizeDirection,
        current: f32,
        target: &mut f32,
        bounds: (f32, f32),
        sensitivity: f32,
        now: Instant,
    ) -> bool {
        let sensitivity = sanitize_sensitivity(sensitivity);
        if let Some(hold) = &mut self.hold
            && hold.direction == direction
            && !hold.heartbeat_expired(now)
        {
            hold.sensitivity = sensitivity;
            hold.last_heartbeat = now;
            hold.saw_repeat = true;
            hold.glide_started_at.get_or_insert(now);
            self.last_frame.get_or_insert(now);
            self.animating = true;
            return true;
        }

        let base = if self.animating {
            target.clamp(bounds.0, bounds.1)
        } else {
            current.clamp(bounds.0, bounds.1)
        };
        *target =
            (base + direction.sign() * BASE_RESIZE_STEP * sensitivity).clamp(bounds.0, bounds.1);
        self.hold = Some(HoldState {
            direction,
            sensitivity,
            started_at: now,
            glide_started_at: None,
            last_heartbeat: now,
            saw_repeat: false,
        });
        self.last_frame = Some(now);
        self.animating = true;
        true
    }

    /// Applies a single deterministic step (e.g. header button click) with smooth easing.
    pub fn step_once(
        &mut self,
        direction: ResizeDirection,
        current: f32,
        target: &mut f32,
        bounds: (f32, f32),
        now: Instant,
    ) {
        self.hold = None;
        let base = if self.animating {
            target.clamp(bounds.0, bounds.1)
        } else {
            current.clamp(bounds.0, bounds.1)
        };
        *target = (base + direction.sign() * BASE_RESIZE_STEP).clamp(bounds.0, bounds.1);
        self.last_frame = Some(now);
        self.animating = true;
    }

    /// Smoothly transitions toward an explicit target height (e.g. double-click reset).
    pub fn animate_to(&mut self, desired: f32, target: &mut f32, bounds: (f32, f32), now: Instant) {
        self.hold = None;
        *target = desired.clamp(bounds.0, bounds.1);
        self.last_frame = Some(now);
        self.animating = true;
    }

    /// Ends an active keyboard hold while letting any remaining ease-out settle smoothly.
    pub fn release_hold(&mut self) {
        self.hold = None;
    }

    /// Immediately stops both hold glide and interpolation (e.g. when starting a manual drag).
    pub fn cancel(&mut self) {
        self.hold = None;
        self.last_frame = None;
        self.animating = false;
    }

    pub fn is_animating(&self) -> bool {
        self.animating || self.hold.is_some()
    }

    #[cfg(test)]
    pub fn is_long_pressing(&self) -> bool {
        self.hold
            .is_some_and(|hold| hold.glide_started_at.is_some())
    }

    /// Advances the continuous velocity-gradient hold and exponential ease-out by one display frame.
    pub fn advance_frame(
        &mut self,
        current: &mut f32,
        target: &mut f32,
        bounds: (f32, f32),
        now: Instant,
        reduce_motion: bool,
    ) -> FrameStep {
        let prev_current = *current;
        let clamped_target = target.clamp(bounds.0, bounds.1);
        *target = clamped_target;
        *current = current.clamp(bounds.0, bounds.1);

        let dt = self
            .last_frame
            .map_or(1.0 / 60.0, |prev| {
                now.saturating_duration_since(prev).as_secs_f32()
            })
            .min(MAX_FRAME_DT_SECS);
        self.last_frame = Some(now);

        let mut holding_with_room = false;
        if let Some(mut hold) = self.hold {
            if hold.heartbeat_expired(now) {
                self.hold = None;
            } else {
                if hold.glide_started_at.is_none()
                    && now.saturating_duration_since(hold.started_at) >= LONG_PRESS_DELAY
                {
                    hold.glide_started_at = Some(now);
                }
                if hold.glide_started_at.is_some() && dt > 0.0 {
                    let speed = hold.velocity(now);
                    *target =
                        (*target + hold.direction.sign() * speed * dt).clamp(bounds.0, bounds.1);
                }
                let at_limit = match hold.direction {
                    ResizeDirection::Grow => *target >= bounds.1,
                    ResizeDirection::Shrink => *target <= bounds.0,
                };
                holding_with_room = !at_limit;
                self.hold = Some(hold);
            }
        }

        let diff = *target - *current;
        if reduce_motion || diff.abs() <= SETTLE_EPSILON {
            *current = *target;
        } else if dt > 0.0 {
            let alpha = 1.0 - (-dt * SMOOTH_FOLLOW_RATE).exp();
            *current = (*current + diff * alpha).clamp(bounds.0, bounds.1);
            if (*target - *current).abs() <= SETTLE_EPSILON {
                *current = *target;
            }
        }

        let settled = (*target - *current).abs() <= SETTLE_EPSILON;
        let keep_animating = !settled || holding_with_room;
        self.animating = keep_animating;
        if !keep_animating {
            self.last_frame = None;
        }

        FrameStep {
            changed: (*current - prev_current).abs() > 0.01,
            keep_animating,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_height_bounds_allow_full_collapse_to_bottom_and_stop_at_file_path_bar() {
        // Default 850px window with status bar and no error banner:
        // fixed chrome = 46 + 28 + 8 + 44 + 30 + 26 = 182; max = 668.
        assert_eq!(results_height_bounds(850.0, true, false), (0.0, 668.0));
        // Without status bar (+26px room).
        assert_eq!(results_height_bounds(850.0, false, false), (0.0, 694.0));
        // With error banner (-40px room).
        assert_eq!(results_height_bounds(850.0, true, true), (0.0, 628.0));
        // Minimum supported window height (480px): 480 - 182 = 298.
        assert_eq!(results_height_bounds(480.0, true, false), (0.0, 298.0));
        // Cramped viewport never goes negative.
        assert_eq!(results_height_bounds(100.0, true, false), (0.0, 0.0));

        assert!(is_results_hidden(0.0));
        assert!(!is_results_hidden(16.0));
        assert!(is_main_hidden(668.0, 668.0));
        assert!(!is_main_hidden(652.0, 668.0));
        assert!(!is_main_hidden(0.0, 0.0));
    }

    #[test]
    fn drag_calculation_grows_upward_shrinks_downward_and_clamps_to_zero_and_max() {
        let drag = ResultsResizeDrag {
            start_y: 500.0,
            start_height: 250.0,
        };
        let bounds = (0.0, 668.0);
        // Dragging up by 120px grows height from 250 -> 370.
        assert_eq!(height_for_drag(drag, 380.0, bounds), 370.0);
        // Dragging down by 100px shrinks height from 250 -> 150.
        assert_eq!(height_for_drag(drag, 600.0, bounds), 150.0);
        // Dragging to top hides main pane below file path bar (668), dragging to bottom hides filter pane (0).
        assert_eq!(height_for_drag(drag, -200.0, bounds), 668.0);
        assert_eq!(height_for_drag(drag, 950.0, bounds), 0.0);
    }

    #[test]
    fn single_tap_eases_smoothly_to_exact_base_step_and_stops() {
        let mut motion = ResizeMotion::default();
        let bounds = (0.0, 668.0);
        let t0 = Instant::now();
        let mut current = 250.0;
        let mut target = 250.0;

        motion.on_key_trigger(
            ResizeDirection::Grow,
            current,
            &mut target,
            bounds,
            DEFAULT_FILTER_RESIZE_SENSITIVITY,
            t0,
        );
        assert_eq!(target, 250.0 + BASE_RESIZE_STEP);
        assert!(!motion.is_long_pressing());

        // Short release after 60ms: ease-out continues smoothly without engaging long-press glide.
        let mut t = t0;
        for frame in 1..=4 {
            t += Duration::from_millis(16);
            let step = motion.advance_frame(&mut current, &mut target, bounds, t, false);
            assert!(step.changed);
            assert!(current > 250.0 && current < target);
            if frame == 4 {
                motion.release_hold();
            }
        }
        assert!(!motion.is_long_pressing());

        // Subsequent frames settle cleanly onto exact target and stop requesting frames.
        let mut settled = false;
        for _ in 0..30 {
            t += Duration::from_millis(16);
            let step = motion.advance_frame(&mut current, &mut target, bounds, t, false);
            if !step.keep_animating {
                settled = true;
                break;
            }
        }
        assert!(settled);
        assert_eq!(current, 250.0 + BASE_RESIZE_STEP);
        assert!(!motion.is_animating());
    }

    #[test]
    fn sustained_hold_auto_detects_long_press_with_smooth_velocity_gradient() {
        let mut motion = ResizeMotion::default();
        let bounds = (0.0, 668.0);
        let t0 = Instant::now();
        let mut current = 100.0;
        let mut target = 100.0;

        motion.on_key_trigger(
            ResizeDirection::Grow,
            current,
            &mut target,
            bounds,
            DEFAULT_FILTER_RESIZE_SENSITIVITY,
            t0,
        );

        // Advance past LONG_PRESS_DELAY at 60Hz while keeping hold alive (no release).
        let mut t = t0;
        for _ in 0..14 {
            t += Duration::from_millis(16);
            motion.advance_frame(&mut current, &mut target, bounds, t, false);
        }
        assert!(
            motion.is_long_pressing(),
            "holding beyond LONG_PRESS_DELAY should auto-engage long-press glide"
        );

        // Measure per-frame displacement early vs later in the hold: velocity gradient ramps up.
        let before_early = current;
        t += Duration::from_millis(16);
        motion.advance_frame(&mut current, &mut target, bounds, t, false);
        let early_delta = current - before_early;
        assert!(early_delta > 0.5);

        // Refresh OS repeat heartbeat periodically while continuing to hold for ~480ms.
        for i in 0..30 {
            t += Duration::from_millis(16);
            if i % 3 == 0 {
                motion.on_key_trigger(
                    ResizeDirection::Grow,
                    current,
                    &mut target,
                    bounds,
                    DEFAULT_FILTER_RESIZE_SENSITIVITY,
                    t,
                );
            }
            motion.advance_frame(&mut current, &mut target, bounds, t, false);
        }
        let before_late = current;
        t += Duration::from_millis(16);
        motion.advance_frame(&mut current, &mut target, bounds, t, false);
        let late_delta = current - before_late;
        assert!(
            late_delta > early_delta * 1.4,
            "velocity gradient should accelerate smoothly (early={early_delta}, late={late_delta})"
        );

        // Releasing stops long press immediately and settles cleanly.
        motion.release_hold();
        assert!(!motion.is_long_pressing());
        let stopped_target = target;
        for _ in 0..30 {
            t += Duration::from_millis(16);
            let step = motion.advance_frame(&mut current, &mut target, bounds, t, false);
            if !step.keep_animating {
                break;
            }
        }
        assert_eq!(current, stopped_target);
        assert!(!motion.is_animating());
    }

    #[test]
    fn keyboard_sensitivity_scales_single_tap_and_long_press_velocity_proportionally() {
        let bounds = (0.0, 2000.0);
        let t0 = Instant::now();

        let simulate_hold = |sensitivity: f32| -> (f32, f32) {
            let mut motion = ResizeMotion::default();
            let mut current = 250.0;
            let mut target = 250.0;
            motion.on_key_trigger(
                ResizeDirection::Grow,
                current,
                &mut target,
                bounds,
                sensitivity,
                t0,
            );
            let tap_target = target - 250.0;

            let mut t = t0;
            // Advance past LONG_PRESS_DELAY and glide for ~320ms.
            for i in 0..35 {
                t += Duration::from_millis(16);
                if i >= 14 && i % 3 == 0 {
                    motion.on_key_trigger(
                        ResizeDirection::Grow,
                        current,
                        &mut target,
                        bounds,
                        sensitivity,
                        t,
                    );
                }
                motion.advance_frame(&mut current, &mut target, bounds, t, false);
            }
            let glide_displacement = target - (250.0 + tap_target);
            (tap_target, glide_displacement)
        };

        let (tap_half, glide_half) = simulate_hold(0.5);
        let (tap_one, glide_one) = simulate_hold(1.0);
        let (tap_double, glide_double) = simulate_hold(2.0);

        assert_eq!(tap_half, BASE_RESIZE_STEP * 0.5);
        assert_eq!(tap_one, BASE_RESIZE_STEP);
        assert_eq!(tap_double, BASE_RESIZE_STEP * 2.0);

        assert!(
            ((glide_half / glide_one) - 0.5).abs() < 1e-4,
            "0.5x sensitivity glide should be half of 1.0x (half={glide_half}, one={glide_one})"
        );
        assert!(
            ((glide_double / glide_one) - 2.0).abs() < 1e-4,
            "2.0x sensitivity glide should be double 1.0x (double={glide_double}, one={glide_one})"
        );

        // Minimum sensitivity (0.1) still steps cleanly and holds all the way to exact bounds.
        let mut motion = ResizeMotion::default();
        let tight_bounds = (0.0, 260.0);
        let mut current = 250.0;
        let mut target = 250.0;
        motion.on_key_trigger(
            ResizeDirection::Grow,
            current,
            &mut target,
            tight_bounds,
            MIN_FILTER_RESIZE_SENSITIVITY,
            t0,
        );
        assert!((target - (250.0 + BASE_RESIZE_STEP * 0.1)).abs() < 1e-4);
        let mut t = t0;
        for i in 0..60 {
            t += Duration::from_millis(16);
            if i % 3 == 0 {
                motion.on_key_trigger(
                    ResizeDirection::Grow,
                    current,
                    &mut target,
                    tight_bounds,
                    MIN_FILTER_RESIZE_SENSITIVITY,
                    t,
                );
            }
            let step = motion.advance_frame(&mut current, &mut target, tight_bounds, t, false);
            if !step.keep_animating {
                break;
            }
        }
        assert_eq!(target, tight_bounds.1);
        assert_eq!(current, tight_bounds.1);

        // Non-finite or out-of-range sensitivity values are defensively sanitized.
        let mut motion = ResizeMotion::default();
        let mut target = 250.0;
        motion.on_key_trigger(
            ResizeDirection::Grow,
            250.0,
            &mut target,
            bounds,
            f32::NAN,
            t0,
        );
        assert_eq!(target, 250.0 + BASE_RESIZE_STEP);
        motion.cancel();
        let mut target = 250.0;
        motion.on_key_trigger(ResizeDirection::Grow, 250.0, &mut target, bounds, 99.0, t0);
        assert_eq!(
            target,
            250.0 + BASE_RESIZE_STEP * MAX_FILTER_RESIZE_SENSITIVITY
        );

        // Button clicks (`step_once`) always apply the fixed base step.
        motion.cancel();
        let mut target = 250.0;
        motion.step_once(ResizeDirection::Grow, 250.0, &mut target, bounds, t0);
        assert_eq!(target, 250.0 + BASE_RESIZE_STEP);
    }
}
