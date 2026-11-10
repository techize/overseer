//! macOS notifications for sessions that need Jonathan.
//!
//! `should_notify` and `note_for` are pure and tested. Delivery goes through
//! `UNUserNotificationCenter` (objc 0.2), which only works inside a real app
//! bundle: under `cargo run` or `cargo test` there is no bundle identifier
//! and the framework raises, so every entry point checks for one first and
//! skips delivery otherwise.
//!
//! Clicking a notification sends its slot to the app through `clicks`; the
//! app brings its window forward and opens that session.

use std::sync::Mutex;

use overseer_proto::{slot_name, ActivityKind, SlotId, SlotInfo};
use tokio::sync::mpsc;

/// Whether `next` should raise a notification. Only a change into NeedsYou
/// does: a NeedsYou already set when the GUI connected (`first_sync`, the
/// first slot list after connecting) was there before Jonathan looked, and
/// a NeedsYou that only changes its reason is the same alert.
pub fn should_notify(prev: Option<&SlotInfo>, next: &SlotInfo, first_sync: bool) -> bool {
    let needs_you = |i: &SlotInfo| {
        i.activity
            .as_ref()
            .is_some_and(|a| a.kind == ActivityKind::NeedsYou)
    };
    !first_sync && needs_you(next) && !prev.is_some_and(needs_you)
}

/// One notification's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub slot: SlotId,
    /// `S3 needs you`.
    pub title: String,
    /// `label: reason`, or whichever of the two there is.
    pub body: String,
}

pub fn note_for(info: &SlotInfo) -> Note {
    let name = slot_name(info.id);
    let label = info.label.trim();
    let reason = info
        .activity
        .as_ref()
        .and_then(|a| a.reason.as_deref())
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let body = match (label.is_empty(), reason) {
        (false, Some(r)) => format!("{label}: {r}"),
        (false, None) => label.to_string(),
        (true, Some(r)) => r.to_string(),
        (true, None) => name.clone(),
    };
    Note {
        slot: info.id,
        title: format!("{name} needs you"),
        body,
    }
}

const ID_PREFIX: &str = "overseer-slot-";

/// The request identifier for `slot`'s notification. One per slot, so a
/// newer one replaces an older one still on screen.
pub fn identifier(slot: SlotId) -> String {
    format!("{ID_PREFIX}{slot}")
}

/// The slot a clicked notification was for.
pub fn slot_from_identifier(id: &str) -> Option<SlotId> {
    let slot: SlotId = id.strip_prefix(ID_PREFIX)?.parse().ok()?;
    (slot <= 6).then_some(slot)
}

/// Where the delegate sends clicked slots.
static CLICKS: Mutex<Option<mpsc::UnboundedSender<SlotId>>> = Mutex::new(None);

/// The stream of clicked notifications' slots. Call once; a later call
/// takes the stream over.
pub fn clicks() -> mpsc::UnboundedReceiver<SlotId> {
    let (tx, rx) = mpsc::unbounded_channel();
    *CLICKS.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    rx
}

fn clicked(slot: SlotId) {
    if let Some(tx) = CLICKS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(slot);
    }
}

/// At launch: asks for permission to alert with sound (macOS only asks
/// Jonathan the first time) and installs the click delegate. Denied
/// permission only loses the banners; the board still shows the state.
pub fn install() {
    platform::install();
}

/// Posts `note`, replacing any earlier one for the same slot.
pub fn post(note: &Note) {
    platform::post(note);
}

// objc 0.2's `msg_send!` expands a cfg this crate does not declare.
#[allow(unexpected_cfgs)]
mod platform {
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::sync::Once;

