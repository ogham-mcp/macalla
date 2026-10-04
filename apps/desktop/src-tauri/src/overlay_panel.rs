// SPDX-License-Identifier: MIT
//! The overlay in a native non-activating panel, so a click on its cancel
//! button leaves keyboard focus in the app the person was typing in.
//!
//! Tauri makes every window a plain `NSWindow`. A click on one activates the
//! app that owns it, whatever `focusable` and `acceptFirstMouse` say, and an
//! Option-click hides the app that was in front.
//! `NSWindowStyleMaskNonactivatingPanel` stops both, but AppKit honours it
//! only on a panel (NSWindow.h: "Only applicable for `NSPanel` (or a
//! subclass thereof)"), and a panel settles whether it activates when it is
//! made, so the style has to be there from `init`.
//!
//! So [`install`] makes a panel of its own, [`OverlayPanel`], with that style
//! from the start, and moves the overlay's web view into it, once, at setup.
//! Tauri's window stays, hidden, holding an empty placeholder view, and keeps
//! everything Tauri knows the overlay by: its label, its capability, its
//! events and the web view's IPC, none of which ask which native window the
//! web view is in. From then on the panel is shown, placed and hidden here
//! ([`show`], [`hide`]), and Tauri's window is never shown again.
//!
//! If the web view is not laid out as expected, [`install`] leaves it where
//! it is with a warning, [`in_panel`] says no, and the overlay stays Tauri's
//! window, shown with [`show_window`]: the cancel button still works, but a
//! click on it activates Macalla.
//!
//! `worksWhenModal` stays NO, so while a window of Macalla's runs modally
//! the cancel button takes no clicks; the tray's Cancel Dictation is there
//! for that case.
//!
//! Nothing here changes the class of an object Tauri made, so nothing needs
//! undoing at exit. The panel is kept for the life of the process and never
//! released; Tauri's window may be released at exit like any other, holding
//! only the placeholder.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSPanel, NSResponder, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use std::cell::OnceCell;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, WebviewWindow, Wry};

define_class!(
    // SAFETY: `NSPanel` may be subclassed, the chain named is its real one,
    // the class adds no instance variables, and it overrides two methods
    // with the types NSWindow.h declares for them (below). Every `NSWindow`
    // is used on the main thread only, which `MainThreadOnly` enforces.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MacallaOverlayPanel"]
    struct OverlayPanel;

    impl OverlayPanel {
        // SAFETY: NSWindow.h declares `canBecomeKeyWindow` as a read-only
        // BOOL property, so the getter takes nothing and returns BOOL.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            false
        }

        // SAFETY: as above, for `canBecomeMainWindow`.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

impl OverlayPanel {
    /// A borderless, non-activating panel over the same screen area as
    /// `like`'s content.
    fn new(mtm: MainThreadMarker, like: &NSWindow) -> Retained<Self> {
        let rect = like.contentRectForFrameRect(like.frame());
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `initWithContentRect:styleMask:backing:defer:` is
        // `NSWindow`'s designated initialiser, and its arguments here have the
        // types NSWindow.h declares: an `NSRect`, an `NSWindowStyleMask`, an
        // `NSBackingStoreType` and a BOOL. `this` was just allocated, has no
        // instance variables of ours to set, and is initialised once.
        unsafe {
            msg_send![
                super(this),
                initWithContentRect: rect,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        }
    }
}

/// Whether the overlay is in the panel. Readable from any thread; set once,
/// on the main thread, by [`install`].
static IN_PANEL: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The panel, on the main thread, the only one that touches it. Never
    /// dropped: the panel lives as long as the process.
    static PANEL: OnceCell<ManuallyDrop<Retained<OverlayPanel>>> = const { OnceCell::new() };
}

/// Whether the overlay is shown and hidden through [`show`] and [`hide`]
/// rather than through Tauri's window.
pub(crate) fn in_panel() -> bool {
    IN_PANEL.load(Ordering::Relaxed)
}

/// Why the overlay's web view was left in Tauri's window.
#[derive(Debug, PartialEq, Eq)]
enum Layout {
    /// The window has no content view.
    NoContentView,
    /// WebKit's web view class is not loaded.
    NoWebKit,
    /// The content view is the web view itself, not a view holding it.
    ContentIsWebView,
    /// The content view is tao's own view: the web view was added to the
    /// window as a child, as wry does for more than one web view per window.
    ContentIsTaoView,
    /// The content view holds this many views, not one.
    Children(usize),
    /// The content view's one child is not a web view.
    NotAWebView,
}

/// Whether a content view of class `content`, holding `children` views of
/// which the first has class `first`, is laid out as wry lays out a
/// window's only web view: a view that is neither a web view nor tao's own
/// view (`tao_view`), holding exactly one view, the web view (`webview`, or
/// a subclass of it). Moving tao's own view would take the window's input
/// handling with it.
fn check_layout(
    content: &AnyClass,
    children: usize,
    first: Option<&AnyClass>,
    webview: Option<&AnyClass>,
    tao_view: Option<&AnyClass>,
) -> Result<(), Layout> {
    let Some(webview) = webview else {
        return Err(Layout::NoWebKit);
    };
    if descends_from(content, webview) {
        return Err(Layout::ContentIsWebView);
    }
    if tao_view.is_some_and(|tao_view| descends_from(content, tao_view)) {
        return Err(Layout::ContentIsTaoView);
    }
    if children != 1 {
        return Err(Layout::Children(children));
    }
    match first {
        Some(first) if descends_from(first, webview) => Ok(()),
        _ => Err(Layout::NotAWebView),
    }
}

/// The class of `object` as the runtime sees it.
fn class_of(object: &AnyObject) -> &'static AnyClass {
    object.class()
}

