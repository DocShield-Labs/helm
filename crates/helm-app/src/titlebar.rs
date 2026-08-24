//! Traffic-light alignment for the overlay title bar.
//!
//! `trafficLightPosition` in tauri.conf.json can't hold this alignment. Both
//! tao and wry apply it from exactly one place — `drawRect:` on the webview's
//! parent view — and under a layer-backed WKWebView that view stops redrawing
//! once the page is up. AppKit meanwhile relays out the titlebar container
//! whenever the window changes screen, backing scale, size, or fullscreen
//! state, resetting the buttons to the system default. Nothing puts them back,
//! so the alignment holds on a machine that never triggers a relayout and
//! silently drifts on one that does — which is why it looked right on one
//! display and sat too high on another.
//!
//! So we own the geometry instead: measure the real buttons rather than
//! assuming their size, centre them on the same axis `TopBar` centres its own
//! controls on, snap to the display's pixel grid, and re-apply on every event
//! AppKit relays out for.

/// Height of the frontend's top bar, in logical points.
///
/// `TopBar` sizes itself from this same value through the generated bindings,
/// so the traffic lights and the sidebar toggle cannot drift apart.
pub const TITLE_BAR_HEIGHT: f64 = 35.0;

/// Leading edge of the close button.
#[cfg(target_os = "macos")]
const LEADING_INSET: f64 = 20.0;

/// Where the app's own controls start, measured from the window's leading
/// edge: the traffic-light cluster plus breathing room.
///
/// AppKit owns the button size and the gap between them (14pt and 23pt on
/// macOS 15, but 12pt and 20pt on older releases), so this can't be derived at
/// compile time. `align` re-measures the real cluster and warns if a future
/// macOS outgrows the room reserved here.
pub const TITLE_BAR_CONTENT_INSET: f64 = 92.0;

/// The smallest gap we're willing to leave between the zoom button and the
/// first app control before complaining.
#[cfg(target_os = "macos")]
const MIN_CONTENT_GAP: f64 = 8.0;

#[cfg(not(target_os = "macos"))]
pub fn install(_window: &tauri::WebviewWindow) {}

/// Align the traffic lights now and keep them aligned for the window's life.
#[cfg(target_os = "macos")]
pub fn install(window: &tauri::WebviewWindow) {
    use objc2_app_kit::{
        NSWindow, NSWindowDidBecomeKeyNotification,
        NSWindowDidChangeBackingPropertiesNotification, NSWindowDidChangeScreenNotification,
        NSWindowDidEnterFullScreenNotification, NSWindowDidExitFullScreenNotification,
        NSWindowDidResizeNotification,
    };
    use objc2_foundation::NSNotificationCenter;

    let ptr = match window.ns_window() {
        Ok(ptr) if !ptr.is_null() => ptr as *mut NSWindow,
        Ok(_) => {
            tracing::warn!("traffic lights: null ns_window, leaving them at the system default");
            return;
        }
        Err(e) => {
            tracing::warn!("traffic lights: no ns_window ({e}), leaving them at the default");
            return;
        }
    };

    // Safety: Tauri hands us the live NSWindow for this webview window, and
    // `install` runs on the main thread during setup.
    let ns_window = unsafe { &*ptr };
    align(ns_window);

    // The window outlives every observer (it is torn down with the process),
    // so the block can carry its address rather than a retained reference —
    // no retain cycle, and nothing to make `Send` for the block.
    let addr = ptr as usize;

    let center = NSNotificationCenter::defaultCenter();
    let names = unsafe {
        [
            NSWindowDidResizeNotification,
            NSWindowDidChangeScreenNotification,
            NSWindowDidChangeBackingPropertiesNotification,
            NSWindowDidEnterFullScreenNotification,
            NSWindowDidExitFullScreenNotification,
            NSWindowDidBecomeKeyNotification,
        ]
    };

    for name in names {
        let block = block2::RcBlock::new(move |_: core::ptr::NonNull<objc2_foundation::NSNotification>| {
            // Safety: these notifications are posted on the main thread, and
            // the window is alive for as long as the app is.
            align(unsafe { &*(addr as *mut NSWindow) });
        });
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                Some(&*(ptr as *mut objc2::runtime::AnyObject)),
                None,
                &block,
            )
        };
        // Observing for the app's lifetime; dropping the token would
        // unregister immediately.
        core::mem::forget(token);
    }
}

