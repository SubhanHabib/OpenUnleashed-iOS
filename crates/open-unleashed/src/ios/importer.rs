//! **PLATFORM GLUE (iOS)**: the screen that brings the player's own game data into the app.
//!
//! The player picks the installed game's folder, or a `.zip` of it, with the system document picker
//! (Files: the device, iCloud Drive, a USB drive, a server). It is read in place, through its
//! security-scoped URL, and copied or unpacked into `Library/Application Support/game`
//! (`nfs_assets::import`). That folder is kept out of iCloud backups: it is the player's own copy of a
//! game they can import again. A game folder or `.zip` dropped into the app's `Documents` (shared with
//! the Files app) is moved in at the next start instead, without a second copy.

use nfs_assets::GamePaths;
use nfs_assets::import::{self, Progress};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{NSArray, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL, NSURLIsExcludedFromBackupKey};
use objc2_ui_kit::{
    NSTextAlignment, UIButton, UIButtonConfiguration, UIButtonType, UIColor, UIControlEvents, UIDocumentPickerDelegate,
    UIDocumentPickerViewController, UIFont, UILabel, UILayoutConstraintAxis, UIProgressView, UIProgressViewStyle,
    UIStackView, UIStackViewAlignment, UIView, UIViewController,
};
use objc2_uniform_type_identifiers::{UTTypeFolder, UTTypeZIP};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

fn intro() -> String {
    format!(
        "This app contains no game data. To play, you need your own copy of Need for Speed: Porsche Unleashed (PC).\n\n\
         Choose the game's installed folder (the one with GameData and FEData in it), or a .zip of that folder. \
         It is copied into the app once; you can delete the original afterwards.\n\n\
         You can also copy the folder or .zip into this app's folder in the Files app (On My iPad › {}), then open the app again.",
        crate::brand::TITLE
    )
}

