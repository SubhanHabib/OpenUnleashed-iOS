//! **PLATFORM GLUE (iOS)**: the split on-screen controls (`Layout::Split`), the touch layout of most phone racing
//! games: the left thumb steers, the right thumb drives.
//!
//! * Left: a stick that comes up where the thumb lands (Steering "Stick"), a fixed one read where the thumb is on
//!   it ("Touchpad"), ◀ ▶ buttons ("D-pad"), or nothing (the device's tilt steers).
//! * Right: GAS and BRAKE side by side with HAND (the handbrake) next to them, ▲ / ▼ (shift up / down) above
//!   them, and three small buttons above those: reset the car, change camera, look back. A finger can slide from
//!   one pedal to another.
//!
//! It is one `UIView` over winit's, taking only the touches that land on its controls (`hitTest:withEvent:`);
//! the rest go to the game as before (one finger the mouse, two a pause, three the settings). What it reads goes
//! to the game as a standard pad (`nfs_input::gamepad::set_host_pad`), with D44's modern pad bindings: right
//! trigger GAS, left trigger BRAKE, A HAND, B look back, X reset, Y camera, RB / LB shift up / down.

use super::settings::{self, Steering};
use super::touch::{self, CGPoint, CGRect, CGSize};
use crate::App;
use nfs_input::gamepad::PadSnapshot;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, Encode, Encoding, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSObject, NSObjectProtocol, NSSet, NSString};
use objc2_ui_kit::{NSTextAlignment, UIColor, UIFont, UILabel, UIResponder, UIView, UIViewAutoresizing};
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Btn {
    Gas,
    Brake,
    Hand,
    Up,
    Down,
    Reset,
    Camera,
    Back,
    Left,
    Right,
}

