//! **PLATFORM GLUE (iOS / iPadOS)**, not in the original: the app shell around the game.
//!
//! The app bundle carries no game data. On iOS, winit's `run_app` is `UIApplicationMain`, which never
//! returns, and nothing can be drawn before it runs. So the event loop is started first, under a `Root`
//! handler, and the game is built inside it:
//!
//! * **Booting**: a game folder or `.zip` the player dropped into the app's `Documents` (the Files app's
//!   "On My iPhone / iPad › Open Unleashed") is imported first. Otherwise, game data already imported
//!   (`Library/Application Support/game`) starts the game at once.
//! * **Import** (`importer.rs`): a UIKit screen that asks for the installed game's folder or a `.zip` of it,
//!   through the system document picker. It copies or unpacks the data into the app's own storage, then
//!   starts the game.
//! * **Game**: the desktop `App`, built by `crate::prepare`. Every winit event is handed to it, after touch
//!   is turned into the mouse the front end already reads (`touch.rs`). An on-screen controller is shown
//!   in a race when no physical pad is connected; its layout is set on the touch-controls page (`settings.rs`).

mod backup;
mod deck;
mod haptics;
mod importer;
pub(crate) mod settings;
pub(crate) mod touch;

use crate::App;
use anyhow::Result;
use nfs_assets::GamePaths;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

/// Runs the app; on iOS this never returns (the process ends in `UIApplicationMain`).
pub fn run() -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    settings::init();
    let mut root = Root {
        state: State::Booting,
        touch: touch::Touch::default(),
        pad: touch::VirtualPad::default(),
        deck: deck::Deck::default(),
        keyboard: touch::Keyboard::default(),
        tilt: settings::Tilt::default(),
        glue_settings_done: false,
        background: false,
    };
    event_loop.run_app(&mut root)?;
    Ok(())
}

enum State {
    Booting,
    Import(importer::Importer),
    Game(Box<App>),
}

struct Root {
    state: State,
    touch: touch::Touch,
    pad: touch::VirtualPad,
    /// the split on-screen controls (`settings::Layout::Split`)
    deck: deck::Deck,
    keyboard: touch::Keyboard,
    tilt: settings::Tilt,
    glue_settings_done: bool,
    /// The app is in the background: iOS forbids GPU work there, so no frame is drawn.
    background: bool,
}

impl Root {
    fn boot(&mut self, el: &ActiveEventLoop) {
        if let Some(src) = importer::documents_drop_in() {
            tracing::info!(src = %src.display(), "game data dropped into Documents: importing it");
            let mut imp = importer::Importer::new(el, None);
            imp.start(src, true);
            self.import_state(el, imp);
            return;
        }
        match GamePaths::locate() {
            Ok(paths) => self.start_game(el, paths, None),
            Err(e) => {
                tracing::info!(error = %e, "no game data yet: showing the import screen");
                let imp = importer::Importer::new(el, None);
                self.import_state(el, imp);
            }
        }
    }

