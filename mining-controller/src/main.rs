use bitcoincore_rpc::{bitcoin::{address::NetworkUnchecked, Address, Network}, Auth, Client, RpcApi};
use std::{env, thread, time::Duration};

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    Client::new(rpc_url, Auth::UserPass(rpc_user.to_string(), rpc_pass.to_string())).unwrap()
}

fn wait_for_rpc(client: &Client, name: &str) {
    loop {
        match client.get_block_count() {
            Ok(_) => return,
            Err(_) => {
                println!("Waiting for {name} RPC...");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

// Poll until the node reports at least `height`, so blocks do not compete
// and stack on each other when mining alternates between nodes.
fn wait_for_height(client: &Client, height: u64) {
    loop {
        match client.get_block_count() {
            Ok(h) if h >= height => return,
            _ => thread::sleep(Duration::from_millis(100)),
        }
    }
}

// Create the wallet and return a wallet-scoped client plus a fresh address.
// A wallet-scoped URL keeps working even if the user loads extra wallets on
// the node later (the generic RPC path breaks with more than one wallet).
// Restart-safe: if the wallet already exists on disk it is loaded instead,
// and if it is already loaded it is used as-is.
fn setup_wallet(rpc_url: &str, rpc_user: &str, rpc_pass: &str, node: &Client, wallet_name: &str) -> (Client, Address) {
    if let Err(create_err) = node.create_wallet(wallet_name, None, None, None, None) {
        match node.load_wallet(wallet_name) {
            Ok(_) => println!("Wallet '{wallet_name}' already exists, loaded it"),
            Err(load_err) if load_err.to_string().contains("already loaded") => {
                println!("Wallet '{wallet_name}' already loaded, reusing it");
            }
            Err(load_err) => {
                panic!("wallet '{wallet_name}': create failed ({create_err}), load failed ({load_err})")
            }
        }
    }
    let wallet = create_client(&format!("{rpc_url}/wallet/{wallet_name}"), rpc_user, rpc_pass);
    let address = wallet.get_new_address(None, None).unwrap();
    let address = address.require_network(Network::Regtest).unwrap();
    (wallet, address)
}

fn main() {
    // Every setting has a default matching docker-compose.yml, so the tool
    // also runs standalone with no environment at all.
    let user_address = env_or("USER_ADDRESS", "bcrt1qtmjqjf4t0mcts4jw9hvm54nl2rhjyeclntf3rr");
    let interval_secs: u64 = env_or("BLOCK_INTERVAL_SECS", "15").parse().expect("BLOCK_INTERVAL_SECS must be a positive integer");

    let rpc_user = env_or("BTC_RPC_USER", "foo");
    let rpc_pass = env_or("BTC_RPC_PASS", "rpcpassword");
    let wallet2_name = env_or("NODE2_WALLET_NAME", "node2");
    let wallet3_name = env_or("NODE3_WALLET_NAME", "node3");

    let node2_url = env_or("NODE2_RPC_URL", "http://btc-simnet-node2:18443");
    let node3_url = env_or("NODE3_RPC_URL", "http://btc-simnet-node3:18443");
    let node2 = create_client(&node2_url, &rpc_user, &rpc_pass);
    let node3 = create_client(&node3_url, &rpc_user, &rpc_pass);

    let user_address: Address<NetworkUnchecked> = user_address.parse().expect("Invalid Bitcoin address");
    let user_address = user_address.require_network(Network::Regtest).unwrap();

    println!("Waiting for nodes to be ready");
    wait_for_rpc(&node2, "node2");
    wait_for_rpc(&node3, "node3");

    // Bootstrap plan: block 1 to node2's wallet, block 2 to node3's wallet,
    // blocks 3 and 4 to the user address, then 100 more blocks. At height
    // 104 all four coinbases are mature: the user has the 2x50 BTC, and each
    // miner wallet has one mature reward to fund the spammer from the start.
    let (_wallet2, addr2) = setup_wallet(&node2_url, &rpc_user, &rpc_pass, &node2, &wallet2_name);
    let (_wallet3, addr3) = setup_wallet(&node3_url, &rpc_user, &rpc_pass, &node3, &wallet3_name);

    // Restart-safe: if the chain is already past the bootstrap height the
    // funding already happened, re-running it would fund the user again.
    let mut height = node2.get_block_count().unwrap();
    if height >= 104 {
        println!("Chain already bootstrapped (height {height}), skipping the funding sequence");
    } else {
        println!("Node 2 => Mining block 1 to its own wallet address {addr2}");
        let _ = node2.generate_to_address(1, &addr2).unwrap();
        height = node2.get_block_count().unwrap();
        println!("Waiting for network sync, so blocks do not compete and stack on each other");
        wait_for_height(&node3, height);

        println!("Node 3 => Mining block 2 to its own wallet address {addr3}");
        let _ = node3.generate_to_address(1, &addr3).unwrap();
        height = node3.get_block_count().unwrap();
        wait_for_height(&node2, height);

        println!("Funding user address {user_address} with blocks 3 and 4");
        println!("Node 2 => Mining a block to address {user_address}");
        let _ = node2.generate_to_address(1, &user_address).unwrap();
        height = node2.get_block_count().unwrap();
        wait_for_height(&node3, height);

        println!("Node 3 => Mining a block to address {user_address}");
        let _ = node3.generate_to_address(1, &user_address).unwrap();
        height = node3.get_block_count().unwrap();
        wait_for_height(&node2, height);
        println!("New block height: {height}");

        // 100 more blocks so blocks 1-4 mature (block 4 matures at height 104)
        println!("Node 2 => Mining 50 blocks to address {addr2}");
        node2.generate_to_address(50, &addr2).unwrap();
        height = node2.get_block_count().unwrap();
        println!("Waiting network to sync");
        wait_for_height(&node3, height);
        println!("New block height: {height}");

        println!("Node 3 => Mining 50 blocks to address {addr3}");
        node3.generate_to_address(50, &addr3).unwrap();
        height = node3.get_block_count().unwrap();
        println!("Waiting network to sync");
        wait_for_height(&node2, height);
        println!("New block height: {height}");
    }

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
            height = node2.get_block_count().unwrap();
            println!("Node 2 => Mined 1 block [{height}] to address {addr2}");
            wait_for_height(&node3, height);
        } else {
            let _ = node3.generate_to_address(1, &addr3).unwrap();
            height = node3.get_block_count().unwrap();
            println!("Node 3 => Mined 1 block [{height}] to address {addr3}");
            wait_for_height(&node2, height);
        }

        toggle = !toggle;

        let elapsed = start_time.elapsed();
        if elapsed < Duration::from_secs(interval_secs) {
            thread::sleep(Duration::from_secs(interval_secs) - elapsed);
        }
    }
}
