//! Cleaning the row under the cursor, without leaving the browser.
//!
//! The browser is where you *see* what is disposable — it colours every row by
//! what the rules say and shows how much of each is reclaimable. Leaving it to
//! retype the same path into `clean` was the friction this removes.
//!
//! Three things make it safe enough to be a keystroke.
//!
//! **It only ever removes what the rules already claim.** The key acts on the
//! plan for the subtree under the cursor, exactly as `clean <that path>` would.
//! On a row no rule claims it does nothing and says so — otherwise the tiers and
//! the denylist would be decoration on a general-purpose file deleter.
//!
//! **It always asks.** On the command line the confirmation is the verb: you
//! type `clean` yourself, and that is the moment of intent. A keypress has no
//! such moment, so the modal supplies one. What differs between them is the
//! price of a mistake, not whether you are asked: the trash takes a `y`, and
//! destroying takes the word `purge` typed out.
//!
//! **It plans on a worker.** Planning walks a tree and runs a `git status` per
//! repository; doing that on the UI thread would freeze the browser for as long
//! as it took, on the one screen whose whole point is that it stays responsive.

use disk_tools_core::{CleanOptions, CleanOutcome, CleanPlan, ScanOptions, apply, plan, scan};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;

/// Where a removal has got to.
#[derive(Debug)]
pub enum Removal {
    /// Walking and planning, on a worker. `Esc` abandons it.
    Planning {
        path: PathBuf,
        answer: Receiver<CleanPlan>,
    },
    /// The plan, waiting to be agreed to.
    Asking {
        path: PathBuf,
        plan: Box<CleanPlan>,
        /// Anything here is destroyed rather than trashed, so agreement is the
        /// word rather than a letter.
        destroys: bool,
        /// What has been typed towards that word so far.
        typed: String,
    },
    /// What it did, until dismissed.
    Done {
        path: PathBuf,
        outcome: Box<CleanOutcome>,
    },
    /// The rules claim nothing here — said out loud rather than ignored, so a
    /// key that did nothing cannot be mistaken for a key that did not register.
    Nothing { path: PathBuf },
}

impl Removal {
    /// Start planning the subtree under `path`.
    ///
    /// The walk happens on a worker and the browser keeps drawing; the answer is
    /// collected by [`Self::settle`] on the next frame that has one.
    pub fn begin(path: &Path, options: CleanOptions) -> Removal {
        let (send, answer) = channel();
        let root = path.to_path_buf();
        let walking = root.clone();
        thread::spawn(move || {
            let tree = scan(&ScanOptions {
                root: walking,
                ..ScanOptions::default()
            });
            // A closed channel means the browser moved on; nothing to report to.
            let _ = send.send(plan(&tree, &options));
        });
        Removal::Planning { path: root, answer }
    }

    /// Take the worker's answer if it has one.
    ///
    /// Returns `true` when the state changed, so the caller knows a redraw is
    /// worth the frame.
    pub fn settle(&mut self) -> bool {
        let Removal::Planning { path, answer } = self else {
            return false;
        };
        match answer.try_recv() {
            Ok(plan) if plan.candidates.is_empty() => {
                *self = Removal::Nothing { path: path.clone() };
                true
            }
            Ok(plan) => {
                // The **strictest** tier in the plan decides how it is agreed
                // to. A subtree usually holds both, and asking by the gentlest
                // would let one purge-tier candidate through on a `y`.
                let destroys = plan.candidates.iter().any(|candidate| candidate.purge);
                *self = Removal::Asking {
                    path: path.clone(),
                    plan: Box::new(plan),
                    destroys,
                    typed: String::new(),
                };
                true
            }
            Err(TryRecvError::Empty) => false,
            // The worker died — a panic in the walk. Say nothing was done,
            // because nothing was.
            Err(TryRecvError::Disconnected) => {
                *self = Removal::Nothing { path: path.clone() };
                true
            }
        }
    }

    /// The word that agrees to this, when a letter will not do.
    pub const WORD: &'static str = "purge";

    /// Has enough been typed to agree?
    pub fn agreed(&self) -> bool {
        match self {
            Removal::Asking {
                destroys: false, ..
            } => false,
            Removal::Asking { typed, .. } => typed == Removal::WORD,
            _ => false,
        }
    }

