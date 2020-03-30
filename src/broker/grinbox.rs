use ws::util::Token;
use ws::{
    connect, CloseCode, Error as WsError, ErrorKind as WsErrorKind, Handler, Handshake, Message,
    Result as WsResult, Sender,
};

use grin_wallet_libwallet::Slate;
use crate::wallet::types::TxProof;
use common::config::Wallet713Config;
use common::crypto::{sign_challenge, Hex, SecretKey};
use common::message::EncryptedMessage;
use common::{Arc, Mutex, Error, ErrorKind};
use contacts::{Address, GrinboxAddress, DEFAULT_GRINBOX_PORT};

use super::protocol::{ProtocolRequest, ProtocolResponse};
use super::types::{CloseReason, Publisher, Subscriber, SubscriptionHandler};

const KEEPALIVE_TOKEN: Token = Token(1);
const KEEPALIVE_INTERVAL_MS: u64 = 30_000;

#[derive(Clone)]
pub struct GrinboxPublisher {
    address: GrinboxAddress,
    broker: GrinboxBroker,
    secret_key: SecretKey,
    config: Wallet713Config,
}

impl GrinboxPublisher {
    pub fn new(
        address: &GrinboxAddress,
        secret_key: &SecretKey,
        protocol_unsecure: bool,
        config: &Wallet713Config,
    ) -> Result<Self, Error> {
        Ok(Self {
            address: address.clone(),
            broker: GrinboxBroker::new(protocol_unsecure)?,
            secret_key: (*secret_key).clone(),
            config: config.clone(),
        })
    }
}

impl Publisher for GrinboxPublisher {
    fn post_slate(&self, slate: &Slate, to: &dyn Address) -> Result<(), Error> {
        let to = GrinboxAddress::from_str(&to.to_string())?;
        self.broker.post_slate(slate, &to, &self.address, &self.secret_key)?;
        Ok(())
    }

    fn post_take(&self, _message: &grinswap::Message, _to: &str) -> Result<(), Error> {
        unimplemented!()
    }
}

#[derive(Clone)]
pub struct GrinboxSubscriber {
    address: GrinboxAddress,
    broker: GrinboxBroker,
    secret_key: SecretKey,
    config: Wallet713Config,
}

impl GrinboxSubscriber {
    pub fn new(publisher: &GrinboxPublisher) -> Result<Self, Error> {
        Ok(Self {
            address: publisher.address.clone(),
            broker: publisher.broker.clone(),
            secret_key: publisher.secret_key.clone(),
            config: publisher.config.clone(),
        })
    }
}

impl Subscriber for GrinboxSubscriber {
    fn start(&mut self, handler: Box<dyn SubscriptionHandler + Send>) -> Result<(), Error> {
        self.broker
            .subscribe(&self.address, &self.secret_key, handler, self.config.clone())?;
        Ok(())
    }

    fn stop(&mut self) -> bool {
        self.broker.stop();
        return true;
    }

    fn is_running(&self) -> bool {
        self.broker.is_running()
    }
}

#[derive(Clone)]
struct GrinboxBroker {
    inner: Arc<Mutex<Option<Sender>>>,
    protocol_unsecure: bool,
}

struct ConnectionMetadata {
    retries: u32,
    connected_at_least_once: bool,
}

impl ConnectionMetadata {
    pub fn new() -> Self {
        Self {
            retries: 0,
            connected_at_least_once: false,
        }
    }
}

impl GrinboxBroker {
    fn new(protocol_unsecure: bool) -> Result<Self, Error> {
        Ok(Self {
            inner: Arc::new(Mutex::new(None)),
            protocol_unsecure,
        })
    }

