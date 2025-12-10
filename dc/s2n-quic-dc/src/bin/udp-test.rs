// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! UDP busy poll testing tool for s2n-quic-dc
//!
//! This tool provides a simple way to test UDP socket performance using busy polling.
//! It can run as either a server (echoing packets back) or a client (sending bursts).

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const SO_BUSY_POLL: libc::c_int = 46;
const SO_PREFER_BUSY_POLL: libc::c_int = 69;

#[derive(Parser)]
#[command(name = "udp-test")]
#[command(about = "UDP busy poll testing tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as a server that echoes packets back
    Server {
        /// IP address to bind to
        #[arg(long, default_value = "0.0.0.0")]
        ip: String,

        /// Port to listen on
        #[arg(long, default_value = "8080")]
        port: u16,

        /// Enable busy polling (microseconds to poll)
        #[arg(long, default_value = "50")]
        busy_poll_us: u32,
    },
    /// Run as a client that sends packet bursts
    Client {
        /// Server IP address to connect to
        #[arg(long)]
        server_ip: String,

        /// Server port to connect to
        #[arg(long)]
        server_port: u16,

        /// Number of packets to send in each burst
        #[arg(long, default_value = "10")]
        burst_size: usize,

        /// Delay between bursts in milliseconds
        #[arg(long, default_value = "100")]
        burst_delay_ms: u64,

        /// Number of bursts to send (0 = infinite)
        #[arg(long, default_value = "100")]
        num_bursts: usize,

        /// Payload size in bytes
        #[arg(long, default_value = "1024")]
        payload_size: usize,

        /// Enable busy polling (microseconds to poll)
        #[arg(long, default_value = "50")]
        busy_poll_us: u32,
    },
}

/// Enable busy polling on a UDP socket
fn enable_busy_poll(socket: &UdpSocket, busy_poll_us: u32) -> std::io::Result<()> {
    let fd = socket.as_raw_fd();

    // Set SO_BUSY_POLL
    let res = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_BUSY_POLL,
            &busy_poll_us as *const u32 as *const libc::c_void,
            std::mem::size_of::<u32>() as libc::socklen_t,
        )
    };

    if res != 0 {
        eprintln!("Warning: Failed to set SO_BUSY_POLL: {}", std::io::Error::last_os_error());
    }

    // Set SO_PREFER_BUSY_POLL
    let prefer: libc::c_int = 1;
    let res = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_PREFER_BUSY_POLL,
            &prefer as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };

    if res != 0 {
        eprintln!("Warning: Failed to set SO_PREFER_BUSY_POLL: {}", std::io::Error::last_os_error());
    }

    Ok(())
}

async fn run_server(ip: String, port: u16, busy_poll_us: u32) -> std::io::Result<()> {
    let addr: SocketAddr = format!("{}:{}", ip, port).parse().unwrap();
    let socket = UdpSocket::bind(addr).await?;
    
    // Enable busy polling
    enable_busy_poll(&socket, busy_poll_us)?;
    
    println!("UDP server listening on {}", socket.local_addr()?);
    println!("Busy polling enabled with {}µs", busy_poll_us);
    
    let mut buf = vec![0u8; 65536];
    let mut total_packets = 0u64;
    let mut total_bytes = 0u64;
    let start = Instant::now();
    let mut last_report = Instant::now();
    
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, peer_addr)) => {
                total_packets += 1;
                total_bytes += len as u64;
                
                // Echo the packet back
                if let Err(e) = socket.send_to(&buf[..len], peer_addr).await {
                    eprintln!("Error sending packet: {}", e);
                }
                
                // Report statistics every second
                if last_report.elapsed() >= Duration::from_secs(1) {
                    let elapsed = start.elapsed().as_secs_f64();
                    let pps = total_packets as f64 / elapsed;
                    let mbps = (total_bytes as f64 * 8.0) / (elapsed * 1_000_000.0);
                    println!(
                        "Stats: {} packets, {} MB, {:.2} pps, {:.2} Mbps",
                        total_packets,
                        total_bytes / 1_000_000,
                        pps,
                        mbps
                    );
                    last_report = Instant::now();
                }
            }
            Err(e) => {
                eprintln!("Error receiving packet: {}", e);
            }
        }
    }
}