    /// Carry it out. Only ever reached through the modal.
    pub fn carry_out(&mut self) {
        let Removal::Asking { path, plan, .. } = self else {
            return;
        };
        let outcome = apply(plan, |_| {});
        *self = Removal::Done {
            path: path.clone(),
            outcome: Box::new(outcome),
        };
    }

    /// The path this is about, whatever state it is in.
    pub fn path(&self) -> &Path {
        match self {
            Removal::Planning { path, .. }
            | Removal::Asking { path, .. }
            | Removal::Done { path, .. }
            | Removal::Nothing { path } => path,
        }
    }
}

/// One rule's share of a plan, for the modal.
pub struct Share {
    pub rule: String,
    pub count: usize,
    pub allocated: u64,
    /// Where they go. The **rule's** tier is not here: what a reader of the
    /// modal needs is the destination, and `--purge` can make a trash-tier
    /// candidate destroyed without changing its tier.
    pub purge: bool,
}

/// The plan, grouped the way the `-d 0` report groups it.
///
/// The same shape deliberately: what the modal shows and what `preview` prints
/// have to be the same claim about the same paths.
pub fn shares(plan: &CleanPlan) -> Vec<Share> {
    let mut shares: Vec<Share> = Vec::new();
    for candidate in &plan.candidates {
        match shares.iter_mut().find(|share| share.rule == candidate.rule) {
            Some(share) => {
                share.count += 1;
                share.allocated += candidate.allocated;
            }
            None => shares.push(Share {
                rule: candidate.rule.clone(),
                count: 1,
                allocated: candidate.allocated,
                purge: candidate.purge,
            }),
        }
    }
    shares.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(a.rule.cmp(&b.rule)));
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::{Candidate, Kept, Tier};

    fn candidate(rule: &str, purge: bool, allocated: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(format!("/p/{rule}")),
            rule: rule.into(),
            tier: if purge { Tier::Purge } else { Tier::Confirm },
            purge,
            duplicate_of: None::<Kept>,
            allocated,
            shared: false,
        }
    }

    fn asking(candidates: Vec<Candidate>) -> Removal {
        let destroys = candidates.iter().any(|c| c.purge);
        Removal::Asking {
            path: PathBuf::from("/p"),
            plan: Box::new(CleanPlan {
                reclaimable: candidates.iter().map(|c| c.allocated).sum(),
                candidates,
                ..CleanPlan::default()
            }),
            destroys,
            typed: String::new(),
        }
    }

    /// A subtree usually holds both tiers, and asking by the gentlest would let
    /// a purge-tier candidate through on one letter.
    #[test]
    fn one_destroying_candidate_makes_the_whole_thing_ask_for_the_word() {
        let mixed = asking(vec![
            candidate("trashed", false, 4096),
            candidate("destroyed", true, 8192),
        ]);

        assert!(matches!(mixed, Removal::Asking { destroys: true, .. }));
    }

    #[test]
    fn nothing_destroying_asks_for_a_letter() {
        let gentle = asking(vec![candidate("trashed", false, 4096)]);
        assert!(matches!(
            gentle,
            Removal::Asking {
                destroys: false,
                ..
            }
        ));
        assert!(
            !gentle.agreed(),
            "a letter is not typed into the word; the caller settles that key"
        );
    }

    /// Typed short, typed wrong, typed right.
    #[test]
    fn the_word_has_to_be_the_word() {
        let mut asking = asking(vec![candidate("destroyed", true, 8192)]);

        for attempt in ["", "pur", "purgeee", "PURGE"] {
            if let Removal::Asking { typed, .. } = &mut asking {
                *typed = attempt.to_owned();
            }
            assert!(!asking.agreed(), "`{attempt}` must not agree");
        }

        if let Removal::Asking { typed, .. } = &mut asking {
            *typed = Removal::WORD.to_owned();
        }
        assert!(asking.agreed());
    }

    /// The modal shows what `preview -d 0` would print about the same paths, so
    /// the two cannot describe one plan differently.
    #[test]
    fn the_shares_are_grouped_by_rule_largest_first() {
        let plan = CleanPlan {
            candidates: vec![
                candidate("small", false, 1024),
                candidate("big", true, 8192),
                candidate("big", true, 4096),
            ],
            ..CleanPlan::default()
        };

        let shares = shares(&plan);
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].rule, "big");
        assert_eq!(shares[0].count, 2);
        assert_eq!(shares[0].allocated, 12_288);
        assert!(shares[0].purge);
        assert_eq!(shares[1].rule, "small");
    }
}