    fn post_slate(
        &self,
        slate: &Slate,
        to: &GrinboxAddress,
        from: &GrinboxAddress,
        secret_key: &SecretKey,
    ) -> Result<(), Error> {
        if !self.is_running() {
            return Err(ErrorKind::ClosedListener("mwcmq".to_string()).into());
        }

        let pkey = to.public_key()?;
        let skey = secret_key.clone();
        let message = EncryptedMessage::new(
            serde_json::to_string(&slate)?,
            &to,
            &pkey,
            &skey,
        )
            .map_err(|_| {
                WsError::new(WsErrorKind::Protocol, "could not encrypt slate!")
            })?;
        let message_ser = serde_json::to_string(&message)?;

        let mut challenge = String::new();
        challenge.push_str(&message_ser);

        let signature = GrinboxClient::generate_signature(&challenge, secret_key);
        let request = ProtocolRequest::PostSlate {
            from: from.stripped(),
            to: to.stripped(),
            str: message_ser,
            signature,
        };

        if let Some(ref sender) = *self.inner.lock() {
            sender.send(serde_json::to_string(&request).unwrap())
                .map_err(|_| ErrorKind::GenericError("failed posting slate!".to_string()).into())
        } else {
            Err(ErrorKind::GenericError("failed posting slate!".to_string()).into())
        }
    }

    fn subscribe(
        &mut self,
        address: &GrinboxAddress,
        secret_key: &SecretKey,
        handler: Box<dyn SubscriptionHandler + Send>,
        config: Wallet713Config,
    ) -> Result<(), Error> {
        let handler = Arc::new(Mutex::new(handler));
        let url = {
            let cloned_address = address.clone();
            match self.protocol_unsecure {
                true => format!(
                    "ws://{}:{}",
                    cloned_address.domain,
                    cloned_address.port.unwrap_or(DEFAULT_GRINBOX_PORT)
                ),
                false => format!(
                    "wss://{}:{}",
                    cloned_address.domain,
                    cloned_address.port.unwrap_or(DEFAULT_GRINBOX_PORT)
                ),
            }
        };
        let cloned_secret_key = secret_key.clone();
        let cloned_address = address.clone();
        let cloned_inner = self.inner.clone();
        let cloned_handler = handler.clone();
        let connection_meta_data = Arc::new(Mutex::new(ConnectionMetadata::new()));
        loop {
            let cloned_address = cloned_address.clone();
            let cloned_handler = cloned_handler.clone();
            let cloned_cloned_inner = cloned_inner.clone();
            let cloned_connection_meta_data = connection_meta_data.clone();
            let cloned_config = config.clone();
            let cloned_cloned_secret_key = cloned_secret_key.clone();

            let result = connect(url.clone(), move |sender| {
                {
                    let mut guard = cloned_cloned_inner.lock();
                    *guard = Some(sender.clone());
                }

                let client = GrinboxClient {
                    sender,
                    handler: cloned_handler.clone(),
                    challenge: None,
                    address: cloned_address.clone(),
                    secret_key: cloned_cloned_secret_key.clone(),
                    connection_meta_data: cloned_connection_meta_data.clone(),
                    config: cloned_config.clone(),
                };
                client
            });

            let is_stopped = cloned_inner.lock().is_none();

            if is_stopped {
                match result {
                    Err(_) => handler.lock().on_close(CloseReason::Abnormal(
                        ErrorKind::GrinboxWebsocketAbnormalTermination.into(),
                    )),
                    _ => handler.lock().on_close(CloseReason::Normal),
                }
                break;
            } else {
                let mut guard = connection_meta_data.lock();
                if guard.retries == 0 && guard.connected_at_least_once {
                    handler.lock().on_dropped();
                }
                let secs = std::cmp::min(32, 2u64.pow(guard.retries));
                let duration = std::time::Duration::from_secs(secs);
                std::thread::sleep(duration);
                guard.retries += 1;
            }
        }
        let mut guard = cloned_inner.lock();
        *guard = None;
        Ok(())
    }

    fn stop(&self) {
        let mut guard = self.inner.lock();
        if let Some(ref sender) = *guard {
            let _ = sender.close(CloseCode::Normal);
        }
        *guard = None;
    }

