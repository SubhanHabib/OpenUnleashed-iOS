//! **PLATFORM GLUE (iOS)**: touch, and the on-screen controller.
//!
//! * One finger is the mouse the front end already reads: touching is a left press at that point, moving drags,
//!   lifting releases. The front end's buttons, lists and sliders then work as they do with a mouse.
//! * A quick two-finger tap is Esc: back in the front end, skip a start-up movie, pause in a race. The first
//!   finger's press is taken away from what it touched first, so the tap does not also press a button.
//! * A three-finger tap opens the touch-controls page (`settings.rs`), after pausing a race.
//! * In a race with no physical pad connected, GameController's `GCVirtualController` draws a pad on the
//!   screen. SDL reads it like any other pad, so the bindings give it the modern pad layout of D44: right
//!   trigger accelerates, left trigger brakes, the left stick steers, A is the handbrake, the shoulders shift.
//!   Which of its controls are shown, and what they do, is the player's choice (`settings.rs`).

use crate::*;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{Encode, Encoding, msg_send};
use objc2_foundation::{NSArray, NSError, NSSet, NSString};
use objc2_ui_kit::UIView;
use std::collections::{HashSet, VecDeque};
use winit::dpi::PhysicalPosition;
use winit::event::{Touch as WinitTouch, TouchPhase};
use winit::platform::ios::{ScreenEdge, ValidOrientations, WindowAttributesExtIOS};
use winit::window::WindowAttributes;

/// The window options every iOS window of the app takes: the whole screen, in landscape, no status bar or home
/// indicator, and the screen edges kept for the game (a swipe from an edge needs a second swipe to leave).
pub(crate) fn window_attributes(mut attrs: WindowAttributes) -> WindowAttributes {
    // the desktop's 1280x800 window would be a 1280x800-point window in the screen's corner; with no size, winit's
    // window is the screen's
    attrs.inner_size = None;
    attrs.min_inner_size = None;
    attrs.max_inner_size = None;
    attrs.position = None;
    attrs
        .with_valid_orientations(ValidOrientations::Landscape)
        .with_prefers_status_bar_hidden(true)
        .with_prefers_home_indicator_hidden(true)
        .with_preferred_screen_edges_deferring_system_gestures(ScreenEdge::ALL)
}

/// How long after the first finger a second one makes a two-finger tap, and how long the tap may last.
const SECOND_FINGER: Duration = Duration::from_millis(250);
const TAP: Duration = Duration::from_millis(450);

#[derive(Default)]
pub struct Touch {
    down: HashSet<u64>,
    /// The finger that is the mouse.
    primary: Option<u64>,
    first_at: Option<Instant>,
    /// A two-finger tap under way (since when).
    multi: Option<Instant>,
    /// The press has reached the game (it waits in `queue` until then).
    pressed: bool,
    /// Mouse events held for later frames, one a frame. The front end works out the widget under the cursor and
    /// reads the button once a frame, so a finger's press must come a frame after the move that puts the cursor
    /// there, and its release a frame after the press; a tap's events all come between two frames.
    queue: VecDeque<(WindowId, Held)>,
    /// A press reached the game since the last look (`take_pressed`).
    just_pressed: bool,
    /// A third finger joined the two-finger tap under way.
    three: bool,
    /// A three-finger tap since the last look (`take_three_finger_tap`).
    three_tap: bool,
}

#[derive(Clone, Copy, Debug)]
enum Held {
    Press,
    Release(PhysicalPosition<f64>),
    Left,
}

impl Touch {
    /// A finger's press reached the game since the last call (with the cursor's widget worked out for it).
    pub fn take_pressed(&mut self) -> bool {
        std::mem::take(&mut self.just_pressed)
    }

    /// A three-finger tap since the last call.
    pub fn take_three_finger_tap(&mut self) -> bool {
        std::mem::take(&mut self.three_tap)
    }

    /// A frame was drawn: the next held event goes.
    pub fn frame(&mut self, app: &mut App, el: &ActiveEventLoop) {
        if let Some((id, held)) = self.queue.pop_front() {
            self.play(app, el, id, held);
            // a cursor leaving goes with the release before it
            if let Some(&(id, Held::Left)) = self.queue.front() {
                self.queue.pop_front();
                self.play(app, el, id, Held::Left);
            }
        }
    }

