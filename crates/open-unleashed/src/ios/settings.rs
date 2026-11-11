//! **PLATFORM GLUE (iOS)**: the on-screen controller's options, the port's display and game options, and the
//! page that sets them.
//!
//! The game's own menus hold only the original's settings, so these live on a small iOS page of their own, opened
//! from the import screen or with a three-finger tap anywhere in the game (a race is paused first). They are kept
//! in `touch-controls.txt` and `ios-options.txt` in the game's data folder, one `key=value` a line. (A desktop
//! sets the same options with environment variables and `nfs5.ini`, which an iOS app has no way to set.)
//!
//! * **Steering**: the left stick, a d-pad (full lock left or right), the left stick as a touchpad (the finger's
//!   place on the pad, not its travel from where it landed), or tilting the device like a wheel.
//! * **Pedals**: the triggers (analogue, on the left) or the A / B buttons (on / off, on the right). With the
//!   buttons, X is the handbrake and Y resets the car.
//! * **Extra buttons**: X / Y, the shoulders (shift up / down) and the right stick (look left / right), each shown
//!   or not.
//!
//! The display and game options ([`GameOptions`]): the HUD kept inside the safe area, the render resolution, a frame
//! cap, the weather fog, texture filtering, the intro movies and car-to-car collisions.
//!
//! **Import texture pack** takes a pack's `.zip` (or folder) from the Files app into `Documents/Textures/<name>`
//! (`nfs_assets::import::import_texture_pack`), on its own thread; [`poll`] follows it between frames.
//!
//! The pad's own buttons stay what SDL reads; [`pad_filter`] rewrites the on-screen pad's snapshot to the chosen
//! layout before the bindings see it, so the game's bindings (Options › Controls) are not touched.

