//! iOS-only WKWebView setup.
//!
//! Three UIKit tweaks that pure CSS/JS can't reach:
//!
//! 1. `UIScrollView.contentInsetAdjustmentBehavior = .never` so
//!    `window.innerHeight` equals the full screen and `env(safe-area-
//!    inset-*)` reflects the real notch / home-indicator insets.
//!
//! 2. A permanent `UIScrollViewDelegate` that snaps `contentOffset`
//!    back to `(0, 0)` on every `scrollViewDidScroll` callback. iOS
//!    auto-scrolls the WKWebView's scrollView whenever a focused input
//!    would be hidden by the keyboard — this animation runs *after*
//!    `UIKeyboardWillShowNotification`, so a one-shot reset in the
//!    notification observer gets clobbered. The delegate catches
//!    every scroll event and pins the scrollView at origin. Our
//!    document is `position: fixed; overflow: hidden` and the messages
//!    area scrolls via CSS `overflow-y: auto`, so there's nothing the
//!    outer scrollView legitimately needs to do.
//!
//! 3. `UIKeyboardWillShow` / `UIKeyboardWillHide` observers that
//!    forward the keyboard height to the frontend as Tauri events.
//!    On iOS the visualViewport does NOT shrink when the soft keyboard
//!    opens — verified empirically: `visualViewport.height ==
//!    innerHeight` even with the keyboard up — so we have no purely-JS
//!    signal. An earlier attempt to resize the webview's UIView frame
//!    via `setFrame:` changed the physical size but WebKit never
//!    recomputed the layout viewport, so the page still thought it was
//!    812 tall. Emitting an event lets the frontend set a CSS custom
//!    property directly, which is what the Android path already does.
//!
//! Reference: <https://github.com/orgs/tauri-apps/discussions/9368>.

#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSString, NSValue,
};
use objc2_ui_kit::{
    UIKeyboardFrameEndUserInfoKey, UIKeyboardWillHideNotification, UIKeyboardWillShowNotification,
    UIScrollView, UIScrollViewDelegate,
};
use serde::Serialize;
use tauri::{Emitter, WebviewWindow};

/// `UIScrollViewContentInsetAdjustmentNever` raw value from UIKit.
const CONTENT_INSET_ADJUSTMENT_NEVER: isize = 2;

define_class! {
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IrohBleChatScrollLock"]
    #[ivars = ()]
    struct ScrollLock;

    unsafe impl NSObjectProtocol for ScrollLock {}

    unsafe impl UIScrollViewDelegate for ScrollLock {
        #[unsafe(method(scrollViewDidScroll:))]
        fn scroll_view_did_scroll(&self, sv: &UIScrollView) {
            let offset = sv.contentOffset();
            if offset.x != 0.0 || offset.y != 0.0 {
                sv.setContentOffset(CGPoint { x: 0.0, y: 0.0 });
            }
        }
    }
}

impl ScrollLock {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    // Delegates are held weakly by UIScrollView; retain ours for the
    // app's lifetime so the weak ref stays valid.
    static SCROLL_LOCK: RefCell<Option<Retained<ScrollLock>>> = const { RefCell::new(None) };
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

        install_scroll_lock(scroll_view);
        install_keyboard_observers(window_clone);
    });
}

/// Install the permanent ScrollLock delegate on the WKWebView's scrollView.
unsafe fn install_scroll_lock(scroll_view: *mut AnyObject) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let lock = ScrollLock::new(mtm);
    let delegate_ref: &ProtocolObject<dyn UIScrollViewDelegate> = ProtocolObject::from_ref(&*lock);
    unsafe {
        let _: () = msg_send![scroll_view, setDelegate: delegate_ref];
    }
    SCROLL_LOCK.with(|cell| *cell.borrow_mut() = Some(lock));
}

fn install_keyboard_observers(webview_window: WebviewWindow) {
    // SAFETY: all UIKit message sends + NSNotificationCenter calls happen
    // on the main thread, which is where Tauri invokes setup hooks and
    // where UIKeyboard* notifications are posted.
    unsafe {
        let center = NSNotificationCenter::defaultCenter();

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
        });
        let show_token = center.addObserverForName_object_queue_usingBlock(
            Some(UIKeyboardWillShowNotification),
            None,
            None,
            &show_block,
        );

        let window_hide = webview_window;
        let hide_block = RcBlock::new(move |_notif: NonNull<NSNotification>| {
            let _ = window_hide.emit("keyboard-hide", ());
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
