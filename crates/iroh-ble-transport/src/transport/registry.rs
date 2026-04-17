//! Pure state-machine registry. No I/O, no blew, no tokio tasks.
//!
//! The actor loop lives in [`Registry::run`] (added in a later task).
//! `handle` is the synchronous entry point used by both the loop and tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use atomic_waker::AtomicWaker;
use blew::DeviceId;
use tokio::sync::mpsc;

use crate::transport::driver::Driver;
use crate::transport::interface::BleInterface;
#[allow(unused_imports)]
use crate::transport::peer::{PeerAction, PeerCommand, PeerEntry, PeerPhase};
use crate::transport::transport::L2capPolicy;

const MAX_CONNECT_ATTEMPTS: u32 = 15;
const DRAINING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const RESTORING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const DEAD_GC_TTL: std::time::Duration = std::time::Duration::from_secs(60);
pub(crate) const L2CAP_SELECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Debug)]
pub struct Registry {
    peers: HashMap<DeviceId, PeerEntry>,
    l2cap_policy: L2capPolicy,
    /// Prefixes whose identity has been verified by iroh's QUIC handshake.
    /// Populated by VerifiedEndpoint; consulted by handle_advertised (Task 4)
    /// and the dedup pass (Task 7).
    verified_prefixes: HashMap<crate::transport::peer::KeyPrefix, iroh_base::EndpointId>,
    my_endpoint: iroh_base::EndpointId,
    my_prefix: crate::transport::peer::KeyPrefix,
}

impl Registry {
    pub fn new(l2cap_policy: L2capPolicy, my_endpoint: iroh_base::EndpointId) -> Self {
        let my_prefix = crate::transport::routing::prefix_from_endpoint(&my_endpoint);
        Self {
            peers: HashMap::new(),
            l2cap_policy,
            verified_prefixes: HashMap::new(),
            my_endpoint,
            my_prefix,
        }
    }

    pub fn new_for_test() -> Self {
        let ep = iroh_base::SecretKey::from_bytes(&[0u8; 32]).public();
        Self::new(L2capPolicy::Disabled, ep)
    }

    pub fn new_for_test_with_policy(l2cap_policy: L2capPolicy) -> Self {
        let ep = iroh_base::SecretKey::from_bytes(&[0u8; 32]).public();
        Self::new(l2cap_policy, ep)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_test_with_endpoint(endpoint: iroh_base::EndpointId) -> Self {
        Self::new(L2capPolicy::Disabled, endpoint)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_test_with_policy_and_endpoint(
        l2cap_policy: L2capPolicy,
        endpoint: iroh_base::EndpointId,
    ) -> Self {
        Self::new(l2cap_policy, endpoint)
    }

    pub fn handle(&mut self, cmd: PeerCommand) -> Vec<PeerAction> {
        let now = std::time::Instant::now();
        let mut actions = Vec::new();
        match cmd {
            PeerCommand::Advertised {
                prefix,
                device,
                rssi,
            } => self.handle_advertised(&mut actions, now, prefix, device, rssi),
            PeerCommand::SendDatagram {
                device_id,
                tx_gen,
                datagram,
                waker,
            } => self.handle_send_datagram(&mut actions, now, device_id, tx_gen, datagram, waker),
            PeerCommand::ConnectSucceeded { device_id, channel } => {
                self.handle_connect_succeeded(&mut actions, now, device_id, channel);
            }
            PeerCommand::ConnectFailed { device_id, error } => {
                self.handle_connect_failed(&mut actions, now, device_id, &error);
            }
            PeerCommand::InboundGattFragment {
                device_id,
                source: _,
                bytes,
            } => self.handle_inbound_gatt_fragment(&mut actions, now, device_id, bytes),
            PeerCommand::CentralDisconnected { device_id, cause } => {
                self.handle_central_disconnected(&mut actions, now, device_id, cause);
            }
            PeerCommand::AdapterStateChanged { powered } => {
                self.handle_adapter_state_changed(&mut actions, now, powered);
            }
            PeerCommand::RestoreFromAdapter { devices } => {
                self.handle_restore_from_adapter(&mut actions, now, devices);
            }
            PeerCommand::Tick(tick_now) => self.handle_tick(&mut actions, tick_now),
            PeerCommand::Forget { device_id } => {
                self.handle_forget(&mut actions, now, device_id);
            }
            PeerCommand::Stalled { device_id } => {
                self.handle_stalled(&mut actions, now, device_id);
            }
            PeerCommand::Shutdown => self.handle_shutdown(&mut actions),
            PeerCommand::DataPipeReady {
                device_id,
                outbound_tx,
                inbound_tx,
                swap_tx,
            } => self.handle_data_pipe_ready(device_id, outbound_tx, inbound_tx, swap_tx),
            PeerCommand::OpenL2capSucceeded {
                device_id,
                channel: l2cap_chan,
            } => self.handle_open_l2cap_succeeded(&mut actions, now, device_id, l2cap_chan),
            PeerCommand::OpenL2capFailed { device_id, error } => {
                self.handle_open_l2cap_failed(&mut actions, now, device_id, &error);
            }
            PeerCommand::InboundL2capChannel { device_id, channel } => {
                self.handle_inbound_l2cap_channel(&mut actions, now, device_id, channel);
            }
            PeerCommand::CentralConnected { device_id: _ } => {
                // Informational: blew signals the physical BLE link came up. The
                // authoritative "Connecting -> Handshaking" transition is driven
                // by PeerCommand::ConnectSucceeded from the driver once GATT
                // subscribe + optional PSM read have completed, so we don't act
                // here. Kept as an explicit arm so the match stays exhaustive.
            }
            PeerCommand::ProtocolVersionMismatch {
                device_id,
                got,
                want,
            } => self.handle_protocol_version_mismatch(&mut actions, now, device_id, got, want),
            PeerCommand::VerifiedEndpoint { endpoint_id } => {
                self.handle_verified_endpoint(&mut actions, now, endpoint_id);
            }
            PeerCommand::PeripheralClientSubscribed {
                client_id,
                char_uuid,
            } => {
                self.handle_peripheral_client_subscribed(&mut actions, now, client_id, char_uuid);
            }
            PeerCommand::L2capHandoverTimeout { device_id } => {
                self.handle_l2cap_handover_timeout(&mut actions, device_id);
            }
        }
        actions
    }

    fn handle_verified_endpoint(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        endpoint_id: iroh_base::EndpointId,
    ) {
        let prefix = crate::transport::routing::prefix_from_endpoint(&endpoint_id);
        self.verified_prefixes.insert(prefix, endpoint_id);
        for entry in self.peers.values_mut() {
            if entry.prefix == Some(prefix) {
                entry.verified_endpoint = Some(endpoint_id);
                entry.verified_at = Some(now);
            }
        }
        let to_cancel: Vec<DeviceId> = self
            .peers
            .iter()
            .filter_map(|(did, e)| match &e.phase {
                PeerPhase::PendingDial { prefix: p, .. } if *p == prefix => Some(did.clone()),
                _ => None,
            })
            .collect();
        for did in to_cancel {
            if let Some(entry) = self.peers.get_mut(&did) {
                let drain_acks = Self::drain_to_draining(
                    entry,
                    now,
                    crate::transport::peer::DisconnectReason::DedupLoser,
                );
                actions.extend(drain_acks);
            }
        }

        let candidates: Vec<DeviceId> = self
            .peers
            .iter()
            .filter(|(_, e)| {
                e.prefix == Some(prefix) && matches!(e.phase, PeerPhase::Connected { .. })
            })
            .map(|(did, _)| did.clone())
            .collect();
        if candidates.len() >= 2 {
            let my_endpoint = self.my_endpoint;
            let winner = candidates
                .iter()
                .find(|did| {
                    let role = self.peers[*did].role;
                    crate::transport::dedup::should_win(role, &my_endpoint, &endpoint_id)
                })
                .cloned();
            for did in candidates {
                if Some(&did) == winner.as_ref() {
                    continue;
                }
                if let Some(entry) = self.peers.get_mut(&did) {
                    let drain_acks = Self::drain_to_draining(
                        entry,
                        now,
                        crate::transport::peer::DisconnectReason::DedupLoser,
                    );
                    actions.extend(drain_acks);
                }
            }
        }

        if matches!(self.l2cap_policy, L2capPolicy::PreferL2cap) {
            let to_upgrade: Vec<DeviceId> = self
                .peers
                .iter()
                .filter(|(_, e)| {
                    e.prefix == Some(prefix)
                        && !e.l2cap_upgrade_failed
                        && matches!(
                            e.phase,
                            PeerPhase::Connected {
                                upgrading: false,
                                channel: crate::transport::peer::ChannelHandle {
                                    path: crate::transport::peer::ConnectPath::Gatt,
                                    ..
                                },
                                ..
                            }
                        )
                })
                .map(|(did, _)| did.clone())
                .collect();
            for did in to_upgrade {
                if let Some(entry) = self.peers.get_mut(&did) {
                    if let PeerPhase::Connected { upgrading, .. } = &mut entry.phase {
                        *upgrading = true;
                    }
                    actions.push(PeerAction::UpgradeToL2cap { device_id: did });
                }
            }
        }
    }

    fn handle_peripheral_client_subscribed(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        client_id: DeviceId,
        _char_uuid: uuid::Uuid,
    ) {
        let entry = self
            .peers
            .entry(client_id.clone())
            .or_insert_with(|| PeerEntry::new(client_id.clone()));
        // Idempotent across C2P/P2C: if we already materialized a
        // Peripheral-role Connected entry on the first subscribe, the second
        // characteristic's SubscriptionChanged must not restart the pipe.
        if matches!(&entry.phase, PeerPhase::Connected { .. })
            && entry.role == crate::transport::peer::ConnectRole::Peripheral
        {
            return;
        }
        entry.role = crate::transport::peer::ConnectRole::Peripheral;
        entry.tx_gen += 1;
        entry.phase = PeerPhase::Connected {
            since: now,
            channel: crate::transport::peer::ChannelHandle {
                id: entry.tx_gen,
                path: crate::transport::peer::ConnectPath::Gatt,
            },
            tx_gen: entry.tx_gen,
            upgrading: false,
        };
        actions.push(PeerAction::StartDataPipe {
            device_id: client_id,
            role: crate::transport::peer::ConnectRole::Peripheral,
            path: crate::transport::peer::ConnectPath::Gatt,
            l2cap_channel: None,
        });
    }

    fn handle_advertised(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        prefix: crate::transport::peer::KeyPrefix,
        device: blew::BleDevice,
        rssi: Option<i16>,
    ) {
        let _ = rssi;
        let device_id = device.id.clone();

        let has_verified_live = self.peers.values().any(|e| {
            e.verified_endpoint
                .map(|eid| crate::transport::routing::prefix_from_endpoint(&eid) == prefix)
                .unwrap_or(false)
                && matches!(e.phase, PeerPhase::Connected { .. })
        });
        let i_am_lower = self.my_prefix < prefix;
        let already_verified_peer = self.verified_prefixes.contains_key(&prefix);
        let should_defer = i_am_lower && !already_verified_peer;

        let entry = self
            .peers
            .entry(device_id.clone())
            .or_insert_with(|| PeerEntry::new(device_id.clone()));
        entry.last_adv = Some(now);
        entry.prefix = Some(prefix);

        if has_verified_live {
            tracing::debug!(
                device = %device_id,
                "suppressing dial: verified live peer already exists for this prefix",
            );
            return;
        }
        match &entry.phase {
            PeerPhase::Unknown => {
                if entry.pending_sends.is_empty() {
                    entry.phase = PeerPhase::Discovered { since: now };
                } else if should_defer {
                    entry.phase = PeerPhase::PendingDial {
                        since: now,
                        deadline: pending_dial_deadline(now, prefix),
                        prefix,
                    };
                } else {
                    entry.phase = PeerPhase::Connecting {
                        attempt: 0,
                        started: now,
                        path: crate::transport::peer::ConnectPath::Gatt,
                    };
                    actions.push(PeerAction::StartConnect {
                        device_id: device_id.clone(),
                        attempt: 0,
                    });
                }
            }
            PeerPhase::Discovered { .. } if !entry.pending_sends.is_empty() => {
                if should_defer {
                    entry.phase = PeerPhase::PendingDial {
                        since: now,
                        deadline: pending_dial_deadline(now, prefix),
                        prefix,
                    };
                } else {
                    entry.phase = PeerPhase::Connecting {
                        attempt: 0,
                        started: now,
                        path: crate::transport::peer::ConnectPath::Gatt,
                    };
                    actions.push(PeerAction::StartConnect {
                        device_id: device_id.clone(),
                        attempt: 0,
                    });
                }
            }
            PeerPhase::Reconnecting {
                attempt, next_at, ..
            } if *next_at <= now => {
                let attempt = *attempt;
                entry.phase = PeerPhase::Connecting {
                    attempt,
                    started: now,
                    path: crate::transport::peer::ConnectPath::Gatt,
                };
                actions.push(PeerAction::StartConnect {
                    device_id: device_id.clone(),
                    attempt,
                });
            }
            _ => {}
        }
    }

    fn handle_send_datagram(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        tx_gen: u64,
        datagram: bytes::Bytes,
        waker: std::task::Waker,
    ) {
        let datagram_len = datagram.len();
        let entry = self
            .peers
            .entry(device_id.clone())
            .or_insert_with(|| PeerEntry::new(device_id.clone()));
        enum SendDecision {
            Enqueue,
            Reject,
            Buffer,
            StartAndEnqueue,
        }
        let entry_phase_kind = PhaseKind::from(&entry.phase);
        let entry_tx_gen = entry.tx_gen;
        let decision = match &entry.phase {
            PeerPhase::Connected {
                tx_gen: live_gen, ..
            } => {
                if tx_gen == *live_gen {
                    SendDecision::Enqueue
                } else {
                    SendDecision::Reject
                }
            }
            PeerPhase::Discovered { .. } => SendDecision::StartAndEnqueue,
            PeerPhase::Unknown
            | PeerPhase::PendingDial { .. }
            | PeerPhase::Connecting { .. }
            | PeerPhase::Handshaking { .. }
            | PeerPhase::Reconnecting { .. }
            | PeerPhase::Restoring { .. } => SendDecision::Buffer,
            PeerPhase::Draining { .. } | PeerPhase::Dead { .. } => SendDecision::Reject,
        };
        let decision_tag = match &decision {
            SendDecision::Enqueue => "enqueue",
            SendDecision::Reject => "reject",
            SendDecision::Buffer => "buffer",
            SendDecision::StartAndEnqueue => "start_and_enqueue",
        };
        tracing::trace!(
            device = %device_id,
            incoming_tx_gen = tx_gen,
            entry_tx_gen,
            ?entry_phase_kind,
            len = datagram_len,
            decision = decision_tag,
            "registry SendDatagram"
        );
        match decision {
            SendDecision::Enqueue => {
                if entry.pipe.is_some() && !entry.pending_sends.is_empty() {
                    // Drain pre-buffered sends first to preserve FIFO ordering.
                    while let Some(old) = entry.pending_sends.pop_front() {
                        let pipe = match entry.pipe.as_ref() {
                            Some(p) => p,
                            None => {
                                entry.pending_sends.push_front(old);
                                break;
                            }
                        };
                        match pipe.outbound_tx.try_send(old) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(old)) => {
                                entry.pending_sends.push_front(old);
                                break;
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(old)) => {
                                entry.pipe = None;
                                entry.pending_sends.push_front(old);
                                break;
                            }
                        }
                    }
                }

                if let Some(pipe) = entry.pipe.as_ref() {
                    let send = crate::transport::peer::PendingSend {
                        tx_gen,
                        datagram,
                        waker: waker.clone(),
                    };
                    match pipe.outbound_tx.try_send(send) {
                        Ok(()) => {
                            actions.push(PeerAction::AckSend {
                                tx_gen,
                                waker,
                                result: Ok(()),
                            });
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            actions.push(PeerAction::AckSend {
                                tx_gen,
                                waker,
                                result: Err(std::io::ErrorKind::WouldBlock),
                            });
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(send)) => {
                            entry.pipe = None;
                            let waker = send.waker.clone();
                            entry.pending_sends.push_back(send);
                            actions.push(PeerAction::AckSend {
                                tx_gen,
                                waker,
                                result: Ok(()),
                            });
                        }
                    }
                } else {
                    entry
                        .pending_sends
                        .push_back(crate::transport::peer::PendingSend {
                            tx_gen,
                            datagram,
                            waker,
                        });
                }
            }
            SendDecision::Reject => {
                actions.push(PeerAction::AckSend {
                    tx_gen,
                    waker,
                    result: Err(std::io::ErrorKind::WouldBlock),
                });
            }
            SendDecision::Buffer => {
                entry
                    .pending_sends
                    .push_back(crate::transport::peer::PendingSend {
                        tx_gen,
                        datagram,
                        waker: waker.clone(),
                    });
                actions.push(PeerAction::AckSend {
                    tx_gen,
                    waker,
                    result: Ok(()),
                });
            }
            SendDecision::StartAndEnqueue => {
                entry
                    .pending_sends
                    .push_back(crate::transport::peer::PendingSend {
                        tx_gen,
                        datagram,
                        waker,
                    });
                entry.phase = PeerPhase::Connecting {
                    attempt: 0,
                    started: now,
                    path: crate::transport::peer::ConnectPath::Gatt,
                };
                actions.push(PeerAction::StartConnect {
                    device_id: device_id.clone(),
                    attempt: 0,
                });
            }
        }
    }

