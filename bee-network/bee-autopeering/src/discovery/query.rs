// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{
    discovery::manager,
    event::EventTx,
    peer::{
        lists::{ActivePeer, ActivePeersList, EntryPeersList, ReplacementPeersList},
        PeerId,
    },
    request::RequestManager,
    server::ServerTx,
    task::Repeat,
};

use rand::{thread_rng, Rng as _};

#[derive(Clone)]
pub(crate) struct QueryContext {
    pub(crate) request_mngr: RequestManager,
    pub(crate) entry_peers: EntryPeersList,
    pub(crate) active_peers: ActivePeersList,
    pub(crate) replacements: ReplacementPeersList,
    pub(crate) server_tx: ServerTx,
    pub(crate) event_tx: EventTx,
}

// Hive.go: pings the oldest active peer.
pub(crate) fn reverify_fn() -> Repeat<QueryContext> {
    Box::new(|ctx| {
        // Determine the next peer to re/verifiy.
        if let Some(peer_id) = peer_to_reverify(&ctx.active_peers) {
            let ctx_ = ctx.clone();

            // TODO: introduce `UnsupervisedTask` type, that always finishes after a timeout.
            let _ = tokio::spawn(async move {
                if let Some(services) =
                    manager::begin_verification(&peer_id, &ctx_.active_peers, &ctx_.request_mngr, &ctx_.server_tx).await
                {
                    // Hive.go: no need to do anything here, as the peer is bumped when handling the pong
                    log::debug!(
                        "Reverified {}. Peer offers {} service/s: {}",
                        &peer_id,
                        services.len(),
                        services
                    );
                } else {
                    log::debug!("Failed to reverify {}. Removing peer.", peer_id);

                    manager::remove_peer_from_active_list(
                        &peer_id,
                        &ctx_.entry_peers,
                        &ctx_.active_peers,
                        &ctx_.replacements,
                        &ctx_.event_tx,
                    )
                }
            });
        } else {
            log::debug!("Currently no peers to reverify.");
        }
    })
}

// Hive.go: returns the oldest peer, or nil if empty.
fn peer_to_reverify(active_peers: &ActivePeersList) -> Option<PeerId> {
    active_peers.read().get_oldest().map(|p| *p.peer_id())
}

// Hive.go:
// The current strategy is to always select the latest verified peer and one of
// the peers that returned the most number of peers the last time it was queried.
pub(crate) fn query_fn() -> Repeat<QueryContext> {
    Box::new(|ctx| {
        let peers = select_peers_to_query(&ctx.active_peers);
        if peers.is_empty() {
            log::debug!("No peers to query.");
        } else {
            log::debug!("Querying {} peer/s...", peers.len());

            for peer_id in peers.into_iter() {
                let ctx_ = ctx.clone();

                // TODO: introduce `UnsupervisedTask` type, that always finishes after a timeout.
                tokio::spawn(async move {
                    if let Some(peers) =
                        manager::begin_discovery(&peer_id, &ctx_.active_peers, &ctx_.request_mngr, &ctx_.server_tx)
                            .await
                    {
                        log::debug!("Query successful. Received {} peers.", peers.len());
                    } else {
                        log::debug!("Query unsuccessful. Removing peer {}.", peer_id);

                        manager::remove_peer_from_active_list(
                            &peer_id,
                            &ctx_.entry_peers,
                            &ctx_.active_peers,
                            &ctx_.replacements,
                            &ctx_.event_tx,
                        )
                    }
                });
            }
        }
    })
}