use nfs_input::gamepad::PadSnapshot;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{DefinedClass, Encode, Encoding, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_uniform_type_identifiers::{UTTypeFolder, UTTypeZIP};
use objc2_ui_kit::{
    NSTextAlignment, UIButton, UIButtonConfiguration, UIButtonType, UIColor, UIControlEvents, UIFont, UILabel,
    UILayoutConstraintAxis, UIModalPresentationStyle, UIScrollView, UISegmentedControl, UISlider, UIStackView,
    UIDocumentPickerDelegate, UIDocumentPickerViewController, UIStackViewAlignment, UIStackViewDistribution, UISwitch, UIView,
    UIViewController,
};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
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

/// Where the on-screen controls are: `Split` is the app's own (steering under the left thumb, the pedals and the
/// gears under the right, `deck.rs`), `Gamepad` Apple's on-screen pad (`touch.rs`), which puts the stick and the
/// triggers together on the left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Split,
    Gamepad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pedals {
    Triggers,
    Buttons,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchSettings {
    pub layout: Layout,
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
        Self { layout: Layout::Split, steering: Steering::Stick, pedals: Pedals::Triggers, xy: true, shoulders: true, right_stick: false, tilt_sensitivity: 1.0 };

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
                "layout" => s.layout = if v == "gamepad" { Layout::Gamepad } else { Layout::Split },
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
        let layout = match self.layout {
            Layout::Split => "split",
            Layout::Gamepad => "gamepad",
        };
        format!(
            "layout={layout}\nsteering={steering}\npedals={pedals}\nxy={}\nshoulders={}\nright_stick={}\ntilt_sensitivity={:.2}\n",
            self.xy as u8, self.shoulders as u8, self.right_stick as u8, self.tilt_sensitivity
        )
    }

    /// What each button does with these settings, for the page.
    fn legend(&self) -> String {
        let mut parts = vec![];
        if self.layout == Layout::Split {
            parts.push(
                match self.steering {
                    Steering::Stick => "Left thumb: steer (the stick comes up where the thumb lands)",
                    Steering::Touchpad => "Left thumb: steer (a fixed stick, read where the thumb is on it)",
                    Steering::DPad => "Left thumb: the ◀ ▶ buttons steer",
                    Steering::Tilt => "Tilt the device to steer",
                }
                .to_owned(),
            );
            parts.push("Right thumb: GAS, BRAKE (held: reverse), HAND (handbrake); slide between them".to_owned());
            parts.push("▲ / ▼ above the pedals: shift up / down".to_owned());
            parts.push("Small buttons: reset the car, change camera, look back".to_owned());
            parts.push("Two-finger tap: pause · Three-finger tap: this page".to_owned());
            return parts.join("\n");
        }
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

/// The page's display and game options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameOptions {
    /// The race HUD is kept clear of the camera cutout and the home indicator (`App::hud_inset`).
    pub safe_hud: bool,
    /// The render target's size as a fraction of the screen's pixels (`App::render_scale`).
    pub resolution: f32,
    /// The most frames a second, 0: no cap (`App::fps_cap`).
    pub fps_cap: u32,
    /// `nfs5.ini`'s `Fog` (off when either is).
    pub fog: bool,
    /// `nfs5.ini`'s `Filtering` (off when either is); at the next start, when the renderer is made.
    pub filtering: bool,
    /// `nfs5.ini`'s `NoMovies` (on when either is).
    pub skip_movies: bool,
    /// Car-to-car collisions (`NFS_NO_CARCOL` when off).
    pub car_collisions: bool,
    /// Replacement textures from `Documents/Textures` (`nfs_renderer::set_texture_pack`); from the next start.
    pub texture_packs: bool,
    /// Small textures without a replacement are upscaled 2× as they load (`nfs_renderer::set_texture_enhance`).
    pub enhance_textures: bool,
    /// 16× anisotropic filtering of the mipmapped textures (`nfs_renderer::set_anisotropy`).
    pub anisotropic: bool,
    /// The game's textures are saved to `Documents/Textures/_dump` as they load (`nfs_renderer::set_texture_dump`).
    pub dump_textures: bool,
    /// The force feedback on the iPhone's vibration motor when no pad can rumble (`haptics.rs`).
    pub haptics: bool,
}

impl GameOptions {
    pub const DEFAULT: Self =
        Self { safe_hud: true, resolution: 1.0, fps_cap: 0, fog: true, filtering: true, skip_movies: false,
        car_collisions: true,
        texture_packs: true,
        enhance_textures: false,
        anisotropic: false,
        dump_textures: false,
        haptics: true,
    };

    fn parse(text: &str) -> Self {
        let mut o = Self::DEFAULT;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let on = v == "1" || v.eq_ignore_ascii_case("true");
            match k {
                "safe_hud" => o.safe_hud = on,
                "resolution" => {
                    if let Some(r) = RESOLUTION.iter().map(|r| r.1).find(|r| v.parse::<f32>().is_ok_and(|f| (f - r).abs() < 0.01)) {
                        o.resolution = r;
                    }
                }
                "fps_cap" => o.fps_cap = v.parse().ok().filter(|c| FRAME_RATE.iter().any(|f| f.1 == *c)).unwrap_or(0),
                "fog" => o.fog = on,
                "filtering" => o.filtering = on,
                "skip_movies" => o.skip_movies = on,
                "car_collisions" => o.car_collisions = on,
                "texture_packs" => o.texture_packs = on,
                "enhance_textures" => o.enhance_textures = on,
                "anisotropic" => o.anisotropic = on,
                "dump_textures" => o.dump_textures = on,
                "haptics" => o.haptics = on,
                _ => {}
            }
        }
        o
    }

    fn text(&self) -> String {
        format!(
            "safe_hud={}\nresolution={:.2}\nfps_cap={}\nfog={}\nfiltering={}\nskip_movies={}\ncar_collisions={}\n\
             texture_packs={}\nenhance_textures={}\nanisotropic={}\ndump_textures={}\nhaptics={}\n",
            self.safe_hud as u8,
            self.resolution,
            self.fps_cap,
            self.fog as u8,
            self.filtering as u8,
            self.skip_movies as u8,
            self.car_collisions as u8,
            self.texture_packs as u8,
            self.enhance_textures as u8,
            self.anisotropic as u8,
            self.dump_textures as u8,
            self.haptics as u8
        )
    }
}

/// The options now in use.
static OPTIONS: RwLock<GameOptions> = RwLock::new(GameOptions::DEFAULT);
/// Set when the options changed; taken by [`take_options_changed`].
static OPTIONS_CHANGED: AtomicBool = AtomicBool::new(false);

pub fn options() -> GameOptions {
    OPTIONS.read().map(|o| *o).unwrap_or(GameOptions::DEFAULT)
}

/// The options changed since the last call.
pub fn take_options_changed() -> bool {
    OPTIONS_CHANGED.swap(false, Ordering::Relaxed)
}

fn options_path() -> PathBuf {
    nfs_assets::platform::data_dir().join("ios-options.txt")
}

fn store_options(o: GameOptions) {
    if let Ok(mut c) = OPTIONS.write() {
        if *c == o {
            return;
        }
        *c = o;
    }
    OPTIONS_CHANGED.store(true, Ordering::Relaxed);
    let p = options_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&p, o.text()) {
        tracing::warn!(error = %e, path = %p.display(), "options not saved");
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

/// The device's tilt as a stick's X (−32767…32767), while tilt steering is on.
pub fn tilt() -> i32 {
    TILT.load(Ordering::Relaxed)
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
    let o = std::fs::read_to_string(options_path()).map(|t| GameOptions::parse(&t)).unwrap_or(GameOptions::DEFAULT);
    tracing::info!(?o, "options");
    if let Ok(mut c) = OPTIONS.write() {
        *c = o;
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
const LAYOUT: [(&str, Layout); 2] = [("Split (left steers, right drives)", Layout::Split), ("Gamepad", Layout::Gamepad)];
const PEDALS: [(&str, Pedals); 2] = [("Triggers (analogue)", Pedals::Triggers), ("A / B buttons", Pedals::Buttons)];
const RESOLUTION: [(&str, f32); 3] = [("Full", 1.0), ("75%", 0.75), ("50%", 0.5)];
const FRAME_RATE: [(&str, u32); 3] = [("Unlimited", 0), ("60 fps", 60), ("30 fps", 30)];

struct Controls {
    vc: Retained<UIViewController>,
    layout: Retained<UISegmentedControl>,
    /// the rows that are only Apple's pad's (`Layout::Gamepad`)
    gamepad_only: Vec<Retained<UIView>>,
    steering: Retained<UISegmentedControl>,
    pedals: Retained<UISegmentedControl>,
    xy: Retained<UISwitch>,
    shoulders: Retained<UISwitch>,
    right_stick: Retained<UISwitch>,
    tilt: Retained<UISlider>,
    tilt_row: Retained<UIStackView>,
    legend: Retained<UILabel>,
    resolution: Retained<UISegmentedControl>,
    frame_rate: Retained<UISegmentedControl>,
    safe_hud: Retained<UISwitch>,
    fog: Retained<UISwitch>,
    filtering: Retained<UISwitch>,
    skip_movies: Retained<UISwitch>,
    car_collisions: Retained<UISwitch>,
    texture_packs: Retained<UISwitch>,
    enhance_textures: Retained<UISwitch>,
    anisotropic: Retained<UISwitch>,
    dump_textures: Retained<UISwitch>,
    haptics: Retained<UISwitch>,
    saves_status: Retained<UILabel>,
    pack_button: Retained<UIButton>,
    pack_status: Retained<UILabel>,
    done: Retained<UIButton>,
}

/// What the document picker that is up was opened for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    ExportSaves,
    ImportSaves,
    TexturePack,
}

/// A texture pack import under way.
struct PackJob {
    progress: Arc<nfs_assets::import::Progress>,
    handle: std::thread::JoinHandle<Result<(String, usize), String>>,
    /// the picked URL, read in place: its security scope is held until the import ends
    scoped: Option<Retained<NSURL>>,
}

pub struct Ivars {
    controls: RefCell<Option<Controls>>,
    picking: Cell<Pick>,
    pack_job: RefCell<Option<PackJob>>,
    /// saves were imported: Done closes the app, which reads them at its next start
    restart: Cell<bool>,
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
            let Some((s, o)) = self.read() else { return };
            store(s);
            store_options(o);
            self.show(s);
        }

        #[unsafe(method(done:))]
        fn done(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.ivars().controls.borrow().as_ref() {
                c.vc.dismissViewControllerAnimated_completion(true, None);
            }
            OPEN.store(false, Ordering::Relaxed);
            tracing::info!(s = ?current(), o = ?options(), "settings page closed");
            if self.ivars().restart.get() {
                tracing::info!("closing after the save import");
                crate::guard::end();
                std::process::exit(0);
            }
            PAGE.with(|p| p.borrow_mut().take());
        }

        #[unsafe(method(exportSaves:))]
        fn export_saves(&self, _sender: Option<&AnyObject>) {
            match super::backup::export() {
                Ok(zip) => {
                    let url = NSURL::fileURLWithPath(&NSString::from_str(&zip.to_string_lossy()));
                    let mtm = self.mtm();
                    let picker = UIDocumentPickerViewController::initForExportingURLs_asCopy(
                        UIDocumentPickerViewController::alloc(mtm),
                        &NSArray::from_retained_slice(&[url]),
                        true,
                    );
                    self.ivars().picking.set(Pick::ExportSaves);
                    self.present_picker(&picker);
                }
                Err(e) => self.saves_status(&e),
            }
        }

        #[unsafe(method(importSaves:))]
        fn import_saves(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            // SAFETY: the system type constant
            let zip = unsafe { UTTypeZIP };
            let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
                UIDocumentPickerViewController::alloc(mtm),
                &NSArray::from_slice(&[zip]),
                true,
            );
            picker.setAllowsMultipleSelection(false);
            self.ivars().picking.set(Pick::ImportSaves);
            self.present_picker(&picker);
        }

        #[unsafe(method(importPack:))]
        fn import_pack(&self, _sender: Option<&AnyObject>) {
            if self.ivars().pack_job.borrow().is_some() {
                return;
            }
            let mtm = self.mtm();
            // SAFETY: the system type constants
            let (zip, folder) = unsafe { (UTTypeZIP, UTTypeFolder) };
            // read in place (a pack can be a gigabyte): no copy into the app first
            let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
                UIDocumentPickerViewController::alloc(mtm),
                &NSArray::from_slice(&[zip, folder]),
                false,
            );
            picker.setAllowsMultipleSelection(false);
            self.ivars().picking.set(Pick::TexturePack);
            self.present_picker(&picker);
        }
    }

    unsafe impl NSObjectProtocol for Page {}

    unsafe impl UIDocumentPickerDelegate for Page {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _controller: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            match self.ivars().picking.get() {
                Pick::ExportSaves => {
                    self.saves_status("Saves exported.");
                    return;
                }
                Pick::TexturePack => {
                    if let Some(url) = urls.firstObject() {
                        self.start_pack(url);
                    }
                    return;
                }
                Pick::ImportSaves => {}
            }
            let Some(path) = urls.firstObject().and_then(|u| u.path()) else { return };
            let path = PathBuf::from(path.to_string());
            match super::backup::import(&path) {
                Ok(n) => {
                    self.ivars().restart.set(true);
                    self.saves_status(&format!(
                        "{n} files imported (the saves they replaced are in Files › {} › Save Backups). Tap Done: the app closes, and loads them when you open it again.",
                        crate::brand::TITLE
                    ));
                }
                Err(e) => self.saves_status(&e),
            }
            let _ = std::fs::remove_file(&path);
        }
    }
);

