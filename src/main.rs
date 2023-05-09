//! Internet monitor
//!
//! This application will automatically find the first hop outside of your network, and monitor the
//! latency to that network device. This will let you see how stable your internet connection is.
//!
//! The internet monitor outputs Prometheus metrics on port 9000. The README.md includes
//! instructions for how to set up both Prometheus to scrape and store the metrics and Grafana to
//! visualize the results.
//!
//! Much of this is based on the ping and trace examples from Kentik's
//! [netdiag](https://github.com/kentik/netdiag) crate.
//!
//!    Usage: internet_quality_monitor [OPTIONS]
//!    
//!    Optional arguments:
//!      -h, --help
//!      -p, --proto PROTO    Protocol to use to find first public IP: ICMP, UDP, or TCP (default: ICMP)
//!      -P, --probe-port PROBE-PORT
//!                           Port the TCP or UDP trace probe will connect to. Required for TCP or UDP
//!      -c, --count COUNT    Retries while tracing to first public IP (default: 4)
//!      -d, --delay DELAY    Time between monitoring pings (default: 500)
//!      -l, --limit LIMIT    Max network hops before giving up finding a public IP (default: 30)
//!      -e, --expiry EXPIRY  Max time to wait for a network reply (default: 250)
use std::collections::HashMap;
use std::net::{IpAddr,Ipv4Addr};
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::{pin_mut, StreamExt};
use gumdrop::Options;
#[macro_use]
extern crate log;
use metrics::{
    describe_counter, describe_histogram, histogram, increment_counter, register_counter,
    register_histogram, Unit,
};
use metrics_exporter_prometheus::PrometheusBuilder;
use netdiag::{trace::Node, Bind, Ping, Pinger, Protocol, Tracer};
use tokio::net::lookup_host;
use tokio::time::sleep;

const NETWORK_ERROR_DELAY: u64 = 10; // seconds before attempting to restart monitoring

#[derive(Debug, Options)]
pub struct Args {
    #[options()]
    help: bool,
    #[options(
        default = "ICMP",
        help = "Protocol to use to find first public IP: ICMP, UDP, or TCP"
    )]
    proto: String,
    #[options(help = "Port the TCP or UDP trace probe will connect to. Required for TCP or UDP")]
    probe_port: u16,
    #[options(default = "4", help = "Retries while tracing to first public IP")]
    count: usize,
    #[options(default = "500", help = "Time between monitoring pings")]
    delay: u64,
    #[options(
        default = "30",
        help = "Max network hops before giving up finding a public IP"
    )]
    limit: u8,
    #[options(default = "250", help = "Max time to wait for a network reply")]
    expiry: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse_args_default_or_exit();
    let Args {
        proto,
        probe_port,
        count,
        delay,
        limit,
        expiry,
        ..
    } = args;
    let ping_interval_delay = Duration::from_millis(delay);
    let network_error_delay = Duration::from_secs(NETWORK_ERROR_DELAY);

    // Logging
    env_logger::init();

    // Metrics
    let builder = PrometheusBuilder::new();
    builder
        .install()
        .expect("failed to install recorder/exporter");
    info!("Serving up metrics on port 9000");

    register_metrics();

    info!("Path to the closest public IP:");
    let public_ip = trace_to_public_ip(proto, probe_port, count, limit, expiry).await?;
    info!("Found first public IP: {}", public_ip);

    info!("starting ping monitoring");
    loop {
        let pinger = Pinger::new(&Bind::default()).await?;
        // Note the usize::MAX here - there's no pinger that just keeps pinging. So, we set it to
        // an unreasonably high number, this will never stop on a 64 bit machine. However, we may
        // want to contribute back a change to make this an Option(usize) to choose whether to have
        // a count or not. Maybe open a ticket first to propose it?
        let ping = Ping {
            addr: public_ip,
            count: std::usize::MAX,
            expiry: Duration::from_millis(expiry),
        };

        record_ping_results(pinger, ping, ping_interval_delay).await?;
        // Since the stream terminates on a network error, we sleep then restart.
        sleep(network_error_delay).await;
        debug!("restarting ping monitoring");
    }
}

/// Register the metrics we'll be emitting.
fn register_metrics() {
    register_counter!("ping_count");
    describe_counter!("ping_count", Unit::Count, "How many pings have been sent.");
    register_counter!("ping_failed_count");
    describe_counter!(
        "ping_failed_count",
        Unit::Count,
        "How many pings have timed out."
    );
    register_histogram!("ping_seconds");
    describe_histogram!(
        "ping_seconds",
        Unit::Seconds,
        "Ping latency to first public network hop in seconds."
    );
}

