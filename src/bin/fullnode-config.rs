#[macro_use]
extern crate clap;
extern crate dirs;
extern crate serde_json;
extern crate solana;

use clap::{App, Arg};
use solana::crdt::{get_ip_addr, parse_port_or_addr};
use solana::fullnode::Config;
use solana::nat::get_public_ip_addr;
use solana::signature::read_pkcs8;
use std::io;
use std::net::SocketAddr;

fn main() {
    let matches = App::new("fullnode-config")
        .version(crate_version!())
        .arg(
            Arg::with_name("local")
                .short("l")
                .long("local")
                .takes_value(false)
                .help("detect network address from local machine configuration"),
        )
        .arg(
            Arg::with_name("keypair")
                .short("k")
                .long("keypair")
                .value_name("PATH")
                .takes_value(true)
                .help("/path/to/id.json"),
        )
        .arg(
            Arg::with_name("public")
                .short("p")
                .long("public")
                .takes_value(false)
                .help("detect public network address using public servers"),
        )
        .arg(
            Arg::with_name("bind")
                .short("b")
                .long("bind")
                .value_name("PORT")
                .takes_value(true)
                .help("bind to port or address"),
        )
        .get_matches();

    let bind_addr: SocketAddr = {
        let mut bind_addr = parse_port_or_addr({
            if let Some(b) = matches.value_of("bind") {
                Some(b.to_string())
            } else {
                None
            }
        });
        if matches.is_present("local") {
            let ip = get_ip_addr().unwrap();
            bind_addr.set_ip(ip);
        }
        if matches.is_present("public") {
            let ip = get_public_ip_addr().unwrap();
            bind_addr.set_ip(ip);
        }
        bind_addr
    };

    let mut path = dirs::home_dir().expect("home directory");
    let id_path = if matches.is_present("keypair") {
        matches.value_of("keypair").unwrap()
    } else {
        path.extend(&[".config", "solana", "id.json"]);
        path.to_str().unwrap()
    };
    let pkcs8 = read_pkcs8(id_path).expect("client keypair");

    // we need all the receiving sockets to be bound within the expected
    // port range that we open on aws
    let config = Config::new(&bind_addr, pkcs8);
    let stdout = io::stdout();
    serde_json::to_writer(stdout, &config).expect("serialize");
}