    fn is_running(&self) -> bool {
        let guard = self.inner.lock();
        guard.is_some()
    }
}

struct GrinboxClient {
    sender: Sender,
    handler: Arc<Mutex<Box<dyn SubscriptionHandler + Send>>>,
    challenge: Option<String>,
    address: GrinboxAddress,
    secret_key: SecretKey,
    connection_meta_data: Arc<Mutex<ConnectionMetadata>>,
    config: Wallet713Config,
}

impl GrinboxClient {
    fn generate_signature(challenge: &str, secret_key: &SecretKey) -> String {
        let signature = sign_challenge(challenge, secret_key).expect("could not sign challenge!");
        signature.to_hex()
    }

    fn subscribe(&self, challenge: &str) -> Result<(), Error> {
        let signature = GrinboxClient::generate_signature(challenge, &self.secret_key);
        let request = ProtocolRequest::Subscribe {
            address: self.address.public_key.to_string(),
            signature,
        };
        self.send(&request)
            .expect("could not send subscribe request!");
        Ok(())
    }

    fn send(&self, request: &ProtocolRequest) -> Result<(), Error> {
        let request = serde_json::to_string(&request).unwrap();
        self.sender.send(request)?;
        Ok(())
    }
}

impl Handler for GrinboxClient {
    fn on_open(&mut self, _shake: Handshake) -> WsResult<()> {
        let mut guard = self.connection_meta_data.lock();

        if guard.connected_at_least_once {
            self.handler.lock().on_reestablished();
        } else {
            self.handler.lock().on_open();
            guard.connected_at_least_once = true;
        }

        guard.retries = 0;

        self.sender.timeout(KEEPALIVE_INTERVAL_MS, KEEPALIVE_TOKEN)?;
        Ok(())
    }

    fn on_timeout(&mut self, event: Token) -> WsResult<()> {
        match event {
            KEEPALIVE_TOKEN => {
                self.sender.ping(vec![])?;
                self.sender.timeout(KEEPALIVE_INTERVAL_MS, KEEPALIVE_TOKEN)
            }
            _ => Err(WsError::new(
                WsErrorKind::Internal,
                "Invalid timeout token encountered!",
            )),
        }
    }

    fn on_message(&mut self, msg: Message) -> WsResult<()> {
        let response = match serde_json::from_str::<ProtocolResponse>(&msg.to_string()) {
            Ok(x) => x,
            Err(_) => {
                cli_message!("Error: could not parse response");
                return Ok(());
            }
        };
        match response {
            ProtocolResponse::Challenge { str } => {
                self.challenge = Some(str.clone());
                self.subscribe(&str).map_err(|_| {
                    WsError::new(WsErrorKind::Protocol, "error attempting to subscribe!")
                })?;
            }
            ProtocolResponse::Slate {
                from,
                str,
                challenge,
                signature,
            } => {
                let (mut slate, mut tx_proof, _) = match TxProof::from_response(
                    from,
                    str,
                    challenge,
                    signature,
                    &self.secret_key,
                    Some(&self.address),
                ) {
                    Ok(x) => x,
                    Err(err) => {
                        cli_message!("Error: {}", err);
                        return Ok(());
                    }
                };

                let address = tx_proof.address.clone();
                self.handler
                    .lock()
                    .on_slate(&address, &mut slate, Some(&mut tx_proof), Some(self.config.clone()));
            }
            ProtocolResponse::Error {
                kind: _,
                description: _,
            } => {
                cli_message!("Error: {}", response);
            }
            _ => {}
        }
        Ok(())
    }

    fn on_error(&mut self, err: WsError) {
        // Ignore connection reset errors by default
        if let WsErrorKind::Io(ref err) = err.kind {
            if let Some(104) = err.raw_os_error() {
                return;
            }
        }

        error!("{:?}", err);
    }
}