    fn import_state(&mut self, el: &ActiveEventLoop, imp: importer::Importer) {
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100)));
        self.state = State::Import(imp);
    }

    /// Builds the game from `paths`. On an error, the import screen comes back with it, so other data can be chosen.
    fn start_game(&mut self, el: &ActiveEventLoop, paths: GamePaths, imp: Option<importer::Importer>) {
        match crate::prepare(paths, |_| Ok(None)) {
            Ok(Some((mut app, _))) => {
                // the game makes its own window; the import screen's goes
                drop(imp);
                el.set_control_flow(ControlFlow::Poll);
                // the options the renderer is made with (filtering) and the start-up reads (the intro movies)
                Self::apply_options(&mut app);
                app.resumed(el);
                app.apply_render_size();
                if let Some(w) = app.intro.as_ref().map(|r| &r.window).or(app.window.as_ref()) {
                    tracing::info!(inner = ?w.inner_size(), outer = ?w.outer_size(), scale = w.scale_factor(), insets = ?safe_insets(w), "game window");
                }
                self.state = State::Game(Box::new(app));
            }
            Ok(None) => std::process::exit(0),
            Err(e) => {
                tracing::error!("the game data could not be loaded: {e:#}");
                let msg = format!("The game data could not be loaded:\n{e:#}\n\nChoose another copy of the game.");
                let imp = match imp {
                    Some(mut imp) => {
                        imp.show_error(&msg);
                        imp
                    }
                    None => importer::Importer::new(el, Some(&msg)),
                };
                self.import_state(el, imp);
            }
        }
    }

    /// The settings page's display and game options, onto the game ([`settings::GameOptions`]). The `nfs5.ini`
    /// ones combine with the file's: an option the file turns off stays off.
    fn apply_options(app: &mut App) {
        let o = settings::options();
        let ini = app.paths.nfs5_ini();
        app.render_scale = o.resolution;
        app.apply_render_size();
        app.fps_cap = (o.fps_cap > 0).then_some(o.fps_cap as f64);
        app.no_carcol = !o.car_collisions;
        app.ini.fog = ini.fog && o.fog;
        app.ini.no_movies = ini.no_movies || o.skip_movies;
        // read when the renderer is made: from the next start
        nfs_renderer::set_point_filter(!(ini.filtering && o.filtering));
        Self::apply_textures(&o);
        Self::apply_haptics(app, o.haptics);
        tracing::info!(?o, "options applied");
    }

    /// The force feedback on the iPhone's vibration motor (`haptics.rs`) while no pad can rumble; the engine is made
    /// once, when the option turns on.
    fn apply_haptics(app: &mut App, on: bool) {
        thread_local! {
            static ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
        }
        if ON.with(|c| c.replace(Some(on))) == Some(on) {
            return;
        }
        let phone = on.then(haptics::PhoneRumble::new).flatten();
        tracing::info!(on, available = phone.is_some(), "phone vibration");
        app.gamepads.set_rumble_fallback(phone.map(|mut p| {
            let f: nfs_input::gamepad::RumbleFallback = Box::new(move |low, high, ms| p.rumble(low, high, ms));
            ("the iPhone's vibration motor".to_string(), f)
        }));
    }

    /// The texture options, read as textures load: they take hold for what loads after (in full, from the next start).
    fn apply_textures(o: &settings::GameOptions) {
        let Some(dir) = nfs_assets::platform::documents_dir().map(|d| d.join("Textures")) else { return };
        if std::fs::create_dir_all(&dir).is_ok() && !dir.join("README.txt").exists() {
            let _ = std::fs::write(dir.join("README.txt"), TEXTURES_README);
        }
        let packs = if o.texture_packs { vec![dir.clone()] } else { Vec::new() };
        nfs_renderer::set_texture_pack(&packs);
        nfs_renderer::set_texture_dump(o.dump_textures.then(|| dir.join("_dump")));
        nfs_renderer::set_texture_enhance(o.enhance_textures);
        nfs_renderer::set_anisotropy(if o.anisotropic { 16 } else { 1 });
    }

    /// `event_loop.exit()` (the front end's quit) does not end an iOS app; the process is ended here instead,
    /// once the game has finished what it does on its way out.
    fn check_exit(app: &mut App, el: &ActiveEventLoop) {
        if el.exiting() {
            app.exiting(el);
            crate::guard::end();
            std::process::exit(0);
        }
    }
}

