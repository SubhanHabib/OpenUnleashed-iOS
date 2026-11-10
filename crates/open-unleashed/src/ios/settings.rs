//! **PLATFORM GLUE (iOS)**: the on-screen controller's options, and the page that sets them.
//!
//! The game's own menus hold only the original's settings, so these live on a small iOS page of their own, opened
//! from the import screen or with a three-finger tap anywhere in the game (a race is paused first). They are kept
//! in `touch-controls.txt` in the game's data folder, one `key=value` a line.
//!
//! * **Steering**: the left stick, a d-pad (full lock left or right), the left stick as a touchpad (the finger's
//!   place on the pad, not its travel from where it landed), or tilting the device like a wheel.
//! * **Pedals**: the triggers (analogue, on the left) or the A / B buttons (on / off, on the right). With the
//!   buttons, X is the handbrake and Y resets the car.
//! * **Extra buttons**: X / Y, the shoulders (shift up / down) and the right stick (look left / right), each shown
//!   or not.
//!
//! The pad's own buttons stay what SDL reads; [`pad_filter`] rewrites the on-screen pad's snapshot to the chosen
//! layout before the bindings see it, so the game's bindings (Options › Controls) are not touched.

use nfs_input::gamepad::PadSnapshot;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{DefinedClass, Encode, Encoding, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString};
use objc2_ui_kit::{
    NSTextAlignment, UIButton, UIButtonConfiguration, UIButtonType, UIColor, UIControlEvents, UIFont, UILabel,
    UILayoutConstraintAxis, UIModalPresentationStyle, UIScrollView, UISegmentedControl, UISlider, UIStackView,
    UIStackViewAlignment, UISwitch, UIView, UIViewController,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// What SDL calls `GCVirtualController`'s pad.
pub const TOUCH_PAD_NAME: &str = "Apple Touch Controller";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Steering {
    Stick,
    DPad,
    Touchpad,
    Tilt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pedals {
    Triggers,
    Buttons,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchSettings {
    pub steering: Steering,
    pub pedals: Pedals,
    pub xy: bool,
    pub shoulders: bool,
    pub right_stick: bool,
    /// Tilt steering's sensitivity, 0.5…2 (1: full lock at [`TILT_LOCK_DEG`] of tilt).
    pub tilt_sensitivity: f32,
}

impl TouchSettings {
    pub const DEFAULT: Self =
        Self { steering: Steering::Stick, pedals: Pedals::Triggers, xy: true, shoulders: true, right_stick: false, tilt_sensitivity: 1.0 };

    fn parse(text: &str) -> Self {
        let mut s = Self::DEFAULT;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let on = v == "1" || v.eq_ignore_ascii_case("true");
            match k {
                "steering" => {
                    s.steering = match v {
                        "dpad" => Steering::DPad,
                        "touchpad" => Steering::Touchpad,
                        "tilt" => Steering::Tilt,
                        _ => Steering::Stick,
                    }
                }
                "pedals" => s.pedals = if v == "buttons" { Pedals::Buttons } else { Pedals::Triggers },
                "xy" => s.xy = on,
                "shoulders" => s.shoulders = on,
                "right_stick" => s.right_stick = on,
                "tilt_sensitivity" => {
                    if let Ok(f) = v.parse::<f32>() {
                        s.tilt_sensitivity = f.clamp(0.5, 2.0);
                    }
                }
                _ => {}
            }
        }
        s
    }

    fn text(&self) -> String {
        let steering = match self.steering {
            Steering::Stick => "stick",
            Steering::DPad => "dpad",
            Steering::Touchpad => "touchpad",
            Steering::Tilt => "tilt",
        };
        let pedals = match self.pedals {
            Pedals::Triggers => "triggers",
            Pedals::Buttons => "buttons",
        };
        format!(
            "steering={steering}\npedals={pedals}\nxy={}\nshoulders={}\nright_stick={}\ntilt_sensitivity={:.2}\n",
            self.xy as u8, self.shoulders as u8, self.right_stick as u8, self.tilt_sensitivity
        )
    }

    /// What each button does with these settings, for the page.
    fn legend(&self) -> String {
        let mut parts = vec![];
        match self.pedals {
            Pedals::Triggers => {
                parts.push("Right trigger: accelerate".to_owned());
                parts.push("Left trigger: brake / reverse".to_owned());
                parts.push("A: handbrake".to_owned());
                parts.push("B: look back".to_owned());
                if self.xy {
                    parts.push("X: reset the car".to_owned());
                    parts.push("Y: change camera".to_owned());
                }
            }
            Pedals::Buttons => {
                parts.push("A: accelerate".to_owned());
                parts.push("B: brake / reverse".to_owned());
                if self.xy {
                    parts.push("X: handbrake".to_owned());
                    parts.push("Y: reset the car".to_owned());
                }
            }
        }
        if self.shoulders {
            parts.push("Right / left shoulder: shift up / down".to_owned());
        }
        if self.right_stick {
            parts.push("Right stick: look left / right".to_owned());
        }
        parts.push("Two-finger tap: pause · Three-finger tap: this page".to_owned());
        parts.join("\n")
    }
}

/// The settings now in use.
static CURRENT: RwLock<TouchSettings> = RwLock::new(TouchSettings::DEFAULT);
/// Set when the settings changed; taken by [`take_changed`].
static CHANGED: AtomicBool = AtomicBool::new(false);
/// The settings page is up.
static OPEN: AtomicBool = AtomicBool::new(false);
/// Tilt steering's stick value, −32767…32767 (written by [`Tilt`], read by [`pad_filter`]).
static TILT: AtomicI32 = AtomicI32::new(0);

pub fn current() -> TouchSettings {
    CURRENT.read().map(|s| *s).unwrap_or(TouchSettings::DEFAULT)
}

pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// The settings changed since the last call.
pub fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::Relaxed)
}

