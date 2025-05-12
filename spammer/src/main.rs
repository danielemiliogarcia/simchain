use bitcoincore_rpc::{bitcoin::{Address, Amount}, Auth, Client, RpcApi};
use std::{env, thread, time::Duration};

fn create_client(rpc_url: &str, rpc_user: &str, rpc_pass: &str) -> Client {
    Client::new(rpc_url, Auth::UserPass(rpc_user.to_string(), rpc_pass.to_string())).unwrap()
}

fn get_new_wallet_address(client: &Client) -> Address {
    let address = client.get_new_address(None,None).unwrap();
    address.require_network(bitcoincore_rpc::bitcoin::Network::Regtest).unwrap()
}

fn send_spam_tx(from: &Client, to_address: &Address, count: u64) {
    for _ in 0..count {
        let _ = from.send_to_address(&to_address, Amount::from_sat(500), None, None, None, None, None, None);
    }
}

fn main() {
    let enable_spam = env::var("ENABLE_SPAM").unwrap_or_else(|_| "false".to_string()) == "true";
    let spam_per_miner_per_block: u64 = env::var("SPAM_PER_MINER_PER_BLOCK").expect("SPAM_PER_MINER_PER_BLOCK missing").parse().unwrap();

    println!("Waiting for nodes to be ready");
    thread::sleep(Duration::from_millis(200));

    let node1 = create_client("http://btc-simnet-node1:18443", "bituser", "bitpass");
    let node2 = create_client("http://btc-simnet-node2:18443", "bituser", "bitpass");
    let node3 = create_client("http://btc-simnet-node3:18443", "bituser", "bitpass");

    println!("Waiting the chain to reach 102 blocks");
    while node1.get_block_count().unwrap() < 102 {
        thread::sleep(Duration::from_millis(200));
    }

    let addr2 = get_new_wallet_address(&node2);
    let addr3 = get_new_wallet_address(&node3);

    //In a loop, If new block detected, spam transactions
    let mut spammed_at_block_height = 0;
    loop {
        let current_block_height = node1.get_block_count().unwrap();
        if enable_spam && current_block_height > spammed_at_block_height {
            spammed_at_block_height = current_block_height;
            // spam transactions cross address
            println!("Node 2 => Spamming {spam_per_miner_per_block} transactions to address {addr3}");
            send_spam_tx(&node2, &addr3, spam_per_miner_per_block);
            println!("Node 3 => Spamming {spam_per_miner_per_block} transactions to address {addr2}");
            send_spam_tx(&node3, &addr2, spam_per_miner_per_block);
        }
        thread::sleep(Duration::from_millis(200));
    }
}