    fn play(&mut self, app: &mut App, el: &ActiveEventLoop, id: WindowId, held: Held) {
        let device_id = DeviceId::dummy();
        let button = |state| WindowEvent::MouseInput { device_id, state, button: MouseButton::Left };
        match held {
            Held::Press => {
                self.pressed = true;
                self.just_pressed = true;
                app.window_event(el, id, button(ElementState::Pressed));
            }
            Held::Release(at) => {
                app.window_event(el, id, WindowEvent::CursorMoved { device_id, position: at });
                if self.pressed {
                    app.window_event(el, id, button(ElementState::Released));
                }
                self.pressed = false;
            }
            Held::Left => app.window_event(el, id, WindowEvent::CursorLeft { device_id }),
        }
    }

    /// Sends `held` now when nothing waits, else after what waits.
    fn later(&mut self, app: &mut App, el: &ActiveEventLoop, id: WindowId, held: Held) {
        if self.queue.is_empty() {
            self.play(app, el, id, held);
        } else {
            self.queue.push_back((id, held));
        }
    }

    pub fn event(&mut self, app: &mut App, el: &ActiveEventLoop, id: WindowId, t: &WinitTouch) {
        // winit's own placeholder device, which the game never compares
        let device_id = DeviceId::dummy();
        let moved = |position: PhysicalPosition<f64>| WindowEvent::CursorMoved { device_id, position };
        match t.phase {
            TouchPhase::Started => {
                self.down.insert(t.id);
                if self.down.len() == 1 {
                    // what an earlier tap still holds goes first
                    while let Some((id, held)) = self.queue.pop_front() {
                        self.play(app, el, id, held);
                    }
                    self.primary = Some(t.id);
                    self.first_at = Some(Instant::now());
                    self.multi = None;
                    self.three = false;
                    app.window_event(el, id, moved(t.location));
                    self.queue.push_back((id, Held::Press));
                } else if self.down.len() == 2 && self.first_at.is_some_and(|f| f.elapsed() < SECOND_FINGER) {
                    self.multi = Some(Instant::now());
                    if self.queue.iter().any(|(_, h)| matches!(h, Held::Press)) {
                        // the first finger's press has not gone yet: it never does
                        self.queue.retain(|(_, h)| !matches!(h, Held::Press));
                    } else {
                        // the first finger's press lands nowhere: off the window, where no widget is
                        app.window_event(el, id, moved(PhysicalPosition::new(-10_000.0, -10_000.0)));
                    }
                } else if self.down.len() == 3 && self.multi.is_some_and(|m| m.elapsed() < SECOND_FINGER) {
                    self.three = true;
                }
            }
            TouchPhase::Moved => {
                if self.primary == Some(t.id) && self.multi.is_none() && self.queue.is_empty() {
                    app.window_event(el, id, moved(t.location));
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.down.remove(&t.id);
                if self.primary == Some(t.id) {
                    self.primary = None;
                    // a start-up movie skips while a mouse button is held at one of its frames; a tap skips as
                    // Esc does
                    if self.multi.is_none()
                        && t.phase == TouchPhase::Ended
                        && let Some(run) = app.intro.as_mut()
                    {
                        run.key(true);
                    }
                    let at = if self.multi.is_none() { t.location } else { PhysicalPosition::new(-10_000.0, -10_000.0) };
                    self.later(app, el, id, Held::Release(at));
                }
                if self.down.is_empty() {
                    if let Some(at) = self.multi.take()
                        && at.elapsed() < TAP
                        && t.phase == TouchPhase::Ended
                    {
                        if std::mem::take(&mut self.three) {
                            self.three_tap = true;
                        } else {
                            app.touch_back(el);
                        }
                    }
                    self.later(app, el, id, Held::Left);
                }
            }
        }
    }
}

impl App {
    /// The two-finger tap: what Esc does where the game is.
    pub(crate) fn touch_back(&mut self, el: &ActiveEventLoop) {
        if let Some(run) = self.intro.as_mut() {
            run.key(true);
            return;
        }
        if self.vehicle.is_some() {
            // the race's pause key (`race_interface_keys`): only with no dialog up, never in a network race
            if !self.paused && !self.hud_dialog_up() && self.fe.race_game_type != Some(game_type::NETWORK) {
                tracing::info!("two-finger tap: pause");
                self.paused = true;
                self.orbit = Orbit::default();
                self.pause_sel = 0;
                self.pause_confirm = None;
            }
            return;
        }
        if self.fe.layout_preview.is_some() && self.fe.dialog.is_none() {
            tracing::info!("two-finger tap: Esc");
            if self.fe_input_swallowed() {
                self.fe_callback_key(0x1B);
            } else {
                self.fe_key_code(0x1B, el);
            }
        }
    }
}

impl App {
    /// A text field has the keyboard: the input dialog's field, a focused input box (the player's name, the chat
    /// line, an address), or the replay's name.
    pub(crate) fn wants_text(&self) -> bool {
        (self.replay_save.up && !self.replay_save.overwrite && !self.paused)
            || self.fe.dialog.as_ref().is_some_and(|(d, _)| d.up && d.field.is_some())
            || self.fe.layout_preview.as_ref().and_then(|l| l.focused_input()).is_some()
    }
}

/// The on-screen keyboard: shown while a text field wants it (winit's `set_ime_allowed` makes its view the
/// first responder, and the keyboard follows), hidden otherwise.
///
/// The player-name box is always focused, so on that screen the keyboard would never go and would cover the
/// buttons under it. Return (which commits the name) hides it, as does the keyboard's own hide key; a tap on
/// the box brings it back.
#[derive(Default)]
pub struct Keyboard {
    shown: bool,
    /// Put away by Return while the field still wants text.
    dismissed: bool,
    /// How far the game's view is moved up, in points ([`Keyboard::keep_in_view`]).
    lift: f64,
}

impl Keyboard {
    pub fn update(&mut self, app: &App) {
        let text = app.wants_text();
        if !text {
            self.dismissed = false;
        }
        let want = text && !self.dismissed;
        if want != self.shown {
            self.set(app, want);
        }
        self.keep_in_view(app);
    }

