use blew::BlewError;
use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum BleError {
    #[error("BLE not supported on this platform")]
    Unsupported,
    #[error("Bluetooth adapter not found")]
    AdapterNotFound,
    #[error("Device not found: {0}")]
    DeviceNotFound(String),
    #[error("Not connected to device: {0}")]
    NotConnected(String),
    #[error("GATT error: {0}")]
    GattError(String),
    #[error("GATT busy on device: {0}")]
    GattBusy(String),
    #[error("Operation timed out")]
    Timeout,
    #[error("Peripheral role error: {0}")]
    PeripheralError(String),
    #[error("Central role error: {0}")]
    CentralError(String),
    #[error("Characteristic not found: {0}")]
    CharacteristicNotFound(String),
    #[error("Already advertising")]
    AlreadyAdvertising,
    #[error("Channel error: {0}")]
    ChannelError(String),
    #[error("protocol version mismatch: local={local}, remote={remote}")]
    ProtocolVersionMismatch { local: u8, remote: u8 },
}

pub type BleResult<T> = Result<T, BleError>;

impl From<BlewError> for BleError {
    fn from(e: BlewError) -> Self {
        match e {
            BlewError::AdapterNotFound => BleError::AdapterNotFound,
            BlewError::NotSupported => BleError::Unsupported,
            BlewError::NotPowered => BleError::PeripheralError("adapter not powered".into()),
            BlewError::DeviceNotFound(id) => BleError::DeviceNotFound(id.to_string()),
            BlewError::NotConnected(id) => BleError::NotConnected(id.to_string()),
            BlewError::CharacteristicNotFound {
                device_id,
                char_uuid,
            } => BleError::CharacteristicNotFound(format!("{device_id}:{char_uuid}")),
            BlewError::LocalCharacteristicNotFound { char_uuid } => {
                BleError::CharacteristicNotFound(char_uuid.to_string())
            }
            BlewError::AlreadyAdvertising => BleError::AlreadyAdvertising,
            BlewError::Timeout => BleError::Timeout,
            BlewError::Gatt { device_id, source } => {
                BleError::GattError(format!("{device_id}: {source}"))
            }
            BlewError::Peripheral { source } => BleError::PeripheralError(source.to_string()),
            BlewError::Central { source } => BleError::CentralError(source.to_string()),
            BlewError::L2cap { source } => BleError::ChannelError(source.to_string()),
            BlewError::GattBusy(id) => BleError::GattBusy(id.to_string()),
            BlewError::Internal(msg) => BleError::PeripheralError(msg),
            other => BleError::PeripheralError(other.to_string()),
        }
    }
}
