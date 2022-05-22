// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

#![recursion_limit = "256"]
// Coding conventions
#![deny(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    unused_mut,
    unused_imports,
    dead_code,
    missing_docs
)]

//! Main executable for farcasterd: farcaster node management microservice.

#[macro_use]
extern crate log;

use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1::rand::thread_rng;
use bitcoin::secp256k1::rand::RngCore;

use clap::Parser;

use farcaster_node::Error;
use farcaster_node::ServiceConfig;
use farcaster_node::{
    config::parse_config,
    farcasterd::{self, Opts},
    rpc::request::Token,
};

fn main() -> Result<(), Error> {
    let mut opts = Opts::parse();
    trace!("Command-line arguments: {:?}", &opts);
    opts.process();
    trace!("Processed arguments: {:?}", &opts);

    let service_config: ServiceConfig = opts.shared.clone().into();
    trace!("Daemon configuration: {:#?}", &service_config);
    debug!("MSG RPC socket {}", &service_config.msg_endpoint);
    debug!("CTL RPC socket {}", &service_config.ctl_endpoint);

    debug!("Config file path: {}", &opts.config);
    let config = parse_config(&opts.config)?;
    debug!("Configuration: {:#?}", &config);

    // Generate runtime token
    let mut dest = [0u8; 16];
    thread_rng().fill_bytes(&mut dest);
    let token = Token(dest.to_hex());

    debug!("Starting runtime ...");
    farcasterd::run(service_config, config, opts, token).expect("Error running farcasterd runtime");

    unreachable!()
}
