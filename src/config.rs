use clap::{App, Arg};
use std::net::SocketAddr;
use stderrlog;

use daemon::Network;

#[derive(Debug)]
pub struct Config {
    pub log: stderrlog::StdErrLog,
    pub network_type: Network,       // bitcoind JSONRPC endpoint
    pub db_path: String,             // RocksDB directory path
    pub rpc_addr: SocketAddr,        // for serving Electrum clients
    pub monitoring_addr: SocketAddr, // for Prometheus monitoring
}

impl Config {
    pub fn from_args() -> Config {
        let m = App::new("Electrum Rust Server")
            .version(crate_version!())
            .arg(
                Arg::with_name("verbosity")
                    .short("v")
                    .multiple(true)
                    .help("Increase logging verbosity"),
            )
            .arg(
                Arg::with_name("timestamp")
                    .long("timestamp")
                    .help("Prepend log lines with a timestamp"),
            )
            .arg(
                Arg::with_name("db_dir")
                    .short("d")
                    .long("db-dir")
                    .help("Directory to store index database")
                    .takes_value(true),
            )
            .arg(
                Arg::with_name("testnet")
                    .long("testnet")
                    .help("Connect to a testnet bitcoind instance"),
            )
            .get_matches();
        let network_type = match m.is_present("testnet") {
            false => Network::Mainnet,
            true => Network::Testnet,
        };
        let db_dir = m.value_of("db_dir").unwrap_or("./db");
        let mut log = stderrlog::new();
        log.verbosity(m.occurrences_of("verbosity") as usize);
        log.timestamp(if m.is_present("timestamp") {
            stderrlog::Timestamp::Microsecond
        } else {
            stderrlog::Timestamp::Off
        });
        log.init().expect("logging initialization failed");
        Config {
            log,
            network_type,
            db_path: match network_type {
                Network::Mainnet => format!("{}/mainnet", db_dir),
                Network::Testnet => format!("{}/testnet", db_dir),
            },
            rpc_addr: match network_type {
                Network::Mainnet => "127.0.0.1:50001",
                Network::Testnet => "127.0.0.1:60001",
            }.parse()
                .unwrap(),
            monitoring_addr: "127.0.0.1:42024".parse().unwrap(),
        }
    }
}
