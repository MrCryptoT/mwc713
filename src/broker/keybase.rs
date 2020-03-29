use std::borrow::Borrow;
use std::collections::HashSet;
use std::iter::FromIterator;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use grin_wallet_libwallet::Slate;
use grinswap::Message;
use super::types::{CloseReason, Publisher, Subscriber, SubscriptionHandler};
use common::{Arc, Mutex, Error, ErrorKind};
use contacts::{Address, KeybaseAddress};
use std::path::Path;

pub const TOPIC_SLATE_NEW: &str = "grin_slate_new";
pub const TOPIC_WALLET713_SLATES: &str = "wallet713_grin_slate";
const TOPIC_SLATE_SIGNED: &str = "grin_slate_signed";
const SLEEP_DURATION: Duration = Duration::from_millis(5000);

#[derive(Clone)]
pub struct KeybasePublisher {
    ttl: Option<String>,
    keybase_binary: Option<String>,
}

impl KeybasePublisher {
    pub fn new(ttl: Option<String>, keybase_binary: Option<String>) -> Result<Self, Error> {
        let _broker = KeybaseBroker::new(keybase_binary.clone())?;
        Ok(Self { ttl, keybase_binary })
    }
}

#[derive(Clone)]
pub struct KeybaseSubscriber {
    stop_signal: Arc<Mutex<bool>>,
    keybase_binary: Option<String>,
}

impl KeybaseSubscriber {
    pub fn new(keybase_binary: Option<String>) -> Result<Self, Error> {
        Ok(Self {
            stop_signal: Arc::new(Mutex::new(true)),
            keybase_binary: keybase_binary,
        })
    }
}

impl Publisher for KeybasePublisher {
    fn post_slate(&self, slate: &Slate, to: &dyn Address) -> Result<(), Error> {
        let keybase_address = KeybaseAddress::from_str(&to.to_string())?;

        // make sure we don't send message with ttl to wallet713 as keybase oneshot does not support exploding lifetimes
        let ttl = match keybase_address.username.as_ref() {
            "wallet713" => &None,
            _ => &self.ttl,
        };

        let topic = match &keybase_address.topic {
            Some(t) => t,
            None => TOPIC_WALLET713_SLATES,
        };

        KeybaseBroker::send(&slate, &to.stripped(), topic, ttl, self.keybase_binary.clone())?;

        Ok(())
    }

    fn post_take(&self, message: &Message, to: &str) -> Result<(), Error> {
        unimplemented!()
    }
}

impl Subscriber for KeybaseSubscriber {
    fn start(&mut self, handler: Box<dyn SubscriptionHandler + Send>) -> Result<(), Error> {
        {
            let mut guard = self.stop_signal.lock();
            *guard = false;
        }

        let mut subscribed = false;
        let mut dropped = false;
        let result: Result<(), Error> = loop {
            if *self.stop_signal.lock() {
                break Ok(());
            };
            let result = KeybaseBroker::get_unread(self.keybase_binary.clone(), HashSet::from_iter(vec![
                TOPIC_WALLET713_SLATES,
                TOPIC_SLATE_NEW,
                TOPIC_SLATE_SIGNED,
            ]));
            if let Ok(unread) = result {
                if !subscribed {
                    subscribed = true;
                    handler.on_open();
                }
                if dropped {
                    dropped = false;
                    handler.on_reestablished();
                }
                for (sender, topic, msg) in &unread {
                    let reply_topic = match topic.as_ref() {
                        TOPIC_SLATE_NEW => TOPIC_SLATE_SIGNED.to_string(),
                        _ => TOPIC_WALLET713_SLATES.to_string(),
                    };
                    let mut slate = Slate::deserialize_upgrade(&msg)?;
                    let address = KeybaseAddress {
                        username: sender.to_string(),
                        topic: Some(reply_topic),
                    };
                    handler.on_slate(address.borrow(), &mut slate, None, None);
                }
            } else {
                if !dropped {
                    dropped = true;
                    if subscribed {
                        handler.on_dropped();
                    } else {
                        break Err(ErrorKind::KeybaseNotFound.into());
                    }
                }
            }
            std::thread::sleep(SLEEP_DURATION);
        };
        match result {
            Err(e) => handler.on_close(CloseReason::Abnormal(e)),
            _ => handler.on_close(CloseReason::Normal),
        }
        Ok(())
    }

    fn stop(&mut self) -> bool {
        let mut guard = self.stop_signal.lock();
        *guard = true;
        return true;
    }

    fn is_running(&self) -> bool {
        let guard = self.stop_signal.lock();
        !*guard
    }
}

struct KeybaseBroker {}

