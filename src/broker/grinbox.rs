use std::sync::{Arc, Mutex};
use std::thread;
use ws::{connect, Sender, Handler, Handshake, Message, CloseCode, Result as WsResult, ErrorKind as WsErrorKind, Error as WsError};
use ws::util::Token;
use colored::*;

use grin_core::libtx::slate::Slate;

use common::{Error, Wallet713Error};
use common::crypto::{SecretKey, Signature, verify_signature, sign_challenge, Hex, EncryptedMessage};
use contacts::{Address, GrinboxAddress, DEFAULT_GRINBOX_PORT};

use super::types::{Publisher, Subscriber, SubscriptionHandler, CloseReason};
use super::protocol::{ProtocolResponse, ProtocolRequest};

const KEEPALIVE_TOKEN: Token = Token(1);
const KEEPALIVE_INTERVAL_MS: u64 = 30_000;

#[derive(Clone)]
pub struct GrinboxPublisher {
    address: GrinboxAddress,
    secret_key: SecretKey,
    use_encryption: bool,
}

impl GrinboxPublisher {
    pub fn new(address: &GrinboxAddress, secret_key: &SecretKey, use_encryption: bool) -> Result<Self, Error> {
        Ok(Self {
            address: address.clone(),
            secret_key: secret_key.clone(),
            use_encryption,
        })
    }
}

impl Publisher for GrinboxPublisher {
    fn post_slate(&self, slate: &Slate, to: &Address) -> Result<(), Error> {
        let broker = GrinboxBroker::new(self.use_encryption)?;
        let to = GrinboxAddress::from_str(&to.to_string())?;
        broker.post_slate(slate, &to, &self.address, &self.secret_key)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct GrinboxSubscriber {
    address: GrinboxAddress,
    broker: GrinboxBroker,
    secret_key: SecretKey,
}

impl GrinboxSubscriber {
    pub fn new(address: &GrinboxAddress, secret_key: &SecretKey, use_encryption: bool) -> Result<Self, Error> {
        Ok(Self {
            address: address.clone(),
            broker: GrinboxBroker::new(use_encryption)?,
            secret_key: secret_key.clone(),
        })
    }
}

impl Subscriber for GrinboxSubscriber {
    fn start(&mut self, handler: Box<SubscriptionHandler + Send>) -> Result<(), Error> {
        self.broker.subscribe(&self.address, &self.secret_key, handler)?;
        Ok(())
    }

    fn stop(&self) {
        self.broker.stop();
    }

    fn is_running(&self) -> bool {
        self.broker.is_running()
    }
}

#[derive(Clone)]
struct GrinboxBroker {
    inner: Arc<Mutex<Option<Sender>>>,
    use_encryption: bool,
}

impl GrinboxBroker {
    fn new(use_encryption: bool) -> Result<Self, Error> {
        Ok(Self {
            inner: Arc::new(Mutex::new(None)),
            use_encryption,
        })
    }

    fn post_slate(&self, slate: &Slate, to: &GrinboxAddress, from: &GrinboxAddress, secret_key: &SecretKey) -> Result<(), Error> {
        let url = {
            let to = to.clone();
            format!("wss://{}:{}", to.domain, to.port.unwrap_or(DEFAULT_GRINBOX_PORT))
        };
        let pkey = to.public_key()?;
        let skey = secret_key.clone();
        connect(url, move |sender| {
            move |msg: Message| {
                let response = serde_json::from_str::<ProtocolResponse>(&msg.to_string()).expect("could not parse response!");
                match response {
                    ProtocolResponse::Challenge { str } => {
                        let slate_str = match self.use_encryption {
                            true => {
                                let message = EncryptedMessage::new(serde_json::to_string(&slate).unwrap(), &pkey, &skey).map_err(|_|
                                    WsError::new(WsErrorKind::Protocol, "could not encrypt slate!")
                                )?;
                                serde_json::to_string(&message).unwrap()
                            },
                            false => serde_json::to_string(&slate).unwrap(),
                        };

                        let mut challenge = String::new();
                        challenge.push_str(&slate_str);
                        challenge.push_str(&str);
                        let signature = GrinboxClient::generate_signature(&challenge, secret_key);
                        let request = ProtocolRequest::PostSlate {
                            from: from.stripped(),
                            to: to.public_key.clone(),
                            str: slate_str,
                            signature,
                        };
                        sender.send(serde_json::to_string(&request).unwrap()).unwrap();
                        sender.close(CloseCode::Normal).is_ok();
                    },
                    _ => {}
                }
                Ok(())
            }
        })?;
        Ok(())
    }

    fn subscribe(&mut self, address: &GrinboxAddress, secret_key: &SecretKey, handler: Box<SubscriptionHandler + Send>) -> Result<(), Error> {
        let handler = Arc::new(Mutex::new(handler));
        let url = {
            let cloned_address = address.clone();
            format!("wss://{}:{}", cloned_address.domain, cloned_address.port.unwrap_or(DEFAULT_GRINBOX_PORT))
        };
        let secret_key = secret_key.clone();
        let cloned_address = address.clone();
        let cloned_inner = self.inner.clone();
        let cloned_handler = handler.clone();
        let use_encryption = self.use_encryption;
        thread::spawn(move || {
            let cloned_cloned_inner = cloned_inner.clone();
            let result = connect(url, move |sender| {
                if let Ok(mut guard) = cloned_cloned_inner.lock() {
                    *guard = Some(sender.clone());
                };

                let client = GrinboxClient {
                    sender,
                    handler: cloned_handler.clone(),
                    challenge: None,
                    address: cloned_address.clone(),
                    secret_key,
                    use_encryption,
                };
                client
            });

            if let Ok(mut guard) = cloned_inner.lock() {
                *guard = None;
            };

            match result {
                Err(_) => handler.lock().unwrap().on_close(CloseReason::Abnormal(Error::from(Wallet713Error::GrinboxWebsocketAbnormalTermination))),
                _ => handler.lock().unwrap().on_close(CloseReason::Normal),
            }
        });
        Ok(())
    }

    fn stop(&self) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref sender) = *guard {
            sender.close(CloseCode::Normal).is_ok();
        }
        *guard = None;
    }

