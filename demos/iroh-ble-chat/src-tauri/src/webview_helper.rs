//! iOS-only WKWebView setup.
//!
//! We do two things natively that pure CSS/JS can't reach:
//!
//! 1. Set `UIScrollView.contentInsetAdjustmentBehavior = .never` so
//!    `window.innerHeight` equals the full screen and `env(safe-area-
//!    inset-*)` reflects the real notch / home-indicator insets. (The
//!    webview otherwise clamps the layout viewport to the safe area,
//!    leaving strips of canvas colour outside it.)
//!
//! 2. Observe `UIKeyboardWillShow` / `UIKeyboardWillHide` and forward
//!    the keyboard height to the frontend as Tauri events. On iOS the
//!    visualViewport does NOT shrink when the soft keyboard opens —
//!    verified empirically: `visualViewport.height == innerHeight`
//!    even with the keyboard up — so we have no purely-JS signal to
//!    drive a resize of `.app`. An earlier attempt to resize the
//!    WKWebView's UIView frame via `setFrame:` changed the physical
//!    size but WebKit never recomputed the layout viewport, so the
//!    page still thought it was 812 tall and the user had to scroll
//!    to see the input bar. Emitting an event lets the frontend
//!    update a CSS custom property directly, which is what the
//!    Android path already does via visualViewport.
//!
//! Reference: <https://github.com/orgs/tauri-apps/discussions/9368>.

#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObjectProtocol, NSString, NSValue,
};
use objc2_ui_kit::{
    UIKeyboardFrameEndUserInfoKey, UIKeyboardWillHideNotification, UIKeyboardWillShowNotification,
};
use serde::Serialize;
use tauri::{Emitter, WebviewWindow};

/// `UIScrollViewContentInsetAdjustmentNever` raw value from UIKit.
const CONTENT_INSET_ADJUSTMENT_NEVER: isize = 2;

thread_local! {
    // Keep the observer tokens alive for the process lifetime.
    // NSNotificationCenter's `addObserverForName:object:queue:usingBlock:`
    // returns an opaque token; dropping it de-registers the observer.
    static OBSERVERS: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(Vec::new()) };
}

#[derive(Serialize, Clone)]
struct KeyboardShowPayload {
    height: f64,
}

pub fn configure_ios_webview(webview_window: &WebviewWindow) {
    let window_clone = webview_window.clone();
    let _ = webview_window.with_webview(move |webview| unsafe {
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

        // usize is Send — necessary because the observer blocks are stored
        // in a thread_local and might outlive this setup closure.
        install_keyboard_observers(window_clone, scroll_view as usize);
    });
}

fn install_keyboard_observers(webview_window: WebviewWindow, scroll_view_addr: usize) {
    // SAFETY: all UIKit message sends + NSNotificationCenter calls happen
    // on the main thread, which is where Tauri invokes setup hooks and
    // where UIKeyboard* notifications are posted.
    unsafe {
        let center = NSNotificationCenter::defaultCenter();

        // ── willShow ────────────────────────────────────────────────────
        let window_show = webview_window.clone();
        let show_block = RcBlock::new(move |notif: NonNull<NSNotification>| {
            let notif = notif.as_ref();
            let Some(kb_frame) = keyboard_end_frame(notif) else {
                return;
            };
            let _ = window_show.emit(
                "keyboard-show",
                KeyboardShowPayload {
                    height: kb_frame.size.height,
                },
            );
            // iOS auto-scrolls the WKWebView's scrollView up to reveal the
            // focused input at its original DOM position before firing
            // this notification. With .app about to shrink above the
            // keyboard (driven by the event we just emitted), that scroll
            // leaves the page visibly offset upward until the user drags
            // it back. Reset the contentOffset so the page sits at origin.
            reset_scroll_origin(scroll_view_addr);
        });
        let show_token = center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillShowNotification),
            None,
            None,
            &show_block,
        );

        // ── willHide ────────────────────────────────────────────────────
        let window_hide = webview_window;
        let hide_block = RcBlock::new(move |_notif: NonNull<NSNotification>| {
            let _ = window_hide.emit("keyboard-hide", ());
            reset_scroll_origin(scroll_view_addr);
        });
        let hide_token = center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillHideNotification),
            None,
            None,
            &hide_block,
        );

        OBSERVERS.with(|cell| {
            let mut v = cell.borrow_mut();
            v.push(show_token);
            v.push(hide_token);
        });
    }
}

/// Snap the WKWebView's scrollView back to `(0, 0)`.
fn reset_scroll_origin(scroll_view_addr: usize) {
    let scroll_view = scroll_view_addr as *mut AnyObject;
    if scroll_view.is_null() {
        return;
    }
    let zero = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: the scroll view is retained by its parent WKWebView for the
    // app's lifetime and all access happens on the main thread.
    unsafe {
        let _: () = msg_send![scroll_view, setContentOffset: zero];
    }
}

/// Pull the keyboard end-frame `CGRect` out of the notification's userInfo.
unsafe fn keyboard_end_frame(notif: &NSNotification) -> Option<CGRect> {
    let user_info = notif.userInfo()?;
    let key: &NSString = UIKeyboardFrameEndUserInfoKey;
    let value: *mut AnyObject = unsafe { msg_send![&*user_info, objectForKey: key] };
    if value.is_null() {
        return None;
    }
    let value = value as *mut NSValue;
    let rect: CGRect = unsafe { msg_send![value, CGRectValue] };
    Some(rect)
}
