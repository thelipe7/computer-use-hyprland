//! Direct uinput **absolute** pointer.
//!
//! ydotool's virtual device is relative-only (`EV=7`: SYN|KEY|REL), so its
//! `--absolute` is faked as "pin-to-corner + relative move", which the
//! compositor then distorts with pointer acceleration and fractional display
//! scaling — clicks land in the wrong place on multi-monitor / `HiDPI` setups.
//!
//! Here we create our own uinput device that exposes a true `ABS_X`/`ABS_Y`
//! axis whose range equals the **logical desktop size** (the same coordinate
//! space the portal screenshot reports). The compositor maps an absolute
//! device's axis range across the whole logical layout, so `ABS(x, y)` lands at
//! screenshot pixel `(x, y)` regardless of scaling — and with no approval
//! dialog (we already hold `/dev/uinput` access).

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType,
    UinputAbsSetup, uinput::VirtualDevice,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "pointer input may be clamped; inspect requested and emitted coordinates"]
pub(crate) struct PointerLanding {
    pub(crate) requested: (i32, i32),
    pub(crate) emitted: (i32, i32),
}

#[derive(Clone, Copy)]
struct AbsPointerGeometry {
    max_x: i32,
    max_y: i32,
}

impl AbsPointerGeometry {
    fn from_dimensions(width: i32, height: i32) -> Self {
        Self {
            max_x: width.max(1).saturating_sub(1),
            max_y: height.max(1).saturating_sub(1),
        }
    }

    fn axis_maxima(self) -> (i32, i32) {
        (self.max_x, self.max_y)
    }

    fn clamp_coordinates(self, x: i32, y: i32) -> (i32, i32) {
        (x.clamp(0, self.max_x), y.clamp(0, self.max_y))
    }

    /// A point one pixel away from `(x, y)`, or `None` when the desktop has
    /// no room to move. See [`AbsPointer::move_to`].
    fn neighbor_of(self, x: i32, y: i32) -> Option<(i32, i32)> {
        if self.max_x > 0 {
            Some((if x > 0 { x - 1 } else { x + 1 }, y))
        } else if self.max_y > 0 {
            Some((x, if y > 0 { y - 1 } else { y + 1 }))
        } else {
            None
        }
    }

    fn landing_for(self, x: i32, y: i32) -> PointerLanding {
        PointerLanding {
            requested: (x, y),
            emitted: self.clamp_coordinates(x, y),
        }
    }
}

pub struct AbsPointer {
    device: VirtualDevice,
    geometry: AbsPointerGeometry,
    /// The axis values the device currently holds, which start at the zero
    /// `AbsInfo` was built with. The kernel drops an `EV_ABS` event whose
    /// value equals the current one, so this is what tells `move_to` that a
    /// move has to be nudged through.
    position: (i32, i32),
}

