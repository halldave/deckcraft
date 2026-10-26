//! The host bridge's decisions, kept free of `web_sys` so they are tested natively: which parent
//! origin the bridge may talk to, and which saves the parent has confirmed.

use std::collections::BTreeMap;

/// How long the parent has to answer `deckcraft:save` with `deckcraft:saved` (milliseconds).
pub const SAVE_ACK_TIMEOUT_MS: f64 = 30_000.0;

/// The origin the bridge posts to and hears from, or `None` for no bridge (saves download).
///
/// - `own`: this page's origin.
/// - `parent`: the parent window's origin, `None` when reading it threw (a cross-origin parent).
/// - `allowed`: the `?host_origin=` query value, an origin the embedding page vouches for.
///
/// A same-origin parent is accepted. A cross-origin parent is accepted only when `allowed` is a
/// valid URL origin (`scheme://host[:port]`, nothing else); messages then go to that exact origin,
/// so the browser drops them if the parent is anything else. Opaque origins (`"null"`) never match.
pub fn target_origin(own: &str, parent: Option<&str>, allowed: Option<&str>) -> Option<String> {
    if let Some(parent) = parent
        && parent == own
        && is_origin(own)
    {
        return Some(own.to_string());
    }
    let allowed = allowed?.trim();
    is_origin(allowed).then(|| allowed.to_string())
}

/// True for a serialised URL origin: `http(s)://host[:port]` with no path, query, fragment or
/// credentials.
pub fn is_origin(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else { return false };
    if !matches!(scheme, "http" | "https") || rest.is_empty() {
        return false;
    }
    // IPv6 literals aren't accepted; name the host instead.
    if rest.contains(['/', '?', '#', '@', ' ', '\\', '[', ']']) {
        return false;
    }
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    if host.is_empty() {
        return false;
    }
    match port {
        Some(p) => p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok(),
        None => true,
    }
}

/// Saves posted to the parent and not yet confirmed, each carrying `T` (for a presentation
/// save: which presentation, as it was when posted).
#[derive(Debug)]
pub struct SaveAcks<T> {
    next_id: u64,
    /// Request id → (file name, deadline in ms, payload).
    pending: BTreeMap<u64, (String, f64, Option<T>)>,
}

impl<T> Default for SaveAcks<T> {
    fn default() -> Self {
        Self { next_id: 0, pending: BTreeMap::new() }
    }
}

impl<T> SaveAcks<T> {
    /// Record a save posted at `now`; returns its request id.
    pub fn posted(&mut self, name: &str, now: f64) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.insert(self.next_id, (name.to_string(), now + SAVE_ACK_TIMEOUT_MS, None));
        self.next_id
    }

    /// Attach what a confirmation of `id` applies to.
    pub fn attach(&mut self, id: u64, payload: T) {
        if let Some(entry) = self.pending.get_mut(&id) {
            entry.2 = Some(payload);
        }
    }

    /// The parent confirmed `id`: its payload, or `None` when no such save is waiting (an
    /// unknown id, or one that already timed out).
    pub fn saved(&mut self, id: u64) -> Option<Option<T>> {
        self.pending.remove(&id).map(|(_, _, payload)| payload)
    }

    /// Forget `id` (its post failed); returns its name.
    pub fn failed(&mut self, id: u64) -> Option<String> {
        self.pending.remove(&id).map(|(name, _, _)| name)
    }

    /// Saves past their deadline at `now`, removed and returned as `(id, name)`.
    pub fn expire(&mut self, now: f64) -> Vec<(u64, String)> {
        let late: Vec<u64> = self.pending.iter().filter(|(_, (_, deadline, _))| now >= *deadline).map(|(id, _)| *id).collect();
        late.into_iter().filter_map(|id| self.pending.remove(&id).map(|(name, _, _)| (id, name))).collect()
    }

    /// Whether any save is still waiting for the parent.
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// A hash of saved bytes, to tell whether a snapshot taken later is the one that was posted.
pub fn bytes_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "https://app.example";

    #[test]
    fn same_origin_parent_enables_the_bridge() {
        assert_eq!(target_origin(OWN, Some(OWN), None).as_deref(), Some(OWN));
    }

    #[test]
    fn cross_origin_parent_disables_the_bridge() {
        // Reading a cross-origin parent's origin throws (None) or names another origin.
        assert_eq!(target_origin(OWN, None, None), None);
        assert_eq!(target_origin(OWN, Some("https://evil.example"), None), None);
        assert_eq!(target_origin(OWN, Some("null"), None), None);
        assert_eq!(target_origin("null", Some("null"), None), None);
    }

    #[test]
    fn host_origin_parameter_must_be_an_exact_origin() {
        let files = "https://files.example:8443";
        assert_eq!(target_origin(OWN, None, Some(files)).as_deref(), Some(files));
        for bad in
            ["*", "null", "files.example", "https://files.example/", "https://a@files.example", "javascript://x", "https://", "https://x:99999"]
        {
            assert_eq!(target_origin(OWN, None, Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn a_save_waits_for_its_own_confirmation() {
        let mut acks = SaveAcks::<&str>::default();
        let id = acks.posted("Deck.deckcraft", 0.0);
        acks.attach(id, "snapshot");
        assert_eq!(acks.saved(id + 1), None, "an unknown id confirms nothing");
        assert!(acks.waiting());
        assert_eq!(acks.saved(id), Some(Some("snapshot")));
        assert!(!acks.waiting());
        assert_eq!(acks.saved(id), None, "a confirmation counts once");
    }

    #[test]
    fn an_unanswered_save_times_out() {
        let mut acks = SaveAcks::<()>::default();
        let id = acks.posted("Deck.deckcraft", 1_000.0);
        assert!(acks.expire(1_000.0 + SAVE_ACK_TIMEOUT_MS - 1.0).is_empty());
        assert_eq!(acks.expire(1_000.0 + SAVE_ACK_TIMEOUT_MS), vec![(id, "Deck.deckcraft".to_string())]);
        assert_eq!(acks.saved(id), None, "a late confirmation is ignored: the save was reported failed");
    }

    #[test]
    fn bytes_hash_tells_saves_apart() {
        assert_eq!(bytes_hash(b"deck"), bytes_hash(b"deck"));
        assert_ne!(bytes_hash(b"deck"), bytes_hash(b"deck!"));
    }
}
