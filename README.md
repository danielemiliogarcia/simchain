# BTC Simchain

This uses the latest bitcoincore version at the time of writing v29.0, but dockerfile is tied to `x86_64-linux-gnu` platform to build the image, modify this for your architecture or use `ruimarinho/bitcoin-core` if you don't mind using older versions.


## Intro

The objective of this project is to be a tool that helps the user to write blockchain regtest test is such a way later that code needs only minimal modifications, (or only configuration in the best case) to switch to testnet or mainnet.

This project create a regtest bitcoin simulation network that consist on 3 node well connected among them. It relays on 5 containers.

Node 1 `btc-simnet-node1` is exposed to the host network (18443), its role is to simulate a production endpoint [-txindex, -disablewallet=1] wallet is disabled like in most 3rd party production networks, so the user must manage his own keys, obtains the outpoint of his addresses UTxOs and send rawTransactions. Also to simulate a production endpoint, it is not a miner
[See the `docker-compose.yml` file for port numbers and more details](./docker-compose.yml)

Node 2 `btc-simnet-node2` is exposed to the host network (28443), its role is to simulate an owned node with internal wallet enabled, this us useful to simulate such situations or to stack for example an ordinals wallet or any layer2 node on top like Lightning networks nodes that need internal wallet management. (be aware that as it is, it has not ZMQ enabled)
This node is a miner!
[See the `docker-compose.yml` file for port numbers and more details](./docker-compose.yml)

Node 3 `btc-simnet-node3` is NOT exposed to the host network, its role is to simulate a node connected via p2p but inaccessible to the user.
This node is a miner!

Mining controller `btc-simnet-mining-controller` this container runs a simple rust program that will fund he user address using 2 next to genesis coinbase transactions and providing maturity to them. The user address will be funded with 2 UTxOs of 50 BTC each allocating a total of 100 BTC. Once this task is achieved it will ask each miner to mine 1 block in a round robing manner every an amount of time setted by the user
[See the `.env.example` file for settings details](./.env.example)

Mining controller `btc-simnet-mining-controller` this container runs a simple rust program that will fund he user address using 2 next to genesis coinbase transactions and providing maturity to them. The user address will be funded with 2 UTxOs of 50 BTC each allocating a total of 100 BTC. Once this task is achieved it will ask each miner to mine 1 block in a round robing manner every an amount of time setted by the user
[See the `.env.example` file for settings details](./.env.example)
This container could be stopped after funding if the user want to control the mining manually.

Spammer `btc-simnet-spammer` this container runs a simple rust program that will spam transactions, so the blocks are not empty. It will spam the amount of transactions from configuration for each miner, resulting in 2x that amount, per block. I will not spam again if a new block is not mined but be aware that spamming many transactions might the cause the block to be mined before all of then are able to be included, the rest will be in the mempool and join then next batch. The user should try with settings combinations to achieve the needed scenery. This can also be disabled by configuration.

## How to run

### Build
Bitcoin node needs a spacial builder, other container could be bul directly by the docker-compose
```bash
./build.sh
```

### Config
```bash
cp .env.example .env
```
Edit the copied .env to your preferred settings

### Run the simnet
```bash
docker-compose up
```

to see Mining logs
```bash
docker-compose logs -ft btc-simnet-spammer
```

See there the banner with user address funded

### Run the block and mempool explorer
```bash
docker-compose -f docker-compose-mempool.yml up
```
browse http://localhost:1080/


## Limitations and future enhancements

### BitcoinCore containers
- Download PGP singatures and verify downloaded files
- Build from sources insted of download binaries
- Determine platform at runtime, now fixed for `x86_64-linux-gnu`

### Rust containers
- Use multistage builds at Dockerfile and copy only the binary to a fresh image

### Simulations
- Add reorg simulations