/// Whether `class` is `ancestor` or one of its subclasses.
fn descends_from(class: &AnyClass, ancestor: &AnyClass) -> bool {
    let mut class = Some(class);
    while let Some(current) = class {
        if std::ptr::eq(current, ancestor) {
            return true;
        }
        class = current.superclass();
    }
    false
}

/// Tauri's native window for the overlay, on the main thread only.
fn ns_window(overlay: &WebviewWindow<Wry>, _mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    let pointer = match overlay.ns_window() {
        Ok(pointer) => pointer.cast::<NSWindow>(),
        Err(e) => {
            tracing::warn!("no native overlay window: {e}");
            return None;
        }
    };
    // SAFETY: Tauri returns a live, autoreleased `NSWindow` pointer (or
    // null, which `retain` turns into `None`); retaining it keeps it alive
    // past the autorelease pool, and the main-thread marker says this runs
    // where AppKit requires.
    unsafe { Retained::retain(pointer) }
}

/// Settings the panel takes from Tauri's window, so the overlay looks and
/// stacks as `tauri.conf.json` says, and the ones only a panel has.
fn configure(panel: &OverlayPanel, window: &NSWindow) {
    panel.setTitle(&window.title());
    // `alwaysOnTop`, which tao makes the floating window level.
    panel.setLevel(window.level());
    panel.setOpaque(window.isOpaque());
    panel.setBackgroundColor(Some(&window.backgroundColor()));
    panel.setHasShadow(window.hasShadow());
    panel.setAcceptsMouseMovedEvents(window.acceptsMouseMovedEvents());
    // `visibleOnAllWorkspaces` gives every Space; the panel also goes over
    // another app's full-screen window, and stays put, like any heads-up
    // display, while Mission Control rearranges windows.
    panel.setCollectionBehavior(
        window.collectionBehavior()
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary,
    );
    // A panel hides whenever its app is not active by default, and Macalla
    // is almost never the active app while dictating.
    panel.setHidesOnDeactivate(false);
    // As before the panel: while a window of Macalla's runs modally, the
    // cancel button takes no clicks, and the tray's Cancel Dictation is the
    // way to discard.
    panel.setWorksWhenModal(false);
    // SAFETY: the panel is kept for the life of the process and never
    // closed, and turning this off only means a close would not release it.
    unsafe { panel.setReleasedWhenClosed(false) };
}