    /// The keyboard covers the lower part of the screen (more than half of an iPhone's, in landscape): while it is
    /// up, the game's view moves up just far enough that the box being typed in sits above it. UIKit takes the move
    /// into account for the touches, so the game sees them where they were.
    fn keep_in_view(&mut self, app: &App) {
        let Some(view) = app.window.as_ref().and_then(|w| ui_view(w)) else { return };
        let lift = if self.shown { field_lift(app, &view).unwrap_or(0.0) } else { 0.0 };
        if (lift - self.lift).abs() < 0.5 {
            return;
        }
        self.lift = lift;
        let t = CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: -lift };
        // SAFETY: `-[UIView setTransform:]` on winit's view, on the main thread
        unsafe {
            let _: () = msg_send![&*view, setTransform: t];
        }
        tracing::info!(lift, "game view moved over the keyboard");
    }

    fn set(&mut self, app: &App, shown: bool) {
        if let Some(w) = app.window.as_ref() {
            tracing::info!(shown, "on-screen keyboard");
            w.set_ime_allowed(shown);
            self.shown = shown;
        }
    }

    /// After a key event went to the game: Return in a front-end input box puts the keyboard away.
    pub fn key(&mut self, app: &App, event: &WindowEvent) {
        use winit::keyboard::{Key, NamedKey};
        if let WindowEvent::KeyboardInput { event, .. } = event
            && event.state == ElementState::Pressed
            && event.logical_key == Key::Named(NamedKey::Enter)
            && self.shown
            && app.fe.dialog.is_none()
            && !app.replay_save.up
            && app.wants_text()
        {
            self.dismissed = true;
        }
    }

