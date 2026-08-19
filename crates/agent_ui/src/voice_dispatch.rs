//! Deciding which thread a spoken command is addressed to.
//!
//! The premise of speaking to Echo is that the user is not looking at the
//! screen, so "the thread you are looking at" is not an answer. What they can
//! perceive is which thread last said something to them, and which one has
//! stopped and is waiting. Those are the two signals this ranks.
//!
//! Ranking is pure over a snapshot so it can be tested without building a
//! panel; the caller assembles the snapshot from the live conversation views.

use agent_client_protocol::schema::v1 as acp;
use std::time::Instant;

/// One thread's standing at the moment a spoken command lands.
#[derive(Clone, Debug)]
pub struct VoiceCandidate {
    pub session_id: acp::SessionId,
    pub blocked_on_approval: bool,
    pub last_spoke_at: Option<Instant>,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTarget {
    pub session_id: acp::SessionId,
}

/// Decides which thread a spoken command addresses.
///
/// Blocked first: a blocked thread is the only one that cannot make progress
/// on its own, and a blocked subagent transitively blocks its parent, so
/// answering it unblocks more than one thing. Then whoever spoke most
/// recently, which is what the user was replying to. The active thread is a
/// last resort rather than the default.
pub fn pick_voice_target(candidates: &[VoiceCandidate]) -> Option<VoiceTarget> {
    let blocked = candidates
        .iter()
        .filter(|candidate| candidate.blocked_on_approval)
        .max_by_key(|candidate| candidate.last_spoke_at);
    let spoke = candidates
        .iter()
        .filter(|candidate| candidate.last_spoke_at.is_some())
        .max_by_key(|candidate| candidate.last_spoke_at);
    let active = candidates.iter().find(|candidate| candidate.is_active);

    blocked.or(spoke).or(active).map(|candidate| VoiceTarget {
        session_id: candidate.session_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(id: &str) -> VoiceCandidate {
        VoiceCandidate {
            session_id: acp::SessionId::new(id),
            blocked_on_approval: false,
            last_spoke_at: None,
            is_active: false,
        }
    }

    /// A blocked thread outranks a talking one: it is the only one that cannot
    /// make progress, and a blocked subagent transitively blocks its parent.
    #[test]
    fn a_thread_blocked_on_approval_wins_over_one_that_is_speaking() {
        let now = Instant::now();
        let mut talking = candidate("talking");
        talking.last_spoke_at = Some(now);
        let mut blocked = candidate("blocked");
        blocked.blocked_on_approval = true;

        let target = pick_voice_target(&[talking, blocked]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("blocked"));
    }

    /// Two blocked threads is the parallel-subagent case: the one that spoke
    /// most recently is the one whose question the user just heard.
    #[test]
    fn the_more_recently_heard_of_two_blocked_threads_wins() {
        let now = Instant::now();
        let mut older = candidate("older");
        older.blocked_on_approval = true;
        older.last_spoke_at = Some(now - Duration::from_secs(30));
        let mut newer = candidate("newer");
        newer.blocked_on_approval = true;
        newer.last_spoke_at = Some(now);

        let target = pick_voice_target(&[older, newer]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("newer"));
    }

    #[test]
    fn the_most_recent_speaker_wins_when_nothing_is_blocked() {
        let now = Instant::now();
        let mut older = candidate("older");
        older.last_spoke_at = Some(now - Duration::from_secs(30));
        let mut newer = candidate("newer");
        newer.last_spoke_at = Some(now);

        let target = pick_voice_target(&[older, newer]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("newer"));
    }

    /// Nothing has spoken yet, so there is nothing the user could have been
    /// replying to and the thread on screen is the best guess left.
    #[test]
    fn the_active_thread_is_the_fallback_when_nothing_has_spoken() {
        let mut active = candidate("active");
        active.is_active = true;

        let target = pick_voice_target(&[candidate("other"), active]).unwrap();
        assert_eq!(target.session_id, acp::SessionId::new("active"));
    }

    #[test]
    fn no_candidates_means_no_target() {
        assert!(pick_voice_target(&[]).is_none());
    }
}
