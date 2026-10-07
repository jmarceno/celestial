//! Deterministic interruption points for tests.
//!
//! An armed point either panics, which unwinds without running the
//! error-handling cleanup an ordinary failure would run (much like a crash
//! or a kill), or runs an action such as an edit landing inside a commit
//! window. In other builds `hit` does nothing.

#[cfg(test)]
type Action = Box<dyn FnMut()>;

#[cfg(test)]
thread_local! {
    static ARMED: std::cell::RefCell<Option<(&'static str, Option<Action>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Panic at `name` the next time it is reached on this thread.
#[cfg(test)]
pub fn arm(name: &'static str) {
    ARMED.with(|armed| *armed.borrow_mut() = Some((name, None)));
}

/// Run `action` at `name` the next time it is reached on this thread.
#[cfg(test)]
pub fn arm_action(name: &'static str, action: impl FnMut() + 'static) {
    ARMED.with(|armed| *armed.borrow_mut() = Some((name, Some(Box::new(action)))));
}

#[cfg(test)]
pub fn disarm() {
    ARMED.with(|armed| *armed.borrow_mut() = None);
}

#[cfg_attr(not(test), allow(unused_variables))]
pub fn hit(name: &str) {
    #[cfg(test)]
    {
        let fired = ARMED.with(|armed| {
            let mut armed = armed.borrow_mut();
            if armed.as_ref().is_some_and(|(point, _)| *point == name) {
                armed.take()
            } else {
                None
            }
        });
        match fired {
            Some((_, Some(mut action))) => action(),
            Some((_, None)) => panic!("failpoint {name}"),
            None => {}
        }
    }
}