impl AbsPointer {
    /// Create the absolute pointer sized to the logical desktop `width`×`height`
    /// (the portal screenshot dimensions). Blocks ~`settle` ms so libinput picks
    /// the device up before the first event.
    pub fn create(width: i32, height: i32) -> Result<Self> {
        let geometry = AbsPointerGeometry::from_dimensions(width, height);
        let (max_x, max_y) = geometry.axis_maxima();
        // value, min, max, fuzz, flat, resolution. resolution=1 unit/px.
        let abs_x =
            UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, AbsInfo::new(0, 0, max_x, 0, 0, 1));
        let abs_y =
            UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, AbsInfo::new(0, 0, max_y, 0, 0, 1));
        let keys =
            AttributeSet::from_iter([KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE]);
        // INPUT_PROP_DIRECT marks the device as a direct (absolute) pointer so
        // libinput maps its axes to screen coordinates rather than treating it
        // as a relative touchpad.
        let props = AttributeSet::from_iter([PropType::DIRECT]);

        let device = VirtualDevice::builder()
            .context("uinput builder (is /dev/uinput writable?)")?
            .name("computer-use-hyprland absolute pointer")
            .with_properties(&props)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .with_keys(&keys)?
            .build()
            .context("failed to create uinput absolute pointer device")?;

        // Give udev/libinput time to enumerate the new device.
        sleep(Duration::from_millis(500));

        Ok(Self {
            device,
            geometry,
            position: (0, 0),
        })
    }

    /// Move the pointer to absolute logical coordinates `(x, y)` and report
    /// both the requested point and the values emitted after edge clamping.
    ///
    /// A move to the point the previous move emitted is nudged through a
    /// neighboring pixel first. The kernel's input core drops an `EV_ABS`
    /// event whose value equals the axis's current value, so without the
    /// nudge the second of two moves to the same point emits nothing at all —
    /// and the pointer stays wherever the compositor warped it in between
    /// (focusing a window warps the cursor to its center), which is where the
    /// click that follows would land.
    pub fn move_to(&mut self, x: i32, y: i32) -> Result<PointerLanding> {
        let landing = self.geometry.landing_for(x, y);
        let (emitted_x, emitted_y) = landing.emitted;
        if (emitted_x, emitted_y) == self.position
            && let Some((nudge_x, nudge_y)) = self.geometry.neighbor_of(emitted_x, emitted_y)
        {
            self.emit_absolute(nudge_x, nudge_y)?;
        }
        self.emit_absolute(emitted_x, emitted_y)?;
        Ok(landing)
    }

    fn emit_absolute(&mut self, x: i32, y: i32) -> Result<()> {
        self.device
            .emit(&[
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
            ])
            .context("failed to emit absolute motion")?;
        self.position = (x, y);
        Ok(())
    }

    /// Move to `(x, y)` then press+release `button` `count` times.
    pub fn click(
        &mut self,
        x: i32,
        y: i32,
        button: PointerButton,
        count: u32,
    ) -> Result<PointerLanding> {
        let landing = self.move_to(x, y)?;
        sleep(Duration::from_millis(30));
        let code = button.key_code();
        for _ in 0..count.max(1) {
            self.device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])?;
            sleep(Duration::from_millis(30));
            self.device
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])?;
            sleep(Duration::from_millis(40));
        }
        Ok(landing)
    }

    /// Press at `start`, travel to `end` in small steps, release — a drag
    /// with `button`, reporting where each end landed and how many steps it
    /// took.
    ///
    /// The travel is stepped rather than jumped because a component that
    /// reacts to the first movement while still under the pointer — a title
    /// bar handing its window to the compositor, a list reordering itself —
    /// never sees a single jump whose destination lies outside it.
    pub fn drag(
        &mut self,
        start: (i32, i32),
        end: (i32, i32),
        button: PointerButton,
    ) -> Result<DragLanding> {
        let code = button.key_code();
        let start_landing = self.move_to(start.0, start.1)?;
        sleep(Duration::from_millis(30));
        self.device
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])?;
        sleep(Duration::from_millis(40));
        let end_landing = self.geometry.landing_for(end.0, end.1);
        let steps = drag_step_count(start_landing.emitted, end_landing.emitted);
        for step in 1..=steps {
            let (step_x, step_y) =
                drag_point_at(start_landing.emitted, end_landing.emitted, step, steps);
            self.emit_absolute(step_x, step_y)?;
            sleep(DRAG_STEP_PAUSE);
        }
        sleep(Duration::from_millis(40));
        self.device
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])?;
        Ok(DragLanding {
            start: start_landing,
            end: end_landing,
            steps,
        })
    }
}

/// Where both ends of a drag landed, and how many moves carried the pointer
/// between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "drag input may be clamped; inspect requested and emitted coordinates"]
pub(crate) struct DragLanding {
    pub(crate) start: PointerLanding,
    pub(crate) end: PointerLanding,
    pub(crate) steps: u32,
}

/// Desktop pixels per drag step, and the ceiling on how many steps one drag
/// spends: a long drag stays under `DRAG_MAX_STEPS * DRAG_STEP_PAUSE`.
const DRAG_STEP_PIXELS: i32 = 8;
const DRAG_MAX_STEPS: u32 = 40;
const DRAG_STEP_PAUSE: Duration = Duration::from_millis(10);

/// How many moves a drag from `start` to `end` is split into: one per
/// `DRAG_STEP_PIXELS` along its longer axis, at least one and at most
/// `DRAG_MAX_STEPS`.
fn drag_step_count(start: (i32, i32), end: (i32, i32)) -> u32 {
    let distance = (i64::from(end.0) - i64::from(start.0))
        .unsigned_abs()
        .max((i64::from(end.1) - i64::from(start.1)).unsigned_abs());
    let steps = distance.div_ceil(DRAG_STEP_PIXELS.unsigned_abs().into());
    u32::try_from(steps)
        .unwrap_or(DRAG_MAX_STEPS)
        .clamp(1, DRAG_MAX_STEPS)
}

/// The point `step` of `steps` along the way from `start` to `end`. The last
/// step is `end` itself, so a drag always releases where it was asked to.
fn drag_point_at(start: (i32, i32), end: (i32, i32), step: u32, steps: u32) -> (i32, i32) {
    if step >= steps {
        return end;
    }
    let along = |from: i32, to: i32| {
        let traveled =
            (i64::from(to) - i64::from(from)) * i64::from(step) / i64::from(steps.max(1));
        i32::try_from(i64::from(from) + traveled).unwrap_or(to)
    };
    (along(start.0, end.0), along(start.1, end.1))
}

/// Pointer buttons we can synthesize.
#[derive(Clone, Copy, Debug)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