thread_local! {
    /// The page while it is up (it holds its own controls; UIKit holds the target only weakly).
    static PAGE: RefCell<Option<Retained<Page>>> = const { RefCell::new(None) };
}

impl Page {
    fn present_picker(&self, picker: &UIDocumentPickerViewController) {
        let c = self.ivars().controls.borrow();
        let Some(c) = c.as_ref() else { return };
        picker.setDelegate(Some(ProtocolObject::from_ref(self)));
        c.vc.presentViewController_animated_completion(picker, true, None);
    }

    fn start_pack(&self, url: Retained<NSURL>) {
        // SAFETY: plain Foundation calls on a URL the picker handed over
        let scoped = unsafe { url.startAccessingSecurityScopedResource() };
        let Some(src) = url.path().map(|p| PathBuf::from(p.to_string())) else {
            if scoped {
                // SAFETY: balances the start above
                unsafe { url.stopAccessingSecurityScopedResource() };
            }
            self.pack_status("That location cannot be read.");
            return;
        };
        let Some(textures) = nfs_assets::platform::documents_dir().map(|d| d.join("Textures")) else {
            self.pack_status("The app's Textures folder is not available.");
            return;
        };
        tracing::info!(src = %src.display(), scoped, "texture pack picked");
        let progress = Arc::new(nfs_assets::import::Progress::default());
        let p = progress.clone();
        let spawned = std::thread::Builder::new().name("texture pack".into()).spawn(move || {
            std::fs::create_dir_all(&textures).map_err(|e| e.to_string())?;
            nfs_assets::import::import_texture_pack(&src, &textures, &p).map_err(|e| match e {
                nfs_assets::import::ImportError::NoPack(_) => {
                    "That holds no texture pack: no PNG files named after a texture (16 hex digits and .png) in it.".to_string()
                }
                e => e.to_string(),
            })
        });
        match spawned {
            Ok(handle) => {
                *self.ivars().pack_job.borrow_mut() = Some(PackJob { progress, handle, scoped: scoped.then_some(url) });
                self.set_pack_busy(true);
                self.pack_status("Importing the texture pack…");
            }
            Err(e) => {
                if scoped {
                    // SAFETY: balances the start above
                    unsafe { url.stopAccessingSecurityScopedResource() };
                }
                self.pack_status(&format!("The import could not start: {e}"));
            }
        }
    }

