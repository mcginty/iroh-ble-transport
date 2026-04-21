//! iOS-only WKWebView tweaks applied once at setup time.
//!
//! We do two things natively that pure CSS/JS can't reach:
//!
//! 1. Set `UIScrollView.contentInsetAdjustmentBehavior = .never` so
//!    `window.innerHeight` equals the full screen and `env(safe-area-
//!    inset-*)` reflects the real notch / home-indicator insets.
//! 2. Observe `UIKeyboardWillShow` / `UIKeyboardWillHide` and animate the
//!    WKWebView's frame shorter by the keyboard height while it's visible,
//!    then restore on dismissal. This makes `100dvh` naturally track the
//!    keyboard and stops iOS from scrolling the page to reveal the focused
//!    input (there's nothing to scroll: the input is already above the
//!    keyboard because the webview itself is smaller).
//!
//! Reference: <https://github.com/orgs/tauri-apps/discussions/9368>.

#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_foundation::{CGRect, CGSize};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObjectProtocol, NSString, NSValue,
};
use objc2_ui_kit::{
    UIKeyboardFrameEndUserInfoKey, UIKeyboardWillHideNotification, UIKeyboardWillShowNotification,
};
use tauri::WebviewWindow;

/// `UIScrollViewContentInsetAdjustmentNever` raw value from UIKit.
const CONTENT_INSET_ADJUSTMENT_NEVER: isize = 2;

thread_local! {
    // Keep the notification observer tokens alive for the process lifetime.
    // NSNotificationCenter's `addObserverForName_object_queue_usingBlock`
    // returns a token you're expected to retain; dropping it de-registers
    // the observer.
    static OBSERVERS: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(Vec::new()) };
    // The WKWebView's original frame height, captured the first time we
    // see the keyboard come up. We restore to this height on hide rather
    // than adding the keyboard height back, so repeated show/hide cycles
    // don't drift if iOS reports different keyboard heights (portrait
    // vs. landscape, keyboard accessory views, etc.).
    static ORIGINAL_HEIGHT: RefCell<Option<f64>> = const { RefCell::new(None) };
}

pub fn configure_ios_webview(webview_window: &WebviewWindow) {
    let _ = webview_window.with_webview(|webview| unsafe {
        let wk_webview = webview.inner() as *mut AnyObject;
        if wk_webview.is_null() {
            return;
        }

        let scroll_view: *mut AnyObject = msg_send![wk_webview, scrollView];
        if !scroll_view.is_null() {
            let _: () = msg_send![
                scroll_view,
                setContentInsetAdjustmentBehavior: CONTENT_INSET_ADJUSTMENT_NEVER
            ];
        }

        install_keyboard_frame_observers(wk_webview);
    });
}

/// Register UIKeyboardWillShow / UIKeyboardWillHide observers that resize
/// `wk_webview`'s own frame to stay above the keyboard.
unsafe fn install_keyboard_frame_observers(wk_webview: *mut AnyObject) {
    // Retain the webview so the observer blocks can outlive the setup
    // closure. The webview itself lives for the app's lifetime, but we
    // still need a strong reference of our own.
    let _: *mut AnyObject = unsafe { msg_send![wk_webview, retain] };
    let wk_webview_addr = wk_webview as usize;

    let center = NSNotificationCenter::defaultCenter();

    // ── willShow ────────────────────────────────────────────────────────
    let show_block = RcBlock::new(move |notif: NonNull<NSNotification>| unsafe {
        let webview = wk_webview_addr as *mut AnyObject;
        let notif = notif.as_ref();
        let Some(kb_frame) = keyboard_end_frame(notif) else { return };
        apply_frame_delta(webview, -kb_frame.size.height);
    });
    let show_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillShowNotification),
            None,
            None,
            &show_block,
        )
    };

    // ── willHide ────────────────────────────────────────────────────────
    let hide_block = RcBlock::new(move |_notif: NonNull<NSNotification>| unsafe {
        let webview = wk_webview_addr as *mut AnyObject;
        restore_original_frame(webview);
    });
    let hide_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillHideNotification),
            None,
            None,
            &hide_block,
        )
    };

    OBSERVERS.with(|cell| {
        let mut v = cell.borrow_mut();
        v.push(show_token);
        v.push(hide_token);
    });
}

/// Pull the keyboard end-frame CGRect out of the notification's userInfo.
unsafe fn keyboard_end_frame(notif: &NSNotification) -> Option<CGRect> {
    let user_info = notif.userInfo()?;
    let key: &NSString = unsafe { UIKeyboardFrameEndUserInfoKey };
    let value: *mut AnyObject = unsafe { msg_send![&*user_info, objectForKey: key] };
    if value.is_null() {
        return None;
    }
    let value = value as *mut NSValue;
    let rect: CGRect = unsafe { msg_send![value, CGRectValue] };
    Some(rect)
}

/// Adjust the webview's frame height by `delta` (signed). Captures the
/// original height on first call so we can restore cleanly later.
unsafe fn apply_frame_delta(webview: *mut AnyObject, delta: f64) {
    let frame: CGRect = unsafe { msg_send![webview, frame] };
    ORIGINAL_HEIGHT.with(|cell| {
        if cell.borrow().is_none() {
            *cell.borrow_mut() = Some(frame.size.height);
        }
    });
    let new_frame = CGRect {
        origin: frame.origin,
        size: CGSize {
            width: frame.size.width,
            height: frame.size.height + delta,
        },
    };
    let _: () = unsafe { msg_send![webview, setFrame: new_frame] };
}

/// Restore the webview's frame to the original height captured on the
/// first willShow. No-op if we never saw a keyboard before.
unsafe fn restore_original_frame(webview: *mut AnyObject) {
    let Some(original_height) = ORIGINAL_HEIGHT.with(|cell| *cell.borrow()) else {
        return;
    };
    let frame: CGRect = unsafe { msg_send![webview, frame] };
    let new_frame = CGRect {
        origin: frame.origin,
        size: CGSize {
            width: frame.size.width,
            height: original_height,
        },
    };
    let _: () = unsafe { msg_send![webview, setFrame: new_frame] };
}