impl Btn {
    fn title(self) -> &'static str {
        match self {
            Btn::Gas => "GAS",
            Btn::Brake => "BRAKE",
            Btn::Hand => "HAND",
            Btn::Up => "▲",
            Btn::Down => "▼",
            Btn::Reset => "RESET",
            Btn::Camera => "CAMERA",
            Btn::Back => "LOOK BACK",
            Btn::Left => "◀",
            Btn::Right => "▶",
        }
    }

    /// A finger sliding onto it presses it (the pedals and the steering buttons).
    fn slides(self) -> bool {
        matches!(self, Btn::Gas | Btn::Brake | Btn::Hand | Btn::Left | Btn::Right)
    }

    /// Its colour, red / green / blue.
    fn tint(self) -> (f64, f64, f64) {
        match self {
            Btn::Gas => (0.25, 0.8, 0.4),
            Btn::Brake => (0.9, 0.3, 0.3),
            _ => (1.0, 1.0, 1.0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Role {
    Stick,
    /// on a button, or on nothing yet (it can slide onto a pedal)
    Button(Option<Btn>),
}

fn contains(r: &CGRect, p: CGPoint, slop: f64) -> bool {
    p.x >= r.origin.x - slop && p.x <= r.origin.x + r.size.width + slop && p.y >= r.origin.y - slop && p.y <= r.origin.y + r.size.height + slop
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct UIEdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

// SAFETY: UIKit's struct of four `CGFloat`s
unsafe impl Encode for UIEdgeInsets {
    const ENCODING: Encoding = Encoding::Struct("UIEdgeInsets", &[f64::ENCODING; 4]);
}

pub struct State {
    steering: Steering,
    /// the size the controls were laid out for
    size: (f64, f64),
    buttons: Vec<(Btn, CGRect, Retained<UIView>)>,
    /// where a touch starts the stick
    zone: CGRect,
    /// the stick's place at rest (and the fixed stick's centre)
    home: CGPoint,
    radius: f64,
    base: Option<Retained<UIView>>,
    knob: Option<Retained<UIView>>,
    touches: Vec<(usize, Role)>,
    /// the stick's touch, its centre and where the finger is
    stick: Option<(usize, CGPoint, CGPoint)>,
    steer: f32,
    /// how far in from the right edge the controls reach, in points: the race HUD keeps left of it
    clear_right: f64,
}

impl State {
    fn new(steering: Steering) -> Self {
        State {
            steering,
            size: (0.0, 0.0),
            clear_right: 0.0,
            buttons: Vec::new(),
            zone: CGRect::default(),
            home: CGPoint::default(),
            radius: 56.0,
            base: None,
            knob: None,
            touches: Vec::new(),
            stick: None,
            steer: 0.0,
        }
    }

    fn has_stick(&self) -> bool {
        matches!(self.steering, Steering::Stick | Steering::Touchpad)
    }

    fn button_at(&self, p: CGPoint, slop: f64) -> Option<Btn> {
        self.buttons.iter().find(|(_, r, _)| contains(r, p, slop)).map(|(b, _, _)| *b)
    }

    fn rect_of(&self, b: Btn) -> Option<CGRect> {
        self.buttons.iter().find(|(x, _, _)| *x == b).map(|(_, r, _)| *r)
    }

    /// Whether a touch at `p` is the deck's.
    fn hits(&self, p: CGPoint) -> bool {
        self.button_at(p, 8.0).is_some() || (self.has_stick() && contains(&self.zone, p, 0.0))
    }

    fn held(&self, b: Btn) -> bool {
        self.touches.iter().any(|(_, r)| matches!(r, Role::Button(Some(x)) if *x == b))
    }

    fn stick_to(&mut self, p: CGPoint) {
        let Some((id, mut c, _)) = self.stick else { return };
        let r = self.radius;
        let dx = p.x - c.x;
        // the floating stick follows a thumb that goes past its edge, so turning the other way is quick
        if self.steering == Steering::Stick && dx.abs() > r {
            c.x = p.x - r * dx.signum();
        }
        self.steer = (((p.x - c.x) / r) as f32).clamp(-1.0, 1.0);
        self.stick = Some((id, c, p));
    }

    fn began(&mut self, id: usize, p: CGPoint) {
        if let Some(b) = self.button_at(p, 8.0) {
            self.touches.push((id, Role::Button(Some(b))));
        } else if self.has_stick() && self.stick.is_none() && contains(&self.zone, p, 0.0) {
            let c = if self.steering == Steering::Stick { p } else { self.home };
            self.stick = Some((id, c, p));
            self.touches.push((id, Role::Stick));
            self.stick_to(p);
        } else {
            self.touches.push((id, Role::Button(None)));
        }
    }

    fn moved(&mut self, id: usize, p: CGPoint) {
        let Some(i) = self.touches.iter().position(|(t, _)| *t == id) else { return };
        match self.touches[i].1 {
            Role::Stick => self.stick_to(p),
            Role::Button(cur) => {
                let keep = cur.and_then(|b| self.rect_of(b)).is_some_and(|r| contains(&r, p, 4.0));
                if !keep {
                    // onto another pedal; off a button that does not slide, it lets go
                    let next = self.button_at(p, 4.0).filter(|b| b.slides());
                    let still = cur.and_then(|b| self.rect_of(b)).is_some_and(|r| contains(&r, p, 28.0));
                    self.touches[i].1 = Role::Button(next.or(cur.filter(|_| still && next.is_none())));
                }
            }
        }
    }

    fn ended(&mut self, id: usize) {
        self.touches.retain(|(t, _)| *t != id);
        if self.stick.is_some_and(|(t, _, _)| t == id) {
            self.stick = None;
            self.steer = 0.0;
        }
    }

    /// What the controls read, as a standard pad (`BUTTON_ORDER`: 0 A, 1 B, 2 Y, 3 X, 6 LB, 7 RB).
    fn snapshot(&self) -> PadSnapshot {
        let steer = match self.steering {
            Steering::Tilt => settings::tilt() as f32 / 32767.0,
            Steering::DPad => f32::from(self.held(Btn::Right) as u8) - f32::from(self.held(Btn::Left) as u8),
            Steering::Stick | Steering::Touchpad => self.steer,
        };
        let full = |b| if self.held(b) { i16::MAX } else { 0 };
        let mut p = PadSnapshot::default();
        p.sticks[0] = (steer.clamp(-1.0, 1.0) * 32767.0) as i16;
        p.triggers = [full(Btn::Brake), full(Btn::Gas)];
        p.buttons[0] = self.held(Btn::Hand);
        p.buttons[1] = self.held(Btn::Back);
        p.buttons[2] = self.held(Btn::Camera);
        p.buttons[3] = self.held(Btn::Reset);
        p.buttons[6] = self.held(Btn::Down);
        p.buttons[7] = self.held(Btn::Up);
        p
    }

    fn draw(&self) {
        for (b, _, v) in &self.buttons {
            let (r, g, bl) = b.tint();
            // outlined and nearly clear at rest, so the HUD's gauges under the pedals still read
            let a = if self.held(*b) { 0.45 } else { 0.08 };
            v.setBackgroundColor(Some(&UIColor::colorWithRed_green_blue_alpha(r, g, bl, a)));
        }
        if let (Some(base), Some(knob)) = (&self.base, &self.knob) {
            base.setHidden(!self.has_stick());
            knob.setHidden(!self.has_stick());
            let (c, p) = self.stick.map_or((self.home, self.home), |(_, c, p)| (c, p));
            let r = self.radius;
            let (dx, dy) = (p.x - c.x, p.y - c.y);
            let d = (dx * dx + dy * dy).sqrt().max(1e-6);
            let k = (r / d).min(1.0);
            set_center(base, c);
            set_center(knob, CGPoint { x: c.x + dx * k, y: c.y + dy * k });
            base.setAlpha(if self.stick.is_some() { 1.0 } else { 0.6 });
        }
    }
}

fn set_center(v: &UIView, c: CGPoint) {
    // SAFETY: `-[UIView setCenter:]`
    unsafe {
        let _: () = msg_send![v, setCenter: c];
    }
}

fn set_frame(v: &UIView, r: CGRect) {
    // SAFETY: `-[UIView setFrame:]`
    unsafe {
        let _: () = msg_send![v, setFrame: r];
    }
}

/// CoreGraphics' colour, only ever handled by pointer.
#[repr(C)]
struct CGColor {
    _private: [u8; 0],
}

// SAFETY: `CGColorRef` is `struct CGColor *`
unsafe impl objc2::RefEncode for CGColor {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
}

fn round(v: &UIView, radius: f64) {
    // SAFETY: `-[UIView layer]` and `-[CALayer setCornerRadius:]`
    unsafe {
        let layer: Retained<AnyObject> = msg_send![v, layer];
        let _: () = msg_send![&*layer, setCornerRadius: radius];
    }
}

/// A thin outline of `color` round `v`.
fn outline(v: &UIView, color: &UIColor, width: f64) {
    // SAFETY: `-[UIView layer]`, `-[UIColor CGColor]` (alive as long as the colour, which outlives the call) and
    // `CALayer`'s border properties, which retain the colour
    unsafe {
        let layer: Retained<AnyObject> = msg_send![v, layer];
        let cg: *mut CGColor = msg_send![color, CGColor];
        let _: () = msg_send![&*layer, setBorderColor: cg];
        let _: () = msg_send![&*layer, setBorderWidth: width];
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Began,
    Moved,
    Ended,
}

define_class!(
    // SAFETY: UIView may be subclassed; `DeckView` does not implement `Drop`
    #[unsafe(super(UIView, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OUTouchDeck"]
    #[ivars = RefCell<State>]
    pub struct DeckView;

    impl DeckView {
        #[unsafe(method(hitTest:withEvent:))]
        fn hit_test(&self, p: CGPoint, _event: *mut AnyObject) -> *mut AnyObject {
            if self.ivars().borrow().hits(p) { self as *const Self as *mut AnyObject } else { std::ptr::null_mut() }
        }

        #[unsafe(method(touchesBegan:withEvent:))]
        fn touches_began(&self, touches: &NSSet<AnyObject>, _event: *mut AnyObject) {
            self.touches(touches, Phase::Began);
        }

        #[unsafe(method(touchesMoved:withEvent:))]
        fn touches_moved(&self, touches: &NSSet<AnyObject>, _event: *mut AnyObject) {
            self.touches(touches, Phase::Moved);
        }

        #[unsafe(method(touchesEnded:withEvent:))]
        fn touches_ended(&self, touches: &NSSet<AnyObject>, _event: *mut AnyObject) {
            self.touches(touches, Phase::Ended);
        }

        #[unsafe(method(touchesCancelled:withEvent:))]
        fn touches_cancelled(&self, touches: &NSSet<AnyObject>, _event: *mut AnyObject) {
            self.touches(touches, Phase::Ended);
        }
    }

    unsafe impl NSObjectProtocol for DeckView {}
);

impl DeckView {
    fn new(mtm: MainThreadMarker, frame: CGRect, steering: Steering) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RefCell::new(State::new(steering)));
        // SAFETY: `UIView`'s designated initialiser
        let v: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        v.setMultipleTouchEnabled(true);
        v.setAutoresizingMask(UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight);
        v
    }

    fn touches(&self, set: &NSSet<AnyObject>, phase: Phase) {
        let mut st = self.ivars().borrow_mut();
        let before = st.snapshot();
        for t in set.allObjects().iter() {
            let id = Retained::as_ptr(&t) as usize;
            // SAFETY: `-[UITouch locationInView:]`, in the deck's own coordinates
            let p: CGPoint = unsafe { msg_send![&*t, locationInView: self] };
            match phase {
                Phase::Began => st.began(id, p),
                Phase::Moved => st.moved(id, p),
                Phase::Ended => st.ended(id),
            }
        }
        st.draw();
        let now = st.snapshot();
        if (now.triggers, now.buttons) != (before.triggers, before.buttons) {
            tracing::info!(triggers = ?now.triggers, buttons = ?&now.buttons[..8], steer = now.sticks[0], "split touch controls");
        }
    }

    /// Lays the controls out for the view's size and safe area, when they changed.
    fn layout(&self, mtm: MainThreadMarker) {
        // SAFETY: `-[UIView bounds]` and `-[UIView safeAreaInsets]`
        let (b, ins): (CGRect, UIEdgeInsets) = unsafe { (msg_send![self, bounds], msg_send![self, safeAreaInsets]) };
        let (w, h) = (b.size.width, b.size.height);
        let mut st = self.ivars().borrow_mut();
        if st.size == (w, h) || w < 100.0 {
            return;
        }
        st.size = (w, h);
        for (_, _, v) in st.buttons.drain(..) {
            v.removeFromSuperview();
        }
        // a phone in landscape is about 400 points tall; an iPad's controls grow a little, not with the screen
        let k = (h / 400.0).clamp(1.0, 1.35);
        // past the safe area on the right: the camera cutout is at the middle of the side, well above the pedals,
        // and the HUD's gauges stop at the safe area
        let right = w - (ins.right * 0.4).max(0.0) - 12.0;
        let left = ins.left.max(16.0);
        let bottom = h - ins.bottom.max(12.0);
        // two columns at the right edge: GAS (tall) with ▲ above it, BRAKE with HAND and ▼ above it
        let (gap, cw) = (8.0 * k, 92.0 * k);
        let gas = rect(right - cw, bottom - 164.0 * k, cw, 164.0 * k);
        let brake = rect(gas.origin.x - gap - cw, bottom - 92.0 * k, cw, 92.0 * k);
        let hand = rect(brake.origin.x, brake.origin.y - gap - 64.0 * k, cw, 64.0 * k);
        let shift_y = gas.origin.y - gap - 48.0 * k;
        let up = rect(gas.origin.x, shift_y, cw, 48.0 * k);
        let down = rect(brake.origin.x, shift_y, cw, 48.0 * k);
        let sh = 30.0 * k;
        let sw = (2.0 * cw + gap - 2.0 * 6.0 * k) / 3.0;
        let small_y = shift_y - gap - sh;
        let back = rect(right - sw, small_y, sw, sh);
        let camera = rect(back.origin.x - 6.0 * k - sw, small_y, sw, sh);
        let reset = rect(camera.origin.x - 6.0 * k - sw, small_y, sw, sh);
        st.clear_right = w - brake.origin.x + 10.0 * k;
        let mut list = vec![
            (Btn::Gas, gas),
            (Btn::Brake, brake),
            (Btn::Hand, hand),
            (Btn::Up, up),
            (Btn::Down, down),
            (Btn::Reset, reset),
            (Btn::Camera, camera),
            (Btn::Back, back),
        ];
        if st.steering == Steering::DPad {
            let s = 84.0 * k;
            list.push((Btn::Left, rect(left, bottom - s, s, s)));
            list.push((Btn::Right, rect(left + s + 14.0 * k, bottom - s, s, s)));
        }
        for (btn, r) in list {
            let v = UIView::new(mtm);
            set_frame(&v, r);
            round(&v, 14.0 * k);
            let (cr, cgr, cb) = btn.tint();
            outline(&v, &UIColor::colorWithRed_green_blue_alpha(cr, cgr, cb, 0.55), 1.5);
            v.setUserInteractionEnabled(false);
            let l = UILabel::new(mtm);
            set_frame(&l, rect(0.0, 0.0, r.size.width, r.size.height));
            l.setText(Some(&NSString::from_str(btn.title())));
            l.setTextAlignment(NSTextAlignment::Center);
            l.setAdjustsFontSizeToFitWidth(true);
            let size = if matches!(btn, Btn::Reset | Btn::Camera | Btn::Back) { 11.0 } else { 17.0 };
            // SAFETY: a plain font and colour
            unsafe {
                l.setFont(Some(&UIFont::boldSystemFontOfSize(size * k)));
                l.setTextColor(Some(&UIColor::colorWithWhite_alpha(1.0, 0.9)));
            }
            v.addSubview(&l);
            self.addSubview(&v);
            st.buttons.push((btn, r, v));
        }
        st.radius = 56.0 * k;
        // right of the HUD's map in the lower left corner
        st.home = CGPoint { x: left + 200.0 * k, y: bottom - 90.0 * k };
        // the lower left, under the HUD, up to the pedals' side
        st.zone = rect(0.0, h * 0.3, (w * 0.45).min(hand.origin.x - 24.0), h * 0.7);
        if st.base.is_none() {
            let base = UIView::new(mtm);
            let knob = UIView::new(mtm);
            for v in [&base, &knob] {
                v.setUserInteractionEnabled(false);
                self.addSubview(v);
            }
            base.setBackgroundColor(Some(&UIColor::colorWithWhite_alpha(1.0, 0.08)));
            outline(&base, &UIColor::colorWithWhite_alpha(1.0, 0.35), 1.5);
            knob.setBackgroundColor(Some(&UIColor::colorWithWhite_alpha(1.0, 0.4)));
            st.base = Some(base);
            st.knob = Some(knob);
        }
        let (r, kr) = (st.radius + 18.0 * k, 30.0 * k);
        if let (Some(base), Some(knob)) = (&st.base, &st.knob) {
            set_frame(base, rect(0.0, 0.0, 2.0 * r, 2.0 * r));
            round(base, r);
            set_frame(knob, rect(0.0, 0.0, 2.0 * kr, 2.0 * kr));
            round(knob, kr);
        }
        st.touches.clear();
        st.stick = None;
        st.steer = 0.0;
        st.draw();
        tracing::info!(w, h, ?ins, steering = ?st.steering, "split touch controls laid out");
    }
}

/// The split controls, shown only in a race with no game controller connected.
#[derive(Default)]
pub struct Deck {
    view: Option<Retained<DeckView>>,
    shown: bool,
    frames: u32,
    physical: bool,
}

impl Deck {
    /// The settings changed: the controls are built anew when next shown.
    pub fn rebuild(&mut self) {
        if let Some(v) = self.view.take() {
            v.removeFromSuperview();
        }
    }

    /// The split controls are up (and are what is being driven with).
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// While shown, how far in from the right edge the controls reach (points): the race HUD keeps left of it.
    pub fn clear_right(&self) -> Option<f64> {
        let v = self.view.as_ref().filter(|_| self.shown)?;
        Some(v.ivars().borrow().clear_right).filter(|r| *r > 0.0)
    }

    /// Once a frame: shown or hidden for `wanted` (the split layout is chosen), the game's state, and whether a
    /// controller is connected; what they read goes to the game.
    pub fn update(&mut self, app: &App, wanted: bool) {
        let in_race = wanted && app.vehicle.is_some() && app.intro.is_none() && !settings::is_open();
        self.frames = self.frames.wrapping_add(1);
        if self.frames % 30 == 1 || in_race != self.shown {
            self.physical = touch::physical_pad(None);
        }
        let want = in_race && !self.physical;
        if !want {
            if self.shown {
                tracing::info!("split touch controls hidden");
                if let Some(v) = &self.view {
                    v.setHidden(true);
                }
                nfs_input::gamepad::set_host_pad(None);
                self.shown = false;
            }
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else { return };
        if self.view.is_none() {
            let Some(parent) = app.window.as_ref().and_then(|w| touch::ui_view(w)) else { return };
            // SAFETY: `-[UIView bounds]`
            let frame: CGRect = unsafe { msg_send![&*parent, bounds] };
            let v = DeckView::new(mtm, frame, settings::current().steering);
            parent.addSubview(&v);
            self.view = Some(v);
        }
        let Some(v) = &self.view else { return };
        if !self.shown {
            tracing::info!("split touch controls shown");
            self.shown = true;
        }
        v.layout(mtm);
        // the pause menu is the front end's, under the fingers' mouse: the controls make way, and read nothing
        v.setHidden(app.paused);
        let p = if app.paused {
            let mut st = v.ivars().borrow_mut();
            st.touches.clear();
            st.stick = None;
            st.steer = 0.0;
            PadSnapshot::default()
        } else {
            v.ivars().borrow().snapshot()
        };
        nfs_input::gamepad::set_host_pad(Some(p));
    }
}
