use {
    backoff::{future::retry, ExponentialBackoff}, clap::Parser as ClapParser, futures::{
        future::TryFutureExt,
        sink::SinkExt,
        stream::StreamExt,
    }, log::{error, info}, raydium_clmm_swap_interface::{accounts::{PoolState,
            AMM_CONFIG_ACCOUNT_DISCM,
            AmmConfig,
            AmmConfigAccount,
            PoolStateAccount,
            POOL_STATE_ACCOUNT_DISCM,
            TICK_ARRAY_STATE_DISCM,
            TickArrayState,              
            TickArrayStateAccount,
    }},serde::{Deserialize, Serialize}, std::{
        collections::HashMap, env, sync::Arc, time::Duration
    }, tokio::sync::Mutex, tonic::transport::channel::ClientTlsConfig, yellowstone_grpc_client::{GeyserGrpcClient, Interceptor}, yellowstone_grpc_proto::{
        geyser::SubscribeRequestFilterAccounts,
        prelude::{
            subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
        },
    }
};
use borsh::{BorshDeserialize, BorshSerialize};

type AccountFilterMap = HashMap<String, SubscribeRequestFilterAccounts>;

const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

#[derive(Debug, Serialize)]
pub enum DecodedAccount {
    PoolState(PoolState),
    AmmConfig(AmmConfig),
    TickArrayState(TickArrayState), 
}

#[derive(Debug)]
pub struct AccountDecodeError {
    pub message: String,
}


#[derive(Debug, Clone, ClapParser)]
#[clap(author, version, about)]
struct Args {
    #[clap(short, long, help = "gRPC endpoint")]
    endpoint: String,

    #[clap(long, help = "X-Token")]
    x_token: String,
}

impl Args {
    async fn connect(&self) -> anyhow::Result<GeyserGrpcClient<impl Interceptor>> {
        GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
            .x_token(Some(self.x_token.clone()))?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10))
            .tls_config(ClientTlsConfig::new().with_native_roots())?
            .max_decoding_message_size(1024 * 1024 * 1024)
            .connect()
            .await
            .map_err(Into::into)
    }

    pub fn get_txn_updates(&self) -> anyhow::Result<SubscribeRequest> {

        let mut accounts: AccountFilterMap = HashMap::new();

        accounts.insert(
            "accountData".to_owned(),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![RAYDIUM_CLMM_PROGRAM_ID.to_string()],
                nonempty_txn_signature: None,
                filters: vec![]
            },
        );

        Ok(SubscribeRequest {
            accounts,
            slots: HashMap::default(),
            transactions:HashMap::default(),
            transactions_status: HashMap::default(),
            blocks: HashMap::default(),
            blocks_meta: HashMap::default(),
            entry: HashMap::default(),
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: Vec::default(),
            ping: None,
            from_slot: None,
        })
    }
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();
    let zero_attempts = Arc::new(Mutex::new(true));

    retry(ExponentialBackoff::default(), move || {
        let args = args.clone();
        let zero_attempts = Arc::clone(&zero_attempts);

        async move {
            let mut zero_attempts = zero_attempts.lock().await;
            if *zero_attempts {
                *zero_attempts = false;
            } else {
                info!("Retry to connect to the server");
            }
            drop(zero_attempts);

            let client = args.connect().await.map_err(backoff::Error::transient)?;
            info!("Connected");

            let request = args.get_txn_updates().map_err(backoff::Error::Permanent)?;

            geyser_subscribe(client, request)
                .await
                .map_err(backoff::Error::transient)?;

            Ok::<(), backoff::Error<anyhow::Error>>(())
        }
        .inspect_err(|error| error!("failed to connect: {error}"))
    })
    .await
    .map_err(Into::into)
}


async fn geyser_subscribe(
    mut client: GeyserGrpcClient<impl Interceptor>,
    request: SubscribeRequest,
) -> anyhow::Result<()> {
    let (mut subscribe_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    info!("stream opened");

    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => match msg.update_oneof {
                
                Some(UpdateOneof::Account(account)) => {
                    
                    let slot = account.slot;
                    if let Some(account_data) = account.account {
                        let pubkey_str = bs58::encode(&account_data.pubkey).into_string();
                        let owner = bs58::encode(&account_data.owner).into_string();
                        let lamports = account_data.lamports;
                        let executable = account_data.executable;


                        let discriminator: [u8; 1] = account_data.data[..1].try_into().expect("Failed to extract first byte");

                        println!("Discriminator Received {}: {:#?}", pubkey_str, discriminator);

                
                        let decoded_account = match decode_account_data(&account_data.data) {
                            Ok(data) => data,
                            Err(e) => {
                                eprintln!("Failed to decode account data: {}", e.message);
                                continue;
                            }
                        };
                
                        let account_info = serde_json::json!({
                            "pubkey": pubkey_str,
                            "lamports": lamports,
                            "owner": owner,
                            "executable": executable,
                            "slot": slot,
                            "decoded_data": decoded_account
                        });
                
                        println!("\nAccount Info: {}", account_info);
                    } else {
                        println!("Account data is None for slot: {}", slot);
                    }

                }
                Some(UpdateOneof::Ping(_)) => {
                    subscribe_tx
                        .send(SubscribeRequest {
                            ping: Some(SubscribeRequestPing { id: 1 }),
                            ..Default::default()
                        })
                        .await?;
                }
                Some(UpdateOneof::Pong(_)) => {}
                None => {
                    error!("update not found in the message");
                    break;
                }
                _ => {}
            },
            Err(error) => {
                error!("error: {error:?}");
                break;
            }
        }
    }

    info!("stream closed");
    Ok(())
}


pub fn decode_account_data(buf: &[u8]) -> Result<DecodedAccount, AccountDecodeError> {
    if buf.len() < 8 {
        return Err(AccountDecodeError {
            message: "Buffer too short to contain a valid discriminator.".to_string(),
        });
    }

    let discriminator: [u8; 8] = buf[..8].try_into().expect("Failed to extract first 8 bytes");

    match discriminator {
        POOL_STATE_ACCOUNT_DISCM => {
            let data = PoolStateAccount::deserialize(buf)
                .map_err(|e| AccountDecodeError {
                    message: format!("Failed to deserialize PoolState: {}", e),
                })?;
            println!("\nDecoded PoolState Structure: {:#?}", data);
            Ok(DecodedAccount::PoolState(data.0))
        }
        AMM_CONFIG_ACCOUNT_DISCM => {
            let data = AmmConfigAccount::deserialize(buf)
                .map_err(|e| AccountDecodeError {
                    message: format!("Failed to deserialize AmmConfig: {}", e),
                })?;
            println!("\nDecoded AmmConfig Structure: {:#?}", data);
            Ok(DecodedAccount::AmmConfig(data.0))
        }
         TICK_ARRAY_STATE_DISCM => {
            let data = TickArrayStateAccount::deserialize(buf)
                .map_err(|e| AccountDecodeError {
                    message: format!("Failed to deserialize AmmConfig: {}", e),
                })?;
            println!("\nDecoded AmmConfig Structure: {:#?}", data);
            Ok(DecodedAccount::TickArrayState(data.0))
        }
        _ => Err(AccountDecodeError {
            message: "Account discriminator not found.".to_string(),
        }),
    }
}