    /// A finger's press reached the game: on the focused input box, the keyboard
    /// comes back, also after the keyboard's own hide key put it away behind winit's back.
    pub fn touched(&mut self, app: &App) {
        let Some(l) = app.fe.layout_preview.as_ref() else { return };
        if app.wants_text() && l.focused_input().is_some() && l.hovered() == l.focused_input() {
            self.dismissed = false;
            self.set(app, true);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGAffineTransform {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    tx: f64,
    ty: f64,
}

// SAFETY: CoreGraphics' structs of `CGFloat`s (`double` on every 64-bit iOS)
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}
unsafe impl Encode for CGAffineTransform {
    const ENCODING: Encoding = Encoding::Struct("CGAffineTransform", &[f64::ENCODING; 6]);
}

/// winit's view of the window.
fn ui_view(window: &winit::window::Window) -> Option<Retained<UIView>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let RawWindowHandle::UiKit(h) = window.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: winit's live `UIView`, retained for our use
    unsafe { Retained::retain(h.ui_view.as_ptr().cast::<UIView>()) }
}

/// How far the view has to move up so that the box being typed in clears the keyboard, in points; `None`: no
/// keyboard on the screen (a hardware keyboard), or nothing to keep in view.
fn field_lift(app: &App, view: &UIView) -> Option<f64> {
    let window = view.window()?;
    // SAFETY: `-[UIView keyboardLayoutGuide]` (iOS 15) and `-[UILayoutGuide layoutFrame]`, in the window's
    // coordinates, which the view's own move does not change
    let (kb, bounds): (CGRect, CGRect) = unsafe {
        let guide: Retained<AnyObject> = msg_send![&*window, keyboardLayoutGuide];
        (msg_send![&*guide, layoutFrame], msg_send![view, bounds])
    };
    // with no keyboard up, the guide is the bottom of the safe area
    if kb.size.height < 80.0 {
        return None;
    }
    let (vw, vh) = (bounds.size.width.max(1.0), bounds.size.height.max(1.0));
    // the front end's 640 × 480 design space, pillarboxed (or letterboxed) into the view
    let k = (vw / 640.0).min(vh / 480.0);
    let y0 = (vh - 480.0 * k) * 0.5;
    let bottom = match app.fe.layout_preview.as_ref().and_then(|l| l.focused_input_box()) {
        Some((_, y, _, h)) if app.vehicle.is_none() && app.fe.dialog.is_none() => y0 + (y + h) as f64 * k,
        // a dialog's field and the replay's name box sit about the middle of the screen
        _ => vh * 0.5 + 30.0,
    };
    Some((bottom + 12.0 - kb.origin.y).clamp(0.0, kb.size.height))
}

/// The keyboard's Return and space come as the characters "\n" and " "; the fields read the named keys, as a
/// desktop keyboard gives them.
pub(crate) fn named_keys(event: WindowEvent) -> WindowEvent {
    use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
    let WindowEvent::KeyboardInput { device_id, mut event, is_synthetic } = event else { return event };
    let named = match event.logical_key.as_ref() {
        Key::Character("\n" | "\r") => Some((NamedKey::Enter, KeyCode::Enter)),
        Key::Character(" ") => Some((NamedKey::Space, KeyCode::Space)),
        _ => None,
    };
    if let Some((key, code)) = named {
        event.logical_key = Key::Named(key);
        event.physical_key = PhysicalKey::Code(code);
    }
    WindowEvent::KeyboardInput { device_id, event, is_synthetic }
}

#[link(name = "GameController", kind = "framework")]
unsafe extern "C" {
    static GCInputLeftThumbstick: &'static NSString;
    static GCInputRightThumbstick: &'static NSString;
    static GCInputDirectionPad: &'static NSString;
    static GCInputButtonA: &'static NSString;
    static GCInputButtonB: &'static NSString;
    static GCInputButtonX: &'static NSString;
    static GCInputButtonY: &'static NSString;
    static GCInputLeftShoulder: &'static NSString;
    static GCInputRightShoulder: &'static NSString;
    static GCInputLeftTrigger: &'static NSString;
    static GCInputRightTrigger: &'static NSString;
}

/// The on-screen pad (iOS 15's `GCVirtualController`), connected only while it is wanted.
#[derive(Default)]
pub struct VirtualPad {
    vc: Option<Retained<AnyObject>>,
    connected: bool,
    frames: u32,
    /// A physical pad was seen at the last look.
    physical: bool,
}

impl VirtualPad {
    /// The settings changed: the next connect builds the pad anew, with the new set of controls.
    pub fn rebuild(&mut self) {
        if let Some(vc) = self.vc.take()
            && self.connected
        {
            // SAFETY: `-[GCVirtualController disconnect]`
            unsafe {
                let _: () = msg_send![&*vc, disconnect];
            }
        }
        self.connected = false;
    }