impl ApplicationHandler for Root {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.background = false;
        match &mut self.state {
            State::Booting => self.boot(el),
            State::Import(_) => {}
            State::Game(app) => {
                app.resumed(el);
                if let Some(w) = app.window.as_ref() {
                    w.request_redraw();
                }
            }
        }
    }

    fn suspended(&mut self, _el: &ActiveEventLoop) {
        self.background = true;
        if let State::Game(app) = &mut self.state {
            app.input.release_all();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match &mut self.state {
            State::Booting => {}
            State::Import(imp) => imp.window_event(&event),
            State::Game(app) => {
                if let WindowEvent::Resized(sz) = &event {
                    tracing::info!(?sz, "window resized");
                }
                if self.background && matches!(event, WindowEvent::RedrawRequested) {
                    return;
                }
                if let WindowEvent::Touch(t) = &event {
                    self.touch.event(app, el, id, t);
                } else {
                    let event = touch::named_keys(event);
                    let key = matches!(event, WindowEvent::KeyboardInput { .. }).then(|| event.clone());
                    let redraw = matches!(event, WindowEvent::RedrawRequested);
                    app.window_event(el, id, event);
                    if let Some(k) = key {
                        self.keyboard.key(app, &k);
                    }
                    if redraw {
                        self.touch.frame(app, el);
                    }
                }
                Self::check_exit(app, el);
            }
        }
    }

    fn device_event(&mut self, el: &ActiveEventLoop, id: DeviceId, event: DeviceEvent) {
        if let State::Game(app) = &mut self.state {
            app.device_event(el, id, event);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        match &mut self.state {
            State::Import(imp) => {
                el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100)));
                if let Some(paths) = imp.poll(el) {
                    let State::Import(imp) = std::mem::replace(&mut self.state, State::Booting) else { unreachable!() };
                    self.start_game(el, paths, Some(imp));
                }
            }
            State::Game(app) => {
                // TEST GLUE: `OPEN_UNLEASHED_TOUCH_SETTINGS` opens the page as a three-finger tap does, once a race is
                // under way (the simulator cannot make a three-finger tap)
                let glue = !self.glue_settings_done
                    && app.vehicle.is_some()
                    && std::env::var_os("OPEN_UNLEASHED_TOUCH_SETTINGS").is_some();
                self.glue_settings_done |= glue;
                if (self.touch.take_three_finger_tap() || glue) && !settings::is_open() {
                    // a race pauses first (a network race cannot pause, and goes on behind the page)
                    if app.vehicle.is_some() {
                        app.touch_back(el);
                    }
                    if let Some(vc) = app.window.as_ref().and_then(|w| importer::view_controller(w)) {
                        settings::present(&vc);
                    }
                }
                settings::poll();
                if settings::take_changed() {
                    self.pad.rebuild();
                    self.deck.rebuild();
                }
                if settings::take_options_changed() {
                    Self::apply_options(app);
                }
                // the race HUD keeps clear of the camera cutout and the home indicator (`App::hud_safe`)
                app.hud_inset = match app.window.as_ref() {
                    Some(w) if settings::options().safe_hud => safe_insets(w),
                    _ => [0.0; 4],
                };
                let split = settings::current().layout == settings::Layout::Split;
                self.deck.update(app, split);
                // the gauges move left of the on-screen pedals
                app.hud_clear_right = match self.deck.clear_right() {
                    Some(r) if settings::current().gauges_aside => r as f32,
                    _ => 0.0,
                };
                if split {
                    // Apple's pad off
                    self.pad.rebuild();
                } else {
                    self.pad.update(app);
                }
                let touch_on = self.pad.connected() || self.deck.shown();
                self.tilt.update(touch_on && settings::current().steering == settings::Steering::Tilt);
                if self.touch.take_pressed() {
                    self.keyboard.touched(app);
                }
                self.keyboard.update(app);
                Self::check_exit(app, el);
                // winit 0.30 turns `request_redraw` on a view it does not see as Metal-backed (wgpu adds its
                // `CAMetalLayer` as a sublayer) into `setNeedsDisplay`, which UIKit drops when it comes from inside
                // `drawRect:` itself, the game's own redraw request after each frame. Asked again here, outside the
                // draw, the next frame comes on the next display refresh.
                if !self.background {
                    let window = app.intro.as_ref().map(|r| &r.window).or(app.window.as_ref());
                    if let Some(w) = window {
                        w.request_redraw();
                    }
                }
            }
            State::Booting => {}
        }
    }

    fn exiting(&mut self, el: &ActiveEventLoop) {
        if let State::Game(app) = &mut self.state {
            app.exiting(el);
        }
    }

    fn memory_warning(&mut self, _el: &ActiveEventLoop) {
        tracing::warn!("iOS memory warning");
    }
}

/// The safe area's insets in logical pixels, `[left, top, right, bottom]`: winit's inner rectangle is the safe area
/// within the whole view (the outer one).
fn safe_insets(w: &winit::window::Window) -> [f32; 4] {
    let s = w.scale_factor();
    let (Ok(i), Ok(o)) = (w.inner_position(), w.outer_position()) else { return [0.0; 4] };
    let (is, os) = (w.inner_size(), w.outer_size());
    let l = (i.x - o.x) as f64 / s;
    let t = (i.y - o.y) as f64 / s;
    let r = (os.width as f64 - is.width as f64) / s - l;
    let b = (os.height as f64 - is.height as f64) / s - t;
    [l, t, r, b].map(|v| v.max(0.0) as f32)
}

/// Written to `Documents/Textures` the first time it is made.
const TEXTURES_README: &str = "Open Unleashed texture packs

Put a pack here as a folder of PNG files. Each file replaces one of the game's textures and is
named after it: 16 hex digits and .png, for example 3f2a9c0d11e4b7a5.png. The file can be any
size (up to 8192 pixels a side); bigger is sharper. Folders inside a pack are read too. When two
packs replace the same texture, the one whose folder name sorts first wins.

To find a texture's name, turn on \"Save the game's textures\" in the settings page, play, and
look in Textures/_dump: every texture the game loads is saved there under its name. Copy the ones
you want to change into a pack folder and edit them. Folders whose name starts with _ (like
_dump) are never read as packs.

Instead of copying a folder here, you can also pick a pack's .zip (or folder) with Import Texture
Pack in the settings page; it is unpacked into a folder here named after it.

Packs and the texture options take hold from the next start of the app.
";

/// The Files app's name for this device's own storage: "On My iPhone", or "On My iPad" on an iPad.
pub(crate) fn on_my_device() -> &'static str {
    let Some(cls) = objc2::runtime::AnyClass::get(c"UIDevice") else { return "On My iPhone" };
    // SAFETY: UIKit's documented `+[UIDevice currentDevice]` and `-userInterfaceIdiom` (an `NSInteger`; 1 is the iPad)
    let idiom: isize = unsafe {
        let d: objc2::rc::Retained<objc2::runtime::AnyObject> = objc2::msg_send![cls, currentDevice];
        objc2::msg_send![&*d, userInterfaceIdiom]
    };
    if idiom == 1 { "On My iPad" } else { "On My iPhone" }
}