impl PointerButton {
    pub fn from_name(name: Option<&str>) -> Option<Self> {
        match name.unwrap_or("left").to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "middle" => Some(Self::Middle),
            _ => None,
        }
    }

    fn key_code(self) -> u16 {
        match self {
            Self::Left => KeyCode::BTN_LEFT.0,
            Self::Right => KeyCode::BTN_RIGHT.0,
            Self::Middle => KeyCode::BTN_MIDDLE.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AbsPointerGeometry, DRAG_MAX_STEPS, DRAG_STEP_PIXELS, PointerButton, drag_point_at,
        drag_step_count,
    };

    #[test]
    fn axis_range_ends_at_last_desktop_pixel() {
        let geometry = AbsPointerGeometry::from_dimensions(1920, 1080);

        assert_eq!(geometry.axis_maxima(), (1919, 1079));
    }

    #[test]
    fn pointer_landing_preserves_the_request_and_emitted_coordinates() {
        let geometry = AbsPointerGeometry::from_dimensions(1920, 1080);

        for (requested, emitted) in [
            ((640, 480), (640, 480)),
            ((1920, 1080), (1919, 1079)),
            ((-1, -1), (0, 0)),
            ((i32::MAX, i32::MAX), (1919, 1079)),
        ] {
            let landing = geometry.landing_for(requested.0, requested.1);
            assert_eq!(landing.requested, requested);
            assert_eq!(landing.emitted, emitted);
        }
    }

    #[test]
    fn a_repeated_point_has_a_neighbor_to_be_nudged_through() {
        let geometry = AbsPointerGeometry::from_dimensions(1920, 1080);

        // The kernel drops an EV_ABS event that repeats the current value, so
        // every point needs somewhere one pixel away to be moved through.
        for point in [(0, 0), (960, 540), (1919, 1079)] {
            let neighbor = geometry
                .neighbor_of(point.0, point.1)
                .expect("a desktop wider than one pixel always has a neighbor");
            assert_ne!(neighbor, point);
            assert_eq!(geometry.clamp_coordinates(neighbor.0, neighbor.1), neighbor);
        }
    }

    #[test]
    fn a_single_pixel_desktop_has_nowhere_to_nudge_through() {
        let geometry = AbsPointerGeometry::from_dimensions(1, 1);

        assert_eq!(geometry.neighbor_of(0, 0), None);
    }

    #[test]
    fn a_drag_takes_one_step_per_eight_pixels_of_its_longer_axis() {
        assert_eq!(drag_step_count((100, 100), (100, 100)), 1);
        assert_eq!(drag_step_count((100, 100), (104, 100)), 1);
        assert_eq!(
            drag_step_count((100, 100), (100 + DRAG_STEP_PIXELS * 10, 140)),
            10
        );
        assert_eq!(drag_step_count((0, 0), (0, -80)), 10);
    }

    #[test]
    fn a_long_drag_stays_within_the_step_ceiling() {
        assert_eq!(drag_step_count((0, 0), (10_000, 0)), DRAG_MAX_STEPS);
        assert_eq!(
            drag_step_count((i32::MIN, 0), (i32::MAX, 0)),
            DRAG_MAX_STEPS
        );
    }

    #[test]
    fn a_drag_walks_from_its_start_to_exactly_its_end() {
        let (start, end) = ((300, 16), (420, 96));
        let steps = drag_step_count(start, end);
        assert!(steps > 1, "a 120-pixel drag is more than one jump");

        let points = (1..=steps)
            .map(|step| drag_point_at(start, end, step, steps))
            .collect::<Vec<_>>();

        assert_eq!(*points.last().expect("at least one step"), end);
        assert!(points.iter().all(|point| {
            (start.0..=end.0).contains(&point.0) && (start.1..=end.1).contains(&point.1)
        }));
        for pair in points.windows(2) {
            assert!(
                (pair[1].0 - pair[0].0).abs() <= DRAG_STEP_PIXELS
                    && (pair[1].1 - pair[0].1).abs() <= DRAG_STEP_PIXELS,
                "no step jumps further than {DRAG_STEP_PIXELS} pixels: {pair:?}"
            );
        }
    }

    #[test]
    fn unsupported_buttons_fall_through_to_other_backends() {
        assert!(matches!(
            PointerButton::from_name(None),
            Some(PointerButton::Left)
        ));
        assert!(matches!(
            PointerButton::from_name(Some("right")),
            Some(PointerButton::Right)
        ));
        assert!(matches!(
            PointerButton::from_name(Some("middle")),
            Some(PointerButton::Middle)
        ));

        for button in ["side", "extra", "forward", "back"] {
            assert!(
                PointerButton::from_name(Some(button)).is_none(),
                "{button} must fall through instead of becoming a left click"
            );
        }
    }
}
