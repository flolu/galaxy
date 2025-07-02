use tonic::{transport::{Server, Channel}, Request, Response, Status, Streaming};
use mining::mining_server::{Mining, MiningServer};
use mining::{
    GetMiningWorkRequest, GetMiningWorkResponse,
    SubmitMinedBlockRequest, SubmitMinedBlockResponse,
    BatchGetMiningWorkRequest, BatchGetMiningWorkResponse,
    StreamMiningWorkRequest, StreamMiningWorkResponse,
    GetMetricsRequest, GetMetricsResponse
};
// use block::block_client::BlockClient Nanoparticle
use block::{AssembleBlockRequest, block_client::BlockClient};
use network::network_client::NetworkClient;
use network::BroadcastBlockRequest;
use auth::auth_client::AuthClient;
use auth::{AuthenticateRequest, AuthorizeRequest};
use sv::block::Block;
use sv::transaction::Transaction;
use sv::util::{deserialize, serialize, hash::Sha256d};
use futures::StreamExt;
use hex;
use std::time::Instant;
use prometheus::{Counter, Gauge, Registry};
use governor::{Quota, RateLimiter, Jitter};
use std::num::NonZeroU32;
use tracing::{info, warn};
use shared::ShardManager;
use toml;

tonic::include_proto!("mining");
tonic::include_proto!("block");
tonic::include_proto!("network");
tonic::include_proto!("auth");
tonic::include_proto!("metrics");

#[derive(Debug)]
struct MiningServiceImpl {
    block_client: BlockClient<Channel>,
    network_client: NetworkClient<Channel>,
    auth_client: AuthClient<Channel>,
    registry: Arc<Registry>,
    work_requests: Counter,
    latency_ms: Gauge,
    blocks_submitted: Counter,
    rate_limiter: Arc<RateLimiter<String, governor::state::direct::NotKeyed, governor::clock::DefaultClock>>,
    shard_manager: Arc<ShardManager>,
}

impl MiningServiceImpl {
    async fn new() -> Self {
        let config_str = include_str!("../../tests/config.toml");
        let config: toml::Value = toml::from_str(config_str).expect("Failed to parse config");
        let shard_id = config["sharding"]["shard_id"].as_integer().unwrap_or(0) as u32;

        let block_client = BlockClient::connect("http://[::1]:50054")
            .await
            .expect("Failed to connect to block_service");
        let network_client = NetworkClient::connect("http://[::1]:50051")
            .await
            .expect("Failed to connect to network_service");
        let auth_client = AuthClient::connect("http://[::1]:50060")
            .await
            .expect("Failed to connect to auth_service");
        let registry = Arc::new(Registry::new());
        let work_requests = Counter::new("mining_work_requests_total", "Total mining work requests").unwrap();
        let latency_ms = Gauge::new("mining_latency_ms", "Average work generation latency").unwrap();
        let blocks_submitted = Counter::new("mining_blocks_submitted", "Total blocks submitted").unwrap();
        registry.register(Box::new(work_requests.clone())).unwrap();
        registry.register(Box::new(latency_ms.clone())).unwrap();
        registry.register(Box::new(blocks_submitted.clone())).unwrap();
        let rate_limiter = Arc::new(RateLimiter::direct(Quota::per_second(NonZeroU32::new(1000).unwrap())));
        let shard_manager = Arc::new(ShardManager::new());

        MiningServiceImpl {
            block_client,
            network_client,
            auth_client,
            registry,
            work_requests,
            latency_ms,
            blocks_submitted,
            rate_limiter,
            shard_manager,
        }
    }

    async fn authenticate(&self, token: &str) -> Result<String, Status> {
        let auth_request = AuthenticateRequest { token: token.to_string() };
        let auth_response = self.auth_client
            .authenticate(auth_request)
            .await
            .map_err(|e| Status::unauthenticated(format!("Authentication failed: {}", e)))?
            .into_inner();
        if !auth_response.success {
            return Err(Status::unauthenticated(auth_response.error));
        }
        Ok(auth_response.user_id)
    }

    async fn authorize(&self, user_id: &str, method: &str) -> Result<(), Status> {
        let auth_request = AuthorizeRequest {
            user_id: user_id.to_string(),
            service: "mining_service".to_string(),
            method: method.to_string(),
        };
        let auth_response = self.auth_client
            .authorize(auth_request)
            .await
            .map_err(|e| Status::permission_denied(format!("Authorization failed: {}", e)))?
            .into_inner();
        if !auth_response.allowed {
            return Err(Status::permission_denied(auth_response.error));
        }
        Ok(())
    }

    async fn select_transactions(&self) -> Result<Vec<Transaction>, String> {
        info!("Selecting transactions for block template");
        let config_str = include_str!("../../tests/config.toml");
        let config: toml::Value = toml::from_str(config_str).expect("Failed to parse config");
        let tx_hex = config["test_cases"]["broadcast_transaction"]["tx_hex"]
            .as_str()
            .unwrap()
            .to_string();
        let tx_bytes = hex::decode(&tx_hex)
            .map_err(|e| format!("Invalid tx_hex: {}", e))?;
        let tx: Transaction = deserialize(&tx_bytes)
            .map_err(|e| format!("Invalid transaction: {}", e))?;
        Ok(vec![tx])
    }

