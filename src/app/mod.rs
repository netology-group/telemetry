use anyhow::{anyhow, format_err, Context, Result};
use async_std::{channel, stream::StreamExt, task};
use isahc::{config::Configurable, config::VersionNegotiation, HttpClient};
use log::{error, info};
use serde_json::{json, Value as JsonValue};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    thread,
    time::Duration,
};
use svc_agent::mqtt::AgentNotification;
use svc_agent::mqtt::{
    Agent, AgentBuilder, ConnectionMode, IncomingEvent, IncomingMessage, QoS, SubscriptionTopic,
};
use svc_agent::{AgentId, Authenticable, SharedGroup, Subscription};
use svc_authn::token::jws_compact;

type Error = std::io::Error;
type ErrorKind = std::io::ErrorKind;

use crate::app::config::TopMindConfig;
use crate::app::messaging_pattern::MessagingPattern;
use crate::app::top_mind_response::TopMindResponse;
pub(crate) const API_VERSION: &str = "v1";
const MAX_HTTP_CONNECTION: usize = 256;
const MAX_ATTEMPTS: u8 = 3;
const DEFAULT_HTTP_TIMEOUT: u64 = 5;

////////////////////////////////////////////////////////////////////////////////

fn json_flatten_prefix(key: &str, prefix: &str) -> String {
    if !prefix.is_empty() {
        [prefix, key].join(".")
    } else {
        key.to_owned()
    }
}

fn json_flatten(prefix: &str, json: &JsonValue, acc: &mut HashMap<String, JsonValue>) {
    if let Some(object) = json.as_object() {
        for (key, value) in object {
            if value.is_object() {
                json_flatten(&json_flatten_prefix(key, prefix), value, acc);
            } else {
                acc.insert(json_flatten_prefix(key, prefix), value.clone());
            }
        }
    }
}

fn json_flatten_one_level_deep(
    prefix: &str,
    json: &JsonValue,
    acc: &mut HashMap<String, JsonValue>,
) {
    if let Some(object) = json.as_object() {
        for (key, value) in object {
            if value.is_string() || value.is_number() || value.is_boolean() {
                acc.insert(json_flatten_prefix(key, prefix), value.clone());
            } else if value.is_object() && key == "tags" {
                json_flatten(&json_flatten_prefix(key, prefix), value, acc);
            }
        }
    }
}

fn adjust_request_properties(acc: &mut HashMap<String, JsonValue>) {
    acc.insert(String::from("properties.type"), json!("request"));
    adjust_properties(acc);
}

fn adjust_response_properties(acc: &mut HashMap<String, JsonValue>) {
    acc.insert(String::from("properties.type"), json!("response"));
    adjust_properties(acc);
}

fn adjust_event_properties(acc: &mut HashMap<String, JsonValue>) {
    acc.insert(String::from("properties.type"), json!("event"));
    adjust_properties(acc);
}

fn adjust_properties(acc: &mut HashMap<String, JsonValue>) {
    adjust_agent_id("properties.agent_id", acc);
    adjust_agent_id("properties.broker_agent_id", acc);
    adjust_tracking_id("properties.tracking_id", acc);
    replace_session_tracking_label("properties.session_tracking_label", acc);
    replace_integer("properties.status", acc);
    replace_integer("properties.broker_initial_processing_timestamp", acc);
    replace_integer("properties.broker_processing_timestamp", acc);
    replace_integer("properties.broker_timestamp", acc);
    replace_integer("properties.local_initial_timediff", acc);
    replace_integer("properties.initial_timestamp", acc);
    replace_integer("properties.timestamp", acc);
    replace_integer("properties.authorization_time", acc);
    replace_integer("properties.processing_time", acc);
    replace_integer("properties.cumulative_authorization_time", acc);
    replace_integer("properties.cumulative_processing_time", acc);
}

fn adjust_pattern(pattern: &MessagingPattern, acc: &mut HashMap<String, JsonValue>) {
    match pattern {
        MessagingPattern::Broadcast(_) => {
            adjust_account_id("pattern.from.account_id", acc);
        }
        MessagingPattern::Multicast(_) => {
            adjust_agent_id("pattern.from", acc);
            adjust_account_id("pattern.to.account_id", acc);
        }
        MessagingPattern::Unicast(_) => {
            adjust_account_id("pattern.from", acc);
            adjust_agent_id("pattern.to.agent_id", acc);
        }
    }
}

fn adjust_payload(acc: &mut HashMap<String, JsonValue>) {
    adjust_agent_id("payload.agent_id", acc);
    adjust_agent_id("payload.created_by", acc);
    adjust_useragent_tag("payload.tags.user_agent", acc);
}

fn adjust_useragent_tag(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(ua_str)) = acc.get(key) {
        let ua_str = ua_str.to_owned();
        append_ua_keys_to_json(&ua_str, key, acc);
    }
}

