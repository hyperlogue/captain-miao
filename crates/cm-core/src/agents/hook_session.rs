//! A native session's mutable interpretation of hooks and asynchronous facts.
//! Construction is inert; only the running adapter activates observations.

use crate::state::{HookMessage, LauncherState};
use futures_util::future::BoxFuture;

pub(crate) trait HookSession: Send {
    fn start(&mut self) {}
    fn dispatch_hook(&mut self, state: &mut LauncherState, message: HookMessage);
    fn changed(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
    fn apply_update(&mut self, _state: &mut LauncherState) -> bool {
        false
    }
    fn transcript_changed(&mut self, _state: &mut LauncherState) {}
}

pub(crate) struct Stateless(pub fn(&mut LauncherState, HookMessage));

impl HookSession for Stateless {
    fn dispatch_hook(&mut self, state: &mut LauncherState, message: HookMessage) {
        (self.0)(state, message)
    }
}