fn path() -> PathBuf {
    nfs_assets::platform::data_dir().join("touch-controls.txt")
}

/// Reads the saved settings and puts the pad filter in place; once, at start.
pub fn init() {
    let s = std::fs::read_to_string(path()).map(|t| TouchSettings::parse(&t)).unwrap_or(TouchSettings::DEFAULT);
    tracing::info!(?s, "touch controls");
    if let Ok(mut c) = CURRENT.write() {
        *c = s;
    }
    nfs_input::gamepad::set_pad_filter(pad_filter);
}

fn store(s: TouchSettings) {
    if let Ok(mut c) = CURRENT.write() {
        if *c == s {
            return;
        }
        *c = s;
    }
    CHANGED.store(true, Ordering::Relaxed);
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&p, s.text()) {
        tracing::warn!(error = %e, path = %p.display(), "touch controls not saved");
    }
}

/// The on-screen pad's snapshot, rewritten to the chosen layout (see the module's text). Other pads are left as
/// they are.
fn pad_filter(name: &str, p: &mut PadSnapshot) {
    if name != TOUCH_PAD_NAME {
        return;
    }
    let s = current();
    match s.steering {
        Steering::DPad => {
            p.sticks[0] = if p.dpad[1] && !p.dpad[3] {
                i16::MAX
            } else if p.dpad[3] && !p.dpad[1] {
                -i16::MAX
            } else {
                0
            };
            p.sticks[1] = 0;
            p.dpad = [false; 4];
        }
        Steering::Tilt => {
            p.sticks[0] = TILT.load(Ordering::Relaxed).clamp(-32767, 32767) as i16;
            p.sticks[1] = 0;
        }
        Steering::Stick | Steering::Touchpad => {}
    }
    if s.pedals == Pedals::Buttons {
        // `BUTTON_ORDER`: 0 A, 1 B, 2 Y, 3 X
        let [a, b, y, x] = [p.buttons[0], p.buttons[1], p.buttons[2], p.buttons[3]];
        p.triggers = [if b { i16::MAX } else { 0 }, if a { i16::MAX } else { 0 }];
        p.buttons[0] = x;
        p.buttons[1] = false;
        p.buttons[2] = false;
        p.buttons[3] = y;
    }
}