/// Ping a target and record latency and failures on those pings.  This is the heart of this app.
async fn record_ping_results(
    pinger: Pinger,
    ping: Ping,
    ping_interval_delay: Duration,
) -> Result<()> {
    let stream = pinger.ping(&ping).enumerate();
    pin_mut!(stream);

    // Note that this stream terminates when there's a network error.
    while let Some((n, item_res)) = stream.next().await {
        match item_res {
            // Ping result.
            Ok(item) => match item {
                Some(d) => {
                    increment_counter!("ping_count");
                    histogram!("ping_seconds", d);
                    debug!("seq {} RTT {:0.2?} ", n, d)
                }
                None => {
                    increment_counter!("ping_failed_count");
                    debug!("seq {} timeout", n)
                }
            },
            // Error, usually a network error - network drops for example.
            Err(_err) => {
                increment_counter!("ping_failed_count");
                debug!("seq {} error (network unreachable?)", n)
            }
        }
        sleep(ping_interval_delay).await;
    }

    Ok(())
}

/// Find the first public IP address as traced towards `1.1.1.1`.
async fn trace_to_public_ip(
    proto: String,
    probe_port: u16,
    count: usize,
    limit: u8,
    expiry: u64,
) -> Result<IpAddr> {
    let proto = match proto.to_uppercase().as_str() {
        "ICMP" => Protocol::ICMP,
        "TCP" if probe_port > 0 => Protocol::TCP(probe_port),
        "UDP" if probe_port > 0 => Protocol::UDP(probe_port),
        _ => Protocol::default(),
    };

    let expiry = Duration::from_millis(expiry);

    // We only use this address to trace towards, we likely won't reach it unless we're at
    // Cloudflare itself.
    let addr = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
    let bind = Bind::default();

    let tracer = Tracer::new(&bind).await?;
    let source = tracer.reserve(proto, addr).await?;

    let mut done = false;
    let mut ttl = 1;
    let mut probe = source.probe()?;

    // If we find a public IP - return it immediately
    while !done && ttl <= limit {
        let mut nodes = HashMap::<IpAddr, Vec<String>>::new();

        let stream = tracer.probe(&mut probe, ttl, expiry);
        let stream = stream.take(count);
        pin_mut!(stream);

        while let Some(Ok(node)) = stream.next().await {
            if let Node::Node(_, ip, rtt, last) = node {
                let rtt = format!("{:>0.2?}", rtt);
                nodes.entry(ip).or_default().push(rtt);
                if let Some(public_ip) = is_public_ipv4_address(ip) {
                    return Ok(public_ip);
                }
                done = last || ip == addr;
            }
        }

        // In the future we may want to record and return the route found rather than printing in
        // here. Then we can print this in main , which is nicer.
        print_found_nodes(&nodes, ttl, count);

        ttl += 1;
    }

    Err(anyhow!("No public IP address could be found to ping."))
}

/// Test to see if this is an ipv4 address, and if that address is publically routable.
fn is_public_ipv4_address(addr: IpAddr) -> Option<IpAddr> {
    let ip_v4 = match addr {
        IpAddr::V4(ip4) => ip4,
        IpAddr::V6(_ip6) => return None,
    };

    if ip_v4.is_private() {
        return None;
    } else {
        return Some(addr);
    };
}

/// Resolve the IP Address of a requested host.
/// Not yet required. May add an option to select host to trace towards.
#[allow(dead_code)]
async fn lookup_address(host: String) -> Result<IpAddr> {
    let addr = format!("{}:0", host);
    let addr = lookup_host(&addr)
        .await?
        .next()
        .ok_or_else(|| anyhow!("invalid target"))?
        .ip();

    // We only have is_private with ipv4 addresses, so we discard ipv6 addresses for now. This will
    // change in later rust versions, it's nightly-only for ipv6 now.
    let _ = match addr {
        IpAddr::V4(ip4) => ip4,
        IpAddr::V6(ip6) => return Err(anyhow!("{} is an ipv6 address - can't continue.", ip6)),
    };

    Ok(addr)
}

/// Print out the currently found path to an address.
fn print_found_nodes(nodes: &HashMap<IpAddr, Vec<String>>, ttl: u8, probes: usize) {
    let mut count = 0;

    let mut output = nodes
        .iter()
        .map(|(node, rtt)| {
            count += rtt.len();
            let node = node.to_string();
            let rtt = rtt.join(", ");
            (node, rtt)
        })
        .collect::<Vec<_>>();

    if count < probes {
        let node = "* ".repeat(probes - count);
        let rtt = String::new();
        output.push((node, rtt));
    }

    for (n, (node, rtt)) in output.iter().enumerate() {
        match n {
            0 => info!("[{:>3}] {:32} {}", ttl, node, rtt),
            _ => info!("[{:>3}] {:32} {}", "", node, rtt),
        }
    }
}
