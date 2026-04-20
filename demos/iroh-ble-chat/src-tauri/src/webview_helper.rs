//! iOS-only tweaks that reach into the underlying WKWebView via objc2.
//!
//! Tauri/wry creates the WKWebView with the default UIScrollView settings,
//! which cause two problems that pure CSS/JS can't reach:
//!
//! 1. The scroll view's `contentInsetAdjustmentBehavior` is `.automatic`,
//!    which reserves the safe-area strips (status bar / home indicator)
//!    *outside* the document's layout viewport. `window.innerHeight` comes
//!    back clamped to the safe area and no CSS height unit can paint into
//!    the notch or home-indicator regions.
//! 2. When a focused input would be hidden by the soft keyboard, WKWebView's
//!    scroll view auto-scrolls the whole page up to reveal the input. Since
//!    we already resize `.app` to the visual viewport in JS, that auto-
//!    scroll leaves a blank strip above the header.
//!
//! Both are fixed natively:
//! - Setting `contentInsetAdjustmentBehavior = .never` makes
//!   `window.innerHeight` equal the full screen. `env(safe-area-inset-*)`
//!   still reports the correct values, so the existing header and input-bar
//!   padding handle the status bar and home indicator correctly.
//! - Setting `isScrollEnabled = false` disables the outer UIScrollView
//!   entirely, which suppresses the keyboard auto-scroll. Our document is
//!   `position: fixed; overflow: hidden` and the messages area scrolls via
//!   CSS `overflow-y: auto`, so there's nothing the outer scroll was doing
//!   that we need to preserve.
//!
//! References:
//! - <https://github.com/orgs/tauri-apps/discussions/9368>
//! - <https://developer.apple.com/forums/thread/134112>

#![cfg(target_os = "ios")]

use objc2::msg_send;
use objc2::runtime::AnyObject;
use tauri::WebviewWindow;

/// `UIScrollViewContentInsetAdjustmentNever` raw value from UIKit.
const CONTENT_INSET_ADJUSTMENT_NEVER: isize = 2;

/// Apply the iOS-specific WKWebView layout tweaks described in the module
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
        let _: () = msg_send![scroll_view, setScrollEnabled: false];
    });
}
