//! PTP constants and the Fujifilm "PTP over TCP" container format.
//!
//! Fujifilm's Wi-Fi protocol is *not* standard PTP/IP (ISO 15740 Annex). After a
//! PTP/IP-like init handshake, every packet is a plain USB-style PTP container
//! sent over TCP:
//!
//! ```text
//! u32 length (total, including this header)
//! u16 container type (1=command, 2=data, 3=response, 4=event)
//! u16 operation / response / event code
//! u32 transaction id
//! ... payload (u32 params for command/response/event, raw bytes for data)
//! ```
//!
//! Everything is little-endian. Values were taken from `libFTLPTPIP.so` /
//! `libFFIR.so` and the Java layer of FUJIFILM Camera Remote 4.10.2.

/// TCP port for the command channel (camera listens, `0xD9BC`).
pub const CMD_PORT: u16 = 55740;
/// TCP port for the asynchronous event channel (opened after InitiateOpenCapture).
pub const EVENT_PORT: u16 = 55741;
/// TCP port for the live-view ("through picture") stream.
pub const LIVEVIEW_PORT: u16 = 55742;
/// Camera address when it acts as an access point (hard-coded in the app as 0xC0A80001).
pub const DEFAULT_CAMERA_IP: &str = "192.168.0.1";

pub const HEADER_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ContainerType {
    Command = 1,
    Data = 2,
    Response = 3,
    Event = 4,
}

impl ContainerType {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Command,
            2 => Self::Data,
            3 => Self::Response,
            4 => Self::Event,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub length: u32,
    pub kind: u16,
    pub code: u16,
    pub tid: u32,
}

impl Header {
    pub fn parse(b: &[u8; HEADER_LEN]) -> Self {
        Header {
            length: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            kind: u16::from_le_bytes([b[4], b[5]]),
            code: u16::from_le_bytes([b[6], b[7]]),
            tid: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        }
    }

    pub fn payload_len(&self) -> usize {
        (self.length as usize).saturating_sub(HEADER_LEN)
    }
}

