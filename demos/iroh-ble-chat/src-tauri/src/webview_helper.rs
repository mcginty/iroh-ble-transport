//! iOS-only WKWebView tweak applied once at setup.
//!
//! Tauri/wry creates the WKWebView with UIScrollView's default
//! `contentInsetAdjustmentBehavior = .automatic`, which reserves the safe-
//! area strips (status bar / home indicator) *outside* the document's
//! layout viewport. `window.innerHeight` then comes back clamped to the
//! safe area (e.g. 728 on an 812-tall screen) and no CSS height unit can
//! paint into the notch or home-indicator regions.
//!
//! Setting the behaviour to `.never` makes `window.innerHeight` equal the
//! full screen. `env(safe-area-inset-*)` still reports the correct values,
//! so our CSS header and input-bar padding handle the notch and home
//! indicator correctly on their own.
//!
//! We leave `isScrollEnabled` alone. Disabling it also suppressed the
//! visualViewport resize signal WebKit emits when the soft keyboard opens
//! (so our JS resize listener stopped firing and `.app` was stuck at full
//! height while the input bar hid behind the keyboard). The keyboard
//! auto-scroll is instead suppressed in the frontend via the gist-method-1
//! opacity trick (see `src/styles.css`).
//!
//! References:
//! - <https://github.com/orgs/tauri-apps/discussions/9368>
//! - <https://gist.github.com/kiding/72721a0553fa93198ae2bb6eefaa3299>

#![cfg(target_os = "ios")]

use objc2::msg_send;
use objc2::runtime::AnyObject;
use tauri::WebviewWindow;

/// `UIScrollViewContentInsetAdjustmentNever` raw value from UIKit.
const CONTENT_INSET_ADJUSTMENT_NEVER: isize = 2;

/// Apply the iOS-specific WKWebView layout tweak described in the module
/// documentation. Idempotent; safe to call once at setup time.
pub fn configure_ios_webview(webview_window: &WebviewWindow) {
    let _ = webview_window.with_webview(|webview| unsafe {
        let wk_webview = webview.inner() as *mut AnyObject;
        if wk_webview.is_null() {
            return;
        }
        let scroll_view: *mut AnyObject = msg_send![wk_webview, scrollView];
        if scroll_view.is_null() {
            return;
        }
        let _: () = msg_send![
            scroll_view,
            setContentInsetAdjustmentBehavior: CONTENT_INSET_ADJUSTMENT_NEVER
        ];
    });
}
