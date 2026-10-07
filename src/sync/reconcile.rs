//! The single reconciliation decision.
//!
//! Metadata and watcher events only schedule work. Whether contents changed
//! is decided here from both observations and the last verified common
//! contents. Timestamps never choose which contents survive.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

impl Kind {
    pub fn of(is_dir: bool) -> Self {
        if is_dir { Self::Dir } else { Self::File }
    }
}

/// How one side's contents compare with something else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delta {
    /// Identical contents and metadata.
    Same,
    /// Identical contents; only metadata (times, inode) moved.
    MetadataOnly,
    Changed,
    /// It could not be determined (still changing, unreadable).
    Unknown(String),
}

impl Delta {
    fn same_contents(&self) -> bool {
        matches!(self, Self::Same | Self::MetadataOnly)
    }
}

/// One side of an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Side {
    Missing,
    /// The observation failed or was incomplete; never evidence of deletion.
    Unknown(String),
    /// `vs_base` compares with the verified baseline; it is ignored when
    /// there is no verified baseline.
    Present {
        kind: Kind,
        vs_base: Delta,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum History {
    /// Never synchronized.
    None,
    /// Legacy or damaged history: the former common contents are unknown.
    Unverified,
    /// A verified common baseline of this kind.
    Verified(Kind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub local: Side,
    pub remote: Side,
    pub history: History,
    /// Whether the two present sides hold the same contents. Only consulted
    /// when it matters, so callers may compute it lazily.
    pub sides: Option<Delta>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// No trusted common baseline and the contents differ.
    Bootstrap,
    /// Both sides changed to different contents.
    EditEdit,
    /// A file on one side and a folder on the other.
    FileFolder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to transfer or record.
    InSync,
    /// Equal contents: establish or advance the baseline without copying.
    Record,
    /// Propagate the local version (create or replace).
    Upload,
    /// Propagate the remote version (create or replace).
    Download,
    /// Propagate a remote deletion; the local item matches the baseline.
    DeleteLocal,
    /// Propagate a local deletion; the remote item matches the baseline.
    DeleteRemote,
    /// The remote was deleted but the local version changed: restore it.
    RestoreToRemote,
    /// The local was deleted but the remote version changed: restore it.
    RestoreToLocal,
    /// Keep both versions.
    KeepBoth(ConflictKind),
    /// Both sides are confirmed absent: retire the history.
    Retire,
    /// Something is uncertain: retry this item later, change nothing.
    Retry(String),
    /// Nothing exists and nothing is recorded.
    Nothing,
}

impl Decision {
    /// Whether the decision removes the item from the local side.
    pub fn removes_local(&self) -> bool {
        matches!(self, Self::DeleteLocal)
    }

    pub fn removes_remote(&self) -> bool {
        matches!(self, Self::DeleteRemote)
    }
}

/// Need for the side comparison, so it is only computed when it matters.
pub fn needs_sides(local: &Side, remote: &Side, history: History) -> bool {
    match (local, remote) {
        (
            Side::Present {
                kind: lk,
                vs_base: lb,
            },
            Side::Present {
                kind: rk,
                vs_base: rb,
            },
        ) if lk == rk && *lk == Kind::File => match history {
            History::Verified(Kind::File) => {
                matches!(lb, Delta::Changed) && matches!(rb, Delta::Changed)
            }
            History::Verified(Kind::Dir) | History::None | History::Unverified => true,
        },
        _ => false,
    }
}

pub fn decide(input: &Input) -> Decision {
    let sides = || {
        input
            .sides
            .clone()
            .unwrap_or_else(|| Delta::Unknown("contents were not compared".into()))
    };
    match (&input.local, &input.remote) {
        (Side::Unknown(reason), _) | (_, Side::Unknown(reason)) => Decision::Retry(reason.clone()),
        (Side::Missing, Side::Missing) => match input.history {
            History::None => Decision::Nothing,
            _ => Decision::Retire,
        },
        (Side::Present { kind, vs_base }, Side::Missing) => {
            one_sided(*kind, vs_base, input.history, Direction::LocalOnly)
        }
        (Side::Missing, Side::Present { kind, vs_base }) => {
            one_sided(*kind, vs_base, input.history, Direction::RemoteOnly)
        }
        (
            Side::Present {
                kind: local_kind,
                vs_base: local_delta,
            },
            Side::Present {
                kind: remote_kind,
                vs_base: remote_delta,
            },
        ) if local_kind == remote_kind => {
            if *local_kind == Kind::Dir {
                return match input.history {
                    History::Verified(Kind::Dir) => Decision::InSync,
                    _ => Decision::Record,
                };
            }
            match input.history {
                History::Verified(Kind::File) => match (local_delta, remote_delta) {
                    (Delta::Unknown(reason), _) | (_, Delta::Unknown(reason)) => {
                        Decision::Retry(reason.clone())
                    }
                    (Delta::Same, Delta::Same) => Decision::InSync,
                    (local, remote) if local.same_contents() && remote.same_contents() => {
                        Decision::Record
                    }
                    (Delta::Changed, remote) if remote.same_contents() => Decision::Upload,
                    (local, Delta::Changed) if local.same_contents() => Decision::Download,
                    _ => match sides() {
                        Delta::Same | Delta::MetadataOnly => Decision::Record,
                        Delta::Changed => Decision::KeepBoth(ConflictKind::EditEdit),
                        Delta::Unknown(reason) => Decision::Retry(reason),
                    },
                },
                // Without a trusted baseline, equal contents converge and
                // different contents are kept side by side. Timestamp ties
                // and clock skew never pick a winner.
                _ => match sides() {
                    Delta::Same | Delta::MetadataOnly => Decision::Record,
                    Delta::Changed => Decision::KeepBoth(ConflictKind::Bootstrap),
                    Delta::Unknown(reason) => Decision::Retry(reason),
                },
            }
        }
        (
            Side::Present {
                kind: local_kind,
                vs_base: local_delta,
            },
            Side::Present {
                kind: remote_kind,
                vs_base: remote_delta,
            },
        ) => match input.history {
            History::Verified(kind) => {
                if let (Delta::Unknown(reason), _) | (_, Delta::Unknown(reason)) =
                    (local_delta, remote_delta)
                {
                    return Decision::Retry(reason.clone());
                }
                // Exactly one side changed its type: propagate it. The
                // caller turns this into a conflict when the replaced
                // folder still holds changed contents.
                if *local_kind == kind && local_delta.same_contents() {
                    Decision::Download
                } else if *remote_kind == kind && remote_delta.same_contents() {
                    Decision::Upload
                } else {
                    Decision::KeepBoth(ConflictKind::FileFolder)
                }
            }
            _ => Decision::KeepBoth(ConflictKind::FileFolder),
        },
    }
}

#[derive(Clone, Copy)]
enum Direction {
    LocalOnly,
    RemoteOnly,
}

fn one_sided(kind: Kind, delta: &Delta, history: History, direction: Direction) -> Decision {
    let (create, delete, restore) = match direction {
        Direction::LocalOnly => (
            Decision::Upload,
            Decision::DeleteLocal,
            Decision::RestoreToRemote,
        ),
        Direction::RemoteOnly => (
            Decision::Download,
            Decision::DeleteRemote,
            Decision::RestoreToLocal,
        ),
    };
    match history {
        History::None => create,
        // Legacy or damaged history cannot prove the missing side was
        // deleted, so it never authorizes deleting the surviving side.
        History::Unverified => create,
        History::Verified(base_kind) if base_kind != kind => restore,
        History::Verified(_) => match delta {
            Delta::Same | Delta::MetadataOnly => delete,
            Delta::Changed => restore,
            Delta::Unknown(reason) => Decision::Retry(reason.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(vs_base: Delta) -> Side {
        Side::Present {
            kind: Kind::File,
            vs_base,
        }
    }

    fn dir() -> Side {
        Side::Present {
            kind: Kind::Dir,
            vs_base: Delta::Same,
        }
    }

    fn input(local: Side, remote: Side, history: History, sides: Option<Delta>) -> Input {
        Input {
            local,
            remote,
            history,
            sides,
        }
    }

    const VERIFIED: History = History::Verified(Kind::File);

    #[test]
    fn no_baseline_and_equal_contents_records_without_copying() {
        for history in [History::None, History::Unverified] {
            let case = input(
                file(Delta::Changed),
                file(Delta::Changed),
                history,
                Some(Delta::Same),
            );
            assert_eq!(decide(&case), Decision::Record);
        }
    }

    #[test]
    fn no_baseline_and_different_contents_keeps_both() {
        let case = input(
            file(Delta::Changed),
            file(Delta::Changed),
            History::None,
            Some(Delta::Changed),
        );
        assert_eq!(decide(&case), Decision::KeepBoth(ConflictKind::Bootstrap));
        let kinds = input(dir(), file(Delta::Changed), History::None, None);
        assert_eq!(decide(&kinds), Decision::KeepBoth(ConflictKind::FileFolder));
    }

    #[test]
    fn no_history_and_one_side_creates_the_missing_copy() {
        let up = input(file(Delta::Changed), Side::Missing, History::None, None);
        assert_eq!(decide(&up), Decision::Upload);
        let down = input(Side::Missing, dir(), History::None, None);
        assert_eq!(decide(&down), Decision::Download);
    }

    #[test]
    fn legacy_history_with_a_missing_side_never_deletes() {
        let case = input(file(Delta::Same), Side::Missing, History::Unverified, None);
        assert_eq!(decide(&case), Decision::Upload);
        let case = input(Side::Missing, file(Delta::Same), History::Unverified, None);
        assert_eq!(decide(&case), Decision::Download);
    }

    #[test]
    fn unchanged_items_need_nothing_and_metadata_moves_are_recorded() {
        let case = input(file(Delta::Same), file(Delta::Same), VERIFIED, None);
        assert_eq!(decide(&case), Decision::InSync);
        let case = input(file(Delta::MetadataOnly), file(Delta::Same), VERIFIED, None);
        assert_eq!(decide(&case), Decision::Record);
    }

    #[test]
    fn one_changed_side_is_propagated() {
        let case = input(file(Delta::Changed), file(Delta::Same), VERIFIED, None);
        assert_eq!(decide(&case), Decision::Upload);
        let case = input(
            file(Delta::MetadataOnly),
            file(Delta::Changed),
            VERIFIED,
            None,
        );
        assert_eq!(decide(&case), Decision::Download);
    }

    #[test]
    fn identical_concurrent_edits_converge_without_conflict() {
        let case = input(
            file(Delta::Changed),
            file(Delta::Changed),
            VERIFIED,
            Some(Delta::Same),
        );
        assert!(needs_sides(&case.local, &case.remote, case.history));
        assert_eq!(decide(&case), Decision::Record);
    }

    #[test]
    fn different_concurrent_edits_keep_both() {
        let case = input(
            file(Delta::Changed),
            file(Delta::Changed),
            VERIFIED,
            Some(Delta::Changed),
        );
        assert_eq!(decide(&case), Decision::KeepBoth(ConflictKind::EditEdit));
    }

    #[test]
    fn deletions_propagate_only_over_unchanged_contents() {
        let case = input(Side::Missing, file(Delta::Same), VERIFIED, None);
        assert_eq!(decide(&case), Decision::DeleteRemote);
        let case = input(file(Delta::MetadataOnly), Side::Missing, VERIFIED, None);
        assert_eq!(decide(&case), Decision::DeleteLocal);
        let case = input(Side::Missing, file(Delta::Changed), VERIFIED, None);
        assert_eq!(decide(&case), Decision::RestoreToLocal);
        let case = input(file(Delta::Changed), Side::Missing, VERIFIED, None);
        assert_eq!(decide(&case), Decision::RestoreToRemote);
    }

    #[test]
    fn both_absent_retires_history() {
        let case = input(Side::Missing, Side::Missing, VERIFIED, None);
        assert_eq!(decide(&case), Decision::Retire);
        let case = input(Side::Missing, Side::Missing, History::None, None);
        assert_eq!(decide(&case), Decision::Nothing);
    }

    #[test]
    fn uncertainty_retries_instead_of_acting() {
        let case = input(
            Side::Unknown("offline".into()),
            Side::Missing,
            VERIFIED,
            None,
        );
        assert_eq!(decide(&case), Decision::Retry("offline".into()));
        let case = input(
            file(Delta::Unknown("changing".into())),
            file(Delta::Same),
            VERIFIED,
            None,
        );
        assert_eq!(decide(&case), Decision::Retry("changing".into()));
        let case = input(
            file(Delta::Changed),
            file(Delta::Changed),
            History::None,
            Some(Delta::Unknown("unreadable".into())),
        );
        assert_eq!(decide(&case), Decision::Retry("unreadable".into()));
    }

    #[test]
    fn type_changes_on_one_side_propagate_and_on_both_sides_conflict() {
        let case = input(dir(), file(Delta::Same), VERIFIED, None);
        assert_eq!(decide(&case), Decision::Upload);
        let case = input(file(Delta::Same), dir(), VERIFIED, None);
        assert_eq!(decide(&case), Decision::Download);
        let case = input(dir(), file(Delta::Changed), VERIFIED, None);
        assert_eq!(decide(&case), Decision::KeepBoth(ConflictKind::FileFolder));
    }

    #[test]
    fn deleted_type_replaced_side_is_restored() {
        let case = input(Side::Missing, dir(), VERIFIED, None);
        assert_eq!(decide(&case), Decision::RestoreToLocal);
    }

    #[test]
    fn folders_compare_by_kind() {
        let case = input(dir(), dir(), History::Verified(Kind::Dir), None);
        assert_eq!(decide(&case), Decision::InSync);
        let case = input(dir(), dir(), History::None, None);
        assert_eq!(decide(&case), Decision::Record);
        let case = input(Side::Missing, dir(), History::Verified(Kind::Dir), None);
        assert_eq!(decide(&case), Decision::DeleteRemote);
    }
}