fn adjust_agent_id(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(val)) = acc.get(key) {
        let arr = val.splitn(2, '.').collect::<Vec<&str>>();
        if let [ref label, ref account_id] = &arr[..] {
            let label = json!(label);
            let account_id = json!(account_id);
            let next = json_flatten_prefix("account_id", key);
            acc.insert(json_flatten_prefix("label", key), label);
            acc.insert(next.clone(), account_id);
            adjust_account_id(&next, acc);
        }
    }
}

fn adjust_account_id(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(val)) = acc.get(key) {
        let arr = val.splitn(2, '.').collect::<Vec<&str>>();
        if let [ref label, ref audience] = &arr[..] {
            let label = json!(label);
            let audience = json!(audience);
            acc.insert(json_flatten_prefix("label", key), label);
            acc.insert(json_flatten_prefix("audience", key), audience);
        }
    }
}

fn adjust_tracking_id(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(val)) = acc.get(key) {
        let arr = val.splitn(2, '.').collect::<Vec<&str>>();
        if let [ref label, ref session_id] = &arr[..] {
            let label = json!(label);
            let session_id = json!(session_id);
            let next = json_flatten_prefix("session_id", key);
            acc.insert(json_flatten_prefix("label", key), label);
            acc.insert(next.clone(), session_id);
            adjust_session_id(&next, acc);
        }
    }
}

fn adjust_session_id(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(val)) = acc.get(key) {
        let arr = val.splitn(2, '.').collect::<Vec<&str>>();
        if let [ref agent_session_label, ref broker_session_label] = &arr[..] {
            let agent_session_label = json!(agent_session_label);
            let broker_session_label = json!(broker_session_label);
            acc.insert(
                json_flatten_prefix("agent_session_label", key),
                agent_session_label,
            );
            acc.insert(
                json_flatten_prefix("broker_session_label", key),
                broker_session_label,
            );
        }
    }
}

fn replace_session_tracking_label(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(val)) = acc.get(key) {
        let arr = val.split(' ').collect::<Vec<&str>>();
        let arr = json!(arr);
        acc.insert(key.to_owned(), arr);
    }
}

fn replace_integer(key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(JsonValue::String(val)) = acc.get(key) {
        if let Ok(integer) = val.parse::<i64>() {
            let integer = json!(integer);
            acc.insert(key.to_owned(), integer);
        }
    }
}

fn subscribe(agent: &mut Agent) {
    let group = SharedGroup::new("loadbalancer", agent.id().as_account_id().clone());
    // FIXME: Subscribe to all messages.
    // agent
    //     .subscribe(&"apps/+/api/+/#", QoS::AtMostOnce, Some(&group))
    //     .expect("Error subscribing to broadcast events");
    // agent
    //     .subscribe(&"agents/+/api/+/out/+", QoS::AtMostOnce, Some(&group))
    //     .expect("Error subscribing to multicast requests and events");
    // agent
    //     .subscribe(&"agents/+/api/+/in/+", QoS::AtMostOnce, Some(&group))
    //     .expect("Error subscribing to unicast requests and responses");

    // Subscribe to audience-level and telemetry events only.
    agent
        .subscribe(&"apps/+/api/+/audiences/#", QoS::AtMostOnce, Some(&group))
        .expect("Error subscribing to broadcast audience events");
    agent
        .subscribe(
            &Subscription::multicast_requests(Some(API_VERSION)),
            QoS::AtMostOnce,
            Some(&group),
        )
        .expect("Error subscribing to telemetry events");
}

////////////////////////////////////////////////////////////////////////////////

