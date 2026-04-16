//! Pure state-machine registry. No I/O, no blew, no tokio tasks.
//!
//! The actor loop lives in [`Registry::run`] (added in a later task).
//! `handle` is the synchronous entry point used by both the loop and tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
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
const HANDSHAKING_L2CAP_WALL: std::time::Duration = std::time::Duration::from_millis(2000);

#[derive(Debug)]
pub struct Registry {
    peers: HashMap<DeviceId, PeerEntry>,
    l2cap_policy: L2capPolicy,
}

impl Registry {
    pub fn new(l2cap_policy: L2capPolicy) -> Self {
        Self {
            peers: HashMap::new(),
            l2cap_policy,
        }
    }

    pub fn new_for_test() -> Self {
        Self::new(L2capPolicy::Disabled)
    }

    pub fn handle(&mut self, cmd: PeerCommand) -> Vec<PeerAction> {
        let now = std::time::Instant::now();
        let mut actions = Vec::new();
        match cmd {
            PeerCommand::Advertised {
                prefix: _,
                device,
                rssi,
            } => {
                let _ = rssi;
                let device_id = device.id.clone();
                let entry = self
                    .peers
                    .entry(device_id.clone())
                    .or_insert_with(|| PeerEntry::new(device_id.clone()));
                entry.last_adv = Some(now);
                match &entry.phase {
                    PeerPhase::Unknown => {
                        if entry.pending_sends.is_empty() {
                            entry.phase = PeerPhase::Discovered { since: now };
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
            PeerCommand::SendDatagram {
                device_id,
                tx_gen,
                datagram,
                waker,
            } => {
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
            PeerCommand::ConnectSucceeded { device_id, channel } => {
                if let Some(entry) = self.peers.get_mut(&device_id)
                    && matches!(entry.phase, PeerPhase::Connecting { .. })
                {
                    entry.consecutive_failures = 0;
                    match self.l2cap_policy {
                        L2capPolicy::PreferL2cap => {
                            entry.phase = PeerPhase::Handshaking {
                                since: now,
                                channel,
                                l2cap_deadline: Some(now + L2CAP_SELECT_TIMEOUT),
                            };
                            actions.push(PeerAction::OpenL2cap {
                                device_id: device_id.clone(),
                            });
                        }
                        L2capPolicy::Disabled => {
                            entry.tx_gen += 1;
                            let tx_gen = entry.tx_gen;
                            entry.phase = PeerPhase::Connected {
                                since: now,
                                channel,
                                tx_gen,
                            };
                            let role = entry.role;
                            actions.push(PeerAction::StartDataPipe {
                                device_id: device_id.clone(),
                                role,
                                path: crate::transport::peer::ConnectPath::Gatt,
                                l2cap_channel: None,
                            });
                        }
                    }
                }
            }
            PeerCommand::ConnectFailed { device_id, error } => {
                if let Some(entry) = self.peers.get_mut(&device_id) {
                    let current_attempt =
                        if let PeerPhase::Connecting { attempt, .. } = &entry.phase {
                            Some(*attempt)
                        } else {
                            None
                        };
                    if let Some(attempt) = current_attempt {
                        let next_attempt = attempt + 1;
                        entry.consecutive_failures += 1;
                        actions.push(PeerAction::EmitMetric(format!("connect_failed:{error}")));
                        if next_attempt >= MAX_CONNECT_ATTEMPTS {
                            entry.phase = PeerPhase::Dead {
                                reason: crate::transport::peer::DeadReason::MaxRetries,
                                at: now,
                            };
                        } else {
                            entry.phase = PeerPhase::Reconnecting {
                                attempt: next_attempt,
                                next_at: now + reconnect_backoff(next_attempt),
                                reason: crate::transport::peer::DisconnectReason::Timeout,
                            };
                        }
                    }
                }
            }
            PeerCommand::InboundGattFragment {
                device_id,
                source: _,
                bytes,
            } => {
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
                    return actions;
                }

                let promoting = matches!(entry.phase, PeerPhase::Handshaking { .. });
                if promoting {
                    let channel = match &entry.phase {
                        PeerPhase::Handshaking { channel, .. } => channel.clone(),
                        _ => {
                            tracing::warn!(device = %device_id, "phase changed unexpectedly during fragment promotion");
                            return actions;
                        }
                    };
                    entry.tx_gen += 1;
                    let tx_gen = entry.tx_gen;
                    entry.phase = PeerPhase::Connected {
                        since: now,
                        channel,
                        tx_gen,
                    };
                    entry.rx_backlog.push_back(bytes);
                    let role = entry.role;
                    actions.push(PeerAction::StartDataPipe {
                        device_id: device_id.clone(),
                        role,
                        path: crate::transport::peer::ConnectPath::Gatt,
                        l2cap_channel: None,
                    });
                    return actions;
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
                    return actions;
                }

                // Any other phase (Discovered / Connecting / Reconnecting /
                // Restoring / Draining / Dead / Unknown): the peer is actively
                // writing to us, which is the strongest liveness signal we
                // have. Override the stale phase and rebuild as a fresh
                // peripheral Connected so the new pipe can be spun up.
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
                };
                entry.rx_backlog.push_back(bytes);
                actions.push(PeerAction::StartDataPipe {
                    device_id: device_id.clone(),
                    role: crate::transport::peer::ConnectRole::Peripheral,
                    path: crate::transport::peer::ConnectPath::Gatt,
                    l2cap_channel: None,
                });
            }
            PeerCommand::CentralDisconnected { device_id, cause } => {
                let Some(entry) = self.peers.get_mut(&device_id) else {
                    return actions;
                };
                let reason = crate::transport::peer::DisconnectReason::from(cause);
                let channel = match &entry.phase {
                    PeerPhase::Connected { channel, .. }
                    | PeerPhase::Handshaking { channel, .. } => Some(channel.clone()),
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
            PeerCommand::AdapterStateChanged { powered } => {
                if !powered {
                    for entry in self.peers.values_mut() {
                        entry.phase = PeerPhase::Restoring { since: now };
                        entry.pending_sends.clear();
                    }
                    actions.push(PeerAction::PurgeAllForAdapterOff);
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
                }
            }
            PeerCommand::RestoreFromAdapter { devices } => {
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
            PeerCommand::Tick(tick_now) => {
                enum TickAction {
                    StartConnect {
                        attempt: u32,
                    },
                    DrainingToDead,
                    RestoringToDead,
                    HandshakingL2capTimeout {
                        channel: crate::transport::peer::ChannelHandle,
                    },
                }

                let mut decisions: Vec<(DeviceId, TickAction)> = Vec::new();
                for (device_id, entry) in &self.peers {
                    let decision = match &entry.phase {
                        PeerPhase::Reconnecting {
                            attempt, next_at, ..
                        } if *next_at <= tick_now => {
                            Some(TickAction::StartConnect { attempt: *attempt })
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
                        PeerPhase::Handshaking {
                            since,
                            l2cap_deadline: Some(_),
                            channel,
                        } if tick_now.saturating_duration_since(*since)
                            > HANDSHAKING_L2CAP_WALL =>
                        {
                            Some(TickAction::HandshakingL2capTimeout {
                                channel: channel.clone(),
                            })
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
                        TickAction::HandshakingL2capTimeout { channel } => {
                            entry.tx_gen += 1;
                            let tx_gen = entry.tx_gen;
                            entry.phase = PeerPhase::Connected {
                                since: tick_now,
                                channel,
                                tx_gen,
                            };
                            let role = entry.role;
                            actions.push(PeerAction::StartDataPipe {
                                device_id: device_id.clone(),
                                role,
                                path: crate::transport::peer::ConnectPath::Gatt,
                                l2cap_channel: None,
                            });
                            actions
                                .push(PeerAction::EmitMetric("l2cap_handshaking_timeout".into()));
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
            PeerCommand::Forget { device_id } => {
                if let Some(entry) = self.peers.get_mut(&device_id) {
                    let channel = match &entry.phase {
                        PeerPhase::Connected { channel, .. }
                        | PeerPhase::Handshaking { channel, .. } => Some(channel.clone()),
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
            }
            PeerCommand::Stalled { device_id } => {
                if let Some(entry) = self.peers.get_mut(&device_id) {
                    let channel = if let PeerPhase::Connected { channel, .. } = &entry.phase {
                        Some(channel.clone())
                    } else {
                        None
                    };
                    if let Some(channel) = channel {
                        let reason = crate::transport::peer::DisconnectReason::LinkDead;
                        let broken_pipe_acks = Self::drain_to_draining(entry, now, reason.clone());
                        actions.extend(broken_pipe_acks);
                        actions.push(PeerAction::CloseChannel {
                            device_id: device_id.clone(),
                            channel,
                            reason,
                        });
                    }
                }
            }
            PeerCommand::Shutdown => {
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
            PeerCommand::DataPipeReady {
                device_id,
                outbound_tx,
                inbound_tx,
            } => {
                let Some(entry) = self.peers.get_mut(&device_id) else {
                    return actions;
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
                });
            }
            PeerCommand::OpenL2capSucceeded {
                device_id,
                channel: l2cap_chan,
            } => {
                if let Some(entry) = self.peers.get_mut(&device_id)
                    && matches!(entry.phase, PeerPhase::Handshaking { .. })
                {
                    let gatt_channel = match &entry.phase {
                        PeerPhase::Handshaking { channel, .. } => channel.clone(),
                        _ => {
                            tracing::warn!(device = %device_id, "phase changed unexpectedly during L2CAP success");
                            return actions;
                        }
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
            PeerCommand::OpenL2capFailed { device_id, error } => {
                if let Some(entry) = self.peers.get_mut(&device_id)
                    && matches!(entry.phase, PeerPhase::Handshaking { .. })
                {
                    let channel = match &entry.phase {
                        PeerPhase::Handshaking { channel, .. } => channel.clone(),
                        _ => {
                            tracing::warn!(device = %device_id, "phase changed unexpectedly during L2CAP failure");
                            return actions;
                        }
                    };
                    entry.tx_gen += 1;
                    let tx_gen = entry.tx_gen;
                    entry.phase = PeerPhase::Connected {
                        since: now,
                        channel,
                        tx_gen,
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
            }
            PeerCommand::InboundL2capChannel { device_id, channel } => {
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
                                    actions.push(PeerAction::EmitMetric(
                                        "l2cap_duplicate_accept".into(),
                                    ));
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
            _ => {}
        }
        actions
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
        entry.pipe = None;
        for send in entry.pending_sends.drain(..) {
            out.push(PeerAction::AckSend {
                tx_gen: send.tx_gen,
                waker: send.waker,
                result: Err(std::io::ErrorKind::BrokenPipe),
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
    ) {
        while let Some(cmd) = rx.recv().await {
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

impl Default for Registry {
    fn default() -> Self {
        Self::new(L2capPolicy::default())
    }
}

/// Backoff computation used by the Reconnecting path. Exposed for tests.
pub(crate) fn reconnect_backoff(attempt: u32) -> Duration {
    let base_ms = 500u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(base_ms.min(30_000))
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Unknown,
    Discovered,
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
                l2cap_deadline: None,
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
    fn adapter_off_moves_all_peers_to_restoring_with_single_purge() {
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
                };
                e
            });
            ids.push(device_id);
        }
        let actions = reg.handle(PeerCommand::AdapterStateChanged { powered: false });
        let purge_count = actions
            .iter()
            .filter(|a| matches!(a, PeerAction::PurgeAllForAdapterOff))
            .count();
        assert_eq!(purge_count, 1, "exactly one purge action for whole adapter");
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
            actions.is_empty(),
            "no actions yet; Tick drives Connecting later"
        );
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Reconnecting { attempt: 0, .. } => {}
            other => panic!("wrong phase: {other:?}"),
        }
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
                l2cap_deadline: None,
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
                l2cap_deadline: None,
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

        let actions = reg.handle(PeerCommand::DataPipeReady {
            device_id: device_id.clone(),
            outbound_tx,
            inbound_tx,
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
            };
            e.rx_backlog.push_back(bytes::Bytes::from_static(b"A"));
            e.rx_backlog.push_back(bytes::Bytes::from_static(b"B"));
            e
        });

        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PendingSend>(8);
        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);

        let actions = reg.handle(PeerCommand::DataPipeReady {
            device_id: device_id.clone(),
            outbound_tx,
            inbound_tx,
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
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
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
                };
                e.pipe = Some(PipeHandles {
                    outbound_tx,
                    inbound_tx,
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
                };
                e.pipe = Some(PipeHandles {
                    outbound_tx,
                    inbound_tx,
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
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
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
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
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
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
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
    fn connect_succeeded_prefer_l2cap_enters_handshaking_and_emits_open_l2cap() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2cap-1");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
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

        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Handshaking {
                l2cap_deadline: Some(_),
                ..
            } => {}
            other => panic!("expected Handshaking with l2cap_deadline, got {other:?}"),
        }
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::OpenL2cap { .. })),
            "expected OpenL2cap action; got {actions:?}"
        );
        assert_eq!(reg.peer(&device_id).unwrap().consecutive_failures, 0);
    }

    #[test]
    fn connect_succeeded_disabled_goes_straight_to_connected() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new(L2capPolicy::Disabled);
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

        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
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
                l2cap_deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
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

        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
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
                l2cap_deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
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
    fn tick_handshaking_l2cap_timeout_falls_back_to_gatt() {
        use crate::transport::peer::{ChannelHandle, ConnectPath, ConnectRole};

        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2cap-5");
        let old = std::time::Instant::now() - std::time::Duration::from_secs(5);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.role = ConnectRole::Central;
            e.tx_gen = 1;
            e.phase = PeerPhase::Handshaking {
                since: old,
                channel: ChannelHandle {
                    id: 42,
                    path: ConnectPath::Gatt,
                },
                l2cap_deadline: Some(old + L2CAP_SELECT_TIMEOUT),
            };
            e
        });

        let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));

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
                PeerAction::EmitMetric(s) if s.contains("l2cap_handshaking_timeout")
            )),
            "expected l2cap_handshaking_timeout metric; got {actions:?}"
        );
    }

    #[test]
    fn tick_handshaking_no_l2cap_deadline_is_not_timed_out() {
        use crate::transport::peer::{ChannelHandle, ConnectPath};

        let mut reg = Registry::new(L2capPolicy::Disabled);
        let device_id = blew::DeviceId::from("dev-l2cap-6");
        let old = std::time::Instant::now() - std::time::Duration::from_secs(5);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: old,
                channel: ChannelHandle {
                    id: 1,
                    path: ConnectPath::Gatt,
                },
                l2cap_deadline: None,
            };
            e
        });

        let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));
        assert!(actions.is_empty());
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Handshaking { .. }
        ));
    }

    #[test]
    fn inbound_l2cap_channel_creates_peripheral_entry_with_l2cap_path() {
        use crate::transport::peer::{ConnectPath, ConnectRole};

        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
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
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
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

        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
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
            };
            e.pipe = Some(PipeHandles {
                outbound_tx,
                inbound_tx,
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
    fn connect_succeeded_with_l2cap_policy_moves_to_handshaking() {
        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2-hs");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Connecting {
                attempt: 0,
                started: std::time::Instant::now(),
                path: crate::transport::peer::ConnectPath::Gatt,
            };
            e
        });
        let ch = crate::transport::peer::ChannelHandle {
            id: 42,
            path: crate::transport::peer::ConnectPath::Gatt,
        };
        let actions = reg.handle(PeerCommand::ConnectSucceeded {
            device_id: device_id.clone(),
            channel: ch,
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, PeerAction::OpenL2cap { .. }))
        );
        assert!(matches!(
            reg.peer(&device_id).unwrap().phase,
            PeerPhase::Handshaking {
                l2cap_deadline: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn open_l2cap_succeeded_moves_handshaking_to_connected_l2cap() {
        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2-ok");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                l2cap_deadline: Some(std::time::Instant::now() + Duration::from_secs(2)),
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
        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2-fail");
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: std::time::Instant::now(),
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                l2cap_deadline: Some(std::time::Instant::now() + Duration::from_secs(2)),
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
    fn handshaking_l2cap_wall_timeout_falls_back_to_gatt() {
        let mut reg = Registry::new(L2capPolicy::PreferL2cap);
        let device_id = blew::DeviceId::from("dev-l2-timeout");
        let old = std::time::Instant::now() - Duration::from_secs(10);
        reg.peers.insert(device_id.clone(), {
            let mut e = PeerEntry::new(device_id.clone());
            e.phase = PeerPhase::Handshaking {
                since: old,
                channel: crate::transport::peer::ChannelHandle {
                    id: 1,
                    path: crate::transport::peer::ConnectPath::Gatt,
                },
                l2cap_deadline: Some(old + Duration::from_millis(1500)),
            };
            e
        });
        let actions = reg.handle(PeerCommand::Tick(std::time::Instant::now()));
        assert!(actions.iter().any(|a| matches!(
            a,
            PeerAction::StartDataPipe {
                path: crate::transport::peer::ConnectPath::Gatt,
                ..
            }
        )));
        assert!(actions.iter().any(
            |a| matches!(a, PeerAction::EmitMetric(s) if s.contains("l2cap_handshaking_timeout"))
        ));
        match &reg.peer(&device_id).unwrap().phase {
            PeerPhase::Connected { channel, .. } => {
                assert_eq!(channel.path, crate::transport::peer::ConnectPath::Gatt);
            }
            other => panic!("expected Connected Gatt after timeout, got {other:?}"),
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
}