    use block::{Block, ConcreteBlock};
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Protocol, Sel, BOOL, NO};
    use objc::{class, msg_send, sel, sel_impl};

    use super::Note;

    type Id = *mut Object;

    // Loads the framework so its classes exist at runtime.
    #[link(name = "UserNotifications", kind = "framework")]
    extern "C" {}

    // UNAuthorizationOptions and UNNotificationPresentationOptions bits.
    const AUTH_SOUND: usize = 1 << 1;
    const AUTH_ALERT: usize = 1 << 2;
    const PRESENT_SOUND: usize = 1 << 1;
    const PRESENT_LIST: usize = 1 << 3;
    const PRESENT_BANNER: usize = 1 << 4;

    /// The notification centre, or None outside an app bundle, where
    /// `currentNotificationCenter` raises instead of returning.
    fn center() -> Option<Id> {
        // SAFETY: Foundation class methods with no arguments; the results
        // are autoreleased or shared singletons and only checked for nil.
        unsafe {
            let bundle: Id = msg_send![class!(NSBundle), mainBundle];
            let ident: Id = if bundle.is_null() {
                bundle
            } else {
                msg_send![bundle, bundleIdentifier]
            };
            if ident.is_null() {
                tracing::debug!("not running from an app bundle; skipping notifications");
                return None;
            }
            let cls = Class::get("UNUserNotificationCenter")?;
            let center: Id = msg_send![cls, currentNotificationCenter];
            (!center.is_null()).then_some(center)
        }
    }

    fn nsstring(s: &str) -> Id {
        let c = CString::new(s.replace('\0', "")).unwrap_or_default();
        // SAFETY: a valid NUL-terminated string; the result is autoreleased.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    /// The text of an NSString, or "" for nil.
    unsafe fn rust_string(s: Id) -> String {
        if s.is_null() {
            return String::new();
        }
        let p: *const c_char = msg_send![s, UTF8String];
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    unsafe fn error_text(err: Id) -> String {
        rust_string(msg_send![err, localizedDescription])
    }

    /// Show banners even while Overseer is frontmost: Jonathan may be in
    /// another session's terminal.
    extern "C" fn will_present(_: &Object, _: Sel, _center: Id, _note: Id, handler: Id) {
        // SAFETY: AppKit passes a block taking the presentation options.
        unsafe {
            let handler = &*(handler as *const Block<(usize,), ()>);
            handler.call((PRESENT_BANNER | PRESENT_LIST | PRESENT_SOUND,));
        }
    }

    /// A click: hand the slot to the app, then tell macOS we are done.
    extern "C" fn did_receive(_: &Object, _: Sel, _center: Id, response: Id, handler: Id) {
        // SAFETY: `response` is a UNNotificationResponse and `handler` a
        // block with no arguments, as the delegate protocol declares.
        unsafe {
            let note: Id = msg_send![response, notification];
            let request: Id = msg_send![note, request];
            let ident = rust_string(msg_send![request, identifier]);
            if let Some(slot) = super::slot_from_identifier(&ident) {
                super::clicked(slot);
            }
            let handler = &*(handler as *const Block<(), ()>);
            handler.call(());
        }
    }

    fn delegate_class() -> &'static Class {
        static REGISTER: Once = Once::new();
        const NAME: &str = "OverseerNotificationDelegate";
        REGISTER.call_once(|| {
            let mut decl = ClassDecl::new(NAME, class!(NSObject)).expect("class name is free");
            if let Some(p) = Protocol::get("UNUserNotificationCenterDelegate") {
                decl.add_protocol(p);
            }
            // SAFETY: the signatures match the UNUserNotificationCenterDelegate
            // methods (three object arguments, void return).
            unsafe {
                decl.add_method(
                    sel!(userNotificationCenter:willPresentNotification:withCompletionHandler:),
                    will_present as extern "C" fn(&Object, Sel, Id, Id, Id),
                );
                decl.add_method(
                    sel!(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:),
                    did_receive as extern "C" fn(&Object, Sel, Id, Id, Id),
                );
            }
            decl.register();
        });
        Class::get(NAME).expect("registered above")
    }

    pub fn install() {
        let Some(center) = center() else { return };
        let done = ConcreteBlock::new(|granted: BOOL, err: Id| {
            if granted == NO {
                // SAFETY: nil or an NSError from the framework.
                let why = unsafe { error_text(err) };
                tracing::info!("notifications not allowed: {why}");
            }
        })
        .copy();
        // SAFETY: the centre retains the delegate weakly, so it is never
        // released; the block is copied by the call.
        unsafe {
            let delegate: Id = msg_send![delegate_class(), new];
            let _: () = msg_send![center, setDelegate: delegate];
            let _: () = msg_send![center,
                requestAuthorizationWithOptions: AUTH_ALERT | AUTH_SOUND
                completionHandler: &*done];
        }
    }

    pub fn post(note: &Note) {
        let Some(center) = center() else { return };
        let (Some(content_cls), Some(request_cls), Some(sound_cls)) = (
            Class::get("UNMutableNotificationContent"),
            Class::get("UNNotificationRequest"),
            Class::get("UNNotificationSound"),
        ) else {
            return;
        };
        let slot = note.slot;
        let done = ConcreteBlock::new(move |err: Id| {
            if !err.is_null() {
                // SAFETY: an NSError from the framework.
                let why = unsafe { error_text(err) };
                tracing::info!("notification for S{slot} not delivered: {why}");
            }
        })
        .copy();
        // SAFETY: the content is ours (`new`) and released after the request
        // retains it; the other objects are autoreleased; the block is
        // copied by the call.
        unsafe {
            let content: Id = msg_send![content_cls, new];
            let _: () = msg_send![content, setTitle: nsstring(&note.title)];
            let _: () = msg_send![content, setBody: nsstring(&note.body)];
            let sound: Id = msg_send![sound_cls, defaultSound];
            let _: () = msg_send![content, setSound: sound];
            let nil: Id = std::ptr::null_mut();
            let request: Id = msg_send![request_cls,
                requestWithIdentifier: nsstring(&super::identifier(slot))
                content: content
                trigger: nil];
            let _: () = msg_send![content, release];
            let _: () = msg_send![center, addNotificationRequest: request
                withCompletionHandler: &*done];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{Activity, ActivityKind, SlotState};

    fn slot(kind: Option<ActivityKind>, reason: Option<&str>) -> SlotInfo {
        SlotInfo {
            id: 3,
            label: "eks upgrade".into(),
            cwd: Some("/tmp".into()),
            command: None,
            state: SlotState::Running { pid: 9, since: 1 },
            cols: 80,
            rows: 24,
            activity: kind.map(|kind| Activity {
                kind,
                since: 5,
                now_doing: None,
                reason: reason.map(String::from),
            }),
        }
    }

    #[test]
    fn notify_only_on_transition_into_needs_you() {
        use ActivityKind::*;
        let needs = slot(Some(NeedsYou), Some("Permission to run Bash"));
        // Into NeedsYou from any other state, or from nothing seen yet.
        assert!(should_notify(
            Some(&slot(Some(Working), None)),
            &needs,
            false
        ));
        assert!(should_notify(Some(&slot(Some(Idle), None)), &needs, false));
        assert!(should_notify(Some(&slot(None, None)), &needs, false));
        assert!(should_notify(None, &needs, false));
        // Already needed you: a new reason is not a new alert.
        assert!(!should_notify(Some(&needs), &needs, false));
        let other = slot(Some(NeedsYou), Some("Waiting for input"));
        assert!(!should_notify(Some(&needs), &other, false));
        // Already set when the GUI connected.
        assert!(!should_notify(None, &needs, true));
        assert!(!should_notify(
            Some(&slot(Some(Working), None)),
            &needs,
            true
        ));
        // Not into NeedsYou.
        for next in [
            slot(Some(Working), None),
            slot(Some(Idle), None),
            slot(None, None),
        ] {
            assert!(!should_notify(Some(&needs), &next, false));
            assert!(!should_notify(None, &next, false));
        }
    }

    #[test]
    fn notification_text() {
        let n = note_for(&slot(
            Some(ActivityKind::NeedsYou),
            Some("Permission to run Bash"),
        ));
        assert_eq!(n.slot, 3);
        assert_eq!(n.title, "S3 needs you");
        assert_eq!(n.body, "eks upgrade: Permission to run Bash");
        let n = note_for(&slot(Some(ActivityKind::NeedsYou), None));
        assert_eq!(n.body, "eks upgrade");
        // No label: the body still says something.
        let mut bare = slot(Some(ActivityKind::NeedsYou), Some("Waiting"));
        bare.label.clear();
        assert_eq!(note_for(&bare).body, "Waiting");
        bare.activity.as_mut().unwrap().reason = None;
        assert_eq!(note_for(&bare).body, "S3");
    }

    #[test]
    fn identifier_round_trips() {
        assert_eq!(identifier(4), "overseer-slot-4");
        assert_eq!(slot_from_identifier("overseer-slot-4"), Some(4));
        assert_eq!(slot_from_identifier("overseer-slot-9"), None);
        assert_eq!(slot_from_identifier("something-else"), None);
    }
}