    /// One look at the pack import, between frames.
    fn poll_pack(&self) {
        let running = match self.ivars().pack_job.borrow().as_ref() {
            None => return,
            Some(job) => {
                if !job.handle.is_finished() {
                    let (d, t) = (job.progress.done.load(Ordering::Relaxed), job.progress.total.load(Ordering::Relaxed));
                    if t > 0 {
                        self.pack_status(&format!("Importing the texture pack… {d} of {t} textures"));
                    }
                    true
                } else {
                    false
                }
            }
        };
        if running {
            return;
        }
        let Some(mut job) = self.ivars().pack_job.borrow_mut().take() else { return };
        if let Some(url) = job.scoped.take() {
            // SAFETY: balances the start in `start_pack`
            unsafe { url.stopAccessingSecurityScopedResource() };
        }
        self.set_pack_busy(false);
        match job.handle.join().unwrap_or_else(|_| Err("The import stopped with an error.".into())) {
            Ok((name, n)) => {
                // packs on, and the Textures folder read again for what loads from now on
                if let Some(c) = self.ivars().controls.borrow().as_ref() {
                    c.texture_packs.setOn(true);
                }
                if let Some((s, o)) = self.read() {
                    store(s);
                    store_options(o);
                }
                OPTIONS_CHANGED.store(true, Ordering::Relaxed);
                self.pack_status(&format!(
                    "Imported “{name}”: {n} textures, in Files › {} › Textures. The next race uses them; quit and reopen the app to see them in the menus too.",
                    crate::brand::TITLE
                ));
                tracing::info!(name, textures = n, "texture pack imported");
            }
            Err(e) => {
                tracing::warn!(error = %e, "texture pack import failed");
                self.pack_status(&e);
            }
        }
    }