    /// The on-screen pad is connected (and is the pad being driven with).
    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn update(&mut self, app: &App) {
        // the touch-controls page covers the game; the pad comes back when it closes
        let in_race = app.vehicle.is_some() && app.intro.is_none() && !super::settings::is_open();
        self.frames = self.frames.wrapping_add(1);
        if self.frames % 30 == 1 || in_race != self.connected {
            self.physical = self.physical_pad();
        }
        let want = in_race && !self.physical;
        if want == self.connected {
            return;
        }
        if want {
            if self.vc.is_none() {
                self.vc = make_virtual_controller(&super::settings::current());
            }
            if let Some(vc) = &self.vc {
                tracing::info!("on-screen controller shown");
                let none: Option<&block2::DynBlock<dyn Fn(*mut NSError)>> = None;
                // SAFETY: `-[GCVirtualController connectWithReplyHandler:]`, on the main thread
                unsafe {
                    let _: () = msg_send![&**vc, connectWithReplyHandler: none];
                }
            }
        } else if let Some(vc) = &self.vc {
            tracing::info!("on-screen controller hidden");
            // SAFETY: `-[GCVirtualController disconnect]`
            unsafe {
                let _: () = msg_send![&**vc, disconnect];
            }
        }
        self.connected = want;
    }

    /// Whether a pad other than the on-screen one is connected. `OPEN_UNLEASHED_TOUCH_PAD` set keeps the
    /// on-screen pad regardless (the simulator always reports a pad of its own).
    fn physical_pad(&self) -> bool {
        if std::env::var_os("OPEN_UNLEASHED_TOUCH_PAD").is_some() {
            return false;
        }
        let Some(cls) = AnyClass::get(c"GCController") else { return false };
        // SAFETY: `+[GCController controllers]`, and `-[GCVirtualController controller]`
        unsafe {
            let list: Retained<NSArray<AnyObject>> = msg_send![cls, controllers];
            let own: Option<Retained<AnyObject>> = self.vc.as_ref().and_then(|vc| msg_send![&**vc, controller]);
            list.iter().any(|c| own.as_ref().is_none_or(|o| !std::ptr::eq(&*c, &**o)))
        }
    }
}

/// The on-screen pad with the controls `s` asks for. Apple's pad places them itself: the steering and the triggers
/// on the left, the face buttons and the right stick on the right, the shoulders above.
fn make_virtual_controller(s: &super::settings::TouchSettings) -> Option<Retained<AnyObject>> {
    use super::settings::{Pedals, Steering};
    let cfg_cls = AnyClass::get(c"GCVirtualControllerConfiguration")?;
    let vc_cls = AnyClass::get(c"GCVirtualController")?;
    // SAFETY: GameController's documented API, on the main thread; the input names are its own constants
    unsafe {
        let cfg: Retained<AnyObject> = msg_send![cfg_cls, new];
        let mut names: Vec<&NSString> = vec![GCInputButtonA, GCInputButtonB];
        match s.steering {
            Steering::Stick | Steering::Touchpad => names.push(GCInputLeftThumbstick),
            Steering::DPad => names.push(GCInputDirectionPad),
            Steering::Tilt => {}
        }
        if s.pedals == Pedals::Triggers {
            names.extend([GCInputLeftTrigger, GCInputRightTrigger]);
        }
        if s.xy {
            names.extend([GCInputButtonX, GCInputButtonY]);
        }
        if s.shoulders {
            names.extend([GCInputLeftShoulder, GCInputRightShoulder]);
        }
        if s.right_stick {
            names.push(GCInputRightThumbstick);
        }
        let set = NSSet::from_slice(&names);
        let _: () = msg_send![&*cfg, setElements: &*set];
        let alloc: Allocated<AnyObject> = msg_send![vc_cls, alloc];
        let vc: Option<Retained<AnyObject>> = msg_send![alloc, initWithConfiguration: &*cfg];
        if let Some(vc) = &vc
            && s.steering == Steering::Touchpad
        {
            // the stick reads where the finger is on it, not how far it moved from where it landed
            let block = block2::RcBlock::new(|el: std::ptr::NonNull<AnyObject>| -> *mut AnyObject {
                let _: () = msg_send![el.as_ref(), setActsAsTouchpad: true];
                el.as_ptr()
            });
            let _: () = msg_send![&**vc, updateConfigurationForElement: GCInputLeftThumbstick, configuration: &*block];
        }
        tracing::info!(?s, "on-screen controller built");
        vc
    }
}
