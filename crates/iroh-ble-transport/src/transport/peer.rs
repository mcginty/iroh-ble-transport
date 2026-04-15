//! Peer types: `PeerEntry`, `PeerPhase`, `PeerCommand`, `PeerAction`.
//!
//! All types in this module are pure data. No I/O, no tokio tasks. Any code
//! here can be exercised from synchronous unit tests.

use std::collections::VecDeque;
use std::task::Waker;
use std::time::Instant;

use blew::{BleDevice, DeviceId, DisconnectCause, L2capChannel};
use bytes::Bytes;

pub const KEY_PREFIX_LEN: usize = 12;
pub type KeyPrefix = [u8; KEY_PREFIX_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentSource {
    /// Produced by `run_central_events` when a P2C notification arrives.
    CentralReceivedP2c,
    /// Produced by `run_peripheral_events` when a C2P write arrives.
    PeripheralReceivedC2p,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectRole {
    /// This node discovered the peer via scanning and drives the GATT client side.
    Central,
    /// This node received an inbound GATT write from the peer and answers as peripheral.
    Peripheral,
}

#[derive(Debug, Clone)]
pub struct PipeHandles {
    pub outbound_tx: tokio::sync::mpsc::Sender<PendingSend>,
    pub inbound_tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
}

#[derive(Debug)]
pub struct PeerEntry {
    pub device_id: DeviceId,
    pub phase: PeerPhase,
    pub last_adv: Option<Instant>,
    pub last_rx: Option<Instant>,
    pub last_tx: Option<Instant>,
    pub consecutive_failures: u32,
    pub tx_gen: u64,
    pub pending_sends: VecDeque<PendingSend>,
    pub role: ConnectRole,
    pub pipe: Option<PipeHandles>,
    pub rx_backlog: VecDeque<Bytes>,
}

impl PeerEntry {
    pub fn new(device_id: DeviceId) -> Self {
        Self {
            device_id,
            phase: PeerPhase::Unknown,
            last_adv: None,
            last_rx: None,
            last_tx: None,
            consecutive_failures: 0,
            tx_gen: 0,
            pending_sends: VecDeque::new(),
            role: ConnectRole::Central,
            pipe: None,
            rx_backlog: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
pub struct PendingSend {
    pub tx_gen: u64,
    pub datagram: Bytes,
    pub waker: Waker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPath {
    L2cap,
    Gatt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    LocalClose,
    RemoteClose,
    LinkLoss,
    AdapterOff,
    Gatt133,
    Timeout,
    ProtocolError,
    LinkDead,
    Unknown(i32),
}

impl From<DisconnectCause> for DisconnectReason {
    fn from(cause: DisconnectCause) -> Self {
        match cause {
            DisconnectCause::LocalClose => DisconnectReason::LocalClose,
            DisconnectCause::RemoteClose => DisconnectReason::RemoteClose,
            DisconnectCause::LinkLoss => DisconnectReason::LinkLoss,
            DisconnectCause::AdapterOff => DisconnectReason::AdapterOff,
            DisconnectCause::Gatt133 => DisconnectReason::Gatt133,
            DisconnectCause::Timeout => DisconnectReason::Timeout,
            DisconnectCause::Unknown(c) => DisconnectReason::Unknown(c),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadReason {
    MaxRetries,
    ProtocolMismatch { got: u8, want: u8 },
    Forgotten,
}

#[derive(Debug)]
pub enum PeerPhase {
    Unknown,
    Discovered {
        since: Instant,
    },
    Connecting {
        attempt: u32,
        started: Instant,
        path: ConnectPath,
    },
    Handshaking {
        since: Instant,
        channel: ChannelHandle,
    },
    Connected {
        since: Instant,
        channel: ChannelHandle,
        tx_gen: u64,
    },
    Draining {
        since: Instant,
        reason: DisconnectReason,
    },
    Reconnecting {
        attempt: u32,
        next_at: Instant,
        reason: DisconnectReason,
    },
    Dead {
        reason: DeadReason,
        at: Instant,
    },
    Restoring {
        since: Instant,
    },
}

/// Opaque handle to a live channel held inside the driver. The registry
/// treats this as a value it passes back to the driver when asking for I/O.
#[derive(Debug, Clone)]
pub struct ChannelHandle {
    pub id: u64,
    pub path: ConnectPath,
}

#[derive(Debug)]
pub enum PeerCommand {
    Advertised {
        prefix: KeyPrefix,
        device: BleDevice,
        rssi: Option<i16>,
    },
    CentralConnected {
        device_id: DeviceId,
    },
    CentralDisconnected {
        device_id: DeviceId,
        cause: DisconnectCause,
    },
    InboundGattFragment {
        device_id: DeviceId,
        source: FragmentSource,
        bytes: Bytes,
    },
    InboundL2capChannel {
        device_id: DeviceId,
        channel: L2capChannel,
    },
    AdapterStateChanged {
        powered: bool,
    },
    RestoreFromAdapter {
        devices: Vec<BleDevice>,
    },
    SendDatagram {
        device_id: DeviceId,
        tx_gen: u64,
        datagram: Bytes,
        waker: Waker,
    },
    Tick(Instant),
    ConnectSucceeded {
        device_id: DeviceId,
        channel: ChannelHandle,
    },
    ConnectFailed {
        device_id: DeviceId,
        error: String,
    },
    OpenL2capSucceeded {
        device_id: DeviceId,
        channel: L2capChannel,
    },
    OpenL2capFailed {
        device_id: DeviceId,
        error: String,
    },
    Stalled {
        device_id: DeviceId,
    },
    /// Tell the registry to evict this DeviceId entirely. Used when the
    /// routing layer detects that a known prefix has flipped to a new
    /// DeviceId (e.g. peer restart with a new MAC) so the stale entry can be
    /// torn down before the new one is rebuilt by an inbound write.
    Forget {
        device_id: DeviceId,
    },
    DataPipeReady {
        device_id: DeviceId,
        outbound_tx: tokio::sync::mpsc::Sender<PendingSend>,
        inbound_tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum PeerAction {
    StartConnect {
        device_id: DeviceId,
        attempt: u32,
    },
    OpenL2cap {
        device_id: DeviceId,
    },
    CloseChannel {
        device_id: DeviceId,
        channel: ChannelHandle,
        reason: DisconnectReason,
    },
    Refresh {
        device_id: DeviceId,
    },
    AckSend {
        tx_gen: u64,
        waker: Waker,
        result: Result<(), std::io::ErrorKind>,
    },
    RebuildGattServer,
    RestartAdvertising,
    PurgeAllForAdapterOff,
    StartDataPipe {
        device_id: DeviceId,
        role: ConnectRole,
    },
    EmitMetric(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_entry_defaults_to_unknown() {
        let e = PeerEntry::new(DeviceId::from("test"));
        assert!(matches!(e.phase, PeerPhase::Unknown));
        assert_eq!(e.tx_gen, 0);
    }

    #[test]
    fn disconnect_cause_maps_to_reason() {
        assert_eq!(
            DisconnectReason::from(DisconnectCause::Gatt133),
            DisconnectReason::Gatt133
        );
    }

    #[test]
    fn peer_entry_defaults_to_central_role_and_no_pipe() {
        let e = PeerEntry::new(DeviceId::from("test"));
        assert_eq!(e.role, ConnectRole::Central);
        assert!(e.pipe.is_none());
        assert!(e.rx_backlog.is_empty());
    }

    #[test]
    fn fragment_source_distinguishes_direction() {
        let c = FragmentSource::CentralReceivedP2c;
        let p = FragmentSource::PeripheralReceivedC2p;
        assert_ne!(c, p);
    }

    #[test]
    fn data_pipe_variants_are_constructible() {
        let (outbound_tx, _) = tokio::sync::mpsc::channel::<PendingSend>(1);
        let (inbound_tx, _) = tokio::sync::mpsc::channel::<Bytes>(1);
        let _cmd = PeerCommand::DataPipeReady {
            device_id: DeviceId::from("x"),
            outbound_tx,
            inbound_tx,
        };
        let _act = PeerAction::StartDataPipe {
            device_id: DeviceId::from("x"),
            role: ConnectRole::Central,
        };
    }
}