    fn set_pack_busy(&self, busy: bool) {
        if let Some(c) = self.ivars().controls.borrow().as_ref() {
            c.pack_button.setEnabled(!busy);
            // the page stays up until the import ends
            c.done.setEnabled(!busy);
        }
    }

    fn pack_status(&self, text: &str) {
        if let Some(c) = self.ivars().controls.borrow().as_ref() {
            c.pack_status.setText(Some(&NSString::from_str(text)));
            c.pack_status.setHidden(false);
        }
    }

    fn saves_status(&self, text: &str) {
        tracing::info!(text, "save backup");
        if let Some(c) = self.ivars().controls.borrow().as_ref() {
            c.saves_status.setText(Some(&NSString::from_str(text)));
            c.saves_status.setHidden(false);
        }
    }

    fn read(&self) -> Option<(TouchSettings, GameOptions)> {
        let c = self.ivars().controls.borrow();
        let c = c.as_ref()?;
        let pick = |seg: &UISegmentedControl| seg.selectedSegmentIndex().max(0) as usize;
        let o = GameOptions {
            safe_hud: c.safe_hud.isOn(),
            resolution: RESOLUTION.get(pick(&c.resolution)).map_or(1.0, |r| r.1),
            fps_cap: FRAME_RATE.get(pick(&c.frame_rate)).map_or(0, |f| f.1),
            fog: c.fog.isOn(),
            filtering: c.filtering.isOn(),
            skip_movies: c.skip_movies.isOn(),
            car_collisions: c.car_collisions.isOn(),
            texture_packs: c.texture_packs.isOn(),
            enhance_textures: c.enhance_textures.isOn(),
            anisotropic: c.anisotropic.isOn(),
            dump_textures: c.dump_textures.isOn(),
            haptics: c.haptics.isOn(),
        };
        let s = TouchSettings {
            layout: LAYOUT.get(pick(&c.layout)).map_or(Layout::Split, |l| l.1),
            steering: STEERING.get(pick(&c.steering)).map_or(Steering::Stick, |s| s.1),
            pedals: PEDALS.get(pick(&c.pedals)).map_or(Pedals::Triggers, |p| p.1),
            xy: c.xy.isOn(),
            shoulders: c.shoulders.isOn(),
            right_stick: c.right_stick.isOn(),
            // in steps of 0.05
            tilt_sensitivity: (c.tilt.value() * 20.0).round() / 20.0,
        };
        Some((s, o))
    }

