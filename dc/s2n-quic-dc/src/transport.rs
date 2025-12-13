//! Transport provider abstraction
//!
//! Provides a trait-based abstraction over different transport implementations:
//! - LibfabricProvider: Uses hardware RDMA via libfabric
//! - UdpProvider: Pure userspace UDP fallback
//!
//! The system attempts to use libfabric at runtime, falling back to UDP if:
//! - Libfabric library is not found
//! - RDMA hardware is not available
//! - Libfabric initialization fails for any reason

use std::io;

/// Transport provider trait
///
/// Abstracts over different transport implementations (libfabric RDMA vs UDP)
pub trait TransportProvider: Send + Sync + std::fmt::Debug {
    /// Returns the transport type for capability negotiation
    fn transport_type(&self) -> TransportType;
    
    /// Check if this provider is available on the current system
    fn is_available(&self) -> bool;
    
    /// Initialize the provider
    ///
    /// Returns an error if the provider cannot be initialized
    /// (e.g., libfabric not found, no RDMA hardware)
    fn initialize(&mut self) -> io::Result<()>;
    
    /// Send data to a remote peer
    fn send(&self, dest: &[u8], data: &[u8]) -> io::Result<usize>;
    
    /// Receive data from a remote peer
    fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, Vec<u8>)>;
    
    /// Perform RDMA write operation (if supported)
    fn rdma_write(&self, dest: &[u8], data: &[u8], remote_addr: u64, remote_key: u64) -> io::Result<()>;
    
    /// Perform RDMA read operation (if supported)
    fn rdma_read(&self, dest: &[u8], buf: &mut [u8], remote_addr: u64, remote_key: u64) -> io::Result<usize>;
}

/// Transport type for capability negotiation
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportType {
    /// EFA/RDMA via libfabric
    Libfabric,
    /// UDP fallback
    Udp,
}

/// Provider selection result
#[derive(Debug)]
pub enum ProviderSelection {
    /// Successfully using libfabric
    Libfabric(Box<dyn TransportProvider>),
    /// Fell back to UDP
    Udp(Box<dyn TransportProvider>),
}

/// Attempt to initialize a transport provider with automatic fallback
///
/// This function implements the runtime fallback strategy:
/// 1. If libfabric feature is enabled, try to initialize LibfabricProvider
/// 2. If that fails or feature is disabled, fall back to UdpProvider
///
/// # Returns
/// - `ProviderSelection::Libfabric` if libfabric successfully initialized
/// - `ProviderSelection::Udp` if using UDP fallback
pub fn select_provider() -> io::Result<ProviderSelection> {
    // Try libfabric first if the feature is enabled
    #[cfg(feature = "libfabric")]
    {
        match try_libfabric_provider() {
            Ok(provider) => {
                tracing::info!("Using libfabric transport provider");
                return Ok(ProviderSelection::Libfabric(Box::new(provider)));
            }
            Err(e) => {
                tracing::warn!("Failed to initialize libfabric provider: {}, falling back to UDP", e);
            }
        }
    }
    
    // Fall back to UDP provider
    let provider = UdpProvider::new()?;
    tracing::info!("Using UDP transport provider");
    Ok(ProviderSelection::Udp(Box::new(provider)))
}

#[cfg(feature = "libfabric")]
fn try_libfabric_provider() -> io::Result<LibfabricProvider> {
    LibfabricProvider::new()
}

/// Libfabric-based transport provider
#[cfg(feature = "libfabric")]
#[derive(Debug)]
pub struct LibfabricProvider {
    // Will hold libfabric state (fabric, domain, endpoints, etc.)
    _initialized: bool,
}