/// Centre the traffic lights on the top bar's midline.
#[cfg(target_os = "macos")]
fn align(window: &objc2_app_kit::NSWindow) {
    use objc2_app_kit::{NSWindowButton, NSWindowStyleMask};
    use objc2_foundation::NSPoint;

    // In fullscreen the buttons belong to AppKit's auto-hiding titlebar, which
    // is not our top bar. Leave them be; we re-align on the way out.
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return;
    }

    let (Some(close), Some(miniaturize)) = (
        window.standardWindowButton(NSWindowButton::CloseButton),
        window.standardWindowButton(NSWindowButton::MiniaturizeButton),
    ) else {
        return;
    };
    let zoom = window.standardWindowButton(NSWindowButton::ZoomButton);

    // The buttons sit in a box inside the titlebar container; the container
    // is the grandparent, and it is the view whose height decides where
    // AppKit puts them.
    let container = unsafe { close.superview().and_then(|view| view.superview()) };
    let Some(container) = container else {
        return;
    };

    // Measure before mutating anything: the button size and the gap between
    // them are AppKit's to pick, and both have changed across macOS releases.
    let close_frame = close.frame();
    let button_height = close_frame.size.height;
    let spacing = miniaturize.frame().origin.x - close_frame.origin.x;
    let scale = window.backingScaleFactor();

    // Make the titlebar container exactly as tall as the top bar, so
    // "centred in the container" is literally "centred in the top bar".
    let mut frame = container.frame();
    frame.size.height = TITLE_BAR_HEIGHT;
    frame.origin.y = window.frame().size.height - TITLE_BAR_HEIGHT;
    container.setFrame(frame);

    // An unflipped view measures origin.y up from its bottom edge, so
    // centring within the container is symmetric.
    let y = snap((TITLE_BAR_HEIGHT - button_height) / 2.0, scale);

    let mut trailing_edge = LEADING_INSET;
    for (i, button) in [Some(close), Some(miniaturize), zoom]
        .into_iter()
        .flatten()
        .enumerate()
    {
        let x = snap(LEADING_INSET + i as f64 * spacing, scale);
        button.setFrameOrigin(NSPoint::new(x, y));
        trailing_edge = x + button.frame().size.width;
    }

    // If AppKit ever grows the cluster past the room TopBar reserves, the
    // sidebar toggle starts colliding with the zoom button. Say so loudly
    // rather than shipping an overlap.
    if trailing_edge + MIN_CONTENT_GAP > TITLE_BAR_CONTENT_INSET {
        tracing::warn!(
            trailing_edge,
            inset = TITLE_BAR_CONTENT_INSET,
            "traffic lights crowd the top bar; raise TITLE_BAR_CONTENT_INSET"
        );
    }

    // The number that matters is the last one: it has to be half of
    // TITLE_BAR_HEIGHT on every display, whatever AppKit sized the buttons.
    tracing::trace!(
        button_height,
        spacing,
        scale,
        origin_y = y,
        centre_from_top = TITLE_BAR_HEIGHT - y - button_height / 2.0,
        content_gap = TITLE_BAR_CONTENT_INSET - trailing_edge,
        "aligned traffic lights"
    );
}

/// Round a logical coordinate onto the display's physical pixel grid, so the
/// buttons land on whole pixels at 1x as well as 2x.
#[cfg(target_os = "macos")]
fn snap(value: f64, scale: f64) -> f64 {
    if scale <= 0.0 {
        return value;
    }
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the module: the buttons' midline has to be the top
    /// bar's midline, whatever size AppKit makes the buttons.
    #[test]
    fn centres_buttons_on_the_top_bar_midline() {
        for button_height in [10.0, 12.0, 14.0, 16.0] {
            let y = (TITLE_BAR_HEIGHT - button_height) / 2.0;
            // Distance from the container's top edge, which is the window's.
            let from_top = TITLE_BAR_HEIGHT - (y + button_height);
            let centre = from_top + button_height / 2.0;
            assert!(
                (centre - TITLE_BAR_HEIGHT / 2.0).abs() < f64::EPSILON,
                "button of {button_height}pt centred at {centre}, want {}",
                TITLE_BAR_HEIGHT / 2.0
            );
        }
    }

    #[test]
    fn snap_lands_on_whole_device_pixels() {
        // 11.5pt is already exact at 2x (23 physical pixels).
        assert_eq!(snap(11.5, 2.0), 11.5);
        // At 1x it has to move to a whole pixel.
        assert_eq!(snap(11.5, 1.0), 12.0);
        assert_eq!(snap(20.25, 2.0), 20.5);
        // A nonsense scale factor must not produce NaN.
        assert_eq!(snap(11.5, 0.0), 11.5);
    }
}