/// Encode a container with the given payload.
pub fn encode(kind: ContainerType, code: u16, tid: u32, payload: &[u8]) -> Vec<u8> {
    let len = HEADER_LEN + payload.len();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&(len as u32).to_le_bytes());
    v.extend_from_slice(&(kind as u16).to_le_bytes());
    v.extend_from_slice(&code.to_le_bytes());
    v.extend_from_slice(&tid.to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// Encode a command container (at most 5 params, as in PTP).
pub fn encode_command(code: u16, tid: u32, params: &[u32]) -> Vec<u8> {
    assert!(params.len() <= 5, "PTP commands take at most 5 parameters");
    let payload: Vec<u8> = params.iter().flat_map(|p| p.to_le_bytes()).collect();
    encode(ContainerType::Command, code, tid, &payload)
}

pub fn decode_params(payload: &[u8]) -> Vec<u32> {
    payload
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ---------------------------------------------------------------------------
// Init handshake (port 55740)
// ---------------------------------------------------------------------------

pub const INIT_COMMAND_REQUEST: u32 = 1;
pub const INIT_COMMAND_ACK: u32 = 2;
pub const INIT_FAIL: u32 = 5;
/// Protocol version word sent by the app (bytes f2 e4 53 8f on the wire).
pub const INIT_PROTOCOL_VERSION: u32 = 0x8f53_e4f2;
/// The registration packet is always exactly 82 bytes.
pub const INIT_REQUEST_LEN: usize = 0x52;
/// A successful ack is exactly 68 bytes.
pub const INIT_ACK_LEN: usize = 0x44;
/// Max UTF-16 code units for the client name (54 bytes incl. terminator).
pub const INIT_NAME_MAX_CHARS: usize = 26;
/// First 12 bytes of the client GUID used by the app; the last 4 are the
/// phone's IPv4 address.
pub const APP_GUID_PREFIX: [u8; 12] = [
    0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5, 0xde, 0xd0,
];
/// GUID the app expects back from the camera in the ack. Only used for a
/// diagnostic warning here.
pub const EXPECTED_CAMERA_GUID: [u8; 16] = [
    0x08, 0x70, 0xb0, 0x61, 0x0a, 0x8b, 0x45, 0x93, 0xb2, 0xe7, 0x93, 0x57, 0xdd, 0x36, 0xe0, 0x50,
];

/// Build the GUID the official app would send from `local_ip`.
pub fn app_guid(local_ip: [u8; 4]) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[..12].copy_from_slice(&APP_GUID_PREFIX);
    g[12..].copy_from_slice(&local_ip);
    g
}

/// Build the 82-byte Init_Command_Request.
pub fn encode_init_request(guid: &[u8; 16], name: &str) -> Vec<u8> {
    let mut v = vec![0u8; INIT_REQUEST_LEN];
    v[0..4].copy_from_slice(&(INIT_REQUEST_LEN as u32).to_le_bytes());
    v[4..8].copy_from_slice(&INIT_COMMAND_REQUEST.to_le_bytes());
    v[8..12].copy_from_slice(&INIT_PROTOCOL_VERSION.to_le_bytes());
    v[12..28].copy_from_slice(guid);
    for (i, unit) in name.encode_utf16().take(INIT_NAME_MAX_CHARS).enumerate() {
        let off = 28 + i * 2;
        v[off..off + 2].copy_from_slice(&unit.to_le_bytes());
    }
    v
}

// ---------------------------------------------------------------------------
// Operation codes
// ---------------------------------------------------------------------------

pub mod op {
    pub const GET_DEVICE_INFO: u16 = 0x1001;
    pub const OPEN_SESSION: u16 = 0x1002;
    pub const CLOSE_SESSION: u16 = 0x1003;
    pub const GET_STORAGE_IDS: u16 = 0x1004;
    pub const GET_STORAGE_INFO: u16 = 0x1005;
    pub const GET_NUM_OBJECTS: u16 = 0x1006;
    pub const GET_OBJECT_HANDLES: u16 = 0x1007;
    pub const GET_OBJECT_INFO: u16 = 0x1008;
    pub const GET_OBJECT: u16 = 0x1009;
    pub const GET_THUMB: u16 = 0x100A;
    pub const DELETE_OBJECT: u16 = 0x100B;
    pub const INITIATE_CAPTURE: u16 = 0x100E;
    pub const GET_DEVICE_PROP_DESC: u16 = 0x1014;
    pub const GET_DEVICE_PROP_VALUE: u16 = 0x1015;
    pub const SET_DEVICE_PROP_VALUE: u16 = 0x1016;
    pub const RESET_DEVICE_PROP_VALUE: u16 = 0x1017;
    pub const TERMINATE_OPEN_CAPTURE: u16 = 0x1018;
    pub const GET_PARTIAL_OBJECT: u16 = 0x101B;
    pub const INITIATE_OPEN_CAPTURE: u16 = 0x101C;
    /// Fuji: half-press shutter (S1) lock.
    pub const FUJI_S1_LOCK: u16 = 0x9026;
    /// Fuji: release S1 lock.
    pub const FUJI_S1_UNLOCK: u16 = 0x9027;

    pub fn name(code: u16) -> &'static str {
        match code {
            GET_DEVICE_INFO => "GetDeviceInfo",
            OPEN_SESSION => "OpenSession",
            CLOSE_SESSION => "CloseSession",
            GET_STORAGE_IDS => "GetStorageIDs",
            GET_STORAGE_INFO => "GetStorageInfo",
            GET_NUM_OBJECTS => "GetNumObjects",
            GET_OBJECT_HANDLES => "GetObjectHandles",
            GET_OBJECT_INFO => "GetObjectInfo",
            GET_OBJECT => "GetObject",
            GET_THUMB => "GetThumb",
            DELETE_OBJECT => "DeleteObject",
            INITIATE_CAPTURE => "InitiateCapture",
            GET_DEVICE_PROP_DESC => "GetDevicePropDesc",
            GET_DEVICE_PROP_VALUE => "GetDevicePropValue",
            SET_DEVICE_PROP_VALUE => "SetDevicePropValue",
            RESET_DEVICE_PROP_VALUE => "ResetDevicePropValue",
            TERMINATE_OPEN_CAPTURE => "TerminateOpenCapture",
            GET_PARTIAL_OBJECT => "GetPartialObject",
            INITIATE_OPEN_CAPTURE => "InitiateOpenCapture",
            FUJI_S1_LOCK => "Fuji_S1Lock",
            FUJI_S1_UNLOCK => "Fuji_S1Unlock",
            _ => "?",
        }
    }
}

