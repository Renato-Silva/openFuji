//! openFuji — talk to Fujifilm cameras over Wi-Fi from Linux.
//!
//! A clean-room Rust implementation of the protocol used by the
//! FUJIFILM Camera Remote app (PTP containers over TCP on ports 55740-55742).

pub mod ble;
pub mod camera;
pub mod dataset;
pub mod link;
pub mod mock;
pub mod ptp;
pub mod transport;
pub mod wifi;

pub use camera::{Camera, ConnectOptions, Mode};