impl KeybaseBroker {
    pub fn new(keybase_binary: Option<String>) -> Result<Self, Error> {
        // where doesn't handle path verification at all. It expect path and pattern.
        // That is why for this case checking for file existance
        if cfg!(target_os = "windows") && keybase_binary.is_some() {
            if Path::new(&keybase_binary.unwrap() ).exists() {
                return Ok(Self {})
            } else {
                return Err(ErrorKind::KeybaseNotFound)?
            }
        }

        let mut proc = if cfg!(target_os = "windows") {
            Command::new("where")
        } else {
            Command::new("which")
        };

        let status = if keybase_binary.is_some() {
            proc.arg(keybase_binary.unwrap()).stdout(Stdio::null()).status()?
        } else {
            proc.arg("keybase").stdout(Stdio::null()).status()?
        };

        if status.success() {
            Ok(Self {})
        } else {
            Err(ErrorKind::KeybaseNotFound)?
        }
    }

    pub fn api_send(keybase_binary: Option<String>, payload: &str) -> Result<Value, Error> {
        let mut proc = if keybase_binary.is_some() {
            Command::new(keybase_binary.unwrap())
        } else {
            Command::new("keybase")
        };
        proc.args(&["chat", "api", "-m", &payload]);
        let output = proc.output().expect("No output").stdout;
        let response = std::str::from_utf8(&output)?;
        let response: Value = serde_json::from_str(response)?;
        Ok(response)
    }

    pub fn read_from_channel(channel: &str, topic: &str, keybase_binary: Option<String>) -> Result<Vec<(String, String, String)>, Error> {
        let payload = json!({
            "method": "read",
            "params": {
                "options": {
                    "channel": {
                        "name": channel,
                        "topic_type": "dev",
                        "topic_name": topic
                    },
                    "unread_only": true,
                    "peek": false
                },
            }
        });
        let payload = serde_json::to_string(&payload)?;
        let response = KeybaseBroker::api_send(keybase_binary, &payload)?;
        let mut unread: Vec<(String, String, String)> = Vec::new();
        let messages = response["result"]["messages"].as_array();
        if let Some(messages) = messages {
            for msg in messages.iter() {
                if (msg["msg"]["content"]["type"] == "text") && (msg["msg"]["unread"] == true) {
                    let message = msg["msg"]["content"]["text"]["body"].as_str().unwrap_or("");
                    let sender: &str = msg["msg"]["sender"]["username"].as_str().unwrap_or("");
                    if !message.is_empty() && !sender.is_empty() {
                        unread.push((sender.to_string(), topic.to_string(), message.to_string()));
                    }
                }
            }
        }
        Ok(unread)
    }

    pub fn get_unread(keybase_binary: Option<String>, topics: HashSet<&str>) -> Result<Vec<(String, String, String)>, Error> {
        let payload = json!({
            "method": "list",
            "params": {
                "options": {
                    "topic_type": "dev",
                },
            }
        });
        let payload = serde_json::to_string(&payload)?;
        let response = KeybaseBroker::api_send(keybase_binary.clone(), &payload)?;

        let mut channels = HashSet::new();
        let messages = response["result"]["conversations"].as_array();
        if let Some(messages) = messages {
            for msg in messages.iter() {
                let topic = msg["channel"]["topic_name"].as_str().unwrap();
                if (msg["unread"] == true) && topics.contains(topic) {
                    let channel = msg["channel"]["name"].as_str().unwrap();
                    channels.insert((channel.to_string(), topic));
                }
            }
        }

        let mut unread: Vec<(String, String, String)> = Vec::new();
        for (channel, topic) in channels.iter() {
            let mut messages = KeybaseBroker::read_from_channel(channel, topic, keybase_binary.clone())?;
            unread.append(&mut messages);
        }
        Ok(unread)
    }

    pub fn send<T: Serialize>(
        message: &T,
        channel: &str,
        topic: &str,
        ttl: &Option<String>,
        keybase_binary: Option<String>,
    ) -> Result<(), Error> {
        let mut payload = json!({
            "method": "send",
            "params": {
                "options": {
                    "channel": {
                        "name": channel,
                        "topic_name": topic,
                        "topic_type": "dev"
                    },
                    "message": {
                        "body": serde_json::to_string(&message)?
                    }
                }
            }
        });

        if let Some(ttl) = ttl {
            payload["params"]["options"]["exploding_lifetime"] = json!(ttl);
        }

        let payload = serde_json::to_string(&payload)?;
        let response = KeybaseBroker::api_send(keybase_binary, &payload)?;
        match response["result"]["message"].as_str() {
            Some("message sent") => Ok(()),
            _ => Err(ErrorKind::KeybaseMessageSendError)?,
        }
    }
}