// ---------------------------------------------------------------------------
// Response codes
// ---------------------------------------------------------------------------

pub mod rc {
    pub const UNDEFINED: u16 = 0x2000;
    pub const OK: u16 = 0x2001;
    pub const GENERAL_ERROR: u16 = 0x2002;
    pub const SESSION_NOT_OPEN: u16 = 0x2003;
    pub const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
    pub const INVALID_OBJECT_HANDLE: u16 = 0x2009;
    pub const DEVICE_PROP_NOT_SUPPORTED: u16 = 0x200A;
    pub const STORE_FULL: u16 = 0x200C;
    pub const ACCESS_DENIED: u16 = 0x200F;
    pub const DEVICE_BUSY: u16 = 0x2019;
    pub const INVALID_PARAMETER: u16 = 0x201D;
    pub const SESSION_ALREADY_OPEN: u16 = 0x201E;
    pub const INVALID_DEVICE_PROP_VALUE: u16 = 0x201C;

    pub fn name(code: u16) -> &'static str {
        match code {
            UNDEFINED => "Undefined",
            OK => "OK",
            GENERAL_ERROR => "GeneralError",
            SESSION_NOT_OPEN => "SessionNotOpen",
            0x2004 => "InvalidTransactionID",
            OPERATION_NOT_SUPPORTED => "OperationNotSupported",
            0x2006 => "ParameterNotSupported",
            0x2007 => "IncompleteTransfer",
            0x2008 => "InvalidStorageID",
            INVALID_OBJECT_HANDLE => "InvalidObjectHandle",
            DEVICE_PROP_NOT_SUPPORTED => "DevicePropNotSupported",
            0x200B => "InvalidObjectFormatCode",
            STORE_FULL => "StoreFull",
            0x200D => "ObjectWriteProtected",
            0x200E => "StoreReadOnly",
            ACCESS_DENIED => "AccessDenied",
            0x2010 => "NoThumbnailPresent",
            0x2013 => "StoreNotAvailable",
            0x2014 => "SpecificationByFormatUnsupported",
            0x2015 => "NoValidObjectInfo",
            DEVICE_BUSY => "DeviceBusy",
            0x201A => "InvalidParentObject",
            0x201B => "InvalidDevicePropFormat",
            INVALID_DEVICE_PROP_VALUE => "InvalidDevicePropValue",
            INVALID_PARAMETER => "InvalidParameter",
            SESSION_ALREADY_OPEN => "SessionAlreadyOpen",
            0x201F => "TransactionCancelled",
            _ => "?",
        }
    }
}

// ---------------------------------------------------------------------------
// Event codes
// ---------------------------------------------------------------------------

pub mod ev {
    pub fn name(code: u16) -> &'static str {
        match code {
            0x4002 => "ObjectAdded",
            0x4003 => "ObjectRemoved",
            0x4004 => "StoreAdded",
            0x4005 => "StoreRemoved",
            0x4006 => "DevicePropChanged",
            0x4008 => "DeviceInfoChanged",
            0x400A => "StoreFull",
            0x400C => "StorageInfoChanged",
            0x400D => "CaptureComplete",
            _ => "?",
        }
    }
}

// ---------------------------------------------------------------------------
// Device property codes
// ---------------------------------------------------------------------------

