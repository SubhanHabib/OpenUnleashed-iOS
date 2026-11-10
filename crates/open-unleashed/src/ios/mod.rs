//! **PLATFORM GLUE (iOS / iPadOS)**, not in the original: the app shell around the game.
//!
//! The app bundle carries no game data. On iOS, winit's `run_app` is `UIApplicationMain`, which never
//! returns, and nothing can be drawn before it runs. So the event loop is started first, under a `Root`
//! handler, and the game is built inside it:
//!
//! * **Booting**: a game folder or `.zip` the player dropped into the app's `Documents` (the Files app's
//!   "On My iPad › Open Unleashed") is imported first. Otherwise, game data already imported
//!   (`Library/Application Support/game`) starts the game at once.
//! * **Import** (`importer.rs`): a UIKit screen that asks for the installed game's folder or a `.zip` of it,
//!   through the system document picker. It copies or unpacks the data into the app's own storage, then
//!   starts the game.
//! * **Game**: the desktop `App`, built by `crate::prepare`. Every winit event is handed to it, after touch
//!   is turned into the mouse the front end already reads (`touch.rs`). An on-screen controller is shown
//!   in a race when no physical pad is connected; its layout is set on the touch-controls page (`settings.rs`).

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
                app.resumed(el);
                if let Some(w) = app.intro.as_ref().map(|r| &r.window).or(app.window.as_ref()) {
                    tracing::info!(inner = ?w.inner_size(), outer = ?w.outer_size(), scale = w.scale_factor(), "game window");
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
                if settings::take_changed() {
                    self.pad.rebuild();
                }
                self.pad.update(app);
                self.tilt.update(self.pad.connected() && settings::current().steering == settings::Steering::Tilt);
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