    /// The parts of the page that follow the settings.
    fn show(&self, s: TouchSettings) {
        if let Some(c) = self.ivars().controls.borrow().as_ref() {
            c.legend.setText(Some(&NSString::from_str(&s.legend())));
            c.tilt_row.setHidden(s.steering != Steering::Tilt);
            for v in &c.gamepad_only {
                v.setHidden(s.layout != Layout::Gamepad);
            }
        }
    }
}

/// Follows a texture pack import on the page, if one is under way (called between frames).
pub fn poll() {
    PAGE.with(|p| {
        if let Some(page) = p.borrow().as_ref() {
            page.poll_pack();
        }
    });
}

/// Opens the page over `presenter`.
pub fn present(presenter: &UIViewController) {
    if is_open() {
        return;
    }
    let mtm = MainThreadMarker::new().expect("main thread");
    let page = Page::alloc(mtm).set_ivars(Ivars {
        controls: RefCell::new(None),
        picking: Cell::new(Pick::ExportSaves),
        pack_job: RefCell::new(None),
        restart: Cell::new(false),
    });
    // SAFETY: `NSObject`'s designated initialiser
    let page: Retained<Page> = unsafe { msg_send![super(page), init] };
    let s = current();
    let controls = build(mtm, &page, s);
    let vc = controls.vc.clone();
    *page.ivars().controls.borrow_mut() = Some(controls);
    page.show(s);
    PAGE.with(|p| *p.borrow_mut() = Some(page));
    OPEN.store(true, Ordering::Relaxed);
    tracing::info!("settings page opened");
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

    let title = label(mtm, "Settings", UIFont::boldSystemFontOfSize(26.0), 1.0);
    let o = options();
    let touch_h = label(mtm, "Touch controls", UIFont::boldSystemFontOfSize(21.0), 1.0);
    let note = label(
        mtm,
        "The on-screen controls appear in a race when no game controller is connected.",
        UIFont::systemFontOfSize(14.0),
        0.65,
    );

    let layout_l = label(mtm, "Layout", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let layout = segments(mtm, page, &LAYOUT.map(|l| l.0), LAYOUT.iter().position(|x| x.1 == s.layout).unwrap_or(0));
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

    let display_h = label(mtm, "Display", UIFont::boldSystemFontOfSize(21.0), 1.0);
    let resolution_l = label(mtm, "Resolution (lower: faster, cooler, longer battery)", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let resolution = segments(mtm, page, &RESOLUTION.map(|r| r.0), RESOLUTION.iter().position(|x| x.1 == o.resolution).unwrap_or(0));
    let frame_rate_l = label(mtm, "Frame rate", UIFont::boldSystemFontOfSize(15.0), 0.75);
    let frame_rate = segments(mtm, page, &FRAME_RATE.map(|f| f.0), FRAME_RATE.iter().position(|x| x.1 == o.fps_cap).unwrap_or(0));
    let (safe_row, safe_hud) = switch_row(mtm, page, "Keep the race HUD clear of the camera cutout", o.safe_hud);
    let (fog_row, fog) = switch_row(mtm, page, "Weather fog", o.fog);
    let (filter_row, filtering) = switch_row(mtm, page, "Smooth textures (from the next start)", o.filtering);
    let game_h = label(mtm, "Game", UIFont::boldSystemFontOfSize(21.0), 1.0);
    let (movies_row, skip_movies) = switch_row(mtm, page, "Skip the intro movies", o.skip_movies);
    let (col_row, car_collisions) = switch_row(mtm, page, "Car-to-car collisions", o.car_collisions);
    let (haptics_row, haptics) = switch_row(mtm, page, "Vibrate with the force feedback (when no controller rumbles)", o.haptics);
    // an iPad has no vibration motor
    haptics_row.setHidden(!super::haptics::supported());
    let saves_h = label(mtm, "Saves", UIFont::boldSystemFontOfSize(21.0), 1.0);
    let saves_note = label(
        mtm,
        &format!(
            "Export puts your saves in a .zip you can keep in iCloud Drive or another app. Import replaces the saves with such a .zip; the ones it replaces are kept in Files › {} › Save Backups.",
            crate::brand::TITLE
        ),
        UIFont::systemFontOfSize(13.0),
        0.6,
    );
    let export_b = plain_button(mtm, page, "Export Saves…", sel!(exportSaves:));
    let import_b = plain_button(mtm, page, "Import Saves…", sel!(importSaves:));
    let saves_buttons = stack(mtm, &[&export_b, &import_b], UILayoutConstraintAxis::Horizontal, 12.0);
    saves_buttons.setDistribution(UIStackViewDistribution::FillEqually);
    let saves_status = label(mtm, "", UIFont::systemFontOfSize(14.0), 0.92);
    saves_status.setHidden(true);
    let tex_h = label(mtm, "Textures (from the next start)", UIFont::boldSystemFontOfSize(21.0), 1.0);
    let (packs_row, texture_packs) = switch_row(mtm, page, &format!("Texture packs (Files › {} › Textures)", crate::brand::TITLE), o.texture_packs);
    let (enhance_row, enhance_textures) = switch_row(mtm, page, "Enhanced textures (2× upscale, experimental)", o.enhance_textures);
    let (aniso_row, anisotropic) = switch_row(mtm, page, "Sharper road at a distance (anisotropic filtering)", o.anisotropic);
    let (dump_row, dump_textures) = switch_row(mtm, page, "Save the game's textures to Textures/_dump (for pack makers)", o.dump_textures);
    let tex_note = label(
        mtm,
        "Import takes a pack's .zip (or folder) from Files. A pack is PNG files named after the texture they replace (the names Textures/_dump uses), at any size; you can also copy its folder into Textures yourself. Folders whose name starts with _ are skipped.",
        UIFont::systemFontOfSize(13.0),
        0.6,
    );
    let pack_button = plain_button(mtm, page, "Import Texture Pack…", sel!(importPack:));
    // empty (no height) until an import says something
    let pack_status = label(mtm, "", UIFont::systemFontOfSize(14.0), 0.92);
    let ini_note = label(
        mtm,
        "Fog, smooth textures and the intro movies follow the game folder's nfs5.ini too: an option turned off there stays off.",
        UIFont::systemFontOfSize(13.0),
        0.6,
    );

    let done = UIButton::buttonWithType(UIButtonType::System, mtm);
    let config = UIButtonConfiguration::filledButtonConfiguration(mtm);
    config.setTitle(Some(&NSString::from_str("Done")));
    done.setConfiguration(Some(&config));
    // SAFETY: as in `segments`
    unsafe { done.addTarget_action_forControlEvents(Some(page), sel!(done:), UIControlEvents::TouchUpInside) };

    let views: [&UIView; 42] = [
        &title, &touch_h, &note, &layout_l, &layout, &steering_l, &steering, &tilt_row, &pedals_l, &pedals, &extra_l, &xy_row, &sh_row, &rs_row, &legend_l,
        &legend, &display_h, &resolution_l, &resolution, &frame_rate_l, &frame_rate, &safe_row, &fog_row, &filter_row, &game_h,
        &movies_row, &col_row, &haptics_row, &tex_h, &packs_row, &enhance_row, &aniso_row, &dump_row, &tex_note, &pack_button,
        &pack_status, &saves_h, &saves_note,
        &saves_buttons, &saves_status, &ini_note, &done,
    ];
    let content = stack(mtm, &views, UILayoutConstraintAxis::Vertical, 12.0);
    content.setAlignment(UIStackViewAlignment::Fill);
    content.setCustomSpacing_afterView(20.0, &note);
    content.setCustomSpacing_afterView(20.0, &layout);
    content.setCustomSpacing_afterView(20.0, &tilt_row);
    content.setCustomSpacing_afterView(20.0, &pedals);
    content.setCustomSpacing_afterView(20.0, &rs_row);
    content.setCustomSpacing_afterView(32.0, &legend);
    content.setCustomSpacing_afterView(20.0, &title);
    content.setCustomSpacing_afterView(20.0, &frame_rate);
    content.setCustomSpacing_afterView(32.0, &filter_row);
    content.setCustomSpacing_afterView(32.0, &haptics_row);
    content.setCustomSpacing_afterView(32.0, &pack_status);
    content.setCustomSpacing_afterView(20.0, &saves_buttons);
    content.setCustomSpacing_afterView(20.0, &saves_status);
    content.setCustomSpacing_afterView(24.0, &ini_note);
    content.setTranslatesAutoresizingMaskIntoConstraints(false);
    title.setTextAlignment(NSTextAlignment::Center);
    note.setTextAlignment(NSTextAlignment::Left);

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

    let gamepad_only: Vec<Retained<UIView>> =
        [Retained::into_super(pedals_l.clone()), Retained::into_super(Retained::into_super(pedals.clone())),
            Retained::into_super(extra_l.clone()), Retained::into_super(xy_row.clone()),
            Retained::into_super(sh_row.clone()), Retained::into_super(rs_row.clone())]
        .into();
    Controls {
        vc,
        layout,
        gamepad_only,
        steering,
        pedals,
        xy,
        shoulders,
        right_stick,
        tilt,
        tilt_row,
        legend,
        resolution,
        frame_rate,
        safe_hud,
        fog,
        filtering,
        skip_movies,
        car_collisions,
        texture_packs,
        enhance_textures,
        anisotropic,
        dump_textures,
        haptics,
        saves_status,
        pack_button,
        pack_status,
        done,
    }
}

fn plain_button(mtm: MainThreadMarker, page: &Page, title: &str, action: objc2::runtime::Sel) -> Retained<UIButton> {
    let b = UIButton::buttonWithType(UIButtonType::System, mtm);
    let config = UIButtonConfiguration::grayButtonConfiguration(mtm);
    config.setTitle(Some(&NSString::from_str(title)));
    b.setConfiguration(Some(&config));
    // SAFETY: as in `segments`
    unsafe { b.addTarget_action_forControlEvents(Some(page), action, UIControlEvents::TouchUpInside) };
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let s = TouchSettings { layout: Layout::Gamepad, steering: Steering::Tilt, pedals: Pedals::Buttons, xy: false, shoulders: true, right_stick: true, tilt_sensitivity: 1.5 };
        assert_eq!(TouchSettings::parse(&s.text()), s);
        assert_eq!(TouchSettings::parse(""), TouchSettings::DEFAULT);
        let o = GameOptions { safe_hud: false, resolution: 0.75, fps_cap: 30, fog: false, filtering: false, skip_movies: true,
            car_collisions: false,
            texture_packs: false,
            enhance_textures: true,
            anisotropic: true,
            dump_textures: true,
            haptics: false,
        };
        assert_eq!(GameOptions::parse(&o.text()), o);
        assert_eq!(GameOptions::parse("resolution=0.3\nfps_cap=45"), GameOptions::DEFAULT);
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