    async fn calculate_difficulty_target(&self) -> u32 {
        info!("Calculating difficulty target");
        0x1d00ffff // Placeholder, to be replaced with dynamic calculation
    }

    async fn generate_block_template(&self) -> Result<(String, u32), String> {
        let start = Instant::now();
        info!("Generating block template");
        let transactions = self.select_transactions().await?;
        let txids: Vec<Sha256d> = transactions.iter().map(|tx| tx.txid()).collect();
        let merkle_root = if txids.is_empty() {
            Sha256d::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap()
        } else {
            let mut current_level = txids;
            while current_level.len() > 1 {
                let mut next_level = vec![];
                for i in (0..current_level.len()).step_by(2) {
                    let left = current_level[i];
                    let right = if i + 1 < current_level.len() { current_level[i + 1] } else { left };
                    let mut combined = [0u8; 64];
                    combined[..32].copy_from_slice(&left[..]);
                    combined[32..].copy_from_slice(&right[..]);
                    let parent = Sha256d::double_sha256(&combined);
                    next_level.push(parent);
                }
                current_level = next_level;
            }
            current_level[0]
        };

        let block = Block {
            header: sv::block::BlockHeader {
                version: 1,
                prev_blockhash: Default::default(),
                merkle_root,
                time: 0,
                bits: self.calculate_difficulty_target().await,
                nonce: 0,
            },
            transactions,
        };

        info!("Generated block template in {}ms", start.elapsed().as_secs_f64() * 1000.0);
        Ok((hex::encode(serialize(&block)), block.header.bits))
    }

    fn validate_proof_of_work(&self, header: &sv::block::BlockHeader, target_difficulty: u32) -> bool {
        let target = Self::bits_to_target(target_difficulty);
        let hash = header.hash();
        let is_valid = hash <= target;
        info!("Proof-of-work validation: {}", if is_valid { "valid" } else { "invalid" });
        is_valid
    }

    fn bits_to_target(bits: u32) -> [u8; 32] {
        let exponent = (bits >> 24) as u8;
        let mantissa = bits & 0x007fffff;
        let mut target = [0u8; 32];
        if exponent <= 3 {
            let value = mantissa >> (8 * (3 - exponent));
            target[31] = (value & 0xff) as u8;
            target[30] = ((value >> 8) & 0xff) as u8;
            target[29] = ((value >> 16) & 0xff) as u8;
        } else {
            let offset = 32 - exponent as usize;
            target[offset] = (mantissa & 0xff) as u8;
            target[offset + 1] = ((mantissa >> 8) & 0xff) as u8;
            target[offset + 2] = ((mantissa >> 16) & 0xff) as u8;
        }
        target
    }
}

#[tonic::async_trait]
impl Mining for MiningServiceImpl {
    async fn get_mining_work(&self, request: Request<GetMiningWorkRequest>) -> Result<Response<GetMiningWorkResponse>, Status> {
        let token = request.metadata().get("authorization").ok_or_else(|| Status::unauthenticated("Missing token"))?;
        let user_id = self.authenticate(token.to_str().map_err(|e| Status::invalid_argument("Invalid token format"))?).await?;
        self.authorize(&user_id, "GetMiningWork").await?;
        self.rate_limiter.until_ready().await;

        self.work_requests.inc();
        let start = Instant::now();
        info!("Generating mining work for miner: {}", request.get_ref().miner_id);
        let _req = request.into_inner();
        let (block_template, target_difficulty) = self
            .generate_block_template()
            .await
            .map_err(|e| {
                warn!("Failed to generate mining work: {}", e);
                Status::internal(format!("Failed to generate work: {}", e))
            })?;

        self.latency_ms.set(start.elapsed().as_secs_f64() * 1000.0);
        info!("Generated mining work for block_template: {}", block_template);
        Ok(Response::new(GetMiningWorkResponse {
            block_template,
            target_difficulty,
            error: "".to_string(),
        }))
    }