    fn handle_connect_succeeded(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        channel: crate::transport::peer::ChannelHandle,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        if !matches!(entry.phase, PeerPhase::Connecting { .. }) {
            return;
        }
        entry.consecutive_failures = 0;
        // Kick off the VERSION probe in parallel with whatever
        // path the handshake takes below. A mismatch arriving
        // after StartDataPipe still Dead-s the peer and closes
        // the channel; QUIC has not yet exchanged any datagrams
        // so the momentary pipe is harmless.
        actions.push(PeerAction::ReadVersion {
            device_id: device_id.clone(),
        });
        entry.tx_gen += 1;
        let tx_gen = entry.tx_gen;
        entry.phase = PeerPhase::Connected {
            since: now,
            channel,
            tx_gen,
            upgrading: false,
        };
        let role = entry.role;
        actions.push(PeerAction::StartDataPipe {
            device_id: device_id.clone(),
            role,
            path: crate::transport::peer::ConnectPath::Gatt,
            l2cap_channel: None,
        });
    }

    fn handle_connect_failed(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        error: &str,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        let Some(attempt) = (if let PeerPhase::Connecting { attempt, .. } = &entry.phase {
            Some(*attempt)
        } else {
            None
        }) else {
            return;
        };
        let next_attempt = attempt + 1;
        entry.consecutive_failures += 1;
        actions.push(PeerAction::EmitMetric(format!("connect_failed:{error}")));
        if next_attempt >= MAX_CONNECT_ATTEMPTS {
            entry.phase = PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::MaxRetries,
                at: now,
            };
            if let Some(prefix) = entry.prefix {
                actions.push(PeerAction::ForgetPeerStore { prefix });
            }
        } else {
            entry.phase = PeerPhase::Reconnecting {
                attempt: next_attempt,
                next_at: now + reconnect_backoff(next_attempt),
                reason: crate::transport::peer::DisconnectReason::Timeout,
            };
        }
    }

    fn handle_inbound_gatt_fragment(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        bytes: bytes::Bytes,
    ) {
        use std::collections::hash_map::Entry;
        let (entry, freshly_inserted) = match self.peers.entry(device_id.clone()) {
            Entry::Vacant(v) => {
                let mut e = PeerEntry::new(device_id.clone());
                e.role = crate::transport::peer::ConnectRole::Peripheral;
                e.phase = PeerPhase::Connected {
                    since: now,
                    channel: crate::transport::peer::ChannelHandle {
                        id: 0,
                        path: crate::transport::peer::ConnectPath::Gatt,
                    },
                    tx_gen: 1,
                    upgrading: false,
                };
                e.tx_gen = 1;
                (v.insert(e), true)
            }
            Entry::Occupied(o) => (o.into_mut(), false),
        };
        entry.last_rx = Some(now);
        if freshly_inserted {
            entry.rx_backlog.push_back(bytes);
            actions.push(PeerAction::StartDataPipe {
                device_id: device_id.clone(),
                role: crate::transport::peer::ConnectRole::Peripheral,
                path: crate::transport::peer::ConnectPath::Gatt,
                l2cap_channel: None,
            });
            return;
        }

        let promoting = matches!(entry.phase, PeerPhase::Handshaking { .. });
        if promoting {
            let channel = match &entry.phase {
                PeerPhase::Handshaking { channel, .. } => channel.clone(),
                _ => {
                    tracing::warn!(device = %device_id, "phase changed unexpectedly during fragment promotion");
                    return;
                }
            };
            entry.tx_gen += 1;
            let tx_gen = entry.tx_gen;
            entry.phase = PeerPhase::Connected {
                since: now,
                channel,
                tx_gen,
                upgrading: false,
            };
            entry.rx_backlog.push_back(bytes);
            let role = entry.role;
            actions.push(PeerAction::StartDataPipe {
                device_id: device_id.clone(),
                role,
                path: crate::transport::peer::ConnectPath::Gatt,
                l2cap_channel: None,
            });
            return;
        }

        if matches!(entry.phase, PeerPhase::Connected { .. }) {
            if let Some(pipe) = entry.pipe.as_ref() {
                if pipe.inbound_tx.try_send(bytes).is_err() {
                    // Pipe is gone or full; drop. Reliable protocol will retransmit.
                }
            } else {
                const RX_BACKLOG_CAP: usize = 16;
                if entry.rx_backlog.len() >= RX_BACKLOG_CAP {
                    entry.rx_backlog.pop_front();
                }
                entry.rx_backlog.push_back(bytes);
            }
            return;
        }

        // Adapter is off on our side; peers whose pipes are still draining
        // will keep injecting ACKs. Drop them instead of re-promoting the
        // entry — `AdapterStateChanged { powered: true }` is the only legal
        // path out of `Restoring`.
        if matches!(entry.phase, PeerPhase::Restoring { .. }) {
            return;
        }

        // Any other phase (Discovered / Connecting / Reconnecting /
        // Draining / Dead / Unknown): the peer is actively writing to us,
        // which is the strongest liveness signal we have. Override the
        // stale phase and rebuild as a fresh peripheral Connected so the
        // new pipe can be spun up.
        entry.pipe = None;
        entry.role = crate::transport::peer::ConnectRole::Peripheral;
        entry.tx_gen += 1;
        entry.phase = PeerPhase::Connected {
            since: now,
            channel: crate::transport::peer::ChannelHandle {
                id: 0,
                path: crate::transport::peer::ConnectPath::Gatt,
            },
            tx_gen: entry.tx_gen,
            upgrading: false,
        };
        entry.rx_backlog.push_back(bytes);
        actions.push(PeerAction::StartDataPipe {
            device_id: device_id.clone(),
            role: crate::transport::peer::ConnectRole::Peripheral,
            path: crate::transport::peer::ConnectPath::Gatt,
            l2cap_channel: None,
        });
    }

    fn handle_central_disconnected(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        cause: blew::DisconnectCause,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        let reason = crate::transport::peer::DisconnectReason::from(cause);
        let channel = match &entry.phase {
            PeerPhase::Connected { channel, .. } | PeerPhase::Handshaking { channel, .. } => {
                Some(channel.clone())
            }
            _ => None,
        };
        let broken_pipe_acks = Self::drain_to_draining(entry, now, reason.clone());
        actions.extend(broken_pipe_acks);
        if let Some(ch) = channel {
            actions.push(PeerAction::CloseChannel {
                device_id: device_id.clone(),
                channel: ch,
                reason: reason.clone(),
            });
        }
        if matches!(reason, crate::transport::peer::DisconnectReason::Gatt133) {
            actions.push(PeerAction::Refresh {
                device_id: device_id.clone(),
            });
        }
    }

    fn handle_adapter_state_changed(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        powered: bool,
    ) {
        if !powered {
            for entry in self.peers.values_mut() {
                entry.phase = PeerPhase::Restoring { since: now };
                entry.pending_sends.clear();
            }
        } else {
            for entry in self.peers.values_mut() {
                if matches!(entry.phase, PeerPhase::Restoring { .. }) {
                    entry.phase = PeerPhase::Reconnecting {
                        attempt: 0,
                        next_at: now,
                        reason: crate::transport::peer::DisconnectReason::AdapterOff,
                    };
                }
            }
            // Platform adapter-cycle wipes the peripheral's GATT table
            // and advertising state on Android (and sometimes macOS); the
            // driver re-registers services, restarts the advertiser, and
            // (if L2CAP is enabled) re-opens the listener so inbound
            // peers can find us again.
            actions.push(PeerAction::RebuildGattServer);
            actions.push(PeerAction::RestartAdvertising);
            if matches!(self.l2cap_policy, L2capPolicy::PreferL2cap) {
                actions.push(PeerAction::RestartL2capListener);
            }
        }
    }

    fn handle_restore_from_adapter(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        devices: Vec<blew::BleDevice>,
    ) {
        for device in devices {
            if let Some(entry) = self.peers.get_mut(&device.id) {
                if matches!(
                    entry.phase,
                    PeerPhase::Connected { .. } | PeerPhase::Handshaking { .. }
                ) {
                    continue;
                }
                entry.phase = PeerPhase::Restoring { since: now };
            } else {
                actions.push(PeerAction::EmitMetric(format!(
                    "restore_unknown_device={}",
                    device.id.as_str()
                )));
            }
        }
    }

    fn handle_tick(&mut self, actions: &mut Vec<PeerAction>, tick_now: std::time::Instant) {
        enum TickAction {
            StartConnect { attempt: u32 },
            PendingDialExpired,
            DrainingToDead,
            RestoringToDead,
        }

        let mut decisions: Vec<(DeviceId, TickAction)> = Vec::new();
        for (device_id, entry) in &self.peers {
            let decision = match &entry.phase {
                PeerPhase::Reconnecting {
                    attempt, next_at, ..
                } if *next_at <= tick_now => Some(TickAction::StartConnect { attempt: *attempt }),
                PeerPhase::PendingDial { deadline, .. } if tick_now >= *deadline => {
                    Some(TickAction::PendingDialExpired)
                }
                PeerPhase::Draining { since, .. }
                    if tick_now.saturating_duration_since(*since) > DRAINING_TIMEOUT =>
                {
                    Some(TickAction::DrainingToDead)
                }
                PeerPhase::Restoring { since }
                    if tick_now.saturating_duration_since(*since) > RESTORING_TIMEOUT =>
                {
                    Some(TickAction::RestoringToDead)
                }
                _ => None,
            };
            if let Some(a) = decision {
                decisions.push((device_id.clone(), a));
            }
        }

        for (device_id, action) in decisions {
            let entry = self
                .peers
                .get_mut(&device_id)
                .expect("device_id was just read from self.peers");
            match action {
                TickAction::StartConnect { attempt } => {
                    entry.phase = PeerPhase::Connecting {
                        attempt,
                        started: tick_now,
                        path: crate::transport::peer::ConnectPath::Gatt,
                    };
                    actions.push(PeerAction::StartConnect {
                        device_id: device_id.clone(),
                        attempt,
                    });
                }
                TickAction::PendingDialExpired => {
                    entry.phase = PeerPhase::Connecting {
                        attempt: 0,
                        started: tick_now,
                        path: crate::transport::peer::ConnectPath::Gatt,
                    };
                    actions.push(PeerAction::StartConnect {
                        device_id: device_id.clone(),
                        attempt: 0,
                    });
                }
                TickAction::DrainingToDead => {
                    // After the drain window, stop trying to rescue
                    // this DeviceId. Reconnection is driven by fresh
                    // advertising creating a new entry (common case:
                    // Android MAC randomization on peer restart) or by
                    // iroh issuing a new SendDatagram, not by the
                    // registry's own retry loop.
                    entry.phase = PeerPhase::Dead {
                        reason: crate::transport::peer::DeadReason::Forgotten,
                        at: tick_now,
                    };
                }
                TickAction::RestoringToDead => {
                    entry.phase = PeerPhase::Dead {
                        reason: crate::transport::peer::DeadReason::Forgotten,
                        at: tick_now,
                    };
                }
            }
        }

        // GC dead peers older than DEAD_GC_TTL
        self.peers.retain(|_, entry| {
            !matches!(
                &entry.phase,
                PeerPhase::Dead { at, .. } if tick_now.saturating_duration_since(*at) > DEAD_GC_TTL
            )
        });
    }

    fn handle_forget(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        let channel = match &entry.phase {
            PeerPhase::Connected { channel, .. } | PeerPhase::Handshaking { channel, .. } => {
                Some(channel.clone())
            }
            _ => None,
        };
        let reason = crate::transport::peer::DisconnectReason::LocalClose;
        entry.pipe = None;
        for send in entry.pending_sends.drain(..) {
            actions.push(PeerAction::AckSend {
                tx_gen: send.tx_gen,
                waker: send.waker,
                result: Err(std::io::ErrorKind::ConnectionAborted),
            });
        }
        entry.rx_backlog.clear();
        entry.phase = PeerPhase::Dead {
            reason: crate::transport::peer::DeadReason::Forgotten,
            at: now,
        };
        if let Some(ch) = channel {
            actions.push(PeerAction::CloseChannel {
                device_id: device_id.clone(),
                channel: ch,
                reason,
            });
        }
    }

    fn handle_stalled(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        let Some(channel) = (if let PeerPhase::Connected { channel, .. } = &entry.phase {
            Some(channel.clone())
        } else {
            None
        }) else {
            return;
        };
        let reason = crate::transport::peer::DisconnectReason::LinkDead;
        let broken_pipe_acks = Self::drain_to_draining(entry, now, reason.clone());
        actions.extend(broken_pipe_acks);
        actions.push(PeerAction::CloseChannel {
            device_id: device_id.clone(),
            channel,
            reason,
        });
    }

    fn handle_shutdown(&mut self, actions: &mut Vec<PeerAction>) {
        for entry in self.peers.values_mut() {
            for send in entry.pending_sends.drain(..) {
                actions.push(PeerAction::AckSend {
                    tx_gen: send.tx_gen,
                    waker: send.waker,
                    result: Err(std::io::ErrorKind::ConnectionAborted),
                });
            }
        }
    }

    fn handle_data_pipe_ready(
        &mut self,
        device_id: DeviceId,
        outbound_tx: tokio::sync::mpsc::Sender<crate::transport::peer::PendingSend>,
        inbound_tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
        swap_tx: tokio::sync::mpsc::Sender<blew::L2capChannel>,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        // Drain rx_backlog first (preserves arrival order).
        while let Some(bytes) = entry.rx_backlog.pop_front() {
            if inbound_tx.try_send(bytes).is_err() {
                break;
            }
        }
        // Then drain pending_sends in FIFO order.
        while let Some(send) = entry.pending_sends.pop_front() {
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(send))
            | Err(tokio::sync::mpsc::error::TrySendError::Closed(send)) =
                outbound_tx.try_send(send)
            {
                entry.pending_sends.push_front(send);
                break;
            }
        }
        entry.pipe = Some(crate::transport::peer::PipeHandles {
            outbound_tx,
            inbound_tx,
            swap_tx,
        });
    }

    fn handle_open_l2cap_succeeded(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        l2cap_chan: blew::L2capChannel,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        match &entry.phase {
            PeerPhase::Handshaking { .. } => {
                let gatt_channel = match &entry.phase {
                    PeerPhase::Handshaking { channel, .. } => channel.clone(),
                    _ => unreachable!(),
                };
                let l2cap_handle = crate::transport::peer::ChannelHandle {
                    id: gatt_channel.id,
                    path: crate::transport::peer::ConnectPath::L2cap,
                };
                entry.l2cap_channel = Some(l2cap_chan);
                entry.tx_gen += 1;
                let tx_gen = entry.tx_gen;
                entry.phase = PeerPhase::Connected {
                    since: now,
                    channel: l2cap_handle,
                    tx_gen,
                    upgrading: false,
                };
                let role = entry.role;
                actions.push(PeerAction::StartDataPipe {
                    device_id: device_id.clone(),
                    role,
                    path: crate::transport::peer::ConnectPath::L2cap,
                    l2cap_channel: entry.l2cap_channel.take(),
                });
            }
            PeerPhase::Connected {
                upgrading: true, ..
            } => {
                if let Some(handles) = &entry.pipe {
                    let swap_tx = handles.swap_tx.clone();
                    if let PeerPhase::Connected { upgrading, .. } = &mut entry.phase {
                        *upgrading = false;
                    }
                    actions.push(PeerAction::SwapPipeToL2cap {
                        device_id,
                        channel: l2cap_chan,
                        swap_tx,
                    });
                } else {
                    // Pipe handles went away (e.g. DataPipeDown raced with the
                    // upgrade). Drop the channel, clear upgrading, and mark the
                    // upgrade failed so the stuck-in-upgrading trap is avoided.
                    tracing::warn!(
                        device = %device_id,
                        "L2CAP upgrade succeeded but pipe handles absent; marking upgrade failed"
                    );
                    entry.l2cap_upgrade_failed = true;
                    if let PeerPhase::Connected { upgrading, .. } = &mut entry.phase {
                        *upgrading = false;
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_open_l2cap_failed(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        error: &str,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        match &entry.phase {
            PeerPhase::Handshaking { .. } => {
                let channel = match &entry.phase {
                    PeerPhase::Handshaking { channel, .. } => channel.clone(),
                    _ => unreachable!(),
                };
                entry.tx_gen += 1;
                let tx_gen = entry.tx_gen;
                entry.phase = PeerPhase::Connected {
                    since: now,
                    channel,
                    tx_gen,
                    upgrading: false,
                };
                let role = entry.role;
                actions.push(PeerAction::StartDataPipe {
                    device_id: device_id.clone(),
                    role,
                    path: crate::transport::peer::ConnectPath::Gatt,
                    l2cap_channel: None,
                });
                actions.push(PeerAction::EmitMetric(format!(
                    "l2cap_fallback_to_gatt:{error}"
                )));
            }
            PeerPhase::Connected {
                upgrading: true, ..
            } => {
                tracing::warn!(device = %device_id, %error, "L2CAP upgrade failed; staying GATT");
                entry.l2cap_upgrade_failed = true;
                if let PeerPhase::Connected { upgrading, .. } = &mut entry.phase {
                    *upgrading = false;
                }
            }
            _ => {}
        }
    }

    fn handle_l2cap_handover_timeout(
        &mut self,
        actions: &mut Vec<PeerAction>,
        device_id: DeviceId,
    ) {
        if let Some(entry) = self.peers.get_mut(&device_id) {
            entry.l2cap_upgrade_failed = true;
            if let PeerPhase::Connected { upgrading, .. } = &mut entry.phase {
                *upgrading = false;
            }
        }
        actions.push(PeerAction::RevertToGattPipe { device_id });
    }

    fn handle_inbound_l2cap_channel(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        channel: blew::L2capChannel,
    ) {
        use std::collections::hash_map::Entry;
        match self.peers.entry(device_id.clone()) {
            Entry::Vacant(v) => {
                let mut e = PeerEntry::new(device_id.clone());
                e.role = crate::transport::peer::ConnectRole::Peripheral;
                e.tx_gen = 1;
                e.l2cap_channel = Some(channel);
                e.phase = PeerPhase::Connected {
                    since: now,
                    channel: crate::transport::peer::ChannelHandle {
                        id: 0,
                        path: crate::transport::peer::ConnectPath::L2cap,
                    },
                    tx_gen: 1,
                    upgrading: false,
                };
                let inserted = v.insert(e);
                actions.push(PeerAction::StartDataPipe {
                    device_id: device_id.clone(),
                    role: crate::transport::peer::ConnectRole::Peripheral,
                    path: crate::transport::peer::ConnectPath::L2cap,
                    l2cap_channel: inserted.l2cap_channel.take(),
                });
            }
            Entry::Occupied(o) => {
                let entry = o.into_mut();
                if entry.pipe.is_some() {
                    let existing_path = match &entry.phase {
                        PeerPhase::Connected { channel, .. } => Some(channel.path),
                        _ => None,
                    };
                    match existing_path {
                        Some(crate::transport::peer::ConnectPath::L2cap) => {
                            actions.push(PeerAction::EmitMetric("l2cap_duplicate_accept".into()));
                        }
                        _ => {
                            actions.push(PeerAction::EmitMetric(
                                "l2cap_late_accept_after_gatt".into(),
                            ));
                        }
                    }
                } else {
                    entry.l2cap_channel = Some(channel);
                    entry.tx_gen += 1;
                    let tx_gen = entry.tx_gen;
                    entry.phase = PeerPhase::Connected {
                        since: now,
                        channel: crate::transport::peer::ChannelHandle {
                            id: 0,
                            path: crate::transport::peer::ConnectPath::L2cap,
                        },
                        tx_gen,
                        upgrading: false,
                    };
                    let role = entry.role;
                    actions.push(PeerAction::StartDataPipe {
                        device_id: device_id.clone(),
                        role,
                        path: crate::transport::peer::ConnectPath::L2cap,
                        l2cap_channel: entry.l2cap_channel.take(),
                    });
                }
            }
        }
    }

    fn handle_protocol_version_mismatch(
        &mut self,
        actions: &mut Vec<PeerAction>,
        now: std::time::Instant,
        device_id: DeviceId,
        got: u8,
        want: u8,
    ) {
        let Some(entry) = self.peers.get_mut(&device_id) else {
            return;
        };
        let channel = match &entry.phase {
            PeerPhase::Handshaking { channel, .. } | PeerPhase::Connected { channel, .. } => {
                Some(channel.clone())
            }
            _ => None,
        };
        let broken_pipe_acks = Self::drain_to_draining(
            entry,
            now,
            crate::transport::peer::DisconnectReason::ProtocolMismatch,
        );
        actions.extend(broken_pipe_acks);
        entry.phase = PeerPhase::Dead {
            reason: crate::transport::peer::DeadReason::ProtocolMismatch { got, want },
            at: now,
        };
        if let Some(ch) = channel {
            actions.push(PeerAction::CloseChannel {
                device_id: device_id.clone(),
                channel: ch,
                reason: crate::transport::peer::DisconnectReason::ProtocolMismatch,
            });
        }
    }

    #[cfg(test)]
    pub fn peer_iter_for_test(&self) -> impl Iterator<Item = (&DeviceId, &PeerEntry)> {
        self.peers.iter()
    }

    fn drain_to_draining(
        entry: &mut PeerEntry,
        now: std::time::Instant,
        reason: crate::transport::peer::DisconnectReason,
    ) -> Vec<PeerAction> {
        let mut out = Vec::new();
        let was_connected = matches!(entry.phase, PeerPhase::Connected { .. });
        entry.pipe = None;
        for send in entry.pending_sends.drain(..) {
            out.push(PeerAction::AckSend {
                tx_gen: send.tx_gen,
                waker: send.waker,
                result: Err(std::io::ErrorKind::BrokenPipe),
            });
        }
        if was_connected && let Some(prefix) = entry.prefix {
            out.push(PeerAction::PutPeerStore {
                prefix,
                snapshot: crate::transport::store::PeerSnapshot {
                    last_device_id: entry.device_id.as_str().to_string(),
                    last_seen: std::time::SystemTime::now(),
                },
            });
        }
        entry.phase = PeerPhase::Draining { since: now, reason };
        out
    }

    pub fn peer(&self, device_id: &DeviceId) -> Option<&PeerEntry> {
        self.peers.get(device_id)
    }

    pub(crate) fn publish_snapshot(&self, target: &ArcSwap<SnapshotMaps>) {
        let mut maps = SnapshotMaps::default();
        for (device_id, entry) in &self.peers {
            let connect_path = match &entry.phase {
                PeerPhase::Connected { channel, .. } => Some(channel.path),
                _ => None,
            };
            maps.peer_states.insert(
                device_id.clone(),
                PeerStateSummary {
                    phase_kind: PhaseKind::from(&entry.phase),
                    tx_gen: entry.tx_gen,
                    consecutive_failures: entry.consecutive_failures,
                    connect_path,
                    role: entry.role,
                    l2cap_upgrade_failed: entry.l2cap_upgrade_failed,
                    verified_endpoint: entry.verified_endpoint,
                },
            );
        }
        target.store(Arc::new(maps));
    }

    pub async fn run<I: BleInterface>(
        mut self,
        mut rx: mpsc::Receiver<PeerCommand>,
        driver: Driver<I>,
        snapshots: Arc<ArcSwap<SnapshotMaps>>,
        inbox_capacity_waker: Arc<AtomicWaker>,
    ) {
        while let Some(cmd) = rx.recv().await {
            // Wake BleSender::poll_send callers waiting on backpressure — we
            // just freed a slot in the inbox.
            inbox_capacity_waker.wake();
            let shutdown = matches!(cmd, PeerCommand::Shutdown);
            let actions = self.handle(cmd);
            for action in actions {
                driver.execute(action).await;
            }
            self.publish_snapshot(&snapshots);
            if shutdown {
                break;
            }
        }
    }
}

/// Backoff computation used by the Reconnecting path. Exposed for tests.
pub(crate) fn reconnect_backoff(attempt: u32) -> Duration {
    let base_ms = 500u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(base_ms.min(30_000))
}

// Deterministic jitter keyed by peer prefix avoids adding a PRNG dep and
// makes tests reproducible while still spreading dials across the window.
fn pending_dial_deadline(
    now: std::time::Instant,
    prefix: crate::transport::peer::KeyPrefix,
) -> std::time::Instant {
    use crate::transport::dedup::{FAIRNESS_JITTER, FAIRNESS_WINDOW};

    let jitter_span_ns = FAIRNESS_JITTER
        .saturating_mul(2)
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX);
    let seed: u64 = prefix
        .iter()
        .enumerate()
        .map(|(i, b)| u64::from(*b) << ((i & 7) * 8))
        .fold(0u64, u64::wrapping_add);
    let jitter = std::time::Duration::from_nanos(seed % jitter_span_ns);
    now + FAIRNESS_WINDOW.saturating_sub(FAIRNESS_JITTER) + jitter
}

#[derive(Debug, Clone, Default)]
pub struct SnapshotMaps {
    pub peer_states: HashMap<DeviceId, PeerStateSummary>,
}

#[derive(Debug, Clone)]
pub struct PeerStateSummary {
    pub phase_kind: PhaseKind,
    pub tx_gen: u64,
    pub consecutive_failures: u32,
    pub connect_path: Option<crate::transport::peer::ConnectPath>,
    pub role: crate::transport::peer::ConnectRole,
    pub l2cap_upgrade_failed: bool,
    pub verified_endpoint: Option<iroh_base::EndpointId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Unknown,
    Discovered,
    PendingDial,
    Connecting,
    Handshaking,
    Connected,
    Draining,
    Reconnecting,
    Dead,
    Restoring,
}

impl From<&PeerPhase> for PhaseKind {
    fn from(p: &PeerPhase) -> Self {
        match p {
            PeerPhase::Unknown => Self::Unknown,
            PeerPhase::Discovered { .. } => Self::Discovered,
            PeerPhase::PendingDial { .. } => Self::PendingDial,
            PeerPhase::Connecting { .. } => Self::Connecting,
            PeerPhase::Handshaking { .. } => Self::Handshaking,
            PeerPhase::Connected { .. } => Self::Connected,
            PeerPhase::Draining { .. } => Self::Draining,
            PeerPhase::Reconnecting { .. } => Self::Reconnecting,
            PeerPhase::Dead { .. } => Self::Dead,
            PeerPhase::Restoring { .. } => Self::Restoring,
        }
    }
}

pub struct RegistryHandle {
    pub inbox: mpsc::Sender<PeerCommand>,
    pub snapshots: Arc<ArcSwap<SnapshotMaps>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_waker() -> std::task::Waker {
        use std::task::{RawWaker, RawWakerVTable, Waker};
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn registry_starts_empty() {
        let reg = Registry::new_for_test();
        assert!(reg.peer(&DeviceId::from("nobody")).is_none());
    }

    #[test]
    fn backoff_caps_at_30s() {
        assert!(reconnect_backoff(20) <= Duration::from_secs(30));
    }

    #[test]
    fn advertised_new_peer_creates_discovered_entry() {
        let mut reg = Registry::new_for_test();
        let device = blew::BleDevice {
            id: blew::DeviceId::from("dev-1"),
            name: None,
            rssi: Some(-60),
            services: vec![],
        };
        let actions = reg.handle(PeerCommand::Advertised {
            prefix: [1u8; 12],
            device: device.clone(),
            rssi: Some(-60),
        });
        assert!(actions.is_empty(), "no actions for first advertisement");
        let entry = reg.peer(&device.id).unwrap();
        assert!(matches!(entry.phase, PeerPhase::Discovered { .. }));
        assert_eq!(entry.device_id, device.id);
    }

    #[test]
    fn advertised_duplicate_in_connecting_is_noop() {
        let mut reg = Registry::new_for_test();
        let device = blew::BleDevice {
            id: blew::DeviceId::from("dev-2"),
            name: None,
            rssi: None,
            services: vec![],
        };
        reg.handle(PeerCommand::Advertised {
            prefix: [2u8; 12],
            device: device.clone(),
            rssi: None,
        });
        if let Some(entry) = reg.peers.get_mut(&device.id) {
            entry.phase = PeerPhase::Connecting {
                attempt: 0,
                started: std::time::Instant::now(),
                path: crate::transport::peer::ConnectPath::Gatt,
            };
        }
        let actions = reg.handle(PeerCommand::Advertised {
            prefix: [2u8; 12],
            device: device.clone(),
            rssi: None,
        });
        assert!(actions.is_empty());
        assert!(matches!(
            reg.peer(&device.id).unwrap().phase,
            PeerPhase::Connecting { .. }
        ));
    }

    #[test]
    fn connect_succeeded_moves_to_connected_and_emits_start_data_pipe() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-4");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 0;
            e.phase = PeerPhase::Connecting {
                attempt: 0,
                started: std::time::Instant::now(),
                path: ConnectPath::Gatt,
            };
            e
        });
        let ch = ChannelHandle {
            id: 42,
            path: ConnectPath::Gatt,
        };
        let actions = reg.handle(PeerCommand::ConnectSucceeded {
            device_id: device_id.clone(),
            channel: ch,
        });
        let has_start = actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe { device_id: d, role: ConnectRole::Central, .. } if *d == device_id
        ));
        assert!(has_start, "expected StartDataPipe; got {actions:?}");
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { tx_gen, .. } => assert_eq!(*tx_gen, 1),
            other => panic!("wrong phase: {other:?}"),
        }
        assert_eq!(reg.peer(&device_id).unwrap().tx_gen, 1);
        assert_eq!(reg.peer(&device_id).unwrap().consecutive_failures, 0);
    }

    #[test]
    fn connect_failed_moves_to_reconnecting_with_backoff() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-5");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connecting {
                attempt: 0,
                started: std::time::Instant::now(),
                path: crate::transport::peer::ConnectPath::Gatt,
            };
            e
        });
        let actions = reg.handle(PeerCommand::ConnectFailed {
            device_id: device_id.clone(),
            error: "boom".into(),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::EmitMetric(_)))
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Reconnecting { attempt, .. } => assert_eq!(*attempt, 1),
            other => panic!("wrong phase: {other:?}"),
        }
        assert_eq!(reg.peer(&device_id).unwrap().consecutive_failures, 1);
    }

    #[test]
    fn connect_failed_past_max_moves_to_dead() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-6");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.consecutive_failures = 14;
            e.phase = PeerPhase::Connecting {
                attempt: 14,
                started: std::time::Instant::now(),
                path: crate::transport::peer::ConnectPath::Gatt,
            };
            e
        });
        let _actions = reg.handle(PeerCommand::ConnectFailed {
            device_id: device_id.clone(),
            error: "final boom".into(),
        });
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::MaxRetries,
                ..
            }
        ));
    }

    #[test]
    fn inbound_fragment_promotes_handshaking_to_connected_and_bumps_tx_gen() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-7");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 0;
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
            };
            e
        });
        let actions = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: crate::transport::peer::FragmentSource::CentralReceivedP2c,
            bytes: bytes::Bytes::from_static(b"frag"),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartDataPipe { .. }))
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { tx_gen, .. } => assert_eq!(*tx_gen, 1),
            other => panic!("wrong phase: {other:?}"),
        }
        assert_eq!(reg.peer(&device_id).unwrap().tx_gen, 1);
    }

    #[test]
    fn send_datagram_matches_tx_gen_enqueues() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-8");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 3;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 3,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 3,
            datagram: bytes::Bytes::from_static(b"hi"),
            waker: noop_waker(),
        });
        assert!(actions.is_empty(), "enqueued, no action needed yet");
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 1);
    }

    #[test]
    fn send_datagram_stale_tx_gen_is_rejected() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-9");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 5;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 5,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 4,
            datagram: bytes::Bytes::from_static(b"stale"),
            waker: noop_waker(),
        });
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::AckSend {
                result: Err(std::io::ErrorKind::WouldBlock),
                ..
            }
        )));
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 0);
    }

    #[test]
    fn disconnected_from_connected_moves_to_draining() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-10");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 1;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id: device_id.clone(),
            cause: blew::DisconnectCause::LinkLoss,
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::CloseChannel { .. }))
        );
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Draining {
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
                ..
            }
        ));
    }

    #[test]
    fn gatt133_disconnect_emits_refresh_action() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-11");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id: device_id.clone(),
            cause: blew::DisconnectCause::Gatt133,
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::Refresh { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::CloseChannel { .. }))
        );
    }

    #[test]
    fn link_loss_does_not_emit_refresh() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-12");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id,
            cause: blew::DisconnectCause::LinkLoss,
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::Refresh { .. }))
        );
    }

    #[test]
    fn adapter_off_moves_all_peers_to_restoring() {
        let mut reg = Registry::new_for_test();
        let now = std::time::Instant::now();
        let mut ids = Vec::new();
        for i in 0..3u8 {
            let device_id = blew::DeviceId::from(format!("dev-off-{i}"));
            reg.peers.insert(device_id.clone(), {
                let mut e = PeerEntry::new(device_id.clone());
                e.phase = PeerPhase::Connected {
                    since: now,
                    channel: crate::transport::peer::ChannelHandle {
                        id: u64::from(i),
                        path: crate::transport::peer::ConnectPath::Gatt,
                    },
                    tx_gen: 1,
                    upgrading: false,
                };
                e
            });
            ids.push(device_id);
        }
        let actions = reg.handle(PeerCommand::AdapterStateChanged { powered: false });
        assert!(
            actions.is_empty(),
            "powered=false emits no driver actions; blew tears the stack down on its own, and rebuild/restart fire on powered=true"
        );
        for device_id in &ids {
            assert!(matches!(
                reg.peer(device_id).unwrap().phase,
                PeerPhase::Restoring { .. }
            ));
        }
    }

    #[test]
    fn send_datagram_from_discovered_starts_connect() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-3");
        reg.handle(PeerCommand::Advertised {
            prefix: [3u8; 12],
            device: blew::BleDevice {
                id: device_id.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });
        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 0,
            datagram: bytes::Bytes::from_static(b"hello"),
            waker: noop_waker(),
        });
        assert!(matches!(
            actions.as_slice(),
            [PeerAction::StartConnect { .. }]
        ));
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Connecting { attempt: 0, .. }
        ));
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 1);
    }

    #[test]
    fn adapter_on_from_restoring_moves_to_reconnecting() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-13");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Restoring {
                since: std::time::Instant::now(),
            };
            e
        });
        let actions = reg.handle(PeerCommand::AdapterStateChanged { powered: true });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::RebuildGattServer)),
            "expected RebuildGattServer on adapter-on"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::RestartAdvertising)),
            "expected RestartAdvertising on adapter-on"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::RestartL2capListener)),
            "L2capPolicy::Disabled should not request an L2CAP restart"
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Reconnecting { attempt: 0, .. } => {}
            other => panic!("wrong phase: {other:?}"),
        }
    }

    #[test]
    fn adapter_on_with_l2cap_policy_also_restarts_listener() {
        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let actions = reg.handle(PeerCommand::AdapterStateChanged { powered: true });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::RestartL2capListener)),
            "PreferL2cap should request an L2CAP listener restart on adapter-on"
        );
    }

    #[test]
    fn tick_past_next_at_moves_reconnecting_to_connecting() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-14");
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Reconnecting {
                attempt: 2,
                next_at: past,
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            };
            e
        });
        let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartConnect { attempt: 2, .. }))
        );
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Connecting { attempt: 2, .. }
        ));
    }

    #[test]
    fn tick_before_next_at_is_noop() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-15");
        let future = std::time::Instant::now() + std::time::Duration::from_secs(10);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Reconnecting {
                attempt: 1,
                next_at: future,
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            };
            e
        });
        let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));
        assert!(actions.is_empty());
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Reconnecting { .. }
        ));
    }

    #[test]
    fn tick_past_draining_timeout_moves_to_dead() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-16");
        let old = std::time::Instant::now() - std::time::Duration::from_secs(10);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Draining {
                since: old,
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            };
            e
        });
        let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));
        assert!(actions.is_empty(), "Draining->Dead does not emit actions");
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Dead { reason, .. } => {
                assert_eq!(*reason, crate::transport::peer::DeadReason::Forgotten);
            }
            other => panic!("wrong phase: {other:?}"),
        }
    }

    #[test]
    fn tick_past_restoring_timeout_moves_to_dead_and_forgets() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-17");
        let old = std::time::Instant::now() - std::time::Duration::from_secs(200);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Restoring { since: old };
            e
        });
        let _actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::Forgotten,
                ..
            }
        ));
    }

    #[test]
    fn tick_garbage_collects_old_dead_peers() {
        let mut reg = Registry::new_for_test();
        let fresh_id = blew::DeviceId::from("dev-fresh");
        let old_id = blew::DeviceId::from("dev-old");
        let now = std::time::Instant::now();
        reg.peers.insert(fresh_id.clone(), {
            let mut e = PeerEntry::new(fresh_id.clone());
            e.phase = PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::MaxRetries,
                at: now - std::time::Duration::from_secs(10),
            };
            e
        });
        reg.peers.insert(old_id.clone(), {
            let mut e = PeerEntry::new(old_id.clone());
            e.phase = PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::MaxRetries,
                at: now - std::time::Duration::from_secs(3600),
            };
            e
        });
        let _ = reg.handle(PeerCommand::Tick(now));
        assert!(reg.peer(&fresh_id).is_some(), "fresh Dead peer survives GC");
        assert!(reg.peer(&old_id).is_none(), "old Dead peer is GC'd");
    }

    #[test]
    fn restore_from_adapter_places_known_peers_in_restoring() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-16");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Unknown;
            e
        });
        let device = blew::BleDevice {
            id: device_id.clone(),
            name: None,
            rssi: None,
            services: vec![],
        };
        let actions = reg.handle(PeerCommand::RestoreFromAdapter {
            devices: vec![device],
        });
        assert!(actions.is_empty());
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Restoring { .. }
        ));
    }

    #[test]
    fn restore_from_adapter_unknown_device_emits_metric() {
        let mut reg = Registry::new_for_test();
        let device = blew::BleDevice {
            id: blew::DeviceId::from("dev-stranger"),
            name: None,
            rssi: None,
            services: vec![],
        };
        let actions = reg.handle(PeerCommand::RestoreFromAdapter {
            devices: vec![device],
        });
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::EmitMetric(s) if s.contains("restore_unknown_device")
        )));
    }

    #[test]
    fn restore_from_adapter_skips_connected_peers() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-20");
        let channel = crate::transport::peer::ChannelHandle {
            id: 42,
            path: crate::transport::peer::ConnectPath::Gatt,
        };
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: channel.clone(),
                tx_gen: 7,
                upgrading: false,
            };
            e
        });
        let device = blew::BleDevice {
            id: device_id.clone(),
            name: None,
            rssi: None,
            services: vec![],
        };
        let actions = reg.handle(PeerCommand::RestoreFromAdapter {
            devices: vec![device],
        });
        assert!(actions.is_empty(), "Connected peer stays put, no actions");
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { tx_gen: 7, .. } => {}
            other => panic!("expected Connected unchanged, got {other:?}"),
        }
    }

    #[test]
    fn restore_from_adapter_skips_handshaking_peers() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-21");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 99,
                    path: crate::transport::peer::ConnectPath::L2cap,
                },
            };
            e
        });
        let device = blew::BleDevice {
            id: device_id.clone(),
            name: None,
            rssi: None,
            services: vec![],
        };
        let actions = reg.handle(PeerCommand::RestoreFromAdapter {
            devices: vec![device],
        });
        assert!(actions.is_empty());
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Handshaking { .. }
        ));
    }

    #[test]
    fn restore_from_adapter_mixes_known_and_unknown() {
        let mut reg = Registry::new_for_test();
        let known_id = blew::DeviceId::from("dev-known");
        reg.peers.insert(known_id.clone(), {
            let mut e = PeerEntry::new(known_id.clone());
            e.phase = PeerPhase::Unknown;
            e
        });
        let actions = reg.handle(PeerCommand::RestoreFromAdapter {
            devices: vec![
                blew::BleDevice {
                    id: known_id.clone(),
                    name: None,
                    rssi: None,
                    services: vec![],
                },
                blew::BleDevice {
                    id: blew::DeviceId::from("dev-unknown"),
                    name: None,
                    rssi: None,
                    services: vec![],
                },
            ],
        });
        assert_eq!(
            actions.len(),
            1,
            "exactly one metric for the unknown device"
        );
        assert!(matches!(
            actions[0],
            PeerAction::EmitMetric(ref s) if s.contains("restore_unknown_device=") && s.contains("dev-unknown")
        ));
        assert!(matches!(
            reg.peer(&known_id).unwrap().phase,
            PeerPhase::Restoring { .. }
        ));
    }

    #[test]
    fn stalled_from_connected_moves_to_draining_link_dead() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-stalled");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::Stalled {
            device_id: device_id.clone(),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::CloseChannel { .. }))
        );
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Draining {
                reason: crate::transport::peer::DisconnectReason::LinkDead,
                ..
            }
        ));
    }

    #[test]
    fn stalled_on_non_connected_peer_is_noop() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-23");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Reconnecting {
                attempt: 3,
                next_at: std::time::Instant::now(),
                reason: crate::transport::peer::DisconnectReason::LinkLoss,
            };
            e
        });
        let actions = reg.handle(PeerCommand::Stalled {
            device_id: device_id.clone(),
        });
        assert!(actions.is_empty());
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Reconnecting { attempt: 3, .. }
        ));
    }

    #[test]
    fn shutdown_wakes_pending_sends_with_connection_aborted() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-18");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.pending_sends
                .push_back(crate::transport::peer::PendingSend {
                    tx_gen: 1,
                    datagram: bytes::Bytes::from_static(b"x"),
                    waker: noop_waker(),
                });
            e
        });
        let actions = reg.handle(PeerCommand::Shutdown);
        let aborted_acks = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    PeerAction::AckSend {
                        result: Err(std::io::ErrorKind::ConnectionAborted),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(aborted_acks, 1);
    }

    #[test]
    fn shutdown_drains_pending_sends_across_all_peers() {
        use std::collections::HashSet;
        let mut reg = Registry::new_for_test();
        for i in 0u8..3 {
            let device_id = blew::DeviceId::from(format!("dev-shut-{i}"));
            let mut entry = PeerEntry::new(device_id.clone());
            entry
                .pending_sends
                .push_back(crate::transport::peer::PendingSend {
                    tx_gen: u64::from(i),
                    datagram: bytes::Bytes::from_static(b"hello"),
                    waker: noop_waker(),
                });
            entry
                .pending_sends
                .push_back(crate::transport::peer::PendingSend {
                    tx_gen: u64::from(i) + 100,
                    datagram: bytes::Bytes::from_static(b"hello"),
                    waker: noop_waker(),
                });
            reg.peers.insert(device_id, entry);
        }
        let actions = reg.handle(PeerCommand::Shutdown);
        let drained_tx_gens: HashSet<u64> = actions
            .iter()
            .filter_map(|a| match a {
                PeerAction::AckSend {
                    tx_gen,
                    result: Err(std::io::ErrorKind::ConnectionAborted),
                    ..
                } => Some(*tx_gen),
                _ => None,
            })
            .collect();
        let expected: HashSet<u64> = [0u64, 1, 2, 100, 101, 102].into_iter().collect();
        assert_eq!(drained_tx_gens, expected);
        for entry in reg.peers.values() {
            assert!(entry.pending_sends.is_empty(), "pending sends drained");
        }
    }

    #[test]
    fn snapshot_reflects_current_phase() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-snap");
        let mut entry = PeerEntry::new(device_id.clone());
        entry.phase = PeerPhase::Connected {
            since: std::time::Instant::now(),
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
            tx_gen: 7,
            upgrading: false,
        };
        entry.tx_gen = 7;
        reg.peers.insert(device_id.clone(), entry);

        let snap = ArcSwap::from(Arc::new(SnapshotMaps::default()));
        reg.publish_snapshot(&snap);
        let loaded = snap.load();
        let summary = loaded
            .peer_states
            .get(&device_id)
            .expect("device_id present");
        assert_eq!(summary.phase_kind, PhaseKind::Connected);
        assert_eq!(summary.tx_gen, 7);
    }

    #[test]
    fn advertised_new_peer_has_central_role() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-30");
        reg.handle(PeerCommand::Advertised {
            prefix: [30u8; 12],
            device: blew::BleDevice {
                id: device_id.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });
        assert_eq!(
            reg.peer(&device_id).unwrap().role,
            crate::transport::peer::ConnectRole::Central
        );
    }

    #[test]
    fn handshaking_fragment_emits_start_data_pipe_and_buffers_bytes() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole, FragmentSource};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-31");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
            };
            e
        });

        let actions = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: FragmentSource::CentralReceivedP2c,
            bytes: bytes::Bytes::from_static(b"first-frag"),
        });

        let has_start = actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe { device_id: d, role: ConnectRole::Central, .. } if *d == device_id
        ));
        assert!(has_start, "expected StartDataPipe; got {actions:?}");

        let entry = reg.peer(&device_id).unwrap();
        assert!(matches!(entry.phase, PeerPhase::Connected { .. }));
        assert_eq!(entry.rx_backlog.len(), 1);
        assert_eq!(&entry.rx_backlog[0][..], b"first-frag");
    }

    #[tokio::test]
    async fn data_pipe_ready_drains_pending_sends_in_fifo() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-40");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 7,
                upgrading: false,
            };
            e.tx_gen = 7;
            for i in 0..3u64 {
                e.pending_sends.push_back(PendingSend {
                    tx_gen: 7,
                    datagram: bytes::Bytes::from(vec![i as u8]),
                    waker: noop_waker(),
                });
            }
            e
        });

        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(8);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);

        let actions = reg.handle(PeerCommand::DataPipeReady {
            device_id: device_id.clone(),
            outbound_tx,
            inbound_tx,
            swap_tx,
        });
        assert!(actions.is_empty());

        assert!(reg.peer(&device_id).unwrap().pipe.is_some());
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 0);

        for expected in 0..3u8 {
            let send = outbound_rx.recv().await.expect("drained");
            assert_eq!(&send.datagram[..], &[expected]);
        }
    }

    #[tokio::test]
    async fn data_pipe_ready_drains_rx_backlog_in_fifo() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-41");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e.rx_backlog.push_back(bytes::Bytes::from_static(b"A"));
            e.rx_backlog.push_back(bytes::Bytes::from_static(b"B"));
            e
        });

        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(8);
        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);

        let actions = reg.handle(PeerCommand::DataPipeReady {
            device_id: device_id.clone(),
            outbound_tx,
            inbound_tx,
            swap_tx,
        });
        assert!(actions.is_empty());
        assert_eq!(reg.peer(&device_id).unwrap().rx_backlog.len(), 0);

        assert_eq!(inbound_rx.recv().await.unwrap().as_ref(), b"A");
        assert_eq!(inbound_rx.recv().await.unwrap().as_ref(), b"B");
    }

    #[tokio::test]
    async fn send_datagram_fast_path_pushes_to_outbound_tx() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend, PipeHandles};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-50");
        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(4);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);

        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 9;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 9,
                upgrading: false,
            };
            let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e
        });

        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 9,
            datagram: bytes::Bytes::from_static(b"fast"),
            waker: noop_waker(),
        });

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::AckSend { result: Ok(()), .. }))
        );
        let got = outbound_rx.recv().await.unwrap();
        assert_eq!(&got.datagram[..], b"fast");
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 0);
    }

    #[tokio::test]
    async fn send_datagram_fast_path_closed_falls_back_to_pending_sends() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend, PipeHandles};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-51");
        {
            let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(4);
            let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
            reg.peers.insert(device_id.clone(), {
                let mut e = PeerEntry::new(device_id.clone());
                e.tx_gen = 8;
                e.phase = PeerPhase::Connected {
                    since: std::time::Instant::now(),
                    channel: ChannelHandle {
                        id: 2,
                        path: ConnectPath::Gatt,
                    },
                    tx_gen: 8,
                    upgrading: false,
                };
                let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
                e.pipe = Some(PipeHandles {
                    outbound_tx,
                    inbound_tx,
                    swap_tx,
                });
                e
            });
        }

        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 8,
            datagram: bytes::Bytes::from_static(b"fallback"),
            waker: noop_waker(),
        });

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::AckSend { result: Ok(()), .. }))
        );
        assert!(reg.peer(&device_id).unwrap().pipe.is_none());
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 1);
        assert_eq!(
            &reg.peer(&device_id).unwrap().pending_sends[0].datagram[..],
            b"fallback"
        );
    }

    #[tokio::test]
    async fn send_datagram_pipe_closed_acks_and_buffers() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend, PipeHandles};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-52");
        {
            let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(4);
            let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
            reg.peers.insert(device_id.clone(), {
                let mut e = PeerEntry::new(device_id.clone());
                e.tx_gen = 7;
                e.phase = PeerPhase::Connected {
                    since: std::time::Instant::now(),
                    channel: ChannelHandle {
                        id: 3,
                        path: ConnectPath::Gatt,
                    },
                    tx_gen: 7,
                    upgrading: false,
                };
                let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
                e.pipe = Some(PipeHandles {
                    outbound_tx,
                    inbound_tx,
                    swap_tx,
                });
                e
            });
        }

        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 7,
            datagram: bytes::Bytes::from_static(b"closed"),
            waker: noop_waker(),
        });

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::AckSend { result: Ok(()), .. })),
            "expected AckSend(Ok) when pipe is closed"
        );
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 1);
        assert_eq!(
            &reg.peer(&device_id).unwrap().pending_sends[0].datagram[..],
            b"closed"
        );
        assert!(reg.peer(&device_id).unwrap().pipe.is_none());
    }

    #[test]
    fn peripheral_lazy_peer_creation_from_inbound_fragment() {
        use crate::transport::peer::{ConnectRole, FragmentSource};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-stranger");

        let actions = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: FragmentSource::PeripheralReceivedC2p,
            bytes: bytes::Bytes::from_static(b"lazy-hello"),
        });

        let entry = reg.peer(&device_id).expect("peripheral entry created");
        assert_eq!(entry.role, ConnectRole::Peripheral);
        assert!(matches!(entry.phase, PeerPhase::Connected { .. }));
        assert_eq!(entry.rx_backlog.len(), 1);
        assert_eq!(&entry.rx_backlog[0][..], b"lazy-hello");

        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe { role: ConnectRole::Peripheral, device_id: d, .. }
                if d == &device_id
        )));
    }

    #[test]
    fn peripheral_client_subscribed_materializes_connected_peer() {
        use crate::transport::peer::{ConnectPath, ConnectRole, PeerPhase};
        use uuid::uuid;

        let mut reg = Registry::new_for_test();
        let client = DeviceId::from("inbound-client");
        let actions = reg.handle(PeerCommand::PeripheralClientSubscribed {
            client_id: client.clone(),
            char_uuid: uuid!("69726f02-8e45-4c2c-b3a5-331f3098b5c2"),
        });
        let entry = reg.peers.get(&client).expect("entry created");
        assert_eq!(entry.role, ConnectRole::Peripheral);
        assert!(matches!(entry.phase, PeerPhase::Connected { .. }));
        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::StartDataPipe {
                    role: ConnectRole::Peripheral,
                    path: ConnectPath::Gatt,
                    ..
                }
            )),
            "must emit StartDataPipe role=Peripheral path=Gatt; got {actions:?}"
        );
    }

    #[test]
    fn peripheral_client_subscribed_idempotent_for_second_char() {
        use crate::transport::peer::{ConnectRole, PeerPhase};
        use uuid::uuid;

        let mut reg = Registry::new_for_test();
        let client = DeviceId::from("inbound");
        let _ = reg.handle(PeerCommand::PeripheralClientSubscribed {
            client_id: client.clone(),
            char_uuid: uuid!("69726f02-8e45-4c2c-b3a5-331f3098b5c2"),
        });
        let tx_gen_after_first = match reg.peers[&client].phase {
            PeerPhase::Connected { tx_gen, .. } => tx_gen,
            _ => panic!(),
        };
        let actions = reg.handle(PeerCommand::PeripheralClientSubscribed {
            client_id: client.clone(),
            char_uuid: uuid!("69726f03-8e45-4c2c-b3a5-331f3098b5c2"),
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartDataPipe { .. })),
            "second subscribe on same client must not start another pipe"
        );
        match reg.peers[&client].phase {
            PeerPhase::Connected { tx_gen, .. } => assert_eq!(tx_gen, tx_gen_after_first),
            _ => panic!(),
        }
        assert_eq!(reg.peers[&client].role, ConnectRole::Peripheral);
    }

    #[tokio::test]
    async fn inbound_fragment_fast_path_pushes_to_inbound_tx() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, FragmentSource, PipeHandles};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-60");

        let (outbound_tx, _outbound_rx) =
            tokio::sync::mpsc::channel::<crate::transport::peer::PendingSend>(4);
        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);

        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.last_rx = Some(std::time::Instant::now());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e
        });

        reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: FragmentSource::CentralReceivedP2c,
            bytes: bytes::Bytes::from_static(b"frag"),
        });

        assert_eq!(inbound_rx.recv().await.unwrap().as_ref(), b"frag");
        assert_eq!(reg.peer(&device_id).unwrap().rx_backlog.len(), 0);
    }

    #[test]
    fn inbound_fragment_without_pipe_buffers_in_rx_backlog() {
        use crate::transport::peer::{ConnectPath, FragmentSource};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-61");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.last_rx = Some(std::time::Instant::now());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let _ = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: FragmentSource::CentralReceivedP2c,
            bytes: bytes::Bytes::from_static(b"race"),
        });
        assert_eq!(reg.peer(&device_id).unwrap().rx_backlog.len(), 1);
    }

    #[test]
    fn inbound_fragment_in_discovered_phase_rebuilds_as_peripheral_connected() {
        use crate::transport::peer::{ConnectRole, FragmentSource};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-rebuild-1");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Discovered {
                since: std::time::Instant::now(),
            };
            e
        });

        let actions = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: FragmentSource::PeripheralReceivedC2p,
            bytes: bytes::Bytes::from_static(b"rebuild"),
        });

        let entry = reg.peer(&device_id).unwrap();
        assert!(matches!(entry.phase, PeerPhase::Connected { .. }));
        assert_eq!(entry.role, ConnectRole::Peripheral);
        assert_eq!(entry.rx_backlog.len(), 1);
        assert_eq!(&entry.rx_backlog[0][..], b"rebuild");
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe { role: ConnectRole::Peripheral, device_id: d, .. }
                if d == &device_id
        )));
    }

    #[test]
    fn forget_evicts_connected_entry_and_emits_close_channel() {
        use crate::transport::peer::{
            ChannelHandle, ConnectPath, DeadReason, PendingSend, PipeHandles,
        };

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-forget");

        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(4);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);

        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 4;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 11,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 4,
                upgrading: false,
            };
            let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e.pending_sends.push_back(PendingSend {
                tx_gen: 4,
                datagram: bytes::Bytes::from_static(b"queued"),
                waker: noop_waker(),
            });
            e
        });

        let actions = reg.handle(PeerCommand::Forget {
            device_id: device_id.clone(),
        });

        let entry = reg.peer(&device_id).expect("entry still present until GC");
        assert!(matches!(
            &entry.phase,
            PeerPhase::Dead {
                reason: DeadReason::Forgotten,
                ..
            }
        ));
        assert!(entry.pipe.is_none());
        assert_eq!(entry.pending_sends.len(), 0);

        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::AckSend {
                result: Err(std::io::ErrorKind::ConnectionAborted),
                ..
            }
        )));
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::CloseChannel { device_id: d, .. } if d == &device_id
        )));
    }

    #[test]
    fn forget_unknown_device_is_noop() {
        let mut reg = Registry::new_for_test();
        let actions = reg.handle(PeerCommand::Forget {
            device_id: blew::DeviceId::from("dev-no-such"),
        });
        assert!(actions.is_empty());
    }

    #[test]
    fn inbound_fragment_in_dead_phase_rebuilds_as_peripheral_connected() {
        use crate::transport::peer::{ConnectRole, DeadReason, FragmentSource};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-rebuild-2");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Dead {
                reason: DeadReason::MaxRetries,
                at: std::time::Instant::now(),
            };
            e
        });

        let actions = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: FragmentSource::PeripheralReceivedC2p,
            bytes: bytes::Bytes::from_static(b"alive-again"),
        });

        let entry = reg.peer(&device_id).unwrap();
        assert!(matches!(entry.phase, PeerPhase::Connected { .. }));
        assert_eq!(entry.role, ConnectRole::Peripheral);
        assert_eq!(entry.tx_gen, 1);
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe {
                role: ConnectRole::Peripheral,
                ..
            }
        )));
    }

    #[test]
    fn draining_wakes_pending_sends_with_broken_pipe_and_drops_pipe() {
        use crate::transport::peer::{
            ChannelHandle, ConnectPath, DisconnectReason, PendingSend, PipeHandles,
        };

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-70");

        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(4);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);

        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e.pending_sends.push_back(PendingSend {
                tx_gen: 1,
                datagram: bytes::Bytes::from_static(b"x"),
                waker: noop_waker(),
            });
            e
        });

        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id: device_id.clone(),
            cause: blew::DisconnectCause::LinkLoss,
        });

        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::AckSend {
                result: Err(std::io::ErrorKind::BrokenPipe),
                ..
            }
        )));
        let entry = reg.peer(&device_id).unwrap();
        assert!(entry.pipe.is_none(), "pipe dropped on Draining");
        assert_eq!(entry.pending_sends.len(), 0);
        assert!(matches!(
            entry.phase,
            PeerPhase::Draining {
                reason: DisconnectReason::LinkLoss,
                ..
            }
        ));
    }

    #[test]
    fn connect_succeeded_disabled_goes_straight_to_connected() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::Disabled);
        let device_id = blew::DeviceId::from("dev-l2cap-2");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 0;
            e.phase = PeerPhase::Connecting {
                attempt: 0,
                started: std::time::Instant::now(),
                path: ConnectPath::Gatt,
            };
            e
        });
        let ch = ChannelHandle {
            id: 10,
            path: ConnectPath::Gatt,
        };
        let actions = reg.handle(PeerCommand::ConnectSucceeded {
            device_id: device_id.clone(),
            channel: ch,
        });

        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::StartDataPipe {
                    path: ConnectPath::Gatt,
                    ..
                }
            )),
            "expected StartDataPipe(Gatt); got {actions:?}"
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { tx_gen, .. } => assert_eq!(*tx_gen, 1),
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[test]
    fn open_l2cap_succeeded_transitions_to_connected_l2cap() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2cap-3");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 1;
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 42,
                    path: ConnectPath::Gatt,
                },
            };
            e
        });

        let (l2cap_chan, _other) = blew::L2capChannel::pair(1024);
        let actions = reg.handle(PeerCommand::OpenL2capSucceeded {
            device_id: device_id.clone(),
            channel: l2cap_chan,
        });

        let entry = reg.peer(&device_id).unwrap();
        match &entry.phase {
            PeerPhase::Connected {
                channel, tx_gen, ..
            } => {
                assert_eq!(channel.path, ConnectPath::L2cap);
                assert_eq!(channel.id, 42);
                assert_eq!(*tx_gen, 2);
            }
            other => panic!("expected Connected(L2cap), got {other:?}"),
        }
        assert!(
            entry.l2cap_channel.is_none(),
            "l2cap_channel should be taken into StartDataPipe"
        );
        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::StartDataPipe {
                    path: ConnectPath::L2cap,
                    l2cap_channel: Some(_),
                    ..
                }
            )),
            "expected StartDataPipe(L2cap) with channel; got {actions:?}"
        );
    }

    #[test]
    fn open_l2cap_failed_falls_back_to_gatt() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2cap-4");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 1;
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 42,
                    path: ConnectPath::Gatt,
                },
            };
            e
        });

        let actions = reg.handle(PeerCommand::OpenL2capFailed {
            device_id: device_id.clone(),
            error: "no PSM".into(),
        });

        let entry = reg.peer(&device_id).unwrap();
        match &entry.phase {
            PeerPhase::Connected {
                channel, tx_gen, ..
            } => {
                assert_eq!(channel.path, ConnectPath::Gatt);
                assert_eq!(*tx_gen, 2);
            }
            other => panic!("expected Connected(Gatt), got {other:?}"),
        }
        assert!(entry.l2cap_channel.is_none());
        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::StartDataPipe {
                    path: ConnectPath::Gatt,
                    ..
                }
            )),
            "expected StartDataPipe(Gatt); got {actions:?}"
        );
        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::EmitMetric(s) if s.contains("l2cap_fallback_to_gatt")
            )),
            "expected l2cap_fallback_to_gatt metric; got {actions:?}"
        );
    }

    #[test]
    fn inbound_l2cap_channel_creates_peripheral_entry_with_l2cap_path() {
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2cap-inbound");
        let (ch_a, _ch_b) = blew::L2capChannel::pair(8192);
        let actions = reg.handle(PeerCommand::InboundL2capChannel {
            device_id: device_id.clone(),
            channel: ch_a,
        });

        let entry = reg.peer(&device_id).unwrap();
        assert_eq!(entry.role, ConnectRole::Peripheral);
        assert_eq!(entry.tx_gen, 1);
        assert!(
            entry.l2cap_channel.is_none(),
            "l2cap_channel should be taken into StartDataPipe"
        );
        match &entry.phase {
            PeerPhase::Connected {
                channel, tx_gen, ..
            } => {
                assert_eq!(channel.path, ConnectPath::L2cap);
                assert_eq!(*tx_gen, 1);
            }
            other => panic!("expected Connected(L2cap), got {other:?}"),
        }
        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::StartDataPipe {
                    role: ConnectRole::Peripheral,
                    path: ConnectPath::L2cap,
                    l2cap_channel: Some(_),
                    device_id: d,
                } if *d == device_id
            )),
            "expected StartDataPipe(L2cap, Peripheral) with channel; got {actions:?}"
        );
    }

    #[tokio::test]
    async fn send_datagram_drains_pending_before_new_data() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend, PipeHandles};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-ordering");

        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(8);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);

        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 5;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 5,
                upgrading: false,
            };
            let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e.pending_sends.push_back(PendingSend {
                tx_gen: 5,
                datagram: bytes::Bytes::from_static(b"old"),
                waker: noop_waker(),
            });
            e
        });

        let actions = reg.handle(PeerCommand::SendDatagram {
            device_id: device_id.clone(),
            tx_gen: 5,
            datagram: bytes::Bytes::from_static(b"new"),
            waker: noop_waker(),
        });

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::AckSend { result: Ok(()), .. })),
            "expected AckSend(Ok) for new datagram; got {actions:?}"
        );

        let first = outbound_rx.recv().await.expect("first item");
        let second = outbound_rx.recv().await.expect("second item");
        assert_eq!(
            &first.datagram[..],
            b"old",
            "old (pre-buffered) must arrive first"
        );
        assert_eq!(
            &second.datagram[..],
            b"new",
            "new datagram must arrive second"
        );
        assert_eq!(reg.peer(&device_id).unwrap().pending_sends.len(), 0);
    }

    #[test]
    fn inbound_l2cap_channel_dropped_when_gatt_pipe_exists() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PendingSend, PipeHandles};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2cap-dup");

        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(4);
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);

        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 3;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 3,
                upgrading: false,
            };
            let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel::<blew::L2capChannel>(1);
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e
        });

        let (ch, _other) = blew::L2capChannel::pair(8192);
        let actions = reg.handle(PeerCommand::InboundL2capChannel {
            device_id: device_id.clone(),
            channel: ch,
        });

        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::EmitMetric(s) if s == "l2cap_late_accept_after_gatt"
            )),
            "expected l2cap_late_accept_after_gatt metric; got {actions:?}"
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected {
                channel, tx_gen, ..
            } => {
                assert_eq!(channel.path, ConnectPath::Gatt, "path unchanged");
                assert_eq!(*tx_gen, 3, "tx_gen unchanged");
            }
            other => panic!("expected Connected(Gatt) unchanged, got {other:?}"),
        }
    }

    #[test]
    fn open_l2cap_succeeded_moves_handshaking_to_connected_l2cap() {
        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2-ok");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
            };
            e
        });
        let (chan, _other) = blew::L2capChannel::pair(1024);
        let actions = reg.handle(PeerCommand::OpenL2capSucceeded {
            device_id: device_id.clone(),
            channel: chan,
        });
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe {
                path: crate::transport::peer::ConnectPath::L2cap,
                ..
            }
        )));
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { channel, .. } => {
                assert_eq!(channel.path, crate::transport::peer::ConnectPath::L2cap);
            }
            other => panic!("expected Connected L2cap, got {other:?}"),
        }
    }

    #[test]
    fn open_l2cap_failed_falls_back_to_gatt_simple() {
        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2-fail");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
            };
            e
        });
        let actions = reg.handle(PeerCommand::OpenL2capFailed {
            device_id: device_id.clone(),
            error: "no l2cap".into(),
        });
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe {
                path: crate::transport::peer::ConnectPath::Gatt,
                ..
            }
        )));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::EmitMetric(s) if s.contains("l2cap_fallback")))
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { channel, .. } => {
                assert_eq!(channel.path, crate::transport::peer::ConnectPath::Gatt);
            }
            other => panic!("expected Connected Gatt, got {other:?}"),
        }
    }

    #[test]
    fn inbound_fragment_reconstructs_dead_peer_as_peripheral() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-resurrect");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::MaxRetries,
                at: std::time::Instant::now(),
            };
            e
        });
        let actions = reg.handle(PeerCommand::InboundGattFragment {
            device_id: device_id.clone(),
            source: crate::transport::peer::FragmentSource::PeripheralReceivedC2p,
            bytes: bytes::Bytes::from_static(b"hello"),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartDataPipe { .. }))
        );
        let entry = reg.peer(&device_id).unwrap();
        assert!(matches!(entry.phase, PeerPhase::Connected { .. }));
        assert_eq!(entry.role, crate::transport::peer::ConnectRole::Peripheral);
        assert_eq!(entry.rx_backlog.len(), 1);
    }

    #[test]
    fn disconnect_acks_all_pending_sends_with_broken_pipe() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-drain-ack");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.tx_gen = 1;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            for i in 0..3u64 {
                e.pending_sends
                    .push_back(crate::transport::peer::PendingSend {
                        tx_gen: 1,
                        datagram: bytes::Bytes::from(vec![i as u8]),
                        waker: noop_waker(),
                    });
            }
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id: device_id.clone(),
            cause: blew::DisconnectCause::RemoteClose,
        });
        let broken_pipe_count = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    PeerAction::AckSend {
                        result: Err(std::io::ErrorKind::BrokenPipe),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            broken_pipe_count, 3,
            "all 3 pending sends acked with BrokenPipe"
        );
        assert!(reg.peer(&device_id).unwrap().pending_sends.is_empty());
    }

    #[test]
    fn connect_succeeded_emits_read_version() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-readver");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connecting {
                attempt: 0,
                started: std::time::Instant::now(),
                path: ConnectPath::Gatt,
            };
            e
        });
        let actions = reg.handle(PeerCommand::ConnectSucceeded {
            device_id: device_id.clone(),
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
        });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                PeerAction::ReadVersion { device_id: d } if *d == device_id
            )),
            "expected ReadVersion; got {actions:?}"
        );
    }

    #[test]
    fn protocol_version_mismatch_from_connected_dead_and_closes_channel() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-pv");
        let channel = ChannelHandle {
            id: 7,
            path: ConnectPath::Gatt,
        };
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: channel.clone(),
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::ProtocolVersionMismatch {
            device_id: device_id.clone(),
            got: 7,
            want: 1,
        });
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::CloseChannel { device_id: d, .. } if *d == device_id
        )));
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::ProtocolMismatch { got: 7, want: 1 },
                ..
            } => {}
            other => panic!("wrong phase: {other:?}"),
        }
    }

    #[test]
    fn protocol_version_mismatch_is_noop_for_unknown_peer() {
        let mut reg = Registry::new_for_test();
        let actions = reg.handle(PeerCommand::ProtocolVersionMismatch {
            device_id: blew::DeviceId::from("dev-unknown"),
            got: 9,
            want: 1,
        });
        assert!(actions.is_empty());
    }

    #[test]
    fn connected_to_draining_emits_put_peer_store_when_prefix_known() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-store-put");
        let prefix: crate::transport::peer::KeyPrefix = [0x77; 12];
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.prefix = Some(prefix);
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id: device_id.clone(),
            cause: blew::DisconnectCause::RemoteClose,
        });
        let put = actions.iter().find_map(|a| match a {
            PeerAction::PutPeerStore {
                prefix: p,
                snapshot,
            } => Some((*p, snapshot.clone())),
            _ => None,
        });
        let (put_prefix, snapshot) = put.expect("expected PutPeerStore");
        assert_eq!(put_prefix, prefix);
        assert_eq!(snapshot.last_device_id, device_id.as_str());
    }

    #[test]
    fn draining_from_handshaking_does_not_emit_put_peer_store() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-store-handshake-drain");
        let prefix: crate::transport::peer::KeyPrefix = [0x78; 12];
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.prefix = Some(prefix);
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
            };
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id,
            cause: blew::DisconnectCause::RemoteClose,
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::PutPeerStore { .. })),
            "Handshaking peers have not yet proved themselves — do not persist them; got {actions:?}"
        );
    }

    #[test]
    fn connected_to_draining_skips_put_when_prefix_unknown() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};

        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-store-no-prefix");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.prefix = None;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
            e
        });
        let actions = reg.handle(PeerCommand::CentralDisconnected {
            device_id,
            cause: blew::DisconnectCause::RemoteClose,
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::PutPeerStore { .. })),
            "prefix-less peers are not persisted; got {actions:?}"
        );
    }

    #[test]
    fn max_retries_emits_forget_peer_store() {
        let mut reg = Registry::new_for_test();
        let device_id = blew::DeviceId::from("dev-forget");
        let prefix: crate::transport::peer::KeyPrefix = [0x79; 12];
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.prefix = Some(prefix);
            e.consecutive_failures = 14;
            e.phase = PeerPhase::Connecting {
                attempt: 14,
                started: std::time::Instant::now(),
                path: crate::transport::peer::ConnectPath::Gatt,
            };
            e
        });
        let actions = reg.handle(PeerCommand::ConnectFailed {
            device_id: device_id.clone(),
            error: "final boom".into(),
        });
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Dead {
                reason: crate::transport::peer::DeadReason::MaxRetries,
                ..
            }
        ));
        let forget = actions.iter().find_map(|a| match a {
            PeerAction::ForgetPeerStore { prefix: p } => Some(*p),
            _ => None,
        });
        assert_eq!(
            forget,
            Some(prefix),
            "expected ForgetPeerStore; {actions:?}"
        );
    }

    #[test]
    fn advertised_records_prefix_on_peer_entry() {
        let mut reg = Registry::new_for_test();
        let device = blew::BleDevice {
            id: blew::DeviceId::from("dev-prefix"),
            name: None,
            rssi: None,
            services: vec![],
        };
        let prefix: crate::transport::peer::KeyPrefix = [0xAB; 12];
        reg.handle(PeerCommand::Advertised {
            prefix,
            device: device.clone(),
            rssi: None,
        });
        assert_eq!(reg.peer(&device.id).unwrap().prefix, Some(prefix));
    }

    #[test]
    fn verified_endpoint_stamps_all_matching_prefix_entries() {
        use crate::transport::routing::prefix_from_endpoint;

        let mut reg = Registry::new_for_test();
        let endpoint = iroh_base::SecretKey::from_bytes(&[7u8; 32]).public();
        let prefix = prefix_from_endpoint(&endpoint);

        // Two entries, same prefix (central view + peripheral view).
        let dev_c = DeviceId::from("central-view");
        let dev_p = DeviceId::from("peripheral-view");
        for did in [&dev_c, &dev_p] {
            reg.peers.insert(did.clone(), PeerEntry::new(did.clone()));
            let e = reg.peers.get_mut(did).unwrap();
            e.prefix = Some(prefix);
        }

        let _actions = reg.handle(PeerCommand::VerifiedEndpoint {
            endpoint_id: endpoint,
        });

        assert_eq!(reg.verified_prefixes.get(&prefix), Some(&endpoint));
        assert_eq!(reg.peers[&dev_c].verified_endpoint, Some(endpoint));
        assert_eq!(reg.peers[&dev_p].verified_endpoint, Some(endpoint));
    }

    #[test]
    fn verified_endpoint_ignores_entries_with_different_prefix() {
        use crate::transport::routing::prefix_from_endpoint;

        let mut reg = Registry::new_for_test();
        let ep_a = iroh_base::SecretKey::from_bytes(&[1u8; 32]).public();
        let ep_b = iroh_base::SecretKey::from_bytes(&[2u8; 32]).public();

        let dev = DeviceId::from("peer-b");
        reg.peers.insert(dev.clone(), PeerEntry::new(dev.clone()));
        reg.peers.get_mut(&dev).unwrap().prefix = Some(prefix_from_endpoint(&ep_b));

        let _ = reg.handle(PeerCommand::VerifiedEndpoint { endpoint_id: ep_a });

        assert!(
            reg.peers[&dev].verified_endpoint.is_none(),
            "entry for different prefix must not be stamped"
        );
    }

    #[test]
    fn advertised_suppressed_when_verified_peer_already_connected() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, PeerPhase, PendingSend};
        use crate::transport::routing::prefix_from_endpoint;

        let mut reg = Registry::new_for_test();
        let endpoint = iroh_base::SecretKey::from_bytes(&[9u8; 32]).public();
        let prefix = prefix_from_endpoint(&endpoint);

        // Existing verified-and-Connected entry for this prefix.
        let alive_dev = DeviceId::from("alive-central");
        reg.peers
            .insert(alive_dev.clone(), PeerEntry::new(alive_dev.clone()));
        let alive = reg.peers.get_mut(&alive_dev).unwrap();
        alive.prefix = Some(prefix);
        alive.verified_endpoint = Some(endpoint);
        alive.phase = PeerPhase::Connected {
            since: std::time::Instant::now(),
            channel: ChannelHandle {
                id: 1,
                path: ConnectPath::Gatt,
            },
            tx_gen: 1,
            upgrading: false,
        };

        // Seed a *different* BleDevice.id for the same peer with a pending send
        // so handle_advertised would normally transition Unknown → Connecting and
        // emit StartConnect. The guard must suppress that.
        let new_dev = DeviceId::from("new-mac");
        reg.peers
            .insert(new_dev.clone(), PeerEntry::new(new_dev.clone()));
        reg.peers
            .get_mut(&new_dev)
            .unwrap()
            .pending_sends
            .push_back(PendingSend {
                tx_gen: 0,
                datagram: bytes::Bytes::from_static(b"x"),
                waker: noop_waker(),
            });

        let actions = reg.handle(PeerCommand::Advertised {
            prefix,
            device: blew::BleDevice {
                id: new_dev.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartConnect { .. })),
            "must not emit StartConnect when a verified live peer already exists; got {actions:?}",
        );
        assert!(
            matches!(
                reg.peers[&new_dev].phase,
                PeerPhase::Discovered { .. } | PeerPhase::Unknown,
            ),
            "duplicate entry should stay pre-Connecting; got {:?}",
            reg.peers[&new_dev].phase,
        );
    }

    #[test]
    fn advertised_still_dials_unverified_peer() {
        use crate::transport::routing::prefix_from_endpoint;

        let mut reg = Registry::new_for_test();
        let endpoint = iroh_base::SecretKey::from_bytes(&[8u8; 32]).public();
        let prefix = prefix_from_endpoint(&endpoint);
        let dev_id = DeviceId::from("fresh");

        let _ = reg.handle(PeerCommand::Advertised {
            prefix,
            device: blew::BleDevice {
                id: dev_id.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });

        // Pre-existing behaviour: a fresh advertisement with no pending_sends
        // transitions Unknown → Discovered. Dedup guard must not interfere.
        assert!(matches!(
            reg.peers[&dev_id].phase,
            PeerPhase::Discovered { .. },
        ));
    }

    #[test]
    fn lower_prefix_side_enters_pending_dial() {
        use crate::transport::peer::{PeerPhase, PendingSend};
        use crate::transport::routing::prefix_from_endpoint;

        // Seed [0xFF;32] yields a lower prefix than [0x01;32] for Ed25519.
        let my_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let mut reg = Registry::new_for_test_with_endpoint(my_ep);

        let peer_ep = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer_prefix = prefix_from_endpoint(&peer_ep);
        let peer_dev = DeviceId::from("peer-high");

        reg.peers
            .insert(peer_dev.clone(), PeerEntry::new(peer_dev.clone()));
        reg.peers
            .get_mut(&peer_dev)
            .unwrap()
            .pending_sends
            .push_back(PendingSend {
                tx_gen: 0,
                datagram: bytes::Bytes::from_static(b"x"),
                waker: noop_waker(),
            });

        let my_prefix = prefix_from_endpoint(&my_ep);
        assert!(
            my_prefix < peer_prefix,
            "test setup invariant violated: my_prefix ({my_prefix:?}) \
             must be < peer_prefix ({peer_prefix:?})",
        );

        let actions = reg.handle(PeerCommand::Advertised {
            prefix: peer_prefix,
            device: blew::BleDevice {
                id: peer_dev.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartConnect { .. })),
            "lower-prefix side must not dial immediately; got {actions:?}",
        );
        assert!(
            matches!(reg.peers[&peer_dev].phase, PeerPhase::PendingDial { .. }),
            "lower side must enter PendingDial; got {:?}",
            reg.peers[&peer_dev].phase,
        );
    }

    #[test]
    fn higher_prefix_side_dials_immediately_on_pending_send() {
        use crate::transport::peer::{PeerPhase, PendingSend};
        use crate::transport::routing::prefix_from_endpoint;

        // Seed [0x01;32] yields a higher prefix than [0xFF;32] for Ed25519.
        let my_ep = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let mut reg = Registry::new_for_test_with_endpoint(my_ep);

        let peer_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let peer_prefix = prefix_from_endpoint(&peer_ep);
        let peer_dev = DeviceId::from("peer-low");

        reg.peers
            .insert(peer_dev.clone(), PeerEntry::new(peer_dev.clone()));
        reg.peers
            .get_mut(&peer_dev)
            .unwrap()
            .pending_sends
            .push_back(PendingSend {
                tx_gen: 0,
                datagram: bytes::Bytes::from_static(b"x"),
                waker: noop_waker(),
            });

        let my_prefix = prefix_from_endpoint(&my_ep);
        assert!(
            my_prefix > peer_prefix,
            "test setup invariant violated: my_prefix ({my_prefix:?}) \
             must be > peer_prefix ({peer_prefix:?})",
        );

        let actions = reg.handle(PeerCommand::Advertised {
            prefix: peer_prefix,
            device: blew::BleDevice {
                id: peer_dev.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartConnect { .. })),
            "higher-prefix side must dial immediately with a pending send; got {actions:?}",
        );
        assert!(
            matches!(reg.peers[&peer_dev].phase, PeerPhase::Connecting { .. }),
            "higher side must transition to Connecting; got {:?}",
            reg.peers[&peer_dev].phase,
        );
    }

    #[test]
    fn pending_dial_expires_into_connecting_on_tick() {
        use crate::transport::peer::{PeerPhase, PendingSend};
        use crate::transport::routing::prefix_from_endpoint;
        use std::time::Duration;

        // [0xFF;32] yields a lower prefix than [0x01;32] for Ed25519 (same as
        // lower_prefix_side_enters_pending_dial).
        let my_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let mut reg = Registry::new_for_test_with_endpoint(my_ep);
        let peer_ep = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer_prefix = prefix_from_endpoint(&peer_ep);
        let peer_dev = DeviceId::from("peer-high");

        // Invariant guard: Ed25519 may flip ordering. Fix seeds if this fires.
        let my_prefix = prefix_from_endpoint(&my_ep);
        assert!(
            my_prefix < peer_prefix,
            "test invariant: my_prefix must be < peer_prefix",
        );

        reg.peers
            .insert(peer_dev.clone(), PeerEntry::new(peer_dev.clone()));
        reg.peers
            .get_mut(&peer_dev)
            .unwrap()
            .pending_sends
            .push_back(PendingSend {
                tx_gen: 0,
                datagram: bytes::Bytes::from_static(b"x"),
                waker: noop_waker(),
            });

        let _ = reg.handle(PeerCommand::Advertised {
            prefix: peer_prefix,
            device: blew::BleDevice {
                id: peer_dev.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });
        let deadline = match reg.peers[&peer_dev].phase {
            PeerPhase::PendingDial { deadline, .. } => deadline,
            ref other => panic!("expected PendingDial, got {other:?}"),
        };

        let actions = reg.handle(PeerCommand::Tick(deadline + Duration::from_millis(10)));

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartConnect { .. })),
            "deadline expiry must emit StartConnect; got {actions:?}",
        );
        assert!(
            matches!(reg.peers[&peer_dev].phase, PeerPhase::Connecting { .. }),
            "pending-dial must advance to Connecting; got {:?}",
            reg.peers[&peer_dev].phase,
        );
    }

    #[test]
    fn pending_dial_cancelled_when_verified_endpoint_arrives_for_same_prefix() {
        use crate::transport::peer::{DisconnectReason, PeerPhase, PendingSend};
        use crate::transport::routing::prefix_from_endpoint;

        // [0xFF;32] yields a lower prefix than [0x01;32] for Ed25519.
        let my_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let mut reg = Registry::new_for_test_with_endpoint(my_ep);
        let peer_ep = iroh_base::SecretKey::from_bytes(&[0x01u8; 32]).public();
        let peer_prefix = prefix_from_endpoint(&peer_ep);
        let pending_dev = DeviceId::from("pending-for-peer");

        let my_prefix = prefix_from_endpoint(&my_ep);
        assert!(my_prefix < peer_prefix, "test invariant: my < peer");

        reg.peers
            .insert(pending_dev.clone(), PeerEntry::new(pending_dev.clone()));
        reg.peers
            .get_mut(&pending_dev)
            .unwrap()
            .pending_sends
            .push_back(PendingSend {
                tx_gen: 0,
                datagram: bytes::Bytes::from_static(b"x"),
                waker: noop_waker(),
            });
        let _ = reg.handle(PeerCommand::Advertised {
            prefix: peer_prefix,
            device: blew::BleDevice {
                id: pending_dev.clone(),
                name: None,
                rssi: None,
                services: vec![],
            },
            rssi: None,
        });
        assert!(matches!(
            reg.peers[&pending_dev].phase,
            PeerPhase::PendingDial { .. },
        ));

        // A separate PeerEntry (e.g. peripheral-side of the same peer) will
        // ultimately be the one that verified. Create it with the matching prefix
        // so handle_verified_endpoint stamps it (and subsequently the PendingDial
        // entry gets cancelled).
        let periph_dev = DeviceId::from("periph-for-peer");
        reg.peers
            .insert(periph_dev.clone(), PeerEntry::new(periph_dev.clone()));
        reg.peers.get_mut(&periph_dev).unwrap().prefix = Some(peer_prefix);

        let _ = reg.handle(PeerCommand::VerifiedEndpoint {
            endpoint_id: peer_ep,
        });

        assert!(
            matches!(
                &reg.peers[&pending_dev].phase,
                PeerPhase::Draining {
                    reason: DisconnectReason::DedupLoser,
                    ..
                },
            ),
            "pending-dial entry must be drained as DedupLoser; got {:?}",
            reg.peers[&pending_dev].phase,
        );
    }

    #[test]
    fn dedup_drains_loser_keeping_winner_by_endpoint_tiebreaker() {
        use crate::transport::peer::{
            ChannelHandle, ConnectPath, ConnectRole, DisconnectReason, PeerPhase,
        };
        use crate::transport::routing::prefix_from_endpoint;

        let my_ep = iroh_base::SecretKey::from_bytes(&[0x80u8; 32]).public();
        let peer_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let peer_prefix = prefix_from_endpoint(&peer_ep);

        assert!(
            my_ep.as_bytes() > peer_ep.as_bytes(),
            "test invariant: my_endpoint must be > peer_endpoint bytewise",
        );

        let mut reg = Registry::new_for_test_with_endpoint(my_ep);

        let dev_c = DeviceId::from("central-side");
        let dev_p = DeviceId::from("peripheral-side");
        for (did, role) in [
            (&dev_c, ConnectRole::Central),
            (&dev_p, ConnectRole::Peripheral),
        ] {
            reg.peers.insert(did.clone(), PeerEntry::new(did.clone()));
            let e = reg.peers.get_mut(did).unwrap();
            e.prefix = Some(peer_prefix);
            e.role = role;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
        }

        let _ = reg.handle(PeerCommand::VerifiedEndpoint {
            endpoint_id: peer_ep,
        });

        assert!(
            matches!(&reg.peers[&dev_c].phase, PeerPhase::Connected { .. }),
            "HIGH side: central entry must survive; got {:?}",
            reg.peers[&dev_c].phase,
        );
        assert!(
            matches!(
                &reg.peers[&dev_p].phase,
                PeerPhase::Draining {
                    reason: DisconnectReason::DedupLoser,
                    ..
                },
            ),
            "HIGH side: peripheral entry must drain as DedupLoser; got {:?}",
            reg.peers[&dev_p].phase,
        );
    }

    #[test]
    fn dedup_lower_side_drains_own_central_keeps_peripheral() {
        use crate::transport::peer::{
            ChannelHandle, ConnectPath, ConnectRole, DisconnectReason, PeerPhase,
        };
        use crate::transport::routing::prefix_from_endpoint;

        let my_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let peer_ep = iroh_base::SecretKey::from_bytes(&[0x80u8; 32]).public();
        let peer_prefix = prefix_from_endpoint(&peer_ep);

        assert!(
            my_ep.as_bytes() < peer_ep.as_bytes(),
            "test invariant: my_endpoint must be < peer_endpoint bytewise",
        );

        let mut reg = Registry::new_for_test_with_endpoint(my_ep);
        let dev_c = DeviceId::from("my-central");
        let dev_p = DeviceId::from("my-peripheral");
        for (did, role) in [
            (&dev_c, ConnectRole::Central),
            (&dev_p, ConnectRole::Peripheral),
        ] {
            reg.peers.insert(did.clone(), PeerEntry::new(did.clone()));
            let e = reg.peers.get_mut(did).unwrap();
            e.prefix = Some(peer_prefix);
            e.role = role;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
        }

        let _ = reg.handle(PeerCommand::VerifiedEndpoint {
            endpoint_id: peer_ep,
        });

        assert!(
            matches!(&reg.peers[&dev_p].phase, PeerPhase::Connected { .. }),
            "LOW side: peripheral entry must survive; got {:?}",
            reg.peers[&dev_p].phase,
        );
        assert!(
            matches!(
                &reg.peers[&dev_c].phase,
                PeerPhase::Draining {
                    reason: DisconnectReason::DedupLoser,
                    ..
                },
            ),
            "LOW side: central entry must drain as DedupLoser; got {:?}",
            reg.peers[&dev_c].phase,
        );
    }

    #[test]
    fn upgrade_to_l2cap_emitted_on_winner_after_verified() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole, PeerPhase};
        use crate::transport::routing::prefix_from_endpoint;

        let my_ep = iroh_base::SecretKey::from_bytes(&[0x80u8; 32]).public();
        let peer_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        // Invariant guard: Ed25519 pubkey byte ordering isn't guaranteed to
        // follow seed byte ordering; make the test fail loud if key derivation
        // changes.
        assert!(
            my_ep.as_bytes() > peer_ep.as_bytes(),
            "test presumes HIGH>LOW on derived pubkeys"
        );
        let peer_prefix = prefix_from_endpoint(&peer_ep);

        let mut reg =
            Registry::new_for_test_with_policy_and_endpoint(L2capPolicy::PreferL2cap, my_ep);
        let dev = DeviceId::from("peer");
        reg.peers.insert(dev.clone(), PeerEntry::new(dev.clone()));
        {
            let e = reg.peers.get_mut(&dev).unwrap();
            e.prefix = Some(peer_prefix);
            e.role = ConnectRole::Central;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
        }

        let actions = reg.handle(PeerCommand::VerifiedEndpoint {
            endpoint_id: peer_ep,
        });

        assert!(
            actions.iter().any(
                |a| matches!(a, PeerAction::UpgradeToL2cap { device_id } if device_id == &dev)
            ),
            "winner must get UpgradeToL2cap; got {actions:?}"
        );
        match &reg.peers[&dev].phase {
            PeerPhase::Connected { upgrading, .. } => assert!(*upgrading),
            other => panic!("expected Connected{{upgrading:true}}, got {other:?}"),
        }
    }

    #[test]
    fn upgrade_to_l2cap_not_emitted_when_policy_disabled() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole, PeerPhase};
        use crate::transport::routing::prefix_from_endpoint;

        let my_ep = iroh_base::SecretKey::from_bytes(&[0x80u8; 32]).public();
        let peer_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        assert!(my_ep.as_bytes() > peer_ep.as_bytes());
        let peer_prefix = prefix_from_endpoint(&peer_ep);

        let mut reg = Registry::new_for_test_with_policy_and_endpoint(L2capPolicy::Disabled, my_ep);
        let dev = DeviceId::from("peer");
        reg.peers.insert(dev.clone(), PeerEntry::new(dev.clone()));
        {
            let e = reg.peers.get_mut(&dev).unwrap();
            e.prefix = Some(peer_prefix);
            e.role = ConnectRole::Central;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: false,
            };
        }

        let actions = reg.handle(PeerCommand::VerifiedEndpoint {
            endpoint_id: peer_ep,
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::UpgradeToL2cap { .. }))
        );
        match &reg.peers[&dev].phase {
            PeerPhase::Connected { upgrading, .. } => assert!(!*upgrading),
            other => panic!("expected Connected{{upgrading:false}}, got {other:?}"),
        }
    }

    #[test]
    fn open_l2cap_succeeded_while_upgrading_emits_swap_pipe() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole, PipeHandles};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-upgrade-ok");
        let (outbound_tx, _) = tokio::sync::mpsc::channel(1);
        let (inbound_tx, _) = tokio::sync::mpsc::channel(1);
        let (swap_tx, _swap_rx) = tokio::sync::mpsc::channel(1);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 2;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 2,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 2,
                upgrading: true,
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
                swap_tx,
            });
            e
        });

        let (l2cap_chan, _other) = blew::L2capChannel::pair(1024);
        let actions = reg.handle(PeerCommand::OpenL2capSucceeded {
            device_id: device_id.clone(),
            channel: l2cap_chan,
        });

        assert!(
            actions.iter().any(|a| matches!(a, PeerAction::SwapPipeToL2cap { device_id: did, .. } if *did == device_id)),
            "expected SwapPipeToL2cap; got {actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartDataPipe { .. })),
            "should NOT emit StartDataPipe for upgrade path; got {actions:?}"
        );
        let entry = reg.peer(&device_id).unwrap();
        match &entry.phase {
            PeerPhase::Connected { upgrading, .. } => assert!(!*upgrading),
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[test]
    fn open_l2cap_succeeded_without_pipe_handles_clears_upgrading_and_marks_failed() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-upgrade-ok-nopipe");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 2;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 2,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 2,
                upgrading: true,
            };
            e.pipe = None;
            e
        });

        let (l2cap_chan, _other) = blew::L2capChannel::pair(1024);
        let actions = reg.handle(PeerCommand::OpenL2capSucceeded {
            device_id: device_id.clone(),
            channel: l2cap_chan,
        });

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::SwapPipeToL2cap { .. })),
            "should NOT emit SwapPipeToL2cap when pipe handles are absent; got {actions:?}"
        );
        let entry = reg.peer(&device_id).unwrap();
        assert!(
            entry.l2cap_upgrade_failed,
            "l2cap_upgrade_failed should be set so the peer is not stuck upgrading"
        );
        match &entry.phase {
            PeerPhase::Connected { upgrading, .. } => assert!(!*upgrading),
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[test]
    fn open_l2cap_failed_while_upgrading_marks_failed_and_clears_upgrading() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new_for_test_with_policy(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-upgrade-fail");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 2;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 2,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 2,
                upgrading: true,
            };
            e
        });

        let actions = reg.handle(PeerCommand::OpenL2capFailed {
            device_id: device_id.clone(),
            error: "upgrade timed out".into(),
        });

        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PeerAction::StartDataPipe { .. })),
            "should NOT emit StartDataPipe for upgrade-fail path; got {actions:?}"
        );
        let entry = reg.peer(&device_id).unwrap();
        assert!(
            entry.l2cap_upgrade_failed,
            "l2cap_upgrade_failed should be set"
        );
        match &entry.phase {
            PeerPhase::Connected { upgrading, .. } => assert!(!*upgrading),
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[test]
    fn l2cap_handover_timeout_reverts_and_marks_failed() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole, PeerPhase};

        let my_ep = iroh_base::SecretKey::from_bytes(&[0xFFu8; 32]).public();
        let mut reg = Registry::new(L2capPolicy::PreferL2cap, my_ep);
        let dev = DeviceId::from("peer");
        reg.peers.insert(dev.clone(), PeerEntry::new(dev.clone()));
        {
            let e = reg.peers.get_mut(&dev).unwrap();
            e.role = ConnectRole::Central;
            e.phase = PeerPhase::Connected {
                since: std::time::Instant::now(),
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                tx_gen: 1,
                upgrading: true,
            };
        }
        let actions = reg.handle(PeerCommand::L2capHandoverTimeout {
            device_id: dev.clone(),
        });

        assert!(
            actions.iter().any(
                |a| matches!(a, PeerAction::RevertToGattPipe { device_id } if device_id == &dev)
            )
        );
        let e = &reg.peers[&dev];
        assert!(e.l2cap_upgrade_failed);
        match &e.phase {
            PeerPhase::Connected { upgrading, .. } => assert!(!*upgrading),
            other => panic!("expected Connected, got {other:?}"),
        }
    }
}
