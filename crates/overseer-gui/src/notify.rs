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

use overseer_proto::{slot_name, ActivityKind, CoordView, LockView, SlotId, SlotInfo};
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
    /// The 2ndBrain push notice: it has its own identifier and a click
    /// opens the board, not a session.
    pub push: bool,
    /// S0 proposed a plan: a click opens the board and the plan.
    pub plan: bool,
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
        push: false,
        plan: false,
        title: format!("{name} needs you"),
        body,
    }
}

/// A notification for each wait in `next` that `prev` did not have (same
/// slot, same target): `S2 blocked: repo:demo-api:main is held by S1`. A
/// wait on a target nobody holds has nothing to name and is skipped.
pub fn new_waits(prev: &LockView, next: &LockView) -> Vec<Note> {
    next.waits
        .iter()
        .filter(|w| {
            !prev
                .waits
                .iter()
                .any(|p| p.slot == w.slot && p.target == w.target)
        })
        .filter_map(|w| {
            let holder = next.claims.iter().find(|c| c.target == w.target)?;
            Some(Note {
                slot: w.slot,
                push: false,
                plan: false,
                title: format!(
                    "{} blocked: {} is held by {}",
                    slot_name(w.slot),
                    w.target,
                    slot_name(holder.slot)
                ),
                body: w.reason.clone().unwrap_or_default(),
            })
        })
        .collect()
}

/// The notification for the push problem `next`, if it has not been
/// announced yet. `notified` is the text last announced: it is set when a
/// note is returned and cleared when the problem is gone, so a recurrence
/// notifies again. The daemon's text already starts `2ndBrain push needs
/// you:`, so the title is just `2ndBrain` and the body is the text in full.
pub fn push_problem_note(notified: &mut Option<String>, next: Option<&str>) -> Option<Note> {
    let Some(next) = next else {
        *notified = None;
        return None;
    };
    if notified.as_deref() == Some(next) {
        return None;
    }
    *notified = Some(next.to_string());
    Some(Note {
        slot: 0,
        push: true,
        plan: false,
        title: "2ndBrain".into(),
        body: next.to_string(),
    })
}

/// The notification for a plan that became pending in `next` and was not
/// in `prev`: once per plan id. The body is the first line of the plan's
/// summary, or names the plan when there is none.
pub fn plan_note(prev: &CoordView, next: &CoordView) -> Option<Note> {
    let plan = next.pending_plan()?;
    if prev.pending_plan().is_some_and(|p| p.id == plan.id) {
        return None;
    }
    let first = plan
        .plan
        .summary
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty());
    Some(Note {
        slot: 0,
        push: false,
        plan: true,
        title: "S0 proposed a plan".into(),
        body: match first {
            Some(l) => l.to_string(),
            None => format!("Plan {} is waiting for you", plan.id),
        },
    })
}

/// What a clicked notification opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Slot(SlotId),
    Board,
    /// The board, then the plan waiting there.
    Plan,
}

const PUSH_PREFIX: &str = "overseer-push-";
const PLAN_PREFIX: &str = "overseer-plan-";

/// The request identifier for the `seq`th plan notice.
pub fn plan_identifier(seq: u64) -> String {
    format!("{PLAN_PREFIX}{seq}")
}

/// The request identifier for the `seq`th push notice; never one of a
/// slot's.
pub fn push_identifier(seq: u64) -> String {
    format!("{PUSH_PREFIX}{seq}")
}

/// What a clicked notification's identifier stands for.
pub fn click_from_identifier(id: &str) -> Option<Click> {
    if id.starts_with(PUSH_PREFIX) {
        return Some(Click::Board);
    }
    if id.starts_with(PLAN_PREFIX) {
        return Some(Click::Plan);
    }
    slot_from_identifier(id).map(Click::Slot)
}

const ID_PREFIX: &str = "overseer-slot-";

/// The request identifier for `slot`'s `seq`th notification. Each post
/// needs a new identifier: macOS updates a delivered notification with the
/// same identifier in place, without a banner, so reusing one per slot
/// showed only the first banner. The previous one for the slot is removed
/// before a new one is posted, so Notification Center keeps one per slot.
pub fn identifier(slot: SlotId, seq: u64) -> String {
    format!("{ID_PREFIX}{slot}-{seq}")
}

