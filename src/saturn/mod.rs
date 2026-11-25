//! ANAN G2 (Saturn) through XDMA, when running on the G2's own Raspberry
//! Pi -- see xdma.rs. Linux only; elsewhere there's never one to find.

#[cfg(target_os = "linux")]
mod xdma;
#[cfg(target_os = "linux")]
pub use xdma::{discover, SaturnLink};

#[cfg(not(target_os = "linux"))]
mod none {
    use std::io;
    use std::sync::Arc;
    use std::time::Duration;

    pub struct Found {
        pub info: Info,
        pub incompatible: bool,
        pub in_use_by: Option<String>,
        pub mac: [u8; 6],
    }

    #[derive(Debug, Clone, Copy)]
    pub struct Info {
        pub minor_version: u32,
    }

    pub fn discover() -> Option<Found> {
        None
    }

    pub struct SaturnLink;

    impl SaturnLink {
        pub fn open() -> io::Result<Arc<SaturnLink>> {
            Err(io::Error::other("Saturn XDMA is Linux only"))
        }
        pub fn send(&self, _port: u16, _packet: &[u8]) {}
        pub fn recv(&self, _timeout: Duration) -> Option<(u16, Vec<u8>)> {
            None
        }
        pub fn close(&self) {}
    }
}
#[cfg(not(target_os = "linux"))]
pub use none::{discover, SaturnLink};