    async fn submit_mined_block(&self, request: Request<SubmitMinedBlockRequest>) -> Result<Response<SubmitMinedBlockResponse>, Status> {
        let token = request.metadata().get("authorization").ok_or_else(|| Status::unauthenticated("Missing token"))?;
        let user_id = self.authenticate(token.to_str().map_err(|e| Status::invalid_argument("Invalid token format"))?).await?;
        self.authorize(&user_id, "SubmitMinedBlock").await?;
        self.rate_limiter.until_ready().await;

        self.work_requests.inc();
        self.blocks_submitted.inc();
        let start = Instant::now();
        info!("Submitting mined block: {}", request.get_ref().block_hex);
        let req = request.into_inner();
        let block_bytes = hex::decode(&req.block_hex)
            .map_err(|e| {
                warn!("Invalid block_hex: {}", e);
                Status::invalid_argument(format!("Invalid block_hex: {}", e))
            })?;
        let block: Block = deserialize(&block_bytes)
            .map_err(|e| {
                warn!("Invalid block: {}", e);
                Status::invalid_argument(format!("Invalid block: {}", e))
            })?;

        let is_valid_pow = self.validate_proof_of_work(&block.header, block.header.bits);
        if !is_valid_pow {
            self.latency_ms.set(start.elapsed().as_secs_f64() * 1000.0);
            warn!("Invalid proof-of-work for block: {}", req.block_hex);
            return Ok(Response::new(SubmitMinedBlockResponse {
                success: false,
                error: "Invalid proof-of-work".to_string(),
            }));
        }

        let mut network_client = self.network_client.clone();
        let broadcast_request = BroadcastBlockRequest { block_hex: req.block_hex };
        let broadcast_response = network_client
            .broadcast_block(broadcast_request)
            .await
            .map_err(|e| {
                warn!("Broadcast failed: {}", e);
                Status::internal(format!("Broadcast failed: {}", e))
            })?
            .into_inner();

        if !broadcast_response.success {
            self.latency_ms.set(start.elapsed().as_secs_f64() * 1000.0);
            warn!("Broadcast failed: {}", broadcast_response.error);
            return Ok(Response::new(SubmitMinedBlockResponse {
                success: false,
                error: broadcast_response.error,
            }));
        }

        self.latency_ms.set(start.elapsed().as_secs_f64() * 1000.0);
        info!("Successfully submitted mined block: {}", req.block_hex);
        Ok(Response::new(SubmitMinedBlockResponse {
            success: true,
            error: "".to_string(),
        }))
    }

    async fn batch_get_mining_work(&self, request: Request<BatchGetMiningWorkRequest>) -> Result<Response<BatchGetMiningWorkResponse>, Status> {
        let token = request.metadata().get("authorization").ok_or_else(|| Status::unauthenticated("Missing token"))?;
        let user_id = self.authenticate(token.to_str().map_err(|e| Status::invalid_argument("Invalid token format"))?).await?;
        self.authorize(&user_id, "BatchGetMiningWork").await?;
        self.rate_limiter.until_ready().await;

        self.work_requests.inc();
        let start = Instant::now();
        info!("Batch generating mining work for {} miners", request.get_ref().miner_ids.len());
        let req = request.into_inner();
        let mut results = vec![];

        for _miner_id in req.miner_ids {
            let result = self
                .get_mining_work(Request::new(GetMiningWorkRequest { miner_id: "".to_string() }))
                .await?
                .into_inner();
            results.push(result);
        }

        self.latency_ms.set(start.elapsed().as_secs_f64() * 1000.0);
        info!("Completed batch mining work generation");
        Ok(Response::new(BatchGetMiningWorkResponse { results }))
    }

    async fn stream_mining_work(&self, request: Request<Streaming<StreamMiningWorkRequest>>) -> Result<Response<Self::StreamMiningWorkResponseStream>, Status> {
        let token = request.metadata().get("authorization").ok_or_else(|| Status::unauthenticated("Missing token"))?;
        let user_id = self.authenticate(token.to_str().map_err(|e| Status::invalid_argument("Invalid token format"))?).await?;
        self.authorize(&user_id, "StreamMiningWork").await?;
        self.rate_limiter.until_ready().await;

        self.work_requests.inc();
        let start = Instant::now();
        info!("Streaming mining work");
        let mut stream = request.into_inner();
        let work_requests = self.work_requests.clone();
        let latency_ms = self.latency_ms.clone();

        let output = async_stream::try_stream! {
            while let Some(_req) = stream.next().await {
                work_requests.inc();
                let start_inner = Instant::now();
                let (block_template, target_difficulty) = self
                    .generate_block_template()
                    .await
                    .map_err(|e| {
                        warn!("Failed to generate streamed mining work: {}", e);
                        Status::internal(format!("Failed to generate work: {}", e))
                    })?;
                yield StreamMiningWorkResponse {
                    block_template,
                    target_difficulty,
                    error: "".to_string(),
                };
                info!("Streamed mining work in {}ms", start_inner.elapsed().as_secs_f64() * 1000.0);
            }
        };

        self.latency_ms.set(start.elapsed().as_secs_f64() * 1000.0);
        info!("Completed streaming mining work");
        Ok(Response::new(Box::pin(output)))
    }

    async fn get_metrics(&self, request: Request<GetMetricsRequest>) -> Result<Response<GetMetricsResponse>, Status> {
        let token = request.metadata().get("authorization").ok_or_else(|| Status::unauthenticated("Missing token"))?;
        let user_id = self.authenticate(token.to_str().map_err(|e| Status::invalid_argument("Invalid token format"))?).await?;
        self.authorize(&user_id, "GetMetrics").await?;

        Ok(Response::new(GetMetricsResponse {
            service_name: "mining_service".to_string(),
            requests_total: self.work_requests.get() as u64,
            avg_latency_ms: self.latency_ms.get(),
            errors_total: 0, // Placeholder
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let addr = "[::1]:50058".parse().unwrap();
    let mining_service = MiningServiceImpl::new().await;

    println!("Mining service listening on {}", addr);

    Server::builder()
        .add_service(MiningServer::new(mining_service))
        .serve(addr)
        .await?;

    Ok(())
}