/// Move the overlay's web view into a non-activating panel. Call once, on
/// the main thread, after the overlay window exists and before it is first
/// shown. On any surprise, leave the overlay as it is and say why.
pub(crate) fn install(overlay: &WebviewWindow<Wry>) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("overlay panel set up off the main thread; skipped");
        return;
    };
    if in_panel() {
        return;
    }
    let Some(window) = ns_window(overlay, mtm) else {
        return;
    };
    let Some(content) = window.contentView() else {
        skipped(&Layout::NoContentView);
        return;
    };
    let subviews = content.subviews();
    let first = subviews.firstObject();
    if let Err(layout) = check_layout(
        class_of(&content),
        subviews.count(),
        first.as_deref().map(|view| class_of(view)),
        AnyClass::get(c"WKWebView"),
        AnyClass::get(c"TaoView"),
    ) {
        skipped(&layout);
        return;
    }

    let panel = OverlayPanel::new(mtm, &window);
    configure(&panel, &window);
    // Tao reads the window's content view back and expects one, so the
    // window keeps an empty view of the same size in place of the web view's.
    let placeholder = NSView::initWithFrame(NSView::alloc(mtm), content.frame());
    window.setContentView(Some(&placeholder));
    // wry's view already resizes with whatever holds it.
    panel.setContentView(Some(&content));
    PANEL.with(|cell| {
        let _ = cell.set(ManuallyDrop::new(panel));
    });
    IN_PANEL.store(true, Ordering::Relaxed);
    tracing::info!("the overlay is in a non-activating panel");
}

fn skipped(layout: &Layout) {
    tracing::warn!(
        ?layout,
        "the overlay's web view is not laid out as expected; it stays in its own window, and a click on it activates Macalla"
    );
}

/// Run `f` with the panel on the main thread, if there is a panel.
fn with_panel(
    app: &AppHandle<Wry>,
    what: &'static str,
    f: impl FnOnce(&OverlayPanel, MainThreadMarker) + Send + 'static,
) {
    let ran = app.run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        PANEL.with(|cell| {
            if let Some(panel) = cell.get() {
                f(panel, mtm);
            }
        });
    });
    if let Err(e) = ran {
        tracing::info!("could not {what} the overlay: {e}");
    }
}

/// The bottom-left corner, in AppKit's screen coordinates (origin at the
/// bottom left, in points), of a panel `width` wide, centred along the
/// bottom of a screen whose frame starts at `(screen_x, screen_y)` and is
/// `screen_width` wide, `bottom_margin` above its bottom edge.
fn bottom_centre(
    (screen_x, screen_y, screen_width): (f64, f64, f64),
    width: f64,
    bottom_margin: f64,
) -> (f64, f64) {
    (
        screen_x + (screen_width - width) / 2.0,
        screen_y + bottom_margin,
    )
}

/// Place the panel, `width` by `height`, at the bottom centre of the screen
/// with the menu bar, and show it without making it key or activating
/// Macalla. That screen is the one Tauri calls the primary monitor: AppKit
/// lists it first (NSScreen.h), and its frame, like the monitor's size,
/// includes the Dock's area.
pub(crate) fn show(app: &AppHandle<Wry>, width: f64, height: f64, bottom_margin: f64) {
    with_panel(app, "show", move |panel, mtm| {
        place_and_show(panel, mtm, width, height, bottom_margin);
    });
}

/// [`show`]'s work, on the main thread.
fn place_and_show(
    panel: &OverlayPanel,
    mtm: MainThreadMarker,
    width: f64,
    height: f64,
    bottom_margin: f64,
) {
    if let Some(screen) = NSScreen::screens(mtm).firstObject() {
        let area = screen.frame();
        let (x, y) = bottom_centre(
            (area.origin.x, area.origin.y, area.size.width),
            width,
            bottom_margin,
        );
        let mut frame = panel.frame();
        frame.origin.x = x;
        frame.origin.y = y;
        frame.size.width = width;
        frame.size.height = height;
        panel.setFrame_display(frame, false);
    }
    panel.orderFrontRegardless();
}

/// Hide the panel. `orderOut:` activates nothing.
pub(crate) fn hide(app: &AppHandle<Wry>) {
    with_panel(app, "hide", |panel, _| panel.orderOut(None));
}