#[cfg(feature = "libfabric")]
impl LibfabricProvider {
    pub fn new() -> io::Result<Self> {
        // Try to initialize libfabric
        // This will fail if:
        // - libfabric library not found at runtime
        // - No RDMA hardware available
        // - Permissions issues
        // - etc.
        
        use crate::libfabric::info;
        
        // Attempt to query for available providers
        let list = match std::panic::catch_unwind(|| {
            info::Query::new().build()
        }) {
            Ok(list) => list,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "libfabric library not found or failed to initialize"
                ));
            }
        };
        
        // Check if any providers are available
        let mut has_provider = false;
        for _info in list.iter() {
            has_provider = true;
            break;
        }
        
        if !has_provider {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No libfabric providers available (no RDMA hardware?)"
            ));
        }
        
        Ok(Self {
            _initialized: true,
        })
    }
}

#[cfg(feature = "libfabric")]
impl TransportProvider for LibfabricProvider {
    fn transport_type(&self) -> TransportType {
        TransportType::Libfabric
    }
    
    fn is_available(&self) -> bool {
        true
    }
    
    fn initialize(&mut self) -> io::Result<()> {
        // Already initialized in new()
        Ok(())
    }
    
    fn send(&self, _dest: &[u8], _data: &[u8]) -> io::Result<usize> {
        // TODO: Implement using libfabric endpoint
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
    
    fn recv(&self, _buf: &mut [u8]) -> io::Result<(usize, Vec<u8>)> {
        // TODO: Implement using libfabric endpoint
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
    
    fn rdma_write(&self, _dest: &[u8], _data: &[u8], _remote_addr: u64, _remote_key: u64) -> io::Result<()> {
        // TODO: Implement RDMA write
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
    
    fn rdma_read(&self, _dest: &[u8], _buf: &mut [u8], _remote_addr: u64, _remote_key: u64) -> io::Result<usize> {
        // TODO: Implement RDMA read
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
}

/// UDP-based transport provider (fallback)
#[derive(Debug)]
pub struct UdpProvider {
    // Will hold UDP sockets, address mappings, etc.
    _initialized: bool,
}

impl UdpProvider {
    pub fn new() -> io::Result<Self> {
        // UDP is always available
        Ok(Self {
            _initialized: true,
        })
    }
}

impl TransportProvider for UdpProvider {
    fn transport_type(&self) -> TransportType {
        TransportType::Udp
    }
    
    fn is_available(&self) -> bool {
        true // UDP is always available
    }
    
    fn initialize(&mut self) -> io::Result<()> {
        // Already initialized in new()
        Ok(())
    }
    
    fn send(&self, _dest: &[u8], _data: &[u8]) -> io::Result<usize> {
        // TODO: Implement UDP send
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
    
    fn recv(&self, _buf: &mut [u8]) -> io::Result<(usize, Vec<u8>)> {
        // TODO: Implement UDP recv
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
    
    fn rdma_write(&self, _dest: &[u8], data: &[u8], _remote_addr: u64, _remote_key: u64) -> io::Result<()> {
        // RDMA write over UDP: send data inline (DATA_CHUNKS message)
        // TODO: Implement
        let _ = data;
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
    
    fn rdma_read(&self, _dest: &[u8], _buf: &mut [u8], _remote_addr: u64, _remote_key: u64) -> io::Result<usize> {
        // RDMA read over UDP: send READ_REQUEST, wait for READ_RESPONSE
        // TODO: Implement
        Err(io::Error::new(io::ErrorKind::Other, "Not yet implemented"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_provider_selection() {
        // Should always succeed (falls back to UDP if needed)
        let result = select_provider();
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_udp_provider_available() {
        let provider = UdpProvider::new().unwrap();
        assert!(provider.is_available());
        assert_eq!(provider.transport_type(), TransportType::Udp);
    }
    
    #[cfg(feature = "libfabric")]
    #[test]
    fn test_libfabric_provider_detection() {
        // This test may fail if no RDMA hardware is available
        // That's expected and correct behavior
        match LibfabricProvider::new() {
            Ok(provider) => {
                assert_eq!(provider.transport_type(), TransportType::Libfabric);
            }
            Err(_) => {
                // Expected on systems without RDMA hardware
                println!("Libfabric not available (expected on systems without RDMA)");
            }
        }
    }
}
