use bitcoincore_rpc::{bitcoin::{address::NetworkUnchecked, Address}, Auth, Client, RpcApi};
use std::{env, thread, time::Duration};

fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    Client::new(rpc_url, Auth::UserPass(rpc_user.to_string(), rpc_pass.to_string())).unwrap()
}

fn setup_wallet(client: &Client, wallet_name: &str) -> Address {
    let _ = client.create_wallet(wallet_name, None, None, None, None).unwrap();
    let address = client.get_new_address(None,None).unwrap();
    address.require_network(bitcoincore_rpc::bitcoin::Network::Regtest).unwrap()
}

fn main() {
    let user_address = env::var("USER_ADDRESS").expect("USER_ADDRESS missing");
    let interval_secs: u64 = env::var("BLOCK_INTERVAL_SECS").expect("BLOCK_INTERVAL_SECS missing").parse().unwrap();

    // let node1 = create_client("http://btc-simnet-node1:18443", "bituser", "bitpass");
    let node2 = create_client("http://btc-simnet-node2:18443", "bituser", "bitpass");
    let node3 = create_client("http://btc-simnet-node3:18443", "bituser", "bitpass");

    // Initial funding blocks
    let user_address: Address<NetworkUnchecked> = user_address.parse().expect("Invalid Bitcoin address");
    let user_address = user_address.require_network(bitcoincore_rpc::bitcoin::Network::Regtest).unwrap();

    println!("Waiting for nodes to be ready");
    thread::sleep(Duration::from_millis(200));

    // Give two block rewars (one from each miner) to the user address
    println!("Funding user address {user_address} with 2 blocks");
    println!("Node 2 => Mining a block to address {user_address}");
    let _ = node2.generate_to_address(1, &user_address).unwrap();
    thread::sleep(Duration::from_millis(100));
    println!("Initial block height: {}", node2.get_block_count().unwrap());

    println!("Waiting for Network sync, so blocks do not compete and stack each other");
    thread::sleep(Duration::from_millis(300));

    println!("Node 3 => Mining a block to address {user_address}");
    let _ = node3.generate_to_address(1, &user_address).unwrap();
    thread::sleep(Duration::from_millis(100));
    println!("New block height: {}", node2.get_block_count().unwrap());

    // Setup wallets and mine 50 blocks alternating
    let addr2 = setup_wallet(&node2, "node2");
    let addr3 = setup_wallet(&node3, "node3");

    println!("Node 2 => Mining 50 blocks to address {addr2}");
    node2.generate_to_address(50, &addr2).unwrap();
    println!("Witing network to sync");
    thread::sleep(Duration::from_secs(1));
    println!("New block height: {}", node2.get_block_count().unwrap());

    println!("Node 3 => Mining 50 blocks to address {addr3}");
    node3.generate_to_address(50, &addr3).unwrap();
    println!("Witing network to sync");
    thread::sleep(Duration::from_secs(1));
    println!("New block height: {}", node2.get_block_count().unwrap());

    println!("\nActual block height: {}", node2.get_block_count().unwrap());

    println!("\n//////////////////////////////////////////////////////////////////\n");
    println!("Funds in address {user_address} are mature and ready to spend.");
    println!("To list UTXOs, use scantxoutset or list_unspent from bdk crate");
    println!("\n//////////////////////////////////////////////////////////////////\n");

    // Continuous mining loop
    let mut toggle = true;
    loop {
        let start_time = std::time::Instant::now();

        if toggle {
            let _ = node2.generate_to_address(1, &addr2).unwrap();
            println!("Node 2 => Mined 1 block [{}] to address {addr2}", node2.get_block_count().unwrap());
        } else {
            let _ = node3.generate_to_address(1, &addr3).unwrap();
            println!("Node 3 => Mined 1 block [{}] to address {addr3}", node3.get_block_count().unwrap());
        }

        toggle = !toggle;

        let elapsed = start_time.elapsed();
        if elapsed < Duration::from_secs(interval_secs) {
            thread::sleep(Duration::from_secs(interval_secs) - elapsed);
        }
    }
}