/// How far the device turns for full lock at sensitivity 1, and the dead zone around level.
pub const TILT_LOCK_DEG: f32 = 35.0;
const TILT_DEAD_DEG: f32 = 2.0;

/// The steering, −1…1, for the gravity `(gx, gy)` in the device's own axes, the device held in landscape like a
/// wheel. Either landscape: the sign of `gx` says which way up it is.
fn tilt_steer(gx: f64, gy: f64, sensitivity: f32) -> f32 {
    if gx * gx + gy * gy < 0.1 {
        // flat on its back: no way to tell
        return 0.0;
    }
    let s = if gx < 0.0 { -1.0 } else { 1.0 };
    let deg = (s * gy).atan2(gx.abs()).to_degrees() as f32;
    let lock = TILT_LOCK_DEG / sensitivity.clamp(0.5, 2.0);
    let past = (deg.abs() - TILT_DEAD_DEG).max(0.0) / (lock - TILT_DEAD_DEG);
    past.min(1.0).copysign(deg)
}

#[link(name = "CoreMotion", kind = "framework")]
unsafe extern "C" {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CMAcceleration {
    x: f64,
    y: f64,
    z: f64,
}

// SAFETY: CoreMotion's `typedef struct { double x, y, z; } CMAcceleration`
unsafe impl Encode for CMAcceleration {
    const ENCODING: Encoding = Encoding::Struct("?", &[f64::ENCODING, f64::ENCODING, f64::ENCODING]);
}

/// CoreMotion's device motion, read while tilt steering is in use.
#[derive(Default)]
pub struct Tilt {
    manager: Option<Retained<AnyObject>>,
    on: bool,
    /// The device has no motion sensors (the simulator): not asked again.
    unavailable: bool,
}

impl Tilt {
    /// One look, between frames: `want` is whether tilt steering is in use now.
    pub fn update(&mut self, want: bool) {
        if want != self.on {
            self.set(want);
        }
        if !self.on {
            TILT.store(0, Ordering::Relaxed);
            return;
        }
        let Some(m) = &self.manager else { return };
        // SAFETY: `-[CMMotionManager deviceMotion]` and `-[CMDeviceMotion gravity]`, on the thread that started them
        let g: Option<CMAcceleration> = unsafe {
            let motion: Option<Retained<AnyObject>> = msg_send![&**m, deviceMotion];
            motion.map(|d| msg_send![&*d, gravity])
        };
        let Some(g) = g else { return };
        let v = tilt_steer(g.x, g.y, current().tilt_sensitivity);
        TILT.store((v * 32767.0) as i32, Ordering::Relaxed);
    }

    fn set(&mut self, on: bool) {
        if self.unavailable {
            return;
        }
        if self.manager.is_none() {
            let Some(cls) = AnyClass::get(c"CMMotionManager") else {
                tracing::warn!("CoreMotion is not available: no tilt steering");
                self.unavailable = true;
                return;
            };
            // SAFETY: `+[CMMotionManager new]`
            self.manager = Some(unsafe { msg_send![cls, new] });
        }
        let Some(m) = &self.manager else { return };
        // SAFETY: the motion manager's documented start / stop, on the main thread
        unsafe {
            if on {
                let available: bool = msg_send![&**m, isDeviceMotionAvailable];
                if !available {
                    tracing::warn!("no device motion on this device: no tilt steering");
                    self.unavailable = true;
                    return;
                }
                let _: () = msg_send![&**m, setDeviceMotionUpdateInterval: 1.0f64 / 60.0];
                let _: () = msg_send![&**m, startDeviceMotionUpdates];
            } else {
                let _: () = msg_send![&**m, stopDeviceMotionUpdates];
            }
        }
        tracing::info!(on, "tilt steering");
        self.on = on;
    }
}

// ---- the page ----

const STEERING: [(&str, Steering); 4] =
    [("Stick", Steering::Stick), ("D-pad", Steering::DPad), ("Touchpad", Steering::Touchpad), ("Tilt", Steering::Tilt)];
const PEDALS: [(&str, Pedals); 2] = [("Triggers (analogue)", Pedals::Triggers), ("A / B buttons", Pedals::Buttons)];

struct Controls {
    vc: Retained<UIViewController>,
    steering: Retained<UISegmentedControl>,
    pedals: Retained<UISegmentedControl>,
    xy: Retained<UISwitch>,
    shoulders: Retained<UISwitch>,
    right_stick: Retained<UISwitch>,
    tilt: Retained<UISlider>,
    tilt_row: Retained<UIStackView>,
    legend: Retained<UILabel>,
}

pub struct Ivars {
    controls: RefCell<Option<Controls>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Page` does not implement `Drop`
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OUTouchSettings"]
    #[ivars = Ivars]
    pub struct Page;

    impl Page {
        #[unsafe(method(changed:))]
        fn changed(&self, _sender: Option<&AnyObject>) {
            let Some(s) = self.read() else { return };
            store(s);
            self.show(s);
        }

        #[unsafe(method(done:))]
        fn done(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.ivars().controls.borrow().as_ref() {
                c.vc.dismissViewControllerAnimated_completion(true, None);
            }
            OPEN.store(false, Ordering::Relaxed);
            tracing::info!(s = ?current(), "touch controls page closed");
            PAGE.with(|p| p.borrow_mut().take());
        }
    }

    unsafe impl NSObjectProtocol for Page {}
);

thread_local! {
    /// The page while it is up (it holds its own controls; UIKit holds the target only weakly).
    static PAGE: RefCell<Option<Retained<Page>>> = const { RefCell::new(None) };
}

impl Page {
    fn read(&self) -> Option<TouchSettings> {
        let c = self.ivars().controls.borrow();
        let c = c.as_ref()?;
        let pick = |seg: &UISegmentedControl| seg.selectedSegmentIndex().max(0) as usize;
        Some(TouchSettings {
            steering: STEERING.get(pick(&c.steering)).map_or(Steering::Stick, |s| s.1),
            pedals: PEDALS.get(pick(&c.pedals)).map_or(Pedals::Triggers, |p| p.1),
            xy: c.xy.isOn(),
            shoulders: c.shoulders.isOn(),
            right_stick: c.right_stick.isOn(),
            // in steps of 0.05
            tilt_sensitivity: (c.tilt.value() * 20.0).round() / 20.0,
        })
    }

    /// The parts of the page that follow the settings.
    fn show(&self, s: TouchSettings) {
        if let Some(c) = self.ivars().controls.borrow().as_ref() {
            c.legend.setText(Some(&NSString::from_str(&s.legend())));
            c.tilt_row.setHidden(s.steering != Steering::Tilt);
        }
    }
}

/// Opens the page over `presenter`.
pub fn present(presenter: &UIViewController) {
    if is_open() {
        return;
    }
    let mtm = MainThreadMarker::new().expect("main thread");
    let page = Page::alloc(mtm).set_ivars(Ivars { controls: RefCell::new(None) });
    // SAFETY: `NSObject`'s designated initialiser
    let page: Retained<Page> = unsafe { msg_send![super(page), init] };
    let s = current();
    let controls = build(mtm, &page, s);
    let vc = controls.vc.clone();
    *page.ivars().controls.borrow_mut() = Some(controls);
    page.show(s);
    PAGE.with(|p| *p.borrow_mut() = Some(page));
    OPEN.store(true, Ordering::Relaxed);
    tracing::info!("touch controls page opened");
    presenter.presentViewController_animated_completion(&vc, true, None);
}

fn label(mtm: MainThreadMarker, text: &str, font: Retained<UIFont>, white: f64) -> Retained<UILabel> {
    let l = UILabel::new(mtm);
    l.setText(Some(&NSString::from_str(text)));
    l.setNumberOfLines(0);
    // SAFETY: a plain font and colour
    unsafe {
        l.setFont(Some(&font));
        l.setTextColor(Some(&UIColor::colorWithWhite_alpha(white, 1.0)));
    }
    l
}

fn stack(mtm: MainThreadMarker, views: &[&UIView], axis: UILayoutConstraintAxis, spacing: f64) -> Retained<UIStackView> {
    let st = UIStackView::initWithArrangedSubviews(UIStackView::alloc(mtm), &NSArray::from_slice(views));
    st.setAxis(axis);
    st.setSpacing(spacing);
    st
}

fn segments(mtm: MainThreadMarker, page: &Page, names: &[&str], selected: usize) -> Retained<UISegmentedControl> {
    let items: Vec<Retained<NSString>> = names.iter().map(|n| NSString::from_str(n)).collect();
    // SAFETY: an array of strings is an array of objects, and strings are the documented item type
    let items: Retained<NSArray> = unsafe { Retained::cast_unchecked(NSArray::from_retained_slice(&items)) };
    let seg = unsafe { UISegmentedControl::initWithItems(UISegmentedControl::alloc(mtm), Some(&items)) };
    seg.setSelectedSegmentIndex(selected as isize);
    // SAFETY: `changed:` is a method of the target, which the page keeps alive while the control is up
    unsafe { seg.addTarget_action_forControlEvents(Some(page), sel!(changed:), UIControlEvents::ValueChanged) };
    seg
}

/// A row with a switch on the right.
fn switch_row(mtm: MainThreadMarker, page: &Page, title: &str, on: bool) -> (Retained<UIStackView>, Retained<UISwitch>) {
    let sw = UISwitch::new(mtm);
    sw.setOn(on);
    // SAFETY: as in `segments`
    unsafe { sw.addTarget_action_forControlEvents(Some(page), sel!(changed:), UIControlEvents::ValueChanged) };
    let l = label(mtm, title, UIFont::systemFontOfSize(17.0), 0.92);
    let row = stack(mtm, &[&l, &sw], UILayoutConstraintAxis::Horizontal, 12.0);
    row.setAlignment(UIStackViewAlignment::Center);
    (row, sw)
}

fn build(mtm: MainThreadMarker, page: &Page, s: TouchSettings) -> Controls {
    let vc = UIViewController::new(mtm);
    vc.setModalPresentationStyle(UIModalPresentationStyle::FormSheet);
    // only Done closes it, so the settings are always taken
    vc.setModalInPresentation(true);
    let root = vc.view().expect("a view controller's view");
    root.setBackgroundColor(Some(&UIColor::colorWithRed_green_blue_alpha(0.08, 0.08, 0.09, 1.0)));

    let title = label(mtm, "Touch Controls", UIFont::boldSystemFontOfSize(26.0), 1.0);
    let note = label(
        mtm,
        "The on-screen controller appears in a race when no game controller is connected.",
        UIFont::systemFontOfSize(14.0),
        0.65,
    );

    let steering_l = label(mtm, "Steering", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let steering = segments(mtm, page, &STEERING.map(|s| s.0), STEERING.iter().position(|x| x.1 == s.steering).unwrap_or(0));
    let pedals_l = label(mtm, "Accelerator and brake", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let pedals = segments(mtm, page, &PEDALS.map(|p| p.0), PEDALS.iter().position(|x| x.1 == s.pedals).unwrap_or(0));

    let tilt = UISlider::new(mtm);
    tilt.setMinimumValue(0.5);
    tilt.setMaximumValue(2.0);
    tilt.setValue(s.tilt_sensitivity);
    // SAFETY: as in `segments`
    unsafe { tilt.addTarget_action_forControlEvents(Some(page), sel!(changed:), UIControlEvents::ValueChanged) };
    let tilt_l = label(mtm, "Tilt sensitivity", UIFont::systemFontOfSize(17.0), 0.92);
    let tilt_row = stack(mtm, &[&tilt_l, &tilt], UILayoutConstraintAxis::Horizontal, 12.0);
    tilt_row.setAlignment(UIStackViewAlignment::Center);

    let extra_l = label(mtm, "Extra buttons", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let (xy_row, xy) = switch_row(mtm, page, "X and Y", s.xy);
    let (sh_row, shoulders) = switch_row(mtm, page, "Shoulder buttons (gears)", s.shoulders);
    let (rs_row, right_stick) = switch_row(mtm, page, "Right stick (look around)", s.right_stick);

    let legend_l = label(mtm, "Buttons", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let legend = label(mtm, "", UIFont::systemFontOfSize(14.0), 0.8);

    let done = UIButton::buttonWithType(UIButtonType::System, mtm);
    let config = UIButtonConfiguration::filledButtonConfiguration(mtm);
    config.setTitle(Some(&NSString::from_str("Done")));
    done.setConfiguration(Some(&config));
    // SAFETY: as in `segments`
    unsafe { done.addTarget_action_forControlEvents(Some(page), sel!(done:), UIControlEvents::TouchUpInside) };

    let views: [&UIView; 14] = [
        &title, &note, &steering_l, &steering, &tilt_row, &pedals_l, &pedals, &extra_l, &xy_row, &sh_row, &rs_row, &legend_l, &legend,
        &done,
    ];
    let content = stack(mtm, &views, UILayoutConstraintAxis::Vertical, 12.0);
    content.setAlignment(UIStackViewAlignment::Fill);
    content.setCustomSpacing_afterView(20.0, &note);
    content.setCustomSpacing_afterView(20.0, &tilt_row);
    content.setCustomSpacing_afterView(20.0, &pedals);
    content.setCustomSpacing_afterView(20.0, &rs_row);
    content.setCustomSpacing_afterView(24.0, &legend);
    content.setTranslatesAutoresizingMaskIntoConstraints(false);
    title.setTextAlignment(NSTextAlignment::Center);
    note.setTextAlignment(NSTextAlignment::Center);

    let scroll = UIScrollView::new(mtm);
    scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
    scroll.addSubview(&content);
    root.addSubview(&scroll);
    let guide = root.safeAreaLayoutGuide();
    let cg = scroll.contentLayoutGuide();
    let fg = scroll.frameLayoutGuide();
    for c in [
        scroll.leadingAnchor().constraintEqualToAnchor(&guide.leadingAnchor()),
        scroll.trailingAnchor().constraintEqualToAnchor(&guide.trailingAnchor()),
        scroll.topAnchor().constraintEqualToAnchor(&guide.topAnchor()),
        scroll.bottomAnchor().constraintEqualToAnchor(&guide.bottomAnchor()),
        content.topAnchor().constraintEqualToAnchor_constant(&cg.topAnchor(), 24.0),
        content.bottomAnchor().constraintEqualToAnchor_constant(&cg.bottomAnchor(), -24.0),
        content.leadingAnchor().constraintEqualToAnchor_constant(&fg.leadingAnchor(), 28.0),
        content.trailingAnchor().constraintEqualToAnchor_constant(&fg.trailingAnchor(), -28.0),
        tilt.widthAnchor().constraintGreaterThanOrEqualToConstant(200.0),
    ] {
        c.setActive(true);
    }

    Controls { vc, steering, pedals, xy, shoulders, right_stick, tilt, tilt_row, legend }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let s = TouchSettings { steering: Steering::Tilt, pedals: Pedals::Buttons, xy: false, shoulders: true, right_stick: true, tilt_sensitivity: 1.5 };
        assert_eq!(TouchSettings::parse(&s.text()), s);
        assert_eq!(TouchSettings::parse(""), TouchSettings::DEFAULT);
    }

    #[test]
    fn tilt_is_symmetric_in_both_landscapes() {
        // level: no steering; either landscape, the same turn steers the same way
        assert_eq!(tilt_steer(-1.0, 0.0, 1.0), 0.0);
        let a = 20f64.to_radians();
        let right = tilt_steer(-a.cos(), -a.sin(), 1.0);
        let right_other = tilt_steer(a.cos(), a.sin(), 1.0);
        assert!(right > 0.4 && (right - right_other).abs() < 1e-6);
        assert_eq!(tilt_steer(-0.5, -0.9, 1.0), 1.0);
    }
}