async fn run_client(
    server_ip: String,
    server_port: u16,
    burst_size: usize,
    burst_delay_ms: u64,
    num_bursts: usize,
    payload_size: usize,
    busy_poll_us: u32,
) -> std::io::Result<()> {
    let server_addr: SocketAddr = format!("{}:{}", server_ip, server_port).parse().unwrap();
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    
    // Enable busy polling
    enable_busy_poll(&socket, busy_poll_us)?;
    
    println!("UDP client bound to {}", socket.local_addr()?);
    println!("Busy polling enabled with {}µs", busy_poll_us);
    println!("Connecting to {}", server_addr);
    println!("Burst size: {} packets, Delay: {}ms, Payload: {} bytes", 
             burst_size, burst_delay_ms, payload_size);
    
    // Prepare payload with sequence numbers
    let mut payload = vec![0u8; payload_size];
    
    // Statistics tracking
    let mut total_sent = 0u64;
    let mut total_received = 0u64;
    let mut total_rtt_us = 0u64;
    let mut min_rtt_us = u64::MAX;
    let mut max_rtt_us = 0u64;
    
    let burst_delay = Duration::from_millis(burst_delay_ms);
    
    for burst_num in 0..num_bursts {
        let mut burst_sent = 0;
        let mut burst_received = 0;
        let burst_start = Instant::now();
        let send_start = Instant::now();
        
        // Send burst
        for seq in 0..burst_size {
            let global_seq = burst_num * burst_size + seq;
            
            // Encode sequence number in first 8 bytes
            payload[..8].copy_from_slice(&(global_seq as u64).to_be_bytes());
            
            match socket.send_to(&payload, server_addr).await {
                Ok(_) => {
                    burst_sent += 1;
                    total_sent += 1;
                }
                Err(e) => {
                    eprintln!("Error sending packet {}: {}", global_seq, e);
                }
            }
        }
        
        // Receive responses for this burst (with timeout)
        let mut recv_buf = vec![0u8; 65536];
        let timeout = Duration::from_millis(1000); // 1 second timeout
        let recv_start = Instant::now();
        
        while burst_received < burst_sent && recv_start.elapsed() < timeout {
            // Try to receive with a short timeout
            match tokio::time::timeout(Duration::from_millis(10), socket.recv_from(&mut recv_buf)).await {
                Ok(Ok((len, _))) => {
                    if len >= 8 {
                        burst_received += 1;
                        total_received += 1;
                        
                        // Calculate approximate RTT for this burst
                        // (from first send to receive time)
                        let rtt = send_start.elapsed().as_micros() as u64;
                        total_rtt_us += rtt;
                        min_rtt_us = min_rtt_us.min(rtt);
                        max_rtt_us = max_rtt_us.max(rtt);
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("Error receiving: {}", e);
                    break;
                }
                Err(_) => {
                    // Timeout on this receive, continue
                    continue;
                }
            }
        }
        
        let burst_duration = burst_start.elapsed();
        let packet_loss = if burst_sent > 0 {
            ((burst_sent - burst_received) as f64 / burst_sent as f64) * 100.0
        } else {
            0.0
        };
        
        println!(
            "Burst {}: sent={}, received={}, loss={:.2}%, duration={:?}",
            burst_num, burst_sent, burst_received, packet_loss, burst_duration
        );
        
        // Wait before next burst
        if burst_num + 1 < num_bursts {
            tokio::time::sleep(burst_delay).await;
        }
    }
    
    // Final statistics
    println!("\n=== Final Statistics ===");
    println!("Total sent: {}", total_sent);
    println!("Total received: {}", total_received);
    let total_loss = if total_sent > 0 {
        ((total_sent - total_received) as f64 / total_sent as f64) * 100.0
    } else {
        0.0
    };
    println!("Packet loss: {:.2}%", total_loss);
    
    if total_received > 0 {
        let avg_rtt_us = total_rtt_us / total_received;
        println!("RTT: min={}µs, max={}µs, avg={}µs", min_rtt_us, max_rtt_us, avg_rtt_us);
    }
    
    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Server { ip, port, busy_poll_us } => {
            run_server(ip, port, busy_poll_us).await
        }
        Commands::Client {
            server_ip,
            server_port,
            burst_size,
            burst_delay_ms,
            num_bursts,
            payload_size,
            busy_poll_us,
        } => {
            run_client(
                server_ip,
                server_port,
                burst_size,
                burst_delay_ms,
                num_bursts,
                payload_size,
                busy_poll_us,
            )
            .await
        }
    }
}
