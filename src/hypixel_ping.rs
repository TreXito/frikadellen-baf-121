//! Minecraft Server List Ping (SLP) latency measurement to `mc.hypixel.net`.
//!
//! This measures the **real** round-trip time to Hypixel's backend through the
//! Cloudflare Spectrum proxy (the same number the in-game tab list shows), which
//! is what actually bounds how fast `/viewauction` → window can complete. An
//! ICMP `ping` only reaches Cloudflare's edge, so it reports a much smaller
//! number than the game connection actually experiences.
//!
//! The measured average is published to [`LATEST_PING_MS`] so the bed-timing
//! loop can lead its clicks by the connection latency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const HYPIXEL_HOST: &str = "mc.hypixel.net";
pub const HYPIXEL_PORT: u16 = 25565;

/// Latest measured average ping to Hypixel, in milliseconds. `0` means "not yet
/// measured". Updated by [`measure`] / the background refresher and read by the
/// bed-timing loop.
pub static LATEST_PING_MS: AtomicU64 = AtomicU64::new(0);

/// Returns the latest measured ping in ms, or `None` if never measured.
pub fn latest_ping_ms() -> Option<u64> {
    match LATEST_PING_MS.load(Ordering::Relaxed) {
        0 => None,
        v => Some(v),
    }
}

fn write_varint(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

/// Prefix a packet payload with its length as a varint.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    write_varint(&mut out, payload.len() as u32);
    out.extend_from_slice(payload);
    out
}

async fn read_varint<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<i32> {
    let mut num: u32 = 0;
    let mut shift = 0u32;
    loop {
        let byte = r.read_u8().await?;
        num |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 35 {
            return Err(anyhow!("varint too long"));
        }
    }
    Ok(num as i32)
}

/// Perform one SLP handshake + status request + ping/pong and return the
/// ping→pong round-trip time. Connection/handshake setup is excluded — only the
/// dedicated ping packet is timed, isolating true RTT.
pub async fn ping_once(host: &str, port: u16, timeout: Duration) -> Result<Duration> {
    let work = async {
        let mut stream = TcpStream::connect((host, port)).await?;
        stream.set_nodelay(true).ok();

        // Handshake → next_state = 1 (status).
        let mut hs = Vec::new();
        write_varint(&mut hs, 0x00); // packet id
        write_varint(&mut hs, 767); // protocol version (any recent value)
        write_string(&mut hs, host);
        hs.extend_from_slice(&port.to_be_bytes());
        write_varint(&mut hs, 1); // next state
        stream.write_all(&frame(&hs)).await?;

        // Status request (empty body).
        let mut sr = Vec::new();
        write_varint(&mut sr, 0x00);
        stream.write_all(&frame(&sr)).await?;

        // Status response: length, then packet id + JSON. Read and discard.
        let resp_len = read_varint(&mut stream).await?;
        if resp_len < 0 || resp_len > 4_000_000 {
            return Err(anyhow!("bad status length {resp_len}"));
        }
        let mut resp = vec![0u8; resp_len as usize];
        stream.read_exact(&mut resp).await?;

        // Ping with a timestamp token; time only the round-trip.
        let token: i64 = 0x0BAF_C0DE_1234_5678;
        let mut pi = Vec::new();
        write_varint(&mut pi, 0x01);
        pi.extend_from_slice(&token.to_be_bytes());
        let t0 = Instant::now();
        stream.write_all(&frame(&pi)).await?;

        // Pong: length, packet id (0x01), i64 token.
        let _len = read_varint(&mut stream).await?;
        let pid = read_varint(&mut stream).await?;
        let mut payload = [0u8; 8];
        stream.read_exact(&mut payload).await?;
        let rtt = t0.elapsed();
        if pid != 0x01 {
            return Err(anyhow!("unexpected pong packet id {pid}"));
        }
        Ok::<Duration, anyhow::Error>(rtt)
    };

    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| anyhow!("ping timed out"))?
}

/// Aggregate stats from a burst of pings.
pub struct PingStats {
    pub avg: Duration,
    pub min: Duration,
    pub max: Duration,
    pub count: usize,
}

/// Measure `count` pings spaced `gap` apart, update [`LATEST_PING_MS`] with the
/// average, and return the stats. Errors only if **every** ping failed.
pub async fn measure(count: usize, gap: Duration) -> Result<PingStats> {
    let mut samples: Vec<Duration> = Vec::with_capacity(count);
    for i in 0..count {
        if i > 0 {
            tokio::time::sleep(gap).await;
        }
        if let Ok(rtt) = ping_once(HYPIXEL_HOST, HYPIXEL_PORT, Duration::from_secs(5)).await {
            samples.push(rtt);
        }
    }
    if samples.is_empty() {
        return Err(anyhow!("all pings failed"));
    }
    let sum: Duration = samples.iter().copied().sum();
    let avg = sum / samples.len() as u32;
    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    LATEST_PING_MS.store(avg.as_millis() as u64, Ordering::Relaxed);
    Ok(PingStats {
        avg,
        min,
        max,
        count: samples.len(),
    })
}

/// Spawn a background task that refreshes [`LATEST_PING_MS`] every `interval`
/// (4 pings each pass) so bed timing always has a recent figure.
pub fn spawn_background_refresher(interval: Duration) {
    tokio::spawn(async move {
        // Small initial delay so startup isn't competing for the network.
        tokio::time::sleep(Duration::from_secs(10)).await;
        loop {
            let _ = measure(4, Duration::from_millis(200)).await;
            tokio::time::sleep(interval).await;
        }
    });
}