// Hive.go: selects the peers that should be queried.
fn select_peers_to_query(active_peers: &ActivePeersList) -> Vec<PeerId> {
    let mut verif_peers = manager::get_verified_peers(active_peers);

    // If we have less than 3 verified peers, then we use those for the query.
    if verif_peers.len() < 3 {
        verif_peers.into_iter().map(|ap| *ap.peer_id()).collect::<Vec<_>>()
    } else {
        // Note: this macro is useful to remove some noise from the pattern matching rules.
        macro_rules! num {
            ($t:expr) => {
                // Panic: we made sure, that unwrap is always okay.
                $t.as_ref().unwrap().metrics().last_new_peers()
            };
        }

        let latest = *verif_peers.remove(0).peer_id();
        let len = verif_peers.len().min(3);

        // Note: This loop finds the three "heaviest" peers with one iteration over an unsorted vec of verified peers.
        let heaviest3 = verif_peers.into_iter().fold(
            (None, None, None),
            |(x, y, z): (Option<ActivePeer>, Option<ActivePeer>, Option<ActivePeer>), p| {
                let n = p.metrics().last_new_peers();

                match (&x, &y, &z) {
                    // set 1st
                    (None, _, _) => (Some(p), y, z),
                    // shift-right + set 1st
                    (t, None, _) if n < num!(t) => (Some(p), t.clone(), z),
                    // set 2nd
                    (t, None, _) if n >= num!(t) => (x, Some(p), z),
                    // shift-right + shift-right + set 1st
                    (s, t, None) if n < num!(s) => (Some(p), s.clone(), t.clone()),
                    // shift-right + set 1st
                    (_, t, None) if n < num!(t) => (x, Some(p), t.clone()),
                    // set 3rd
                    (_, t, None) if n >= num!(t) => (x, y, Some(p)),
                    // no-op
                    (t, _, _) if n < num!(t) => (x, y, z),
                    // set 1st
                    (_, t, _) if n < num!(t) => (Some(p), y, z),
                    // shift-left + set 2nd
                    (_, _, t) if n < num!(t) => (y, Some(p), z),
                    // shift-left + shift-left + set 3rd
                    (_, _, _) => (y, z, Some(p)),
                }
            },
        );

        let r = thread_rng().gen_range(0..len);
        let heaviest = *match r {
            0 => heaviest3.0,
            1 => heaviest3.1,
            2 => heaviest3.2,
            _ => unreachable!(),
        }
        // Panic: we made sure that the unwrap is always possible.
        .unwrap()
        .peer_id();

        vec![latest, heaviest]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::{lists::ActivePeer, Peer};

    fn create_peerlist_of_size(n: usize) -> ActivePeersList {
        // Create a set of active peer entries.
        let entries = (0..n as u8).map(Peer::new_test_peer).map(ActivePeer::new);

        // Create a peerlist, and insert the peer entries setting the `last_new_peers` metric
        // equal to its peerlist index. We also need to set the `verified_count` to at least 1.
        let peerlist = ActivePeersList::default();
        let mut pl = peerlist.write();
        for (i, mut entry) in entries.into_iter().enumerate() {
            entry.metrics_mut().set_last_new_peers((n - 1) - i);
            entry.metrics_mut().increment_verified_count();

            pl.insert(entry);
        }
        drop(pl);
        peerlist
    }

    #[test]
    fn find_peers_to_query_in_peerlist_1() {
        let peerlist = create_peerlist_of_size(1);

        let selected = select_peers_to_query(&peerlist);
        assert_eq!(1, selected.len());
    }

    #[test]
    fn find_peers_to_query_in_peerlist_2() {
        let peerlist = create_peerlist_of_size(2);

        let selected = select_peers_to_query(&peerlist);
        assert_eq!(2, selected.len());
    }

    #[test]
    fn find_peers_to_query_in_peerlist_3() {
        let peerlist = create_peerlist_of_size(3);

        macro_rules! equal {
            ($a:expr, $b:expr) => {{ $a == peerlist.read().get($b).unwrap().peer_id() }};
        }

        let selected = select_peers_to_query(&peerlist);
        assert_eq!(2, selected.len());

        assert!(equal!(&selected[0], 0));
        assert!(equal!(&selected[1], 1) || equal!(&selected[1], 2));
    }

    #[test]
    fn find_peers_to_query_in_peerlist_10() {
        let peerlist = create_peerlist_of_size(10);

        macro_rules! equal {
            ($a:expr, $b:expr) => {{ $a == peerlist.read().get($b).unwrap().peer_id() }};
        }

        // 0 1 2 3 4 ... 7 8 9 (index)
        // 0 1 2 3 4 ... 7 8 9 (last_new_peers)
        // ^             ^ ^ ^
        // 0             1 1 1 (expected)
        let selected = select_peers_to_query(&peerlist);
        assert_eq!(2, selected.len());

        // Always the newest peer (index 0) is selected.
        assert!(equal!(&selected[0], 0));
        // Either of the 3 "heaviest" peers is selected.
        assert!(equal!(&selected[1], 7) || equal!(&selected[1], 8) || equal!(&selected[1], 9));

        // 0 1 2 3 4 ... 7 8 9 (index)
        // 8 9 0 1 2 ... 5 6 7 (last_new_peers)
        // ^ ^             ^ ^
        // 0 1             1 1 (expected)
        peerlist.write().rotate_forwards();
        peerlist.write().rotate_forwards();

        let selected = select_peers_to_query(&peerlist);
        assert_eq!(2, selected.len());

        assert!(equal!(&selected[0], 0));
        assert!(equal!(&selected[1], 1) || equal!(&selected[1], 8) || equal!(&selected[1], 9));
    }
}