pub mod prop {
    pub const BATTERY_LEVEL: u16 = 0x5001;
    pub const IMAGE_SIZE: u16 = 0x5003;
    pub const WHITE_BALANCE: u16 = 0x5005;
    pub const F_NUMBER: u16 = 0x5007;
    pub const FOCUS_MODE: u16 = 0x500A;
    pub const EXPOSURE_PROGRAM_MODE: u16 = 0x500E;
    pub const EXPOSURE_INDEX: u16 = 0x500F;
    pub const EXPOSURE_BIAS: u16 = 0x5010;
    pub const STILL_CAPTURE_MODE: u16 = 0x5013;
    pub const FLASH_MODE: u16 = 0x500C;

    /// Fuji: list of changed properties, `u16 count` then `count × (u16 code, u32 value)`.
    /// The app polls this with GetDevicePropValue.
    pub const FUJI_EVENTS_LIST: u16 = 0xD212;
    pub const FUJI_SELECTED_IMGS_MODE: u16 = 0xD220;
    /// Fuji: number of images on the card available to the client.
    pub const FUJI_OBJECT_COUNT: u16 = 0xD222;
    /// Fuji: camera-side mode (u16). Set by `SDK_SetCameraFunctionMode`.
    pub const FUJI_CAMERA_STATE: u16 = 0xDF00;
    /// Fuji: client function mode (u16). Set by `SDK_SetFunctionMode`.
    pub const FUJI_FUNCTION_MODE: u16 = 0xDF01;
    /// Fuji: "photo receive" function version (u32).
    pub const FUJI_VERSION_PHOTO_RECEIVE: u16 = 0xDF21;
    /// Fuji: "photo viewer" (browse in camera) function version (u32).
    pub const FUJI_VERSION_PHOTO_VIEW: u16 = 0xDF22;
    pub const FUJI_VERSION_DF23: u16 = 0xDF23;
    /// Fuji: remote shooting function version (u32).
    pub const FUJI_VERSION_REMOTE: u16 = 0xDF24;
    pub const FUJI_VERSION_DF25: u16 = 0xDF25;
    /// Fuji: "reserved photo receive" function version (u32).
    pub const FUJI_VERSION_RESERVED_RECEIVE: u16 = 0xDF26;
    /// Fuji: GPS / location assist function version (u32).
    pub const FUJI_VERSION_GPS: u16 = 0xDF31;

    pub fn name(code: u16) -> &'static str {
        match code {
            BATTERY_LEVEL => "BatteryLevel",
            0x5002 => "FunctionalMode",
            IMAGE_SIZE => "ImageSize",
            0x5004 => "CompressionSetting",
            WHITE_BALANCE => "WhiteBalance",
            0x5006 => "RGBGain",
            F_NUMBER => "FNumber",
            0x5008 => "FocalLength",
            0x5009 => "FocusDistance",
            FOCUS_MODE => "FocusMode",
            0x500B => "ExposureMeteringMode",
            FLASH_MODE => "FlashMode",
            0x500D => "ExposureTime",
            EXPOSURE_PROGRAM_MODE => "ExposureProgramMode",
            EXPOSURE_INDEX => "ExposureIndex(ISO)",
            EXPOSURE_BIAS => "ExposureBiasCompensation",
            0x5011 => "DateTime",
            0x5012 => "CaptureDelay",
            STILL_CAPTURE_MODE => "StillCaptureMode",
            0x5018 => "BurstNumber",
            0x501C => "FocusMeteringMode",
            FUJI_EVENTS_LIST => "Fuji_EventsList",
            FUJI_SELECTED_IMGS_MODE => "Fuji_SelectedImgsMode",
            FUJI_OBJECT_COUNT => "Fuji_ObjectCount",
            FUJI_CAMERA_STATE => "Fuji_CameraState",
            FUJI_FUNCTION_MODE => "Fuji_FunctionMode",
            FUJI_VERSION_PHOTO_RECEIVE => "Fuji_VersionPhotoReceive",
            FUJI_VERSION_PHOTO_VIEW => "Fuji_VersionPhotoView",
            FUJI_VERSION_DF23 => "Fuji_VersionDF23",
            FUJI_VERSION_REMOTE => "Fuji_VersionRemote",
            FUJI_VERSION_DF25 => "Fuji_VersionDF25",
            FUJI_VERSION_RESERVED_RECEIVE => "Fuji_VersionReservedReceive",
            FUJI_VERSION_GPS => "Fuji_VersionGPS",
            _ => "?",
        }
    }
}

