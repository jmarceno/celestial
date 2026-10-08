//! Deterministic interruption points for tests.
//!
//! An armed point either panics, which unwinds without running the
//! error-handling cleanup an ordinary failure would run (much like a crash
//! or a kill), or runs an action such as an edit landing inside a commit
//! window. In other builds `hit` does nothing.

#[cfg(test)]
type Action = Box<dyn FnMut() + Send>;

/// Armed for the whole process, so a point inside a copy running on another
/// thread fires too. Tests that use failpoints hold the fixture's lock.
#[cfg(test)]
static ARMED: std::sync::Mutex<Option<(&'static str, Option<Action>)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn armed() -> std::sync::MutexGuard<'static, Option<(&'static str, Option<Action>)>> {
    ARMED.lock().unwrap_or_else(|err| err.into_inner())
}

/// Panic at `name` the next time it is reached.
#[cfg(test)]
pub fn arm(name: &'static str) {
    *armed() = Some((name, None));
}

/// Run `action` at `name` the next time it is reached.
#[cfg(test)]
pub fn arm_action(name: &'static str, action: impl FnMut() + Send + 'static) {
    *armed() = Some((name, Some(Box::new(action))));
}

#[cfg(test)]
pub fn disarm() {
    *armed() = None;
}

#[cfg_attr(not(test), allow(unused_variables))]
pub fn hit(name: &str) {
    #[cfg(test)]
    {
        let fired = {
            let mut armed = armed();
            if armed.as_ref().is_some_and(|(point, _)| *point == name) {
                armed.take()
            } else {
                None
            }
        };
        match fired {
            Some((_, Some(mut action))) => action(),
            Some((_, None)) => panic!("failpoint {name}"),
            None => {}
        }
    }
}