pub(crate) async fn run() -> Result<()> {
    // Config
    let config = config::load().context("Failed to load config")?;
    info!("App config: {:?}", config);

    // Agent
    let agent_id = AgentId::new(&config.agent_label, config.id.clone());
    info!("Agent id: {:?}", &agent_id);

    let token = jws_compact::TokenBuilder::new()
        .issuer(&agent_id.as_account_id().audience().to_string()) //?
        .subject(&agent_id)
        .key(config.id_token.algorithm, config.id_token.key.as_slice())
        .build()
        .context("Error creating an id token")?;

    let mut agent_config = config.mqtt.clone();
    agent_config.set_password(&token);

    let (mut agent, rx) = AgentBuilder::new(agent_id.clone(), API_VERSION)
        .connection_mode(ConnectionMode::Observer)
        .start(&agent_config)
        .context("Failed to create an agent")?;

    // Message loop for incoming messages of MQTT Agent
    let (mq_tx, mut mq_rx) = channel::unbounded();
    thread::spawn(move || {
        for message in rx {
            let mq_tx = mq_tx.clone();
            task::spawn(async move {
                if mq_tx.send(message).await.is_err() {
                    error!("Error sending message to the internal channel");
                }
            });
        }
    });

    // Subscription
    subscribe(&mut agent);

    // Http client
    let topmind = Arc::new(config.topmind);
    let timeout = std::time::Duration::from_secs(topmind.timeout.unwrap_or(DEFAULT_HTTP_TIMEOUT));
    let client = Arc::new(
        HttpClient::builder()
            .version_negotiation(VersionNegotiation::http11())
            .max_connections(MAX_HTTP_CONNECTION)
            .timeout(timeout)
            .build()?,
    );

    // Message loop
    let term_check_period = Duration::from_secs(1);
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::SIGTERM, Arc::clone(&term))?;
    signal_hook::flag::register(signal_hook::SIGINT, Arc::clone(&term))?;
    while !term.load(Ordering::Relaxed) {
        let fut = async_std::future::timeout(term_check_period, mq_rx.next());

        if let Ok(Some(message)) = fut.await {
            let client = client.clone();
            let mut agent = agent.clone();
            let agent_id = agent_id.clone();
            let topmind = topmind.clone();

            task::spawn(async move {
                match message {
                    AgentNotification::Message(message, metadata) => {
                        let topic: &str = &metadata.topic;

                        let result =
                            handle_message(&client, &agent_id, topic, &message, topmind.clone())
                                .await;

                        if let Err(err) = result {
                            error!(
                                "Error processing a message sent to the topic = '{topic}', {detail}",
                                topic = topic,
                                detail = err,
                            );
                        }
                    }
                    AgentNotification::Disconnection => error!("Disconnected from broker"),
                    AgentNotification::Reconnection => {
                        subscribe(&mut agent);
                    }
                    AgentNotification::Puback(_) => (),
                    AgentNotification::Pubrec(_) => (),
                    AgentNotification::Pubcomp(_) => (),
                    AgentNotification::Suback(_) => (),
                    AgentNotification::Unsuback(_) => (),
                    AgentNotification::Abort(err) => {
                        error!("{}", anyhow!("MQTT client aborted: {}", err))
                    }
                }
            });
        }
    }

    Ok(())
}

async fn handle_message(
    client: &HttpClient,
    agent_id: &AgentId,
    topic: &str,
    message: &Result<IncomingMessage<String>, String>,
    topmind: Arc<TopMindConfig>,
) -> Result<()> {
    let mut acc: HashMap<String, JsonValue> = HashMap::new();

    let pattern = topic
        .parse::<MessagingPattern>()
        .context("Failed to parse message pattern")?;
    let json_pattern =
        serde_json::to_value(pattern.clone()).context("Failed to serialize message pattern")?;
    json_flatten("pattern", &json_pattern, &mut acc);
    adjust_pattern(&pattern, &mut acc);

    match message {
        Ok(ref message) => match message {
            IncomingMessage::Request(ref req) => {
                let json_properties = serde_json::to_value(req.properties())
                    .context("Failed to serialize message properties")?;
                json_flatten("properties", &json_properties, &mut acc);
                adjust_request_properties(&mut acc);

                // NOTE: We don't parse message payload for requests & responses.
                // let json_payload = IncomingRequest::convert_payload::<JsonValue>(req)
                //     .context("Failed to serialize message payload")?;
                // // For any request: send only first level key/value pairs from the message payload.
                // json_flatten_one_level_deep("payload", &json_payload, &mut acc);
                // adjust_payload(&mut acc);

                let payload = serde_json::to_value(acc)?;
                try_send(&client, payload, topmind).await
            }
            IncomingMessage::Response(ref resp) => {
                let json_properties = serde_json::to_value(resp.properties())
                    .context("Failed to serialize message properties")?;
                json_flatten("properties", &json_properties, &mut acc);
                adjust_response_properties(&mut acc);

                // NOTE: We don't parse message payload for requests & responses.
                // let json_payload = IncomingResponse::convert_payload::<JsonValue>(resp)
                //     .context("Failed to serialize message payload")?;
                // // For any response: send only first level key/value pairs from the message payload.
                // json_flatten_one_level_deep("payload", &json_payload, &mut acc);
                // adjust_payload(&mut acc);

                let payload = serde_json::to_value(acc)?;
                try_send(&client, payload, topmind).await
            }
            IncomingMessage::Event(ref event) => {
                let json_properties = serde_json::to_value(event.properties())
                    .context("Failed to serialize message properties")?;
                json_flatten("properties", &json_properties, &mut acc);
                adjust_event_properties(&mut acc);

                let json_payload = IncomingEvent::convert_payload::<JsonValue>(event)
                    .context("Failed to serialize message payload")?;

                let telemetry_topic =
                    Subscription::multicast_requests_from(event.properties(), Some(API_VERSION))
                        .subscription_topic(agent_id, API_VERSION)
                        .context("Error building telemetry subscription topic")?;

                // Telemetry only events: send entire payload.
                if topic == telemetry_topic {
                    if let Some(json_payload_array) = json_payload.as_array() {
                        // Send multiple metrics.
                        for json_payload_object in json_payload_array {
                            let topmind = topmind.clone();
                            let mut acc2 = acc.clone();
                            json_flatten("payload", &json_payload_object, &mut acc2);
                            adjust_payload(&mut acc);

                            let payload = serde_json::to_value(acc2)
                                .context("Failed to serialize message payload")?;
                            try_send(&client, payload, topmind).await?
                        }
                    } else {
                        // Send a single metric.
                        json_flatten("payload", &json_payload, &mut acc);
                        adjust_payload(&mut acc);

                        let payload = serde_json::to_value(acc)
                            .context("Failed to serialize message payload")?;
                        try_send(&client, payload, topmind).await?
                    }
                }
                // All the other events: send only first level key/value pairs from the message payload.
                else {
                    json_flatten_one_level_deep("payload", &json_payload, &mut acc);
                    adjust_payload(&mut acc);

                    let payload =
                        serde_json::to_value(acc).context("Failed to serialize message payload")?;
                    try_send(&client, payload, topmind).await?
                }

                Ok(())
            }
        },
        Err(e) => Err(format_err!(e.to_owned()).context("Failed to parse message envelope")),
    }
}

