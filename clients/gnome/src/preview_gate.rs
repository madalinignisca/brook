// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Whether an attachment row's preview is still wanted (#259). The row's widgets hold one of
//! these: asking for a preview (the "Show preview" click, or the setting being on) takes a
//! [`Token`]; turning the setting off cancels it, and every step of the preview (the queued
//! job, the fetch, the decode, the draw) checks the token, so nothing is fetched, decoded or
//! drawn for a preview nobody wants any more. One preview per generation: asking again while
//! one is asked for does nothing.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A claim on one generation of a row's preview. `Send`, so the fetch and decode (on the
/// runtime) can check it between their steps.
#[derive(Clone, Debug)]
pub struct Token {
    generation: u64,
    shared: Arc<AtomicU64>,
}

impl Token {
    /// Still the row's current generation (nothing cancelled it since).
    pub fn is_current(&self) -> bool {
        self.shared.load(Ordering::SeqCst) == self.generation
    }
}

/// Per-row state: the current generation, and whether a preview was asked for in it.
#[derive(Debug, Default)]
pub struct Gate {
    generation: Arc<AtomicU64>,
    asked: Cell<bool>,
}

impl Gate {
    /// A claim on the current generation, without asking (to check later that nothing was
    /// cancelled meanwhile, e.g. while the metered-connection rule is being looked up).
    pub fn token(&self) -> Token {
        Token {
            generation: self.generation.load(Ordering::SeqCst),
            shared: self.generation.clone(),
        }
    }

    /// Ask for a preview: a token for it, or `None` when one is already asked for (queued,
    /// running or drawn) in this generation.
    pub fn begin(&self) -> Option<Token> {
        if self.asked.replace(true) {
            None
        } else {
            Some(self.token())
        }
    }

    /// Whether a preview is asked for in this generation.
    pub fn is_asked(&self) -> bool {
        self.asked.get()
    }

    /// Drop what is asked for: every token taken so far stops being current, and a new
    /// preview can be asked for.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.asked.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_preview_is_asked_for_at_a_time() {
        let gate = Gate::default();
        let token = gate.begin().expect("the first ask");
        assert!(token.is_current());
        assert!(
            gate.begin().is_none(),
            "a click then the setting on: one fetch"
        );
        assert!(gate.is_asked());
    }

    #[test]
    fn cancelling_stops_every_step_of_what_was_asked_for() {
        let gate = Gate::default();
        let token = gate.begin().unwrap();
        // The job is queued, the fetch is running: the setting goes off.
        gate.cancel();
        assert!(
            !token.is_current(),
            "the queued job, the fetch and the draw all check this"
        );
        assert!(!gate.is_asked());
    }

    #[test]
    fn a_new_ask_after_a_cancel_is_a_new_generation() {
        let gate = Gate::default();
        let old = gate.begin().unwrap();
        gate.cancel();
        let new = gate.begin().expect("on again after off");
        assert!(new.is_current());
        assert!(!old.is_current(), "the old one does not come back to life");
    }

    #[test]
    fn a_claim_taken_before_a_wait_notices_a_cancel_during_it() {
        // The metered rule: take a claim, look the file up, then start only if still current.
        let gate = Gate::default();
        let before = gate.token();
        gate.cancel();
        assert!(!before.is_current());
        let before = gate.token();
        assert!(before.is_current());
        assert!(
            gate.begin().is_some(),
            "and starting takes the same generation"
        );
        assert!(before.is_current());
    }

    #[test]
    fn a_token_can_be_checked_from_another_thread() {
        let gate = Gate::default();
        let token = gate.begin().unwrap();
        let seen = std::thread::spawn({
            let token = token.clone();
            move || token.is_current()
        })
        .join()
        .unwrap();
        assert!(seen);
        gate.cancel();
        let seen = std::thread::spawn(move || token.is_current())
            .join()
            .unwrap();
        assert!(!seen);
    }
}