pub struct Ivars {
    picked: RefCell<Option<Retained<NSURL>>>,
    presenter: RefCell<Option<Retained<UIViewController>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Delegate` does not implement `Drop`
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OUImportDelegate"]
    #[ivars = Ivars]
    pub struct Delegate;

    impl Delegate {
        #[unsafe(method(choose:))]
        fn choose(&self, _sender: Option<&AnyObject>) {
            self.present_picker();
        }

        #[unsafe(method(controls:))]
        fn controls(&self, _sender: Option<&AnyObject>) {
            if let Some(p) = self.ivars().presenter.borrow().as_ref() {
                super::settings::present(p);
            }
        }
    }

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UIDocumentPickerDelegate for Delegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _controller: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            *self.ivars().picked.borrow_mut() = urls.firstObject();
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, presenter: Option<Retained<UIViewController>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { picked: RefCell::new(None), presenter: RefCell::new(presenter) });
        // SAFETY: `NSObject`'s designated initialiser
        unsafe { msg_send![super(this), init] }
    }

    fn present_picker(&self) {
        let mtm = self.mtm();
        let Some(presenter) = self.ivars().presenter.borrow().clone() else { return };
        // SAFETY: the two system type constants
        let (folder, zip) = unsafe { (UTTypeFolder, UTTypeZIP) };
        let types = NSArray::from_slice(&[folder, zip]);
        let picker =
            UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(UIDocumentPickerViewController::alloc(mtm), &types, false);
        picker.setAllowsMultipleSelection(false);
        picker.setDelegate(Some(ProtocolObject::from_ref(self)));
        presenter.presentViewController_animated_completion(&picker, true, None);
    }
}

struct Job {
    progress: Arc<Progress>,
    handle: Option<JoinHandle<Result<(), String>>>,
    /// The picked URL, whose security-scoped access lasts as long as the job.
    scoped: Option<Retained<NSURL>>,
}

pub struct Importer {
    window: Option<Window>,
    delegate: Retained<Delegate>,
    button: Option<Retained<UIButton>>,
    progress_bar: Option<Retained<UIProgressView>>,
    status: Option<Retained<UILabel>>,
    job: Option<Job>,
    /// Frames to wait before the game starts, so the "Starting" text is on screen while it loads.
    starting: Option<(u8, GamePaths)>,
}

impl Importer {
    /// Opens the import screen; `error` is shown under the button.
    pub fn new(el: &ActiveEventLoop, error: Option<&str>) -> Self {
        let mtm = MainThreadMarker::new().expect("main thread");
        let attrs = super::touch::window_attributes(Window::default_attributes().with_title(crate::brand::TITLE));
        let window = el.create_window(attrs).map_err(|e| tracing::error!(error = %e, "import window")).ok();
        let presenter = window.as_ref().and_then(view_controller);
        let delegate = Delegate::new(mtm, presenter.clone());
        let mut me = Self { window, delegate, button: None, progress_bar: None, status: None, job: None, starting: None };
        if let Some(vc) = presenter {
            me.build(mtm, &vc);
        }
        if let Some(e) = error {
            me.show_error(e);
        }
        me
    }

    fn build(&mut self, mtm: MainThreadMarker, vc: &UIViewController) {
        let Some(root) = vc.view() else { return };
        root.setBackgroundColor(Some(&UIColor::colorWithRed_green_blue_alpha(0.06, 0.06, 0.07, 1.0)));

        let title = label(mtm, crate::brand::TITLE, UIFont::boldSystemFontOfSize(34.0), &UIColor::whiteColor());
        let body = label(mtm, &intro(), UIFont::systemFontOfSize(17.0), &UIColor::colorWithWhite_alpha(0.82, 1.0));

        let button = UIButton::buttonWithType(UIButtonType::System, mtm);
        let config = UIButtonConfiguration::filledButtonConfiguration(mtm);
        config.setTitle(Some(&NSString::from_str("Choose Game Folder or .zip")));
        button.setConfiguration(Some(&config));
        // SAFETY: `choose:` is a method of the target, which outlives the button's use (`Importer` owns both)
        unsafe { button.addTarget_action_forControlEvents(Some(&self.delegate), sel!(choose:), UIControlEvents::TouchUpInside) };

        let controls = UIButton::buttonWithType(UIButtonType::System, mtm);
        let config = UIButtonConfiguration::grayButtonConfiguration(mtm);
        config.setTitle(Some(&NSString::from_str("Touch Controls…")));
        controls.setConfiguration(Some(&config));
        // SAFETY: as for `choose:`
        unsafe { controls.addTarget_action_forControlEvents(Some(&self.delegate), sel!(controls:), UIControlEvents::TouchUpInside) };

        let bar = UIProgressView::initWithProgressViewStyle(UIProgressView::alloc(mtm), UIProgressViewStyle::Default);
        bar.setHidden(true);
        let status = label(mtm, "", UIFont::systemFontOfSize(15.0), &UIColor::colorWithWhite_alpha(0.65, 1.0));

        let views: [&UIView; 6] = [&title, &body, &button, &controls, &bar, &status];
        let stack = UIStackView::initWithArrangedSubviews(UIStackView::alloc(mtm), &NSArray::from_slice(&views));
        stack.setAxis(UILayoutConstraintAxis::Vertical);
        stack.setAlignment(UIStackViewAlignment::Center);
        stack.setSpacing(22.0);
        stack.setTranslatesAutoresizingMaskIntoConstraints(false);
        root.addSubview(&stack);
        let guide = root.safeAreaLayoutGuide();
        for c in [
            stack.centerXAnchor().constraintEqualToAnchor(&guide.centerXAnchor()),
            stack.centerYAnchor().constraintEqualToAnchor(&guide.centerYAnchor()),
            stack.widthAnchor().constraintLessThanOrEqualToConstant(640.0),
            stack.leadingAnchor().constraintGreaterThanOrEqualToAnchor_constant(&guide.leadingAnchor(), 24.0),
            body.widthAnchor().constraintEqualToAnchor(&stack.widthAnchor()),
            status.widthAnchor().constraintEqualToAnchor(&stack.widthAnchor()),
            bar.widthAnchor().constraintEqualToAnchor_multiplier(&stack.widthAnchor(), 0.8),
        ] {
            c.setActive(true);
        }
        self.button = Some(button);
        self.progress_bar = Some(bar);
        self.status = Some(status);
    }

    pub fn window_event(&mut self, _event: &WindowEvent) {}

    pub fn show_error(&mut self, text: &str) {
        self.set_busy(false);
        self.set_status(text, true);
    }

    fn set_status(&self, text: &str, error: bool) {
        if let Some(s) = &self.status {
            s.setText(Some(&NSString::from_str(text)));
            let c = if error { UIColor::colorWithRed_green_blue_alpha(1.0, 0.45, 0.4, 1.0) } else { UIColor::colorWithWhite_alpha(0.65, 1.0) };
            // SAFETY: a plain colour
            unsafe { s.setTextColor(Some(&c)) };
        }
    }

    fn set_busy(&self, busy: bool) {
        if let Some(b) = &self.button {
            b.setEnabled(!busy);
        }
        if let Some(p) = &self.progress_bar {
            p.setHidden(!busy);
            p.setProgress_animated(0.0, false);
        }
    }

    /// Imports the game at `src` (a folder or a `.zip`). `drop_in`: it is the player's drop into `Documents`,
    /// moved rather than copied, and removed once imported.
    pub fn start(&mut self, src: PathBuf, drop_in: bool) {
        self.start_job(src, drop_in, None);
    }

    fn start_job(&mut self, src: PathBuf, drop_in: bool, scoped: Option<Retained<NSURL>>) {
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let handle = std::thread::Builder::new()
            .name("import".into())
            .spawn(move || run_import(&src, drop_in, &p))
            .map_err(|e| tracing::error!(error = %e, "import thread"))
            .ok();
        self.set_busy(true);
        self.set_status("Looking for the game…", false);
        self.job = Some(Job { progress, handle, scoped });
    }

    /// One look at the screen's state, between events. `Some`: the game data is in place and the game can start.
    pub fn poll(&mut self, _el: &ActiveEventLoop) -> Option<GamePaths> {
        if let Some((frames, _)) = self.starting.as_mut() {
            if *frames > 0 {
                *frames -= 1;
                return None;
            }
            return self.starting.take().map(|(_, p)| p);
        }
        // a pick from the document picker
        let picked = self.delegate.ivars().picked.borrow_mut().take();
        if let Some(url) = picked
            && self.job.is_none()
        {
            // SAFETY: plain Foundation calls on a URL the picker handed over
            let scoped = unsafe { url.startAccessingSecurityScopedResource() };
            match url.path().map(|p| PathBuf::from(p.to_string())) {
                Some(path) => {
                    tracing::info!(path = %path.display(), scoped, "game data picked");
                    self.start_job(path, false, scoped.then_some(url));
                }
                None => self.show_error("That location cannot be read."),
            }
        }
        let job = self.job.as_ref()?;
        let fraction = job.progress.fraction();
        let stage = job.progress.stage.lock().map(|s| s.clone()).unwrap_or_default();
        let running = job.handle.as_ref().is_some_and(|h| !h.is_finished());
        if let Some(bar) = &self.progress_bar {
            bar.setProgress_animated(fraction, true);
        }
        if !stage.is_empty() {
            self.set_status(&stage, false);
        }
        if running {
            return None;
        }
        let mut job = self.job.take()?;
        if let Some(url) = job.scoped.take() {
            // SAFETY: balances the start above
            unsafe { url.stopAccessingSecurityScopedResource() };
        }
        let result = job.handle.take().map_or(Err("The import could not start.".into()), |h| h.join().unwrap_or(Err("The import stopped with an error.".into())));
        match result.and_then(|()| GamePaths::locate().map_err(|e| e.to_string())) {
            Ok(paths) => {
                if let Some(dir) = nfs_assets::platform::ios_game_dir() {
                    exclude_from_backup(&dir);
                }
                tracing::info!(root = %paths.root.display(), "game data imported");
                self.set_status("Starting the game…", false);
                if let Some(bar) = &self.progress_bar {
                    bar.setProgress_animated(1.0, false);
                }
                self.starting = Some((3, paths));
                None
            }
            Err(e) => {
                tracing::error!(error = %e, "import failed");
                self.show_error(&e);
                None
            }
        }
    }
}

impl Drop for Importer {
    fn drop(&mut self) {
        if let Some(job) = self.job.as_ref() {
            job.progress.cancel.store(true, Ordering::Relaxed);
        }
        // the UIWindow goes away with the winit window; hide it first so it never covers the game's
        if let Some(vc) = self.window.as_ref().and_then(view_controller)
            && let Some(view) = vc.view()
            && let Some(w) = view.window()
        {
            w.setHidden(true);
        }
    }
}

fn label(mtm: MainThreadMarker, text: &str, font: Retained<UIFont>, color: &UIColor) -> Retained<UILabel> {
    let l = UILabel::new(mtm);
    l.setText(Some(&NSString::from_str(text)));
    l.setNumberOfLines(0);
    l.setTextAlignment(NSTextAlignment::Center);
    // SAFETY: a plain font and colour
    unsafe {
        l.setFont(Some(&font));
        l.setTextColor(Some(color));
    }
    l
}

/// The view controller winit made for `window`.
pub(super) fn view_controller(window: &Window) -> Option<Retained<UIViewController>> {
    let RawWindowHandle::UiKit(h) = window.window_handle().ok()?.as_raw() else { return None };
    if let Some(vc) = h.ui_view_controller {
        // SAFETY: winit's live `UIViewController`, retained for our use
        return unsafe { Retained::retain(vc.as_ptr().cast::<UIViewController>()) };
    }
    // SAFETY: winit's live `UIView`
    let view: Retained<UIView> = unsafe { Retained::retain(h.ui_view.as_ptr().cast::<UIView>()) }?;
    view.window()?.rootViewController()
}

/// The import itself, on its own thread.
fn run_import(src: &Path, drop_in: bool, progress: &Progress) -> Result<(), String> {
    let dest = nfs_assets::platform::ios_game_dir().ok_or("The app's data folder is not available.")?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let is_zip = src.is_file() && src.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    let r = if is_zip { import::import_zip(src, &dest, progress) } else { import::import_folder(src, &dest, drop_in, progress).map(|_| ()) };
    r.map_err(|e| e.to_string())?;
    if drop_in {
        let docs = nfs_assets::platform::documents_dir();
        let removed = if is_zip {
            std::fs::remove_file(src)
        } else if docs.as_deref() == Some(src) {
            Ok(())
        } else {
            std::fs::remove_dir_all(src)
        };
        if let Err(e) = removed {
            tracing::warn!(error = %e, src = %src.display(), "the imported drop-in could not be removed");
        }
    }
    Ok(())
}

/// A game folder or `.zip` waiting in `Documents` (put there with the Files app or Finder), if any.
pub fn documents_drop_in() -> Option<PathBuf> {
    let docs = nfs_assets::platform::documents_dir()?;
    if import::is_game_root(&docs) {
        return Some(docs);
    }
    let mut entries: Vec<_> = std::fs::read_dir(&docs).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    entries.into_iter().find(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if import::is_junk(&name) || name == "Logs" || name == "Photos" || name == "Inbox" {
            return false;
        }
        if p.is_dir() {
            import::find_game_root(p).is_some()
        } else {
            p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        }
    })
}

/// Keeps `dir` out of iCloud and computer backups (the player's game data, which they can import again).
fn exclude_from_backup(dir: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
    let yes = NSNumber::new_bool(true);
    // SAFETY: a documented resource key, with an NSNumber value
    if let Err(e) = unsafe { url.setResourceValue_forKey_error(Some(&yes), NSURLIsExcludedFromBackupKey) } {
        tracing::warn!(error = %e.localizedDescription(), "could not exclude the game data from backups");
    }
}