async fn try_send(
    client: &HttpClient,
    payload: JsonValue,
    topmind: Arc<TopMindConfig>,
) -> Result<()> {
    let retry = topmind.retry.unwrap_or(MAX_ATTEMPTS);
    let mut errors = vec![];
    for _ in 0..retry {
        let payload = payload.clone();
        let topmind = topmind.clone();

        match send(client, payload, topmind).await {
            ok @ Ok(_) => return ok,
            Err(err) => errors.push(format!("{:?}", err)),
        }
    }

    errors.dedup();
    Err(Error::new(ErrorKind::Other, errors.join(", ")).into())
}

async fn send(client: &HttpClient, payload: JsonValue, topmind: Arc<TopMindConfig>) -> Result<()> {
    use isahc::prelude::*;

    let tracking_id = payload
        .get("properties.tracking_id")
        .map(|val| val.to_string())
        .unwrap_or_else(|| String::from("None"));
    let body = serde_json::to_string(&payload).context("Failed to build TopMind request")?;
    let req = Request::post(&topmind.uri)
        .header("authorization", format!("Bearer {}", topmind.token))
        .header("content-type", "application/json")
        // Must not be used with HTTP/2.
        .header("connection", "keep-alive")
        .header("user-agent", "telemetry")
        .body(body)?;

    let mut resp = client.send_async(req).await.context(format!(
        "Error sending the TopMind request with tracking_id={}",
        tracking_id
    ))?;
    let data = resp
        .text_async()
        .await
        .context("Invalid format of the TopMind response, received data isn't even a string")?;
    let object = serde_json::from_str::<TopMindResponse>(&data).with_context(|| {
        format!(
            "Invalid format of the TopMind response, received data = '{}'",
            data
        )
    })?;
    if let TopMindResponse::Error(data) = object {
        return Err(anyhow::Error::from(data).context("TopMind responded with the error"));
    }

    Ok(())
}

fn append_ua_keys_to_json(ua_str: &str, key: &str, acc: &mut HashMap<String, JsonValue>) {
    if let Some(ua) = woothee::parser::Parser::new().parse(ua_str) {
        acc.insert(
            json_flatten_prefix("name", key),
            serde_json::to_value(ua.name).expect("String serialization cant fail"),
        );
        acc.insert(
            json_flatten_prefix("category", key),
            serde_json::to_value(ua.category).expect("String serialization cant fail"),
        );
        acc.insert(
            json_flatten_prefix("os", key),
            serde_json::to_value(ua.os).expect("String serialization cant fail"),
        );
        acc.insert(
            json_flatten_prefix("os_version", key),
            serde_json::to_value(ua.os_version.as_ref()).expect("String serialization cant fail"),
        );
        acc.insert(
            json_flatten_prefix("browser_type", key),
            serde_json::to_value(ua.browser_type).expect("String serialization cant fail"),
        );
        acc.insert(
            json_flatten_prefix("version", key),
            serde_json::to_value(ua.version).expect("String serialization cant fail"),
        );
        acc.insert(
            json_flatten_prefix("vendor", key),
            serde_json::to_value(ua.vendor).expect("String serialization cant fail"),
        );
    }
}

////////////////////////////////////////////////////////////////////////////////

mod config;
mod messaging_pattern;
mod top_mind_response;
