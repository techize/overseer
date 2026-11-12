//! Write target locks: who holds which shared target, who is queued for
//! it, and the relays that tell sessions about handoffs.

// The lock service (a later task) is the main caller; until then only the
// ledger's lock view and the tests use these functions.
#[allow(dead_code)]
pub(crate) mod table;

// The PreToolUse hook path (the next task) is the caller; until then only
// the tests use these.
#[allow(dead_code)]
pub(crate) mod context;
#[allow(dead_code)]
pub(crate) mod targets;