/// Values written to [`prop::FUJI_FUNCTION_MODE`] by the app.
pub mod function_mode {
    /// "Camera viewer" (browse/import images on the card from the phone).
    pub const PHOTO_VIEW: u16 = 2;
    /// Remote shooting with live view.
    pub const REMOTE: u16 = 5;
    /// Newer "Ver1.2" variant of photo view used for some bodies.
    pub const PHOTO_VIEW_V12: u16 = 9;
    /// Sent by the app when it receives an unknown camera mode, before disconnecting.
    pub const UNKNOWN_MODE_ABORT: u16 = 7;
}

/// Version values the app writes to the version properties (from `PhotoGateUtil`).
pub mod app_version {
    pub const REMOTE_MODE: u32 = 0x0002_000C;
    pub const PHOTO_VIEWER: u32 = 5;
    pub const PHOTO_VIEWER_2: u32 = 2;
    pub const PHOTO_RECEIVER: u32 = 4;
    pub const GPS_SET: u32 = 2;
    pub const RESERVED_PHOTO_RECEIVER: u32 = 1;
}

/// PTP data type codes used in property descriptors.
pub mod dtc {
    pub const UNDEF: u16 = 0x0000;
    pub const INT8: u16 = 0x0001;
    pub const UINT8: u16 = 0x0002;
    pub const INT16: u16 = 0x0003;
    pub const UINT16: u16 = 0x0004;
    pub const INT32: u16 = 0x0005;
    pub const UINT32: u16 = 0x0006;
    pub const INT64: u16 = 0x0007;
    pub const UINT64: u16 = 0x0008;
    pub const INT128: u16 = 0x0009;
    pub const UINT128: u16 = 0x000A;
    pub const ARRAY_MASK: u16 = 0x4000;
    pub const STR: u16 = 0xFFFF;
}

/// Object format codes (subset).
pub fn format_name(code: u16) -> &'static str {
    match code {
        0x3000 => "Undefined",
        0x3001 => "Association",
        0x3801 => "JPEG",
        0x3800 => "UndefinedImage",
        0x380D => "TIFF",
        0xB103 => "RAF",
        0x300D => "MOV",
        0x300B => "MPEG",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_request_layout() {
        let guid = app_guid([192, 168, 0, 2]);
        let p = encode_init_request(&guid, "openFuji");
        assert_eq!(p.len(), 82);
        assert_eq!(&p[0..4], &[0x52, 0, 0, 0]);
        assert_eq!(&p[4..8], &[1, 0, 0, 0]);
        assert_eq!(&p[8..12], &[0xf2, 0xe4, 0x53, 0x8f]);
        assert_eq!(&p[12..24], &APP_GUID_PREFIX);
        assert_eq!(&p[24..28], &[192, 168, 0, 2]);
        assert_eq!(&p[28..32], &[b'o', 0, b'p', 0]);
        assert!(p[28 + 16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn long_name_is_truncated_and_terminated() {
        let p = encode_init_request(&[0; 16], &"x".repeat(100));
        assert_eq!(p.len(), 82);
        assert_eq!(&p[80..82], &[0, 0]);
    }

    #[test]
    fn command_roundtrip() {
        let c = encode_command(op::OPEN_SESSION, 1, &[1]);
        assert_eq!(c.len(), 16);
        let h = Header::parse(c[..12].try_into().unwrap());
        assert_eq!(h.length, 16);
        assert_eq!(h.kind, 1);
        assert_eq!(h.code, 0x1002);
        assert_eq!(h.tid, 1);
        assert_eq!(decode_params(&c[12..]), vec![1]);
    }
}