/// Show Tauri's own overlay window without making it key, when the panel
/// could not be set up. Tauri's `show()` calls `makeKeyAndOrderFront:`.
/// Checked again on the main thread: if the web view has moved to the panel
/// since this was asked, the panel is shown instead, `width` by `height`
/// and `bottom_margin` up, as [`show`] would, and Tauri's window, now
/// empty, stays hidden.
pub(crate) fn show_window(
    overlay: &WebviewWindow<Wry>,
    width: f64,
    height: f64,
    bottom_margin: f64,
) {
    let target = overlay.clone();
    let shown = overlay.run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        if in_panel() {
            PANEL.with(|cell| {
                if let Some(panel) = cell.get() {
                    place_and_show(panel, mtm, width, height, bottom_margin);
                }
            });
            return;
        }
        if let Some(window) = ns_window(&target, mtm) {
            window.orderFrontRegardless();
        }
    });
    if let Err(e) = shown {
        tracing::info!("could not show the overlay: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::runtime::ClassBuilder;
    use objc2::{sel, ClassType};
    use std::ffi::CStr;

    /// A class named `name` under `superclass`, registered once.
    fn class(name: &CStr, superclass: &AnyClass) -> &'static AnyClass {
        if let Some(class) = AnyClass::get(name) {
            return class;
        }
        ClassBuilder::new(name, superclass)
            .expect("an unused class name")
            .register()
    }

    #[test]
    fn the_panel_is_an_nspanel_that_overrides_becoming_key_and_main() {
        let panel = OverlayPanel::class();
        assert_eq!(panel.name(), c"MacallaOverlayPanel");
        assert!(std::ptr::eq(
            panel.superclass().expect("a superclass"),
            NSPanel::class()
        ));
        assert!(descends_from(panel, NSWindow::class()));
        for selector in [sel!(canBecomeKeyWindow), sel!(canBecomeMainWindow)] {
            let ours = panel.instance_method(selector).expect("the override");
            let inherited = NSPanel::class()
                .instance_method(selector)
                .expect("AppKit's own");
            assert_ne!(
                ours.implementation() as usize,
                inherited.implementation() as usize,
                "{selector:?} is overridden"
            );
        }
    }

    #[test]
    fn wrys_layout_is_accepted_and_a_subclassed_web_view_too() {
        let webview = class(c"MacallaTestWebView", NSObject::class());
        let observed = class(c"MacallaTestObservedWebView", webview);
        let parent = class(c"MacallaTestWebViewParent", NSObject::class());
        assert_eq!(
            check_layout(parent, 1, Some(webview), Some(webview), None),
            Ok(())
        );
        assert_eq!(
            check_layout(parent, 1, Some(observed), Some(webview), None),
            Ok(())
        );
    }

    #[test]
    fn any_other_layout_is_refused() {
        // Names of its own: tests run in parallel, and one name is
        // registered once.
        let webview = class(c"MacallaTestRefusedWebView", NSObject::class());
        let parent = class(c"MacallaTestRefusedParent", NSObject::class());
        let other = class(c"MacallaTestOtherView", NSObject::class());
        assert_eq!(
            check_layout(parent, 1, Some(webview), None, None),
            Err(Layout::NoWebKit)
        );
        assert_eq!(
            check_layout(webview, 0, None, Some(webview), None),
            Err(Layout::ContentIsWebView)
        );
        assert_eq!(
            check_layout(parent, 0, None, Some(webview), None),
            Err(Layout::Children(0))
        );
        assert_eq!(
            check_layout(parent, 2, Some(webview), Some(webview), None),
            Err(Layout::Children(2))
        );
        assert_eq!(
            check_layout(parent, 1, Some(other), Some(webview), None),
            Err(Layout::NotAWebView)
        );
        // wry's child layout: tao's own view, holding the web view.
        let tao_view = class(c"MacallaTestTaoView", NSObject::class());
        let observed_tao_view = class(c"MacallaTestObservedTaoView", tao_view);
        for content in [tao_view, observed_tao_view] {
            assert_eq!(
                check_layout(content, 1, Some(webview), Some(webview), Some(tao_view)),
                Err(Layout::ContentIsTaoView)
            );
        }
        assert_eq!(
            check_layout(parent, 1, Some(webview), Some(webview), Some(tao_view)),
            Ok(()),
            "wry's own parent view is not tao's"
        );
        assert_eq!(
            check_layout(parent, 1, None, Some(webview), None),
            Err(Layout::NotAWebView)
        );
    }

    #[test]
    fn the_panel_sits_centred_above_the_bottom_of_the_screen() {
        assert_eq!(
            bottom_centre((0.0, 0.0, 1512.0), 360.0, 72.0),
            (576.0, 72.0)
        );
        // A screen left of and below the menu bar's: AppKit's coordinates
        // carry its origin.
        assert_eq!(
            bottom_centre((-1920.0, -200.0, 1920.0), 360.0, 72.0),
            (-1140.0, -128.0)
        );
    }
}