/// The slot a clicked notification was for. Also accepts the older
/// `overseer-slot-<n>` form, which may still be in Notification Center.
pub fn slot_from_identifier(id: &str) -> Option<SlotId> {
    let rest = id.strip_prefix(ID_PREFIX)?;
    let slot = rest.split_once('-').map_or(rest, |(slot, _)| slot);
    let slot: SlotId = slot.parse().ok()?;
    (slot <= 6).then_some(slot)
}

/// Where the delegate sends clicked slots.
static CLICKS: Mutex<Option<mpsc::UnboundedSender<Click>>> = Mutex::new(None);

/// The stream of clicked notifications' slots. Call once; a later call
/// takes the stream over.
pub fn clicks() -> mpsc::UnboundedReceiver<Click> {
    let (tx, rx) = mpsc::unbounded_channel();
    *CLICKS.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    rx
}

fn clicked(click: Click) {
    if let Some(tx) = CLICKS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(click);
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, Once};

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
            if let Some(click) = super::click_from_identifier(&ident) {
                super::clicked(click);
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

    /// A fresh identifier for `note`, and the one it replaces, if any. The
    /// push notice replaces only an earlier push notice (entry 7).
    fn next_identifier(note: &Note) -> (String, Option<String>) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        static LAST: Mutex<[Option<String>; 9]> = Mutex::new([const { None }; 9]);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let (ident, key) = if note.plan {
            (super::plan_identifier(seq), 8)
        } else if note.push {
            (super::push_identifier(seq), 7)
        } else {
            (super::identifier(note.slot, seq), note.slot as usize)
        };
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let previous = last.get_mut(key).and_then(|l| l.replace(ident.clone()));
        (ident, previous)
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
        let (ident, previous) = next_identifier(note);
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
            if let (Some(previous), Some(array_cls)) = (previous, Class::get("NSArray")) {
                let old: Id = msg_send![array_cls, arrayWithObject: nsstring(&previous)];
                let _: () = msg_send![center, removeDeliveredNotificationsWithIdentifiers: old];
            }
            let request: Id = msg_send![request_cls,
                requestWithIdentifier: nsstring(&ident)
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
                idle_wait: false,
            }),
            session_note: None,
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
    fn resumable_slots_never_notify() {
        let mut s = slot(None, None);
        s.state = SlotState::Resumable {
            session_id: "abc".into(),
            at: 1,
        };
        assert!(!should_notify(None, &s, false));
        assert!(!should_notify(
            Some(&slot(Some(ActivityKind::Working), None)),
            &s,
            false
        ));
        assert!(!should_notify(None, &s, true));
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
        assert_eq!(identifier(4, 17), "overseer-slot-4-17");
        assert_ne!(identifier(4, 17), identifier(4, 18));
        assert_eq!(slot_from_identifier(&identifier(4, 17)), Some(4));
        assert_eq!(slot_from_identifier("overseer-slot-4"), Some(4));
        assert_eq!(slot_from_identifier("overseer-slot-9-1"), None);
        assert_eq!(slot_from_identifier("overseer-slot-x-1"), None);
        assert_eq!(slot_from_identifier("overseer-slot-9"), None);
        assert_eq!(slot_from_identifier("something-else"), None);
    }

    #[test]
    fn push_problem_notifies_once_per_occurrence() {
        let mut seen = None;
        // Already there at connect: notifies.
        let n = push_problem_note(&mut seen, Some("2ndBrain push needs you: rejected")).unwrap();
        assert_eq!(n.title, "2ndBrain");
        assert_eq!(n.body, "2ndBrain push needs you: rejected");
        assert!(n.push);
        // The same text on a later event does not.
        assert_eq!(
            push_problem_note(&mut seen, Some("2ndBrain push needs you: rejected")),
            None
        );
        // Different text (the autostash notice, in full) is news.
        let full =
            "2ndBrain push needs you: x. Resolve the conflicted files, then drop that stash.";
        assert_eq!(push_problem_note(&mut seen, Some(full)).unwrap().body, full);
        // Cleared, then recurring, notifies again.
        assert_eq!(push_problem_note(&mut seen, None), None);
        assert_eq!(seen, None);
        assert!(push_problem_note(&mut seen, Some(full)).is_some());
    }

    #[test]
    fn the_push_notice_has_its_own_identifier_and_opens_the_board() {
        for slot in 0..=6 {
            assert_ne!(push_identifier(5), identifier(slot, 5));
            assert_eq!(
                click_from_identifier(&identifier(slot, 5)),
                Some(Click::Slot(slot))
            );
        }
        assert_eq!(slot_from_identifier(&push_identifier(5)), None);
        assert_eq!(
            click_from_identifier(&push_identifier(5)),
            Some(Click::Board)
        );
    }

    #[test]
    fn new_wait_notifies_once() {
        use overseer_proto::{Claim, ClaimKind, Wait};
        let claim = Claim {
            target: "repo:demo-api:main".into(),
            slot: 1,
            kind: ClaimKind::Command,
            reason: None,
            item: None,
            since: 0,
            last_used: 0,
            pin_revision: None,
        };
        let wait = |slot, target: &str| Wait {
            target: target.into(),
            slot,
            at: 0,
            reason: None,
        };
        let view = |waits| LockView {
            claims: vec![claim.clone()],
            waits,
            relays: vec![],
            flag_after_secs: 3600,
        };
        let none = view(vec![]);
        let one = view(vec![wait(2, "repo:demo-api:main")]);
        let notes = new_waits(&none, &one);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].slot, 2);
        assert_eq!(
            notes[0].title,
            "S2 blocked: repo:demo-api:main is held by S1"
        );
        // The same wait again, or one that merely stays, is not new.
        assert!(new_waits(&one, &one).is_empty());
        // A second wait notifies for itself only.
        let two = view(vec![
            wait(2, "repo:demo-api:main"),
            wait(3, "repo:demo-api:main"),
        ]);
        let notes = new_waits(&one, &two);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].slot, 3);
        // Waiting on a second target is new for that target.
        let other = view(vec![wait(2, "repo:demo-api:main"), wait(2, "tf:demo")]);
        let mut held = other.clone();
        held.claims.push(Claim {
            target: "tf:demo".into(),
            slot: 4,
            ..claim.clone()
        });
        let notes = new_waits(&one, &held);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].title.ends_with("tf:demo is held by S4"));
        // The wait going away notifies nothing.
        assert!(new_waits(&two, &none).is_empty());
    }
    #[test]
    fn plan_note_fires_once_per_new_plan() {
        use overseer_proto::PlanState;
        let empty = crate::app::empty_coord();
        // Nothing pending: nothing to announce (also the first sync, which
        // the app applies without asking).
        assert_eq!(plan_note(&empty, &empty), None);
        let mut one = empty.clone();
        one.plans.push(crate::app::test_plan(1, PlanState::Pending));
        let mut two = empty.clone();
        let mut p2 = crate::app::test_plan(2, PlanState::Pending);
        p2.plan.summary = "Move DEMO-12 first\nthen DEMO-13".into();
        two.plans.push(p2);
        two.plans
            .push(crate::app::test_plan(1, PlanState::Superseded));
        let note = plan_note(&one, &two).expect("plan 2 is new");
        assert_eq!(note.title, "S0 proposed a plan");
        assert_eq!(note.body, "Move DEMO-12 first");
        assert!(note.plan);
        assert_eq!(plan_note(&two, &two), None);
        // No summary: the fallback names the plan.
        let note = plan_note(&empty, &one).unwrap();
        assert_eq!(note.body, "Plan 1 is waiting for you");
        // A decision is not news.
        let mut done = two.clone();
        done.plans[0].state = PlanState::Approved;
        assert_eq!(plan_note(&two, &done), None);
    }

    #[test]
    fn the_plan_notice_opens_the_plan() {
        assert_eq!(
            click_from_identifier(&plan_identifier(3)),
            Some(Click::Plan)
        );
        assert_eq!(slot_from_identifier(&plan_identifier(3)), None);
    }
}