    fn is_running(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.is_some()
    }
}

struct GrinboxClient {
    sender: Sender,
    handler: Arc<Mutex<Box<SubscriptionHandler + Send>>>,
    challenge: Option<String>,
    address: GrinboxAddress,
    secret_key: SecretKey,
    use_encryption: bool,
}

impl GrinboxClient {
    fn generate_signature(challenge: &str, secret_key: &SecretKey) -> String {
        let signature = sign_challenge(challenge, secret_key).expect("could not sign challenge!");
        signature.to_hex()
    }

    fn subscribe(&self, challenge: &str) -> Result<(), Error> {
        let signature = GrinboxClient::generate_signature(challenge, &self.secret_key);
        let request = ProtocolRequest::Subscribe { address: self.address.public_key.to_string(), signature };
        self.send(&request).expect("could not send subscribe request!");
        Ok(())
    }

    fn verify_slate_signature(&self, from: &str, str: &str, challenge: &str, signature: &str) -> Result<(), Error> {
        let from = GrinboxAddress::from_str(from)?;
        let public_key = from.public_key()?;
        let signature = Signature::from_hex(signature)?;
        let mut challenge_builder = String::new();
        challenge_builder.push_str(str);
        challenge_builder.push_str(challenge);
        verify_signature(&challenge_builder, &signature, &public_key)?;
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
        self.handler.lock().unwrap().on_open();
        try!(self.sender.timeout(KEEPALIVE_INTERVAL_MS, KEEPALIVE_TOKEN));
        Ok(())
    }

    fn on_timeout(&mut self, event: Token) -> WsResult<()> {
        match event {
            KEEPALIVE_TOKEN => {
                self.sender.ping(vec![])?;
                self.sender.timeout(KEEPALIVE_INTERVAL_MS, KEEPALIVE_TOKEN)
            }
            _ => Err(WsError::new(WsErrorKind::Internal, "Invalid timeout token encountered!")),
        }
    }


    fn on_message(&mut self, msg: Message) -> WsResult<()> {
        let response = serde_json::from_str::<ProtocolResponse>(&msg.to_string()).map_err(|_| {
            WsError::new(WsErrorKind::Protocol, "could not parse response!")
        })?;
        match response {
            ProtocolResponse::Challenge { str } => {
                self.challenge = Some(str.clone());
                self.subscribe(&str).map_err(|_| {
                    WsError::new(WsErrorKind::Protocol, "error attempting to subscribe!")
                })?;
            },
            ProtocolResponse::Slate { from, str, challenge, signature } => {
                if let Ok(_) = self.verify_slate_signature(&from, &str, &challenge, &signature) {

                    let from = match GrinboxAddress::from_str(&from) {
                        Ok(x) => x,
                        Err(_) => {
                            cli_message!("could not parse address!");
                            return Ok(());
                        },
                    };

                    let mut slate: Slate = match self.use_encryption {
                        true => {
                            let encrypted_message: EncryptedMessage = match serde_json::from_str(&str) {
                                Ok(x) => x,
                                Err(_) => {
                                    cli_message!("could not parse encrypted message!");
                                    return Ok(());
                                },
                            };
                            let pkey = match from.public_key() {
                                Ok(x) => x,
                                Err(_) => {
                                    cli_message!("could not parse public key!");
                                    return Ok(());
                                },
                            };

                            let decrypted_message = match encrypted_message.decrypt(&pkey, &self.secret_key) {
                                Ok(x) => x,
                                Err(_) => {
                                    cli_message!("could not decrypt message!");
                                    return Ok(());
                                },
                            };

                            let slate: Slate = match serde_json::from_str(&decrypted_message) {
                                Ok(x) => x,
                                Err(_) => {
                                    cli_message!("could not parse slate!");
                                    return Ok(());
                                },
                            };

                            slate
                        },
                        false => match serde_json::from_str(&str) {
                            Ok(x) => x,
                            Err(_) => {
                                cli_message!("could not parse slate!");
                                return Ok(());
                            },
                        },
                    };

                    self.handler.lock().unwrap().on_slate(&from, &mut slate);
                } else {
                    cli_message!("{}: received slate with invalid signature!", "ERROR".bright_red());
                }
            },
            ProtocolResponse::Error { kind: _, description: _ } => {
                cli_message!("{}", response);
            },
            _ => {}
        }
        Ok(())
    }
}
